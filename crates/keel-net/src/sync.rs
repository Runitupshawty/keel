//! Library sync between paired devices: each device pulls the other's tag, favorite and
//! content-id changes (keel-core's sync log) on connect and every
//! `NodeOptions::sync_every`, in pages of at most `keel_core::SYNC_PAGE`.
//!
//! Both sides must turn it on (`Node::set_sync_peers`): a device pulls only from devices
//! it syncs with, and answers `Request::SyncPull` only for them (`Response::Denied`
//! otherwise). The puller names the entries' device itself (the connection proves it),
//! applies at most `NodeOptions::sync_rate` entries per device and minute (the rest wait
//! for the next minute), and stops when the switch goes off or the device is forgotten;
//! what was received stays in the library.

use crate::*;
use anyhow::{bail, ensure, Result};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

/// `[devices] sync_secs` by default.
pub const SYNC_EVERY: Duration = Duration::from_secs(60);
/// Entries applied from one device per minute, by default.
pub const SYNC_RATE: usize = 10_000;
const MINUTE: Duration = Duration::from_secs(60);
/// Devices named in one `SyncPull` (only the host's own entry is read).
const MAX_SINCE: usize = 64;

#[derive(Default)]
pub(crate) struct SyncState {
    /// The devices sync is on with.
    peers: HashSet<PeerId>,
    /// One pull at a time per device.
    pulls: HashMap<PeerId, Arc<tokio::sync::Mutex<()>>>,
    /// Per device: when its minute started and the entries applied in it.
    budget: HashMap<PeerId, (Instant, usize)>,
}

impl SyncState {
    /// A forgotten device: sync with it is off.
    pub(crate) fn forget(&mut self, peer: &PeerId) {
        self.peers.remove(peer);
        self.budget.remove(peer);
    }
}

/// Pulls from every device sync is on with, every `sync_every` and when woken (a device
/// connected, sync was turned on), until the node closes. A `sync_every` of zero leaves
/// pulls to `Node::sync_now`.
pub(crate) fn spawn_loop(node: &Arc<Node>) {
    if node.options.sync_every.is_zero() {
        return;
    }
    let (weak, stop) = (node.weak.clone(), node.stop.clone());
    let (every, wake) = (node.options.sync_every, node.sync_wake.clone());
    node.tasks.spawn(async move {
        loop {
            {
                let Some(node) = weak.upgrade() else { break };
                for peer in node.sync_peers() {
                    let weak = weak.clone();
                    // A device already being pulled is left to that pull.
                    let Ok(pulling) = node.pull_lock(&peer).try_lock_owned() else {
                        continue;
                    };
                    node.tasks.spawn(async move {
                        let Some(node) = weak.upgrade() else { return };
                        if let Err(e) = node.sync_locked(&peer, pulling).await {
                            tracing::debug!(peer = %peer.0, "library sync: {e:#}");
                        }
                    });
                }
            }
            tokio::select! { biased;
                _ = stop.cancelled() => break,
                _ = wake.notified() => {}
                _ = tokio::time::sleep(every) => {}
            }
        }
    });
}

impl Node {
    /// The devices library sync is on with (Settings → Devices, `[devices] sync`); pulls
    /// from a device just added start at once. Devices that are not paired are ignored.
    pub fn set_sync_peers(&self, peers: impl IntoIterator<Item = PeerId>) {
        let peers: HashSet<PeerId> = peers.into_iter().collect();
        let added = {
            let mut s = self.sync.lock();
            let added = peers.iter().any(|p| !s.peers.contains(p));
            s.budget.retain(|p, _| peers.contains(p));
            s.pulls.retain(|p, _| peers.contains(p));
            s.peers = peers;
            added
        };
        if added {
            self.sync_wake.notify_one();
        }
    }

    /// The paired devices library sync is on with.
    pub fn sync_peers(&self) -> Vec<PeerId> {
        let on = self.sync.lock().peers.clone();
        let state = self.state.lock();
        state
            .data
            .peers
            .iter()
            .map(|r| r.peer.id)
            .filter(|p| on.contains(p))
            .collect()
    }

    /// Whether library sync is on with `peer` (and it is paired).
    pub fn syncs_with(&self, peer: &PeerId) -> bool {
        let on = self.sync.lock().peers.contains(peer);
        on && self
            .state
            .lock()
            .data
            .peers
            .iter()
            .any(|r| &r.peer.id == peer)
    }

