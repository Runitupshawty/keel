use crate::{
    store::{Record, Session, State},
    *,
};
use anyhow::{bail, ensure, Context, Result};
use iroh::{
    endpoint::{presets, Builder, Connection},
    Endpoint, SecretKey,
};
use parking_lot::Mutex;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub const ALPN: &[u8] = b"keel/net/1";

/// Transport configuration, shared by the permanent and pairing endpoints.
#[derive(Clone, Debug)]
pub struct NodeOptions {
    pub relay_mode: iroh::RelayMode,
    pub discovery: bool,
    pub bind_addr: Option<SocketAddr>,
    pub relay_only: bool,
    pub request_timeout: Duration,
}
impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            relay_mode: iroh::RelayMode::Default,
            discovery: true,
            bind_addr: None,
            relay_only: false,
            request_timeout: Duration::from_secs(20),
        }
    }
}
impl NodeOptions {
    /// Turns the default (public) relays off, or back on (for a settings toggle).
    pub fn with_relay(mut self, on: bool) -> Self {
        self.relay_mode = if on {
            iroh::RelayMode::Default
        } else {
            iroh::RelayMode::Disabled
        };
        self
    }
    /// No DNS, public relay, port mapping or non-loopback socket.
    pub fn offline() -> Self {
        Self {
            relay_mode: iroh::RelayMode::Disabled,
            discovery: false,
            bind_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            relay_only: false,
            request_timeout: Duration::from_secs(3),
        }
    }
    pub(crate) fn builder(&self, key: SecretKey) -> Result<Builder> {
        ensure!(
            !(self.relay_only && matches!(self.relay_mode, iroh::RelayMode::Disabled)),
            "relay-only needs a relay"
        );
        let mut builder = if self.discovery {
            Endpoint::builder(presets::N0)
        } else {
            Endpoint::builder(presets::Minimal)
        };
        // Never consult or install the rustls process default (cloud uses graviola).
        builder = builder
            .crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(self.relay_mode.clone());
        if self.relay_only || self.bind_addr.is_some() {
            builder = builder.clear_ip_transports();
        }
        if !self.relay_only {
            if let Some(addr) = self.bind_addr {
                builder = builder.bind_addr(addr)?;
            }
        }
        Ok(builder)
    }
}

pub struct Node {
    pub(crate) endpoint: Endpoint,
    pub(crate) options: NodeOptions,
    pub(crate) handler: Arc<dyn Handler>,
    pub(crate) state: Mutex<State>,
    pub(crate) stop: CancellationToken,
    pub(crate) tasks: TaskTracker,
    pub(crate) weak: Weak<Node>,
    pub(crate) pairing: tokio::sync::Mutex<Option<crate::pairing::Invitation>>,
    subscribers: Mutex<Vec<crossbeam_channel::Sender<NetEvent>>>,
    /// Spacedrop: accepted incoming drops by id.
    pub(crate) drops: Mutex<std::collections::HashMap<String, crate::spacedrop::Incoming>>,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub(crate) fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("random source unavailable"))?;
    Ok(bytes)
}