    /// Takes up to `want` entries of `peer`'s budget for this minute.
    fn take_budget(&self, peer: &PeerId, want: usize) -> usize {
        let rate = self.options.sync_rate;
        let mut s = self.sync.lock();
        let (start, used) = s.budget.entry(*peer).or_insert((Instant::now(), 0));
        if start.elapsed() >= MINUTE {
            *start = Instant::now();
            *used = 0;
        }
        let n = want.min(rate.saturating_sub(*used));
        *used += n;
        n
    }

    /// Answers a `SyncPull`: this device's changes after the sequence number given for it.
    pub(crate) async fn serve_sync(
        &self,
        peer: PeerId,
        since: Vec<(NodeId, u64)>,
    ) -> Result<Response> {
        if !self.syncs_with(&peer) {
            return Ok(Response::Denied("library sync is off".into()));
        }
        ensure!(since.len() <= MAX_SINCE, "too many devices in a pull");
        let me = self.id();
        let since = since.iter().find(|(d, _)| *d == me).map_or(0, |(_, s)| *s);
        let h = self.handler.clone();
        let mut page =
            tokio::task::spawn_blocking(move || h.sync_page(since, keel_core::SYNC_PAGE)).await??;
        let me = me.to_string();
        for e in &mut page.entries {
            e.device = me.clone();
        }
        Ok(Response::SyncEntries {
            entries: page.entries,
            more: page.more,
            upto: page.upto,
        })
    }

    fn pull_lock(&self, peer: &PeerId) -> Arc<tokio::sync::Mutex<()>> {
        self.sync.lock().pulls.entry(*peer).or_default().clone()
    }

    /// Pulls `peer`'s changes now (after a pull already under way), page after page
    /// within its budget, and applies them. Returns how many entries changed the library.
    /// Fails when sync with `peer` is off here, or the other device refuses (its switch
    /// for this device is off).
    pub async fn sync_now(&self, peer: &PeerId) -> Result<usize> {
        ensure!(
            self.syncs_with(peer),
            "library sync with this device is off"
        );
        let pulling = self.pull_lock(peer).lock_owned().await;
        self.sync_locked(peer, pulling).await
    }

    async fn sync_locked(
        &self,
        peer: &PeerId,
        _pulling: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<usize> {
        ensure!(
            self.syncs_with(peer),
            "library sync with this device is off"
        );
        let mut applied = 0;
        let result = self.pull(peer, &mut applied).await;
        if applied > 0 {
            self.emit(NetEvent::LibrarySynced {
                peer: *peer,
                applied,
            });
        }
        result.map(|()| applied)
    }

    async fn pull(&self, peer: &PeerId, applied: &mut usize) -> Result<()> {
        let device = peer.0.to_string();
        // Turned off or forgotten meanwhile: stop.
        while self.syncs_with(peer) {
            let (h, p) = (self.handler.clone(), *peer);
            let since = tokio::task::spawn_blocking(move || h.sync_since(&p)).await??;
            let pull = Request::SyncPull {
                since: vec![(peer.0, since)],
            };
            let (mut entries, mut more, mut upto) = match self.request(peer, pull).await? {
                Response::SyncEntries {
                    entries,
                    more,
                    upto,
                } => (entries, more, upto),
                Response::Denied(why) => bail!("the device refused: {why}"),
                Response::Error(e) => bail!("the device failed: {e}"),
                _ => bail!("unexpected answer to a sync pull"),
            };
            ensure!(
                entries.len() <= keel_core::SYNC_PAGE
                    && upto >= since
                    && entries.windows(2).all(|w| w[0].seq < w[1].seq)
                    && entries.iter().all(|e| e.seq > since && e.seq <= upto),
                "malformed sync page"
            );
            let n = self.take_budget(peer, entries.len());
            let limited = n < entries.len();
            if limited {
                entries.truncate(n);
                upto = entries.last().map_or(since, |e| e.seq);
                more = true;
            }
            for e in &mut entries {
                e.device = device.clone();
            }
            let page = keel_core::SyncPage {
                entries,
                more,
                upto,
            };
            let (h, p) = (self.handler.clone(), *peer);
            let done = tokio::task::spawn_blocking(move || h.sync_apply(&p, &page)).await??;
            *applied += done.applied;
            if limited {
                tracing::debug!(peer = %peer.0, "library sync: rate limit reached; the rest waits");
            }
            if !more || limited {
                break;
            }
        }
        Ok(())
    }
}