impl Node {
    pub async fn open(
        secrets: Arc<dyn keel_vfs::cloud::SecretStore>,
        data_dir: &Path,
        handler: Arc<dyn Handler>,
    ) -> Result<Arc<Node>> {
        Self::open_with_options(secrets, data_dir, handler, NodeOptions::default()).await
    }
    pub async fn open_with_options(
        secrets: Arc<dyn keel_vfs::cloud::SecretStore>,
        data_dir: &Path,
        handler: Arc<dyn Handler>,
        options: NodeOptions,
    ) -> Result<Arc<Node>> {
        let secret = tokio::task::spawn_blocking(move || -> Result<SecretKey> {
            let bytes = match secrets.get("net/node-secret")? {
                Some(s) => data_encoding::BASE32_NOPAD
                    .decode(s.as_bytes())
                    .context("invalid stored node secret")?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid stored node secret length"))?,
                None => {
                    let bytes = random::<32>()?;
                    secrets.set(
                        "net/node-secret",
                        &data_encoding::BASE32_NOPAD.encode(&bytes),
                    )?;
                    bytes
                }
            };
            Ok(SecretKey::from_bytes(&bytes))
        })
        .await??;
        let state = State::open(data_dir)?;
        let endpoint = options.builder(secret)?.bind().await?;
        let node = Arc::new_cyclic(|weak| Self {
            endpoint,
            options,
            handler,
            state: Mutex::new(state),
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
            weak: weak.clone(),
            pairing: tokio::sync::Mutex::new(None),
            subscribers: Mutex::new(Vec::new()),
            drops: Mutex::default(),
        });
        crate::spacedrop::register_node(&node);
        let ep = node.endpoint.clone();
        let weak = node.weak.clone();
        let stop = node.stop.clone();
        node.tasks.spawn(async move {
            loop {
                let incoming = tokio::select! { biased; _ = stop.cancelled() => break, incoming = ep.accept() => match incoming { Some(i) => i, None => break } };
                let Some(node) = weak.upgrade() else { break };
                let weak = weak.clone(); let stop = stop.clone(); let timeout = node.options.request_timeout;
                node.tasks.spawn(async move {
                    let conn = tokio::select! { biased; _ = stop.cancelled() => return, conn = tokio::time::timeout(timeout, incoming) => match conn { Ok(Ok(c)) => c, _ => return } };
                    if let Some(node) = weak.upgrade() {
                        if let Ok(cancel) = node.register(&conn) { node.serve_connection(conn, cancel).await; }
                    }
                });
            }
        });
        Ok(node)
    }
    pub fn id(&self) -> NodeId {
        NodeId(*self.endpoint.id().as_bytes())
    }
    pub fn label(&self) -> String {
        self.state.lock().data.label.clone()
    }
    /// Best effort because the specified API has no error return. Prefer
    /// `try_set_label` if the caller needs to report persistence failure.
    pub fn set_label(&self, s: &str) {
        if self.try_set_label(s).is_err() {
            tracing::warn!("could not save node label");
        }
    }
    pub fn try_set_label(&self, s: &str) -> Result<()> {
        ensure!(
            s.len() <= 256 && !s.chars().any(char::is_control),
            "invalid label"
        );
        let mut state = self.state.lock();
        ensure!(!state.closed, "node closed");
        state.update(|d| d.label = s.into())
    }
    pub fn peers(&self) -> Vec<Peer> {
        self.state
            .lock()
            .data
            .peers
            .iter()
            .map(|r| r.peer.clone())
            .collect()
    }
    pub fn grants(&self) -> Vec<Grant> {
        self.state.lock().data.grants.clone()
    }
    /// Each call subscribes independently (capacity 256). Slow consumers can lose
    /// events; refresh `peers()` / `grants()` to obtain the current state.
    pub fn events(&self) -> crossbeam_channel::Receiver<NetEvent> {
        let (tx, rx) = crossbeam_channel::bounded(256);
        self.subscribers.lock().push(tx);
        rx
    }
    pub(crate) fn emit(&self, event: NetEvent) {
        self.subscribers.lock().retain(|tx| {
            !matches!(
                tx.try_send(event.clone()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
    }
    pub fn grant(&self, g: Grant) -> Result<()> {
        ensure!(
            !g.source.is_empty() && g.source.len() <= 1024 && scope::valid_path(&g.subtree),
            "invalid grant scope"
        );
        let mut state = self.state.lock();
        ensure!(!state.closed, "node closed");
        ensure!(
            state.data.peers.iter().any(|r| r.peer.id == g.peer),
            "unknown peer"
        );
        let peer = g.peer;
        let reduces_access = g.access == Access::Read
            && state.data.grants.iter().any(|old| {
                old.peer == peer
                    && old.source == g.source
                    && old.subtree == g.subtree
                    && old.access == Access::ReadWrite
            });
        state.update(|d| {
            d.grants.retain(|old| {
                !(old.peer == peer && old.source == g.source && old.subtree == g.subtree)
            });
            d.grants.push(g);
        })?;
        if reduces_access {
            state.disconnect(&peer);
        }
        drop(state);
        self.emit(NetEvent::GrantChanged);
        Ok(())
    }
    pub fn revoke(&self, peer: &PeerId, source: &str, subtree: &str) -> Result<()> {
        let mut state = self.state.lock();
        ensure!(!state.closed, "node closed");
        state.update(|d| {
            d.grants
                .retain(|g| !(&g.peer == peer && g.source == source && g.subtree == subtree))
        })?;
        state.disconnect(peer);
        drop(state);
        self.emit(NetEvent::GrantChanged);
        Ok(())
    }
    pub fn forget_peer(&self, peer: &PeerId) -> Result<()> {
        let mut state = self.state.lock();
        ensure!(!state.closed, "node closed");
        state.update(|d| {
            d.peers.retain(|r| &r.peer.id != peer);
            d.grants.retain(|g| &g.peer != peer);
        })?;
        state.disconnect(peer);
        drop(state);
        self.emit(NetEvent::PeerOffline(*peer));
        self.emit(NetEvent::GrantChanged);
        Ok(())
    }
    pub(crate) fn paired(&self, addr: iroh::EndpointAddr, label: String) -> Result<Peer> {
        ensure!(
            label.len() <= 256 && !label.chars().any(char::is_control),
            "invalid label"
        );
        let peer = Peer {
            id: PeerId(NodeId(*addr.id.as_bytes())),
            label,
            last_seen: Some(now()),
            link: Link::Offline,
            storage: None,
        };
        ensure!(peer.id.0 != self.id(), "cannot pair with self");
        let mut state = self.state.lock();
        ensure!(!state.closed, "node closed");
        // Re-pairing never changes grants. Only explicit grant/revoke does that.
        state.update(|d| {
            d.peers.retain(|r| r.peer.id != peer.id);
            d.peers.push(Record {
                peer: peer.clone(),
                addr,
            });
        })?;
        drop(state);
        self.emit(NetEvent::Paired(peer.clone()));
        Ok(peer)
    }
    pub(crate) fn register(&self, conn: &Connection) -> Result<CancellationToken> {
        let peer = PeerId(NodeId(*conn.remote_id().as_bytes()));
        let mut state = self.state.lock();
        if state.closed || !state.data.peers.iter().any(|r| r.peer.id == peer) {
            tracing::debug!(peer = %peer.0, what = "connect", allowed = false, "net request");
            conn.close(1u8.into(), b"unpaired");
            bail!("unpaired or closed");
        }
        let sessions = state.sessions.entry(peer).or_default();
        if let Some(s) = sessions.get(&conn.stable_id()) {
            return Ok(s.cancel.clone());
        }
        let cancel = self.stop.child_token();
        sessions.insert(
            conn.stable_id(),
            Session {
                conn: conn.clone(),
                cancel: cancel.clone(),
            },
        );
        drop(state);
        self.observe(peer, conn);
        let weak = self.weak.clone();
        let conn = conn.clone();
        let token = cancel.clone();
        self.tasks.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! { biased;
                    _ = token.cancelled() => break,
                    _ = conn.closed() => break,
                    _ = interval.tick() => { if let Some(node) = weak.upgrade() { node.observe(peer, &conn); } else { break; } }
                }
            }
            token.cancel();
            if let Some(node) = weak.upgrade() {
                let mut state = node.state.lock();
                if let Some(sessions) = state.sessions.get_mut(&peer) { sessions.remove(&conn.stable_id()); }
                if state.sessions.get(&peer).is_none_or(|s| s.is_empty()) {
                    if let Some(r) = state.data.peers.iter_mut().find(|r| r.peer.id == peer) { r.peer.link = Link::Offline; }
                    drop(state); node.emit(NetEvent::PeerOffline(peer));
                }
            }
        });
        Ok(cancel)
    }
    fn observe(&self, peer: PeerId, conn: &Connection) {
        let link = if conn.paths().iter().any(|p| p.is_ip()) {
            Link::Lan
        } else {
            Link::Relay
        };
        let mut state = self.state.lock();
        if let Some(r) = state.data.peers.iter_mut().find(|r| r.peer.id == peer) {
            let changed = r.peer.link != link;
            r.peer.link = link;
            r.peer.last_seen = Some(now());
            drop(state);
            if changed {
                self.emit(NetEvent::PeerOnline(peer, link));
            }
        }
    }
    pub(crate) async fn connect(&self, peer: &PeerId) -> Result<Connection> {
        let addr = {
            let state = self.state.lock();
            ensure!(!state.closed, "node closed");
            let record = state
                .data
                .peers
                .iter()
                .find(|r| &r.peer.id == peer)
                .context("unknown peer")?;
            record.addr.clone()
        };
        let conn = tokio::time::timeout(
            self.options.request_timeout,
            self.endpoint.connect(addr, ALPN),
        )
        .await??;
        let cancel = self.register(&conn)?;
        let weak = self.weak.clone();
        let serving = conn.clone();
        self.tasks.spawn(async move {
            if let Some(node) = weak.upgrade() {
                node.serve_connection(serving, cancel).await;
            }
        });
        Ok(conn)
    }
    pub async fn close(&self) {
        {
            let mut state = self.state.lock();
            state.closed = true;
            for peer in state.sessions.keys().copied().collect::<Vec<_>>() {
                state.disconnect(&peer);
            }
        }
        self.stop.cancel();
        if let Some(invite) = self.pairing.lock().await.take() {
            invite.stop.cancel();
            invite.endpoint.close().await;
        }
        self.endpoint.close().await;
        self.tasks.close();
        self.tasks.wait().await;
    }
}
