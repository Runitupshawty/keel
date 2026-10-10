//! Paired devices (spec 2.10, Task 36): the keel-net node serving the open library, the
//! sidebar's Devices section, the pair and shares dialogs, Spacedrop (send, the incoming
//! offer prompt) and Settings → Devices. The node runs on its own tokio runtime; opening,
//! pairing and closing run on workers, never on the UI thread.
//!
//! The node opens once the library is open (it serves the library's sources and runs
//! Spacedrop as library jobs) and Settings → Devices is on. Devices are off by default:
//! turning them on creates this device's identity, in the OS keychain;
//! `KEEL_NET_SECRET=memory` keeps it in memory instead (a developer setting for tests and
//! live checks: the device gets a new identity each run).

use crate::backend::{Remote, Rpc};
use crate::keys::Action;
use crate::state::{AppState, Msg};
use crate::worker;
use anyhow::Context as _;
use crossbeam_channel::{Receiver, Sender};
use keel_api::types as api;
use keel_core::{Library, SourceId, SourceKind, SourceSummary};
use keel_net::{
    Access, Grant, IncomingDrop, LibraryHandler, Link, NetEvent, Node, NodeOptions, NodeProvider,
    PairCode, Peer, PeerId, Request,
};
use keel_vfs::{Router, VPath};
use parking_lot::RwLock;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Paired devices are pinged this often (link state and storage in the sidebar).
const PING_EVERY: Duration = Duration::from_secs(30);
/// Events waiting for the UI (`forward`).
const FORWARD_BACKLOG: usize = 256;
/// While a drop prompt is up, this often the UI checks whether it was withdrawn.
const PROMPT_CHECK: Duration = Duration::from_millis(250);

/// Settings → Devices (`[devices]` in config.toml).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    /// Off until the user turns Devices on (that creates the device identity).
    pub enabled: bool,
    /// This device's name on the others ("" = keep the current one).
    pub label: String,
    /// The Spacedrop inbox ("" = `<Downloads>/Keel Drops`, or `<data dir>/inbox` without a
    /// Downloads folder).
    pub inbox: String,
    /// Devices (ids) whose drops are accepted without asking.
    pub auto_accept: Vec<String>,
    /// Public relays when no direct path works (applies when the node opens).
    pub relay: bool,
    /// `enabled` is the user's choice (the Settings switch sets this). Configs saved while
    /// Devices defaulted on carry `enabled = true` without it: `migrate` turns those off.
    pub explicit: bool,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            label: String::new(),
            inbox: String::new(),
            auto_accept: Vec::new(),
            relay: true,
            explicit: false,
        }
    }
}

impl DeviceSettings {
    /// Once, for a config saved while Devices defaulted on: an `enabled = true` the user
    /// never chose becomes off (and the next save records `explicit = true`).
    pub fn migrate(&mut self) {
        if self.enabled && !self.explicit {
            self.enabled = false;
            self.explicit = true;
        }
    }

    pub fn inbox_dir(&self) -> Option<PathBuf> {
        inbox_in(&self.inbox, downloads(), keel_core::data_dir())
    }
}

/// The user's Downloads folder, if the desktop has one.
fn downloads() -> Option<PathBuf> {
    Some(directories::UserDirs::new()?.download_dir()?.to_owned())
}

/// The inbox: `set` when given, else `<downloads>/Keel Drops`, else `<data>/inbox`.
pub fn inbox_in(set: &str, downloads: Option<PathBuf>, data: Option<PathBuf>) -> Option<PathBuf> {
    match set.trim() {
        "" => downloads
            .map(|d| d.join("Keel Drops"))
            .or_else(|| data.map(|d| d.join("inbox"))),
        dir => Some(dir.into()),
    }
}

/// The offer prompt: who, how many files and bytes (sums saturate: the sizes come from the
/// other device), and the first few names.
pub fn offer_text(from: &str, files: &[(String, u64)]) -> String {
    let bytes = files
        .iter()
        .fold(0u64, |sum, (_, n)| sum.saturating_add(*n));
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    offer_summary(from, files.len() as u64, bytes, &names)
}

/// The offer prompt from counts and the first names (an offer waiting in keel-daemon).
pub fn offer_summary(from: &str, files: u64, bytes: u64, names: &[&str]) -> String {
    const SHOWN: usize = 3;
    let mut text = format!(
        "{from} wants to send {files} file(s) ({})",
        humansize::format_size(bytes, humansize::DECIMAL)
    );
    for name in names.iter().take(SHOWN) {
        text += &format!("\n• {name}");
    }
    let shown = names.len().min(SHOWN) as u64;
    if files > shown {
        text += &format!("\n…and {} more", files - shown);
    }
    text
}

/// Node events into a channel the UI drains each frame, asking for a repaint as each one
/// arrives (an idle UI otherwise sleeps). Ends when the node or the UI side goes. Bounded
/// like the node's own backlog: while the UI does not drain it, this thread waits and the
/// node's backlog rules apply (requests dropped, pairings and received files kept).
pub fn forward(events: Receiver<NetEvent>, ctx: egui::Context) -> Receiver<NetEvent> {
    let (tx, rx) = crossbeam_channel::bounded(FORWARD_BACKLOG);
    let _ = std::thread::Builder::new()
        .name("keel-net-events".into())
        .spawn(move || {
            for ev in events {
                if tx.send(ev).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
    rx
}

/// `node://<id>/`: a device's granted sources.
pub fn root_of(peer: &PeerId) -> VPath {
    VPath::parse(&format!("node://{}/", peer.0)).expect("a node id is a valid authority")
}

/// One sidebar row (rebuilt from the node's peers every frame).
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRow {
    pub id: PeerId,
    pub label: String,
    pub link: Link,
    /// (used, total) bytes, as the device last said.
    pub storage: Option<(u64, u64)>,
    pub root: VPath,
}

fn short(peer: &PeerId) -> String {
    peer.0.to_string().chars().take(8).collect()
}

/// A device's label, or the start of its id when it has none.
pub fn label_of(peers: &[Peer], peer: &PeerId) -> String {
    peers
        .iter()
        .find(|p| p.id == *peer)
        .map(|p| p.label.trim().to_owned())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| short(peer))
}

pub fn device_rows(peers: &[Peer]) -> Vec<DeviceRow> {
    let mut rows: Vec<DeviceRow> = peers
        .iter()
        .map(|p| DeviceRow {
            id: p.id,
            label: label_of(peers, &p.id),
            link: p.link,
            storage: p.storage.as_ref().map(|s| (s.used, s.total)),
            root: root_of(&p.id),
        })
        .collect();
    rows.sort_by_key(|r| (r.label.to_lowercase(), r.id.0.to_string()));
    rows
}

/// The pair dialog.
#[derive(Clone, Debug, PartialEq)]
pub enum Pair {
    Choose,
    /// Waiting for this device's code.
    Requesting,
    /// Showing the code until the other device joins.
    Showing {
        short: String,
        ticket: String,
    },
    Entering {
        text: String,
        error: Option<String>,
    },
    Joining {
        text: String,
    },
    Paired(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PairEvent {
    ShowCode,
    /// (short code, full ticket) or why there is none.
    Code(Result<(String, String), String>),
    EnterCode,
    Submit,
    /// Joined the device with this label, or why not.
    Joined(Result<String, String>),
    /// The device that used this one's code (its label).
    PairedWith(String),
    Back,
}

impl Pair {
    /// The dialog's next state (side effects: `Requesting` asks for a code, `Joining`
    /// pairs, both on a worker).
    pub fn next(self, ev: PairEvent) -> Pair {
        use PairEvent as E;
        match (self, ev) {
            (_, E::Back) => Pair::Choose,
            (Pair::Choose, E::ShowCode) => Pair::Requesting,
            (Pair::Requesting, E::Code(Ok((short, ticket)))) => Pair::Showing { short, ticket },
            (Pair::Requesting, E::Code(Err(e))) => Pair::Failed(e),
            (Pair::Choose, E::EnterCode) => Pair::Entering {
                text: String::new(),
                error: None,
            },
            (Pair::Entering { text, .. }, E::Submit) => match text.trim().parse::<PairCode>() {
                Ok(_) => Pair::Joining { text },
                Err(_) => Pair::Entering {
                    text,
                    error: Some("That is not a pairing code".into()),
                },
            },
            (Pair::Joining { .. }, E::Joined(Ok(label))) => Pair::Paired(label),
            (Pair::Joining { text }, E::Joined(Err(e))) => Pair::Entering {
                text,
                error: Some(e),
            },
            (Pair::Showing { .. }, E::PairedWith(label)) => Pair::Paired(label),
            (state, _) => state,
        }
    }
}

/// The ticket as a QR code (4 px modules, 4-module quiet zone).
pub fn qr_image(ticket: &str) -> Option<egui::ColorImage> {
    let code = qrcode::QrCode::new(ticket.as_bytes()).ok()?;
    let (n, quiet, px) = (code.width(), 4, 4);
    let colors = code.to_colors();
    let side = (n + 2 * quiet) * px;
    let mut pixels = vec![egui::Color32::WHITE; side * side];
    for (i, c) in colors.iter().enumerate() {
        if *c == qrcode::Color::Dark {
            let (x, y) = ((i % n + quiet) * px, (i / n + quiet) * px);
            for dy in 0..px {
                for dx in 0..px {
                    pixels[(y + dy) * side + x + dx] = egui::Color32::BLACK;
                }
            }
        }
    }
    Some(egui::ColorImage {
        size: [side, side],
        pixels,
    })
}

/// One grant in the shares dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct ShareRow {
    pub source: String,
    pub label: String,
    /// "" = the whole source.
    pub subtree: String,
    pub access: Access,
}

/// What `peer` was granted, by source label then subtree.
pub fn share_rows(grants: &[Grant], sources: &[SourceSummary], peer: &PeerId) -> Vec<ShareRow> {
    let mut rows: Vec<ShareRow> = grants
        .iter()
        .filter(|g| g.peer == *peer)
        .map(|g| ShareRow {
            source: g.source.clone(),
            label: sources
                .iter()
                .find(|s| s.id.0 == g.source)
                .map_or_else(|| "(removed source)".into(), |s| s.label.clone()),
            subtree: g.subtree.clone(),
            access: g.access,
        })
        .collect();
    rows.sort_by(|a, b| (&a.label, &a.subtree).cmp(&(&b.label, &b.subtree)));
    rows
}

/// Sources a device may be granted (another device's source is never re-shared).
pub fn shareable(sources: &[SourceSummary]) -> Vec<&SourceSummary> {
    sources
        .iter()
        .filter(|s| s.kind != SourceKind::Device)
        .collect()
}

/// The library's device sources that browse `peer` (they leave with it when it is
/// forgotten: what they claim to hold is that device's word).
pub fn device_sources_of(sources: &[SourceSummary], peer: &PeerId) -> Vec<SourceId> {
    sources
        .iter()
        .filter(|s| {
            s.root.scheme == "node"
                && (s.root.authority.eq_ignore_ascii_case(&peer.0.to_string())
                    || s.root.authority.parse::<keel_net::NodeId>().ok() == Some(peer.0))
        })
        .map(|s| s.id.clone())
        .collect()
}

/// A subtree as typed: slashes, no leading or trailing one.
pub fn clean_subtree(s: &str) -> String {
    s.trim().replace('\\', "/").trim_matches('/').to_owned()
}

pub struct SharesDialog {
    pub peer: PeerId,
    pub source: Option<String>,
    pub subtree: String,
    pub access: Access,
}

/// A device / dialog command (`Action::Devices`).
#[derive(Clone, Debug, PartialEq)]
pub enum DevCmd {
    Pair,
    PairStep(PairEvent),
    Browse(PeerId),
    /// Pick files with the OS dialog, then send them.
    SendFiles(PeerId),
    /// The context menu's "Send with Spacedrop…": pick a device for the targets.
    SendSelection,
    Send {
        peer: PeerId,
        paths: Vec<VPath>,
    },
    Shares(PeerId),
    Grant {
        peer: PeerId,
        source: String,
        subtree: String,
        access: Access,
    },
    Revoke {
        peer: PeerId,
        source: String,
        subtree: String,
    },
    /// Asks first.
    Forget(PeerId),
    ForgetConfirmed(PeerId),
    /// The incoming offer prompt.
    Answer {
        id: String,
        accept: bool,
        always: bool,
    },
}

pub enum DevMsg {
    Opened {
        lib: Arc<Library>,
        result: Result<(Arc<Node>, Arc<LibraryHandler>), String>,
    },
    Code(Result<(String, String), String>),
    Joined(Result<String, String>),
    Picked {
        peer: PeerId,
        paths: Vec<PathBuf>,
    },
    /// Attached: the daemon's devices, grants and waiting offers (or why there are none).
    Remote(Result<RemoteDevices, String>),
}

/// Paired devices as keel-daemon reports them.
pub struct RemoteDevices {
    pub id: String,
    pub label: String,
    pub peers: Vec<Peer>,
    pub grants: Vec<Grant>,
    pub offers: Vec<keel_api::types::DropOffer>,
}

/// A Spacedrop offer waiting in keel-daemon.
pub struct RemoteOffer {
    pub offer: keel_api::types::DropOffer,
    pub always: bool,
}

/// A Spacedrop offer waiting in the prompt.
pub struct Offer {
    pub drop: IncomingDrop,
    pub always: bool,
}

pub struct Devices {
    rt: Option<Arc<tokio::runtime::Runtime>>,
    pub node: Option<Arc<Node>>,
    handler: Option<Arc<LibraryHandler>>,
    /// The library the node serves (or is opening for).
    lib: Option<Arc<Library>>,
    pub opening: bool,
    pub error: Option<String>,
    events: Option<Receiver<NetEvent>>,
    pub peers: Vec<Peer>,
    pub grants: Vec<Grant>,
    pub pair: Option<Pair>,
    qr: Option<(String, egui::TextureHandle)>,
    pub shares: Option<SharesDialog>,
    /// "Send with Spacedrop": the paths waiting for a device.
    pub send_pick: Option<Vec<VPath>>,
    pub forget: Option<PeerId>,
    pub offers: Vec<Offer>,
    offer_tx: Sender<IncomingDrop>,
    offer_rx: Receiver<IncomingDrop>,
    auto_accept: Arc<RwLock<Vec<String>>>,
    inbox: Option<PathBuf>,
    next_ping: Instant,
    // --- attached to keel-daemon: its node, through the API ---
    /// The daemon connection whose devices are shown (and whose `node://` is registered).
    remote: Option<u64>,
    /// This device as the daemon reports it (id, label); None until read or with devices
    /// off there (`error` says why).
    pub remote_self: Option<(String, String)>,
    pub remote_offers: Vec<RemoteOffer>,
    next_poll: Instant,
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Devices {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        let (offer_tx, offer_rx) = crossbeam_channel::unbounded();
        Self {
            rt: None,
            node: None,
            handler: None,
            lib: None,
            opening: false,
            error: None,
            events: None,
            peers: Vec::new(),
            grants: Vec::new(),
            pair: None,
            qr: None,
            shares: None,
            send_pick: None,
            forget: None,
            offers: Vec::new(),
            offer_tx,
            offer_rx,
            auto_accept: Arc::default(),
            inbox: None,
            next_ping: Instant::now(),
            remote: None,
            remote_self: None,
            remote_offers: Vec::new(),
            next_poll: Instant::now(),
            tx,
            ctx,
        }
    }

    fn rt(&mut self) -> Arc<tokio::runtime::Runtime> {
        self.rt
            .get_or_insert_with(|| {
                Arc::new(
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .thread_name("keel-net")
                        .enable_all()
                        .build()
                        .expect("tokio runtime"),
                )
            })
            .clone()
    }

    /// Opens, closes or reopens the node to match the open library and the settings;
    /// applies the inbox, auto-accept list and label. Every frame.
    pub fn sync(&mut self, lib: Option<&Arc<Library>>, s: &DeviceSettings, router: &Arc<Router>) {
        if *self.auto_accept.read() != s.auto_accept {
            *self.auto_accept.write() = s.auto_accept.clone();
        }
        let want = lib.filter(|_| s.enabled);
        let same = match (&self.lib, want) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        // Unit tests never open a node: it would use the real data folder and keychain.
        if !same && !self.opening && !cfg!(test) {
            self.reopen(want.cloned(), s, router);
        }
        let inbox = s.inbox_dir();
        if let Some(handler) = self.handler.as_ref().filter(|_| self.inbox != inbox) {
            self.inbox = inbox.clone();
            if let Some(dir) = inbox {
                self.on_drop(handler, dir);
            }
        }
        if let Some(node) = &self.node {
            let label = s.label.trim();
            if !label.is_empty() && node.label() != label {
                node.set_label(label);
            }
        }
    }

    fn on_drop(&self, handler: &LibraryHandler, inbox: PathBuf) {
        let (auto, tx, ctx) = (
            self.auto_accept.clone(),
            self.offer_tx.clone(),
            self.ctx.clone(),
        );
        handler.on_drop(inbox, move |offer| {
            if auto.read().contains(&offer.peer.0.to_string()) {
                offer.reply.answer(true);
            } else {
                let _ = tx.send(offer);
                ctx.request_repaint();
            }
        });
    }

    fn reopen(&mut self, lib: Option<Arc<Library>>, s: &DeviceSettings, router: &Arc<Router>) {
        let old = self.node.take();
        self.handler = None;
        self.events = None;
        self.peers.clear();
        self.grants.clear();
        self.offers.clear();
        self.inbox = None;
        self.lib = lib.clone();
        let rt = self.rt();
        let Some(lib) = lib else {
            if let Some(old) = old {
                worker::spawn("keel-net-close", move || rt.block_on(old.close()));
            }
            return;
        };
        self.opening = true;
        self.error = None;
        let (tx, ctx, router) = (self.tx.clone(), self.ctx.clone(), router.clone());
        let relay = s.relay;
        worker::spawn("keel-net-open", move || {
            if let Some(old) = old {
                rt.block_on(old.close());
            }
            let result = (|| -> anyhow::Result<(Arc<Node>, Arc<LibraryHandler>)> {
                let data = keel_core::data_dir().context("no data folder")?;
                let secrets: Arc<dyn keel_vfs::SecretStore> =
                    if std::env::var("KEEL_NET_SECRET").as_deref() == Ok("memory") {
                        Arc::new(keel_vfs::cloud::MemoryStore::default())
                    } else {
                        Arc::new(keel_vfs::cloud::KeyringStore)
                    };
                let handler = Arc::new(LibraryHandler::new(lib.clone()));
                let options = NodeOptions::default().with_relay(relay);
                let node = rt.block_on(Node::open_with_options(
                    secrets,
                    &data,
                    handler.clone(),
                    options,
                ))?;
                router.register(Arc::new(NodeProvider::new(
                    node.clone(),
                    rt.handle().clone(),
                )));
                Ok((node, handler))
            })();
            let result = result.map_err(|e| format!("{e:#}"));
            worker::send(&tx, &ctx, Msg::Devices(DevMsg::Opened { lib, result }));
        });
    }

    /// Exit: closes the node (waits up to 3 s).
    pub fn close_now(&mut self) {
        if let (Some(node), Some(rt)) = (self.node.take(), self.rt.clone()) {
            rt.block_on(async {
                let _ = tokio::time::timeout(Duration::from_secs(3), node.close()).await;
            });
        }
    }

    fn refresh(&mut self) {
        if let Some(node) = &self.node {
            self.peers = node.peers();
            self.grants = node.grants();
        }
    }

    /// Node events, incoming offers and pings; returns toasts to show.
    fn tick(&mut self) -> Vec<String> {
        let mut toasts = Vec::new();
        let events: Vec<NetEvent> = self.events.iter().flat_map(|e| e.try_iter()).collect();
        for ev in events {
            match ev {
                NetEvent::Paired(peer) => {
                    let label = label_of(std::slice::from_ref(&peer), &peer.id);
                    if let Some(pair) = self.pair.take() {
                        self.pair = Some(pair.next(PairEvent::PairedWith(label)));
                    }
                }
                NetEvent::DropReceived { peer, path, .. } => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    toasts.push(format!(
                        "Received {name} from {}",
                        label_of(&self.peers, &peer)
                    ));
                }
                _ => {}
            }
        }
        self.offers
            .extend(self.offer_rx.try_iter().map(|drop| Offer {
                drop,
                always: false,
            }));
        // The sender cancelled or stopped asking, or the device was forgotten: down at
        // once (checked again shortly while any prompt is up).
        self.offers.retain(|o| !o.drop.reply.withdrawn());
        if !self.offers.is_empty() {
            self.ctx.request_repaint_after(PROMPT_CHECK);
        }
        self.refresh();
        if let (Some(node), Some(rt)) = (&self.node, &self.rt) {
            if Instant::now() >= self.next_ping {
                self.next_ping = Instant::now() + PING_EVERY;
                for peer in self.peers.iter().map(|p| p.id) {
                    let node = node.clone();
                    rt.spawn(async move {
                        let _ = node.request(&peer, Request::Ping).await;
                    });
                }
            }
            self.ctx.request_repaint_after(PING_EVERY);
        }
        toasts
    }
}

impl AppState {
    /// Every frame: node lifecycle, events, sidebar rows. Attached to keel-daemon, the
    /// daemon's node instead (this window opens none).
    pub fn devices_tick(&mut self) {
        let lib = self.library.lib.clone();
        self.devices
            .sync(lib.as_ref(), &self.settings.devices, &self.router);
        for t in self.devices.tick() {
            self.toasts.info(t);
        }
        if let Some(remote) = self.library.remote().cloned() {
            return self.remote_devices_tick(remote);
        }
        if self.devices.remote.take().is_some() {
            self.devices.peers.clear();
            self.devices.grants.clear();
            self.devices.remote_self = None;
            self.devices.remote_offers.clear();
            self.devices.error = None;
        }
        self.sidebar.devices = self
            .devices
            .node
            .as_ref()
            .map(|_| device_rows(&self.devices.peers));
        self.sidebar.devices_note = match (&self.devices.node, &self.devices.error) {
            (Some(_), _) => None,
            (None, Some(e)) => Some(format!("Not available: {e}")),
            _ if self.devices.opening => Some("Starting…".into()),
            _ if !self.settings.devices.enabled => Some("Off (Settings → Devices)".into()),
            _ => Some("Needs the library (Settings → Library)".into()),
        };
    }

    /// Attached: the daemon's devices, grants and offers, read every `REMOTE_POLL`;
    /// `node://` paths go to the daemon.
    fn remote_devices_tick(&mut self, remote: Arc<Remote>) {
        let d = &mut self.devices;
        if d.remote != Some(remote.id) {
            d.remote = Some(remote.id);
            d.remote_self = None;
            d.error = None;
            d.next_poll = Instant::now();
            self.router.register(Arc::new(keel_api::DaemonProvider::new(
                "node",
                Rpc(remote.clone()),
            )));
        }
        if Instant::now() >= d.next_poll && !self.library.lost {
            d.next_poll = Instant::now() + REMOTE_POLL;
            let (tx, ctx) = (d.tx.clone(), d.ctx.clone());
            worker::spawn("keel-daemon-devices", move || {
                let read = || -> anyhow::Result<RemoteDevices> {
                    let devices: api::Devices = remote.call("devices.list", json!({}))?;
                    let grants: Vec<api::GrantInfo> = remote.call("shares.list", json!({}))?;
                    let inbox: api::Inbox = remote.call("spacedrop.inbox", json!({}))?;
                    Ok(RemoteDevices {
                        id: devices.id,
                        label: devices.label,
                        peers: devices.peers.iter().filter_map(peer_of).collect(),
                        grants: grants.iter().filter_map(grant_of).collect(),
                        offers: inbox.pending,
                    })
                };
                let msg = DevMsg::Remote(read().map_err(|e| format!("{e:#}")));
                worker::send(&tx, &ctx, Msg::Devices(msg));
            });
        }
        if !self.devices.remote_offers.is_empty() {
            self.ctx.request_repaint_after(REMOTE_POLL);
        }
        let d = &self.devices;
        self.sidebar.devices = d.remote_self.as_ref().map(|_| device_rows(&d.peers));
        self.sidebar.devices_note = match (&d.remote_self, &d.error) {
            (Some(_), _) => None,
            (None, Some(e)) => Some(format!("keel-daemon: {e}")),
            (None, None) => Some("Asking keel-daemon…".into()),
        };
    }

    /// A `net.event` from the attached daemon.
    pub fn devices_event(&mut self, ev: serde_json::Value) {
        let label = || ev["label"].as_str().unwrap_or_default().to_owned();
        match ev["event"].as_str() {
            Some("paired") => {
                if let Some(pair) = self.devices.pair.take() {
                    self.devices.pair = Some(pair.next(PairEvent::PairedWith(label())));
                }
            }
            Some("drop_received") => {
                let path = ev["path"].as_str().unwrap_or_default();
                let name = std::path::Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let from = (ev["peer"].as_str())
                    .and_then(|p| p.parse::<keel_net::NodeId>().ok())
                    .map(|n| label_of(&self.devices.peers, &PeerId(n)))
                    .unwrap_or_default();
                self.toasts.info(format!("Received {name} from {from}"));
            }
            _ => {}
        }
        // Peers, grants and offers follow at once.
        self.devices.next_poll = Instant::now();
    }

    pub fn devices_msg(&mut self, msg: DevMsg) {
        let d = &mut self.devices;
        match msg {
            DevMsg::Opened { lib, result } => {
                let current = d.lib.as_ref().is_some_and(|l| Arc::ptr_eq(l, &lib));
                d.opening = false;
                match result {
                    Ok((node, handler)) if current => {
                        tracing::info!("devices: node {} open", node.id());
                        d.events = Some(forward(node.events(), d.ctx.clone()));
                        d.node = Some(node);
                        d.handler = Some(handler);
                        d.next_ping = Instant::now();
                        d.refresh();
                    }
                    // Superseded meanwhile (library switched, devices turned off).
                    Ok((node, _)) => {
                        if let Some(rt) = d.rt.clone() {
                            worker::spawn("keel-net-close", move || rt.block_on(node.close()));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("devices: {e}");
                        d.error = Some(e);
                    }
                }
            }
            DevMsg::Code(code) => self.pair_step(PairEvent::Code(code)),
            DevMsg::Joined(r) => self.pair_step(PairEvent::Joined(r)),
            DevMsg::Picked { peer, paths } => {
                let paths = paths.into_iter().map(VPath::local).collect();
                self.devices_cmd(0, DevCmd::Send { peer, paths });
            }
            DevMsg::Remote(_) if self.library.remote().is_none() => {}
            DevMsg::Remote(Ok(r)) => {
                d.remote_self = Some((r.id, r.label));
                d.error = None;
                d.peers = r.peers;
                d.grants = r.grants;
                // Offers still waiting keep their "always" box.
                let old = std::mem::take(&mut d.remote_offers);
                d.remote_offers = (r.offers.into_iter())
                    .map(|offer| RemoteOffer {
                        always: (old.iter()).any(|o| {
                            o.always && o.offer.id == offer.id && o.offer.peer == offer.peer
                        }),
                        offer,
                    })
                    .collect();
            }
            DevMsg::Remote(Err(e)) => {
                d.remote_self = None;
                d.peers.clear();
                d.grants.clear();
                d.remote_offers.clear();
                d.error = Some(if e.contains("devices are off") {
                    "devices are off there (turn on Settings → Devices, then restart \
                     keel-daemon)"
                        .into()
                } else {
                    e
                });
            }
        }
    }

    fn pair_step(&mut self, ev: PairEvent) {
        let d = &mut self.devices;
        let Some(state) = d.pair.take() else { return };
        let next = state.next(ev);
        if let Some(remote) = self.library.remote().cloned() {
            let (tx, ctx) = (d.tx.clone(), d.ctx.clone());
            match &next {
                Pair::Requesting => {
                    worker::spawn("keel-pair-code", move || {
                        let code = (|| -> anyhow::Result<(String, String)> {
                            let done = remote.apply("devices.pair_code", json!({}))?;
                            let info: api::PairCodeInfo =
                                serde_json::from_value(done.result.unwrap_or_default())?;
                            Ok((info.code, info.ticket))
                        })()
                        .map_err(|e| format!("{e:#}"));
                        worker::send(&tx, &ctx, Msg::Devices(DevMsg::Code(code)));
                    });
                }
                Pair::Joining { text } => {
                    let text = text.trim().to_owned();
                    worker::spawn("keel-pair-join", move || {
                        let joined = (|| -> anyhow::Result<String> {
                            let done =
                                remote.apply("devices.pair_with", json!({ "code": text }))?;
                            let peer: api::PeerInfo =
                                serde_json::from_value(done.result.unwrap_or_default())?;
                            Ok(match peer.label.trim() {
                                "" => peer.id.chars().take(8).collect(),
                                l => l.to_owned(),
                            })
                        })()
                        .map_err(|e| format!("Pairing failed: {e:#}"));
                        worker::send(&tx, &ctx, Msg::Devices(DevMsg::Joined(joined)));
                    });
                }
                _ => {}
            }
            self.devices.pair = Some(next);
            return;
        }
        let (Some(node), Some(rt)) = (d.node.clone(), d.rt.clone()) else {
            d.pair = Some(next);
            return;
        };
        let (tx, ctx) = (d.tx.clone(), d.ctx.clone());
        match &next {
            Pair::Requesting => {
                worker::spawn("keel-pair-code", move || {
                    let code = rt
                        .block_on(node.pair_code())
                        .map(|c| (c.to_string(), c.ticket()))
                        .map_err(|e| format!("{e:#}"));
                    worker::send(&tx, &ctx, Msg::Devices(DevMsg::Code(code)));
                });
            }
            Pair::Joining { text } => {
                let text = text.trim().to_owned();
                worker::spawn("keel-pair-join", move || {
                    let joined = (|| -> anyhow::Result<String> {
                        let code: PairCode = text.parse()?;
                        let peer = rt.block_on(node.pair_with(&code))?;
                        Ok(label_of(std::slice::from_ref(&peer), &peer.id))
                    })()
                    .map_err(|e| format!("Pairing failed: {e:#}"));
                    worker::send(&tx, &ctx, Msg::Devices(DevMsg::Joined(joined)));
                });
            }
            _ => {}
        }
        d.pair = Some(next);
    }

    pub fn devices_cmd(&mut self, p: usize, cmd: DevCmd) {
        if let Some(remote) = self.library.remote().cloned() {
            return self.remote_devices_cmd(p, cmd, remote);
        }
        let Some(node) = self.devices.node.clone() else {
            if !matches!(cmd, DevCmd::Answer { .. }) {
                self.toasts
                    .error("Devices are not running (Settings → Devices, and the library)");
            }
            return;
        };
        let label = |s: &Self, peer: &PeerId| label_of(&s.devices.peers, peer);
        let Some(cmd) = self.ui_devices_cmd(p, cmd) else {
            return;
        };
        match cmd {
            DevCmd::Send { peer, paths } => {
                if let Some(bad) = paths.iter().find(|p| p.split_archive().is_some()) {
                    return self.toasts.error(format!(
                        "Extract {} first: Spacedrop sends files and folders",
                        bad.name()
                    ));
                }
                let n = paths.len();
                self.toasts
                    .info(format!("Sending {n} item(s) to {}", label(self, &peer)));
                self.library.send_drop(node, peer, paths);
            }
            DevCmd::Grant {
                peer,
                source,
                subtree,
                access,
            } => {
                let grant = Grant {
                    peer,
                    source,
                    subtree: clean_subtree(&subtree),
                    access,
                    created: chrono::Utc::now().timestamp(),
                };
                if let Err(e) = node.grant(grant) {
                    self.toasts.error(format!("Could not share: {e:#}"));
                }
                self.devices.refresh();
            }
            DevCmd::Revoke {
                peer,
                source,
                subtree,
            } => {
                if let Err(e) = node.revoke(&peer, &source, &subtree) {
                    self.toasts.error(format!("Could not revoke: {e:#}"));
                }
                self.devices.refresh();
            }
            DevCmd::ForgetConfirmed(peer) => {
                let id = peer.0.to_string();
                self.settings.devices.auto_accept.retain(|a| *a != id);
                if let Err(e) = node.forget_peer(&peer) {
                    self.toasts
                        .error(format!("Could not forget the device: {e:#}"));
                }
                for id in device_sources_of(&self.library.sources, &peer) {
                    self.library_cmd(p, crate::library::LibCmd::RemoveConfirmed(id));
                }
                self.devices.refresh();
            }
            DevCmd::Answer { id, accept, always } => {
                let Some(i) = self.devices.offers.iter().position(|o| o.drop.id == id) else {
                    return;
                };
                let offer = self.devices.offers.remove(i);
                let peer = offer.drop.peer.0.to_string();
                if accept && always && !self.settings.devices.auto_accept.contains(&peer) {
                    self.settings.devices.auto_accept.push(peer);
                }
                offer.drop.reply.answer(accept);
            }
            // Handled by `ui_devices_cmd`.
            _ => {}
        }
    }

    /// The device commands that only open dialogs or pickers (the same with this window's
    /// node or the daemon's); the others come back.
    fn ui_devices_cmd(&mut self, p: usize, cmd: DevCmd) -> Option<DevCmd> {
        match cmd {
            DevCmd::Pair => self.devices.pair = Some(Pair::Choose),
            DevCmd::PairStep(ev) => self.pair_step(ev),
            DevCmd::Browse(peer) => self.run(p, Action::NewTabAt(root_of(&peer))),
            DevCmd::SendFiles(peer) => {
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                worker::spawn("keel-drop-pick", move || {
                    let picked = rfd::FileDialog::new()
                        .set_title("Send with Spacedrop")
                        .pick_files();
                    if let Some(paths) = picked.filter(|p| !p.is_empty()) {
                        worker::send(&tx, &ctx, Msg::Devices(DevMsg::Picked { peer, paths }));
                    }
                });
            }
            DevCmd::SendSelection => {
                let paths: Vec<VPath> = (self.tab(p).targets().iter())
                    .filter_map(|e| crate::library::real_of(&self.library.sources, &e.path))
                    .collect();
                if paths.is_empty() {
                    self.toasts.error("Nothing selected to send");
                } else if self.devices.peers.is_empty() {
                    self.toasts.error("No paired devices (Devices → Pair…)");
                } else {
                    self.devices.send_pick = Some(paths);
                }
            }
            DevCmd::Shares(peer) => {
                let source = shareable(&self.library.sources)
                    .first()
                    .map(|s| s.id.0.clone());
                self.devices.shares = Some(SharesDialog {
                    peer,
                    source,
                    subtree: String::new(),
                    access: Access::Read,
                });
            }
            DevCmd::Forget(peer) => self.devices.forget = Some(peer),
            cmd => return Some(cmd),
        }
        None
    }
}

/// How often the attached daemon's devices, grants and offers are read.
const REMOTE_POLL: Duration = Duration::from_secs(3);

/// A daemon's device as the sidebar shows it.
fn peer_of(p: &api::PeerInfo) -> Option<Peer> {
    Some(Peer {
        id: PeerId(p.id.parse().ok()?),
        label: p.label.clone(),
        last_seen: p.last_seen,
        link: match p.link.as_str() {
            "lan" => Link::Lan,
            "relay" => Link::Relay,
            _ => Link::Offline,
        },
        storage: p
            .storage_used
            .zip(p.storage_total)
            .map(|(used, total)| keel_net::Storage { used, total }),
    })
}

fn grant_of(g: &api::GrantInfo) -> Option<Grant> {
    Some(Grant {
        peer: PeerId(g.peer.parse().ok()?),
        source: g.source.clone(),
        subtree: g.subtree.clone(),
        access: match g.access {
            api::Access::Read => Access::Read,
            api::Access::ReadWrite => Access::ReadWrite,
        },
        created: g.created,
    })
}

impl AppState {
    /// A device command while attached: through the daemon's API, on workers.
    fn remote_devices_cmd(&mut self, p: usize, cmd: DevCmd, remote: Arc<Remote>) {
        if self.library.lost {
            return self.toasts.error(crate::library::LOST);
        }
        if self.devices.remote_self.is_none() && !matches!(cmd, DevCmd::Answer { .. }) {
            return self.toasts.error(match &self.devices.error {
                Some(e) => format!("keel-daemon: {e}"),
                None => "Still asking keel-daemon about devices".into(),
            });
        }
        // Runs `f` on a worker; an error is a toast; peers and grants are read again.
        let run = |s: &mut Self,
                   what: &'static str,
                   f: Box<dyn FnOnce() -> anyhow::Result<()> + Send>| {
            let (tx, ctx) = (s.tx.clone(), s.ctx.clone());
            worker::spawn("keel-daemon-device", move || {
                if let Err(e) = f() {
                    worker::send(&tx, &ctx, Msg::Toast(format!("{what}: {e:#}")));
                }
            });
            s.devices.next_poll = Instant::now() + Duration::from_millis(300);
        };
        let Some(cmd) = self.ui_devices_cmd(p, cmd) else {
            return;
        };
        match cmd {
            DevCmd::Send { peer, paths } => {
                if let Some(bad) = paths.iter().find(|p| p.split_archive().is_some()) {
                    return self.toasts.error(format!(
                        "Extract {} first: Spacedrop sends files and folders",
                        bad.name()
                    ));
                }
                self.toasts.info(format!(
                    "Sending {} item(s) to {}",
                    paths.len(),
                    label_of(&self.devices.peers, &peer)
                ));
                let paths: Vec<String> = paths.iter().map(VPath::display).collect();
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                worker::spawn("keel-drop-send", move || {
                    let sent = remote.apply(
                        "spacedrop.send",
                        json!({"peer": peer.0.to_string(), "paths": paths}),
                    );
                    let msg = match sent {
                        Ok(done) => Msg::Library(crate::library::LibMsg::Spawned {
                            kind: "drop",
                            id: done.job,
                        }),
                        Err(e) => Msg::Toast(format!("Spacedrop: {e:#}")),
                    };
                    worker::send(&tx, &ctx, msg);
                });
            }
            DevCmd::Grant {
                peer,
                source,
                subtree,
                access,
            } => {
                let access = match access {
                    Access::Read => "read",
                    Access::ReadWrite => "read_write",
                };
                let params = json!({"peer": peer.0.to_string(), "source": source,
                    "subtree": clean_subtree(&subtree), "access": access});
                run(
                    self,
                    "Could not share",
                    Box::new(move || remote.apply("shares.grant", params).map(drop)),
                );
            }
            DevCmd::Revoke {
                peer,
                source,
                subtree,
            } => {
                let params =
                    json!({"peer": peer.0.to_string(), "source": source, "subtree": subtree});
                run(
                    self,
                    "Could not revoke",
                    Box::new(move || {
                        remote
                            .call::<api::Revoked>("shares.revoke", params)
                            .map(drop)
                    }),
                );
            }
            DevCmd::ForgetConfirmed(peer) => {
                let id = peer.0.to_string();
                self.settings.devices.auto_accept.retain(|a| *a != id);
                run(
                    self,
                    "Could not forget the device",
                    Box::new(move || {
                        remote
                            .apply("devices.forget", json!({ "peer": id }))
                            .map(drop)
                    }),
                );
                for id in device_sources_of(&self.library.sources, &peer) {
                    self.library_cmd(p, crate::library::LibCmd::RemoveConfirmed(id));
                }
            }
            DevCmd::Answer { id, accept, always } => {
                let Some(i) = (self.devices.remote_offers.iter()).position(|o| o.offer.id == id)
                else {
                    return;
                };
                let offer = self.devices.remote_offers.remove(i).offer;
                // keel-daemon reads the list when it starts.
                if accept && always && !self.settings.devices.auto_accept.contains(&offer.peer) {
                    self.settings.devices.auto_accept.push(offer.peer.clone());
                }
                let params = json!({"peer": offer.peer, "id": offer.id, "accept": accept});
                run(
                    self,
                    "Spacedrop",
                    Box::new(move || remote.apply("spacedrop.answer", params).map(drop)),
                );
            }
            // Handled by `ui_devices_cmd`.
            _ => {}
        }
    }
}

fn dev(cmd: DevCmd) -> Action {
    Action::Devices(cmd)
}

fn dot_color(ui: &egui::Ui, link: Link) -> egui::Color32 {
    match link {
        Link::Lan => egui::Color32::from_rgb(70, 180, 90),
        Link::Relay => egui::Color32::from_rgb(230, 180, 40),
        Link::Offline => ui.visuals().weak_text_color(),
    }
}

/// The sidebar's Devices section (under its heading). `rows`: None while the node is
/// not running (`note` says why). Files dragged from a pane onto a row are sent there.
pub fn sidebar(
    ui: &mut egui::Ui,
    rows: Option<&[DeviceRow]>,
    note: Option<&str>,
    current: &VPath,
    accent: egui::Color32,
    out: &mut Vec<Action>,
) {
    let Some(rows) = rows else {
        ui.weak(note.unwrap_or("Off"));
        return;
    };
    for row in rows {
        let r = ui
            .horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size([16.0, 16.0].into(), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 4.5, dot_color(ui, row.link));
                let here = current.scheme == "node" && current.authority == row.id.0.to_string();
                ui.add(
                    egui::Button::new(row.label.as_str())
                        .frame(false)
                        .selected(here),
                )
            })
            .inner;
        let tip = match row.link {
            Link::Lan => "Connected directly",
            Link::Relay => "Connected through a relay",
            Link::Offline => "Offline",
        };
        let r = r.on_hover_text(tip);
        if r.clicked() {
            out.push(Action::Navigate(row.root.clone()));
        } else if r.middle_clicked() {
            out.push(Action::NewTabAt(row.root.clone()));
        }
        if let Some(drag) = r.dnd_release_payload::<crate::pane::DragPayload>() {
            out.push(dev(DevCmd::Send {
                peer: row.id,
                paths: drag.paths.clone(),
            }));
        }
        r.context_menu(|ui| {
            for (text, cmd) in [
                ("Browse", DevCmd::Browse(row.id)),
                ("Send files…", DevCmd::SendFiles(row.id)),
                ("Shares…", DevCmd::Shares(row.id)),
                ("Forget…", DevCmd::Forget(row.id)),
            ] {
                if ui.button(text).clicked() {
                    out.push(dev(cmd));
                    ui.close_menu();
                }
            }
        });
        if let Some((used, total)) = row.storage.filter(|(_, t)| *t > 0) {
            let share = used as f32 / total as f32;
            ui.add(
                egui::ProgressBar::new(share)
                    .desired_height(4.0)
                    .fill(accent),
            )
            .on_hover_text(format!(
                "{} used of {}",
                humansize::format_size(used, humansize::DECIMAL),
                humansize::format_size(total, humansize::DECIMAL)
            ));
        }
    }
    if ui.small_button("Pair…").clicked() {
        out.push(dev(DevCmd::Pair));
    }
}

/// The devices' windows: pair, shares, device picker, forget, incoming offers.
pub fn windows(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    pair_window(ctx, s, out);
    shares_window(ctx, s, out);
    pick_window(ctx, s, out);
    if let Some(peer) = s.devices.forget {
        let label = label_of(&s.devices.peers, &peer);
        let mut open = true;
        egui::Window::new("Forget device")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Forget {label}? Its shares end now and its folders leave the library; \
                     pair again to reconnect."
                ));
                ui.horizontal(|ui| {
                    if ui.button("Forget").clicked() {
                        out.push(dev(DevCmd::ForgetConfirmed(peer)));
                        s.devices.forget = None;
                    }
                    if ui.button("Cancel").clicked() {
                        s.devices.forget = None;
                    }
                });
            });
        if !open {
            s.devices.forget = None;
        }
    }
    offers(ctx, s, out);
}

fn pair_window(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let Some(state) = s.devices.pair.clone() else {
        return;
    };
    let step = |ev| dev(DevCmd::PairStep(ev));
    let mut open = true;
    egui::Window::new("Pair a device")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| match state {
            Pair::Choose => {
                ui.label("Pair with another Keel: show this device's code there, or enter its code here.");
                ui.horizontal(|ui| {
                    if ui.button("Show code").clicked() {
                        out.push(step(PairEvent::ShowCode));
                    }
                    if ui.button("Enter code").clicked() {
                        out.push(step(PairEvent::EnterCode));
                    }
                });
            }
            Pair::Requesting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Making a code…");
                });
            }
            Pair::Showing { short, ticket } => {
                ui.label("On the other device: Pair… → Enter code, then type or scan:");
                ui.monospace(egui::RichText::new(&short).size(18.0));
                if ui.small_button("Copy full code").clicked() {
                    ui.ctx().copy_text(ticket.clone());
                }
                if s.devices.qr.as_ref().is_none_or(|(t, _)| *t != ticket) {
                    s.devices.qr = qr_image(&ticket).map(|img| {
                        let tex =
                            ctx.load_texture("keel-pair-qr", img, egui::TextureOptions::NEAREST);
                        (ticket.clone(), tex)
                    });
                }
                if let Some((_, tex)) = &s.devices.qr {
                    ui.add(egui::Image::new(tex).fit_to_exact_size([240.0, 240.0].into()));
                }
                ui.weak("The code works once, for ten minutes. Anyone who has it can pair.");
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Waiting for the other device…");
                });
            }
            Pair::Entering { mut text, error } => {
                ui.label("Code shown on the other device (short code or full code):");
                let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(360.0));
                if r.changed() {
                    s.devices.pair = Some(Pair::Entering {
                        text: text.clone(),
                        error: error.clone(),
                    });
                }
                if let Some(e) = &error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Pair").clicked() || enter {
                    out.push(step(PairEvent::Submit));
                }
            }
            Pair::Joining { .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Pairing…");
                });
            }
            Pair::Paired(label) => {
                ui.label(format!("Paired with {label}."));
                ui.weak("It sees nothing until you share a source: right-click it → Shares….");
                if ui.button("Done").clicked() {
                    s.devices.pair = None;
                }
            }
            Pair::Failed(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                if ui.button("Back").clicked() {
                    out.push(step(PairEvent::Back));
                }
            }
        });
    if !open {
        s.devices.pair = None;
        s.devices.qr = None;
    }
}

fn shares_window(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let Some(d) = s.devices.shares.as_mut() else {
        return;
    };
    let peer = d.peer;
    let label = label_of(&s.devices.peers, &peer);
    let rows = share_rows(&s.devices.grants, &s.library.sources, &peer);
    let sources = shareable(&s.library.sources);
    let mut open = true;
    egui::Window::new(format!("Shares with {label}"))
        .id(egui::Id::new("keel-shares"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            if rows.is_empty() {
                ui.weak("Nothing shared: this device sees none of your sources.");
            }
            egui::Grid::new("keel-shares-grid")
                .num_columns(4)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    for row in &rows {
                        ui.label(&row.label);
                        ui.label(if row.subtree.is_empty() {
                            "(whole source)"
                        } else {
                            row.subtree.as_str()
                        });
                        ui.label(match row.access {
                            Access::Read => "Read",
                            Access::ReadWrite => "Read-write",
                        });
                        if ui.button("Revoke").clicked() {
                            out.push(dev(DevCmd::Revoke {
                                peer,
                                source: row.source.clone(),
                                subtree: row.subtree.clone(),
                            }));
                        }
                        ui.end_row();
                    }
                });
            ui.separator();
            ui.strong("Share");
            if sources.is_empty() {
                ui.weak("Add a source to the library first.");
                return;
            }
            let shown = (d.source.as_ref())
                .and_then(|id| sources.iter().find(|s| s.id.0 == *id))
                .map_or("", |s| s.label.as_str());
            egui::ComboBox::from_id_salt("keel-share-source")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    for src in &sources {
                        ui.selectable_value(&mut d.source, Some(src.id.0.clone()), &src.label);
                    }
                });
            ui.horizontal(|ui| {
                ui.label("Folder inside it");
                ui.add(
                    egui::TextEdit::singleline(&mut d.subtree)
                        .hint_text("empty: the whole source")
                        .desired_width(220.0),
                );
            });
            ui.horizontal(|ui| {
                ui.radio_value(&mut d.access, Access::Read, "Read");
                ui.radio_value(&mut d.access, Access::ReadWrite, "Read-write");
            });
            if ui.button("Share").clicked() {
                if let Some(source) = d.source.clone() {
                    out.push(dev(DevCmd::Grant {
                        peer,
                        source,
                        subtree: d.subtree.clone(),
                        access: d.access,
                    }));
                }
            }
        });
    if !open {
        s.devices.shares = None;
    }
}

fn pick_window(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let Some(paths) = s.devices.send_pick.clone() else {
        return;
    };
    let mut open = true;
    egui::Window::new("Send with Spacedrop")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label(format!("Send {} item(s) to:", paths.len()));
            for row in device_rows(&s.devices.peers) {
                if ui.button(&row.label).clicked() {
                    out.push(dev(DevCmd::Send {
                        peer: row.id,
                        paths: paths.clone(),
                    }));
                    s.devices.send_pick = None;
                }
            }
        });
    if !open {
        s.devices.send_pick = None;
    }
}

/// Incoming offers, bottom-right above the toasts: Accept / Decline, "always" (this
/// window's node's, or the attached daemon's).
fn offers(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let peers = s.devices.peers.clone();
    let from = |label: &str, peer: Option<PeerId>| match (label.trim(), peer) {
        ("", Some(peer)) => label_of(&peers, &peer),
        (label, _) => label.to_owned(),
    };
    let mut y = -140.0;
    for offer in s.devices.offers.iter_mut() {
        let d = &offer.drop;
        let text = offer_text(&from(&d.label, Some(d.peer)), &d.files);
        offer_prompt(ctx, &d.id, &text, &mut offer.always, &mut y, out);
    }
    for o in s.devices.remote_offers.iter_mut() {
        let d = &o.offer;
        let peer = d.peer.parse().ok().map(PeerId);
        let names: Vec<&str> = d.names.iter().map(String::as_str).collect();
        let text = offer_summary(&from(&d.label, peer), d.files, d.bytes, &names);
        offer_prompt(ctx, &d.id, &text, &mut o.always, &mut y, out);
    }
}

fn offer_prompt(
    ctx: &egui::Context,
    id: &str,
    text: &str,
    always: &mut bool,
    y: &mut f32,
    out: &mut Vec<Action>,
) {
    let shown = egui::Area::new(egui::Id::new(("keel-offer", id)))
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, *y])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(420.0);
                ui.label(text);
                ui.checkbox(always, "Always accept from this device");
                ui.horizontal(|ui| {
                    for (text, accept) in [("Accept", true), ("Decline", false)] {
                        if ui.button(text).clicked() {
                            out.push(dev(DevCmd::Answer {
                                id: id.to_owned(),
                                accept,
                                always: *always,
                            }));
                        }
                    }
                });
            });
        });
    *y -= shown.response.rect.height() + 4.0;
}

/// Settings → Devices.
pub fn settings_page(ui: &mut egui::Ui, s: &mut crate::settings::Settings, d: &Devices) {
    let ds = &mut s.devices;
    if ui
        .checkbox(&mut ds.enabled, "Devices (pairing, shares, Spacedrop)")
        .changed()
    {
        ds.explicit = true;
    }
    ui.add_space(6.0);
    egui::Grid::new("settings-devices")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("This device");
            let current = match (&d.node, &d.remote_self) {
                (Some(n), _) => n.label(),
                (None, Some((_, label))) => label.clone(),
                _ => String::new(),
            };
            ui.add(
                egui::TextEdit::singleline(&mut ds.label)
                    .hint_text(current)
                    .desired_width(240.0),
            );
            ui.end_row();
            ui.label("Inbox");
            let default = DeviceSettings::default()
                .inbox_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            ui.add(
                egui::TextEdit::singleline(&mut ds.inbox)
                    .hint_text(default)
                    .desired_width(240.0),
            );
            ui.end_row();
            if ds.inbox.trim().is_empty() && downloads().is_none() {
                ui.label("");
                match ds.inbox_dir() {
                    Some(dir) => ui.weak(format!(
                        "No Downloads folder: drops go to {}",
                        dir.display()
                    )),
                    None => ui.colored_label(
                        ui.visuals().error_fg_color,
                        "No inbox: drops are declined until you set one",
                    ),
                };
                ui.end_row();
            }
            ui.label("Relays");
            ui.checkbox(&mut ds.relay, "Use public relays when no direct path works")
                .on_hover_text("Applies the next time devices start");
            ui.end_row();
        });
    ui.add_space(6.0);
    ui.strong("Always accept drops from");
    if ds.auto_accept.is_empty() {
        ui.weak("Nobody: every drop asks first.");
    }
    let mut remove = None;
    for (i, id) in ds.auto_accept.iter().enumerate() {
        ui.horizontal(|ui| {
            let label = id
                .parse::<keel_net::NodeId>()
                .map(|n| label_of(&d.peers, &PeerId(n)))
                .unwrap_or_else(|_| id.clone());
            ui.label(label);
            if ui.small_button("Remove").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        ds.auto_accept.remove(i);
    }
    if let Some(node) = &d.node {
        ui.add_space(6.0);
        ui.weak(format!("Device id: {}", node.id()));
    } else if let Some((id, _)) = &d.remote_self {
        ui.add_space(6.0);
        ui.weak(format!(
            "Device id: {id} (keel-daemon's; label, inbox, relays and the always-accept \
             list apply when it starts)"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::{SourceId, SourceStatus};
    use keel_net::{NodeId, Storage};

    fn peer(n: u8) -> PeerId {
        // Any 32 bytes make a NodeId for display and comparison.
        PeerId(NodeId([n; 32]))
    }

    #[test]
    fn device_rows_sort_by_label_and_carry_link_and_storage() {
        let peers = vec![
            Peer {
                id: peer(2),
                label: "laptop".into(),
                last_seen: None,
                link: Link::Relay,
                storage: Some(Storage { used: 5, total: 10 }),
            },
            Peer {
                id: peer(1),
                label: "  ".into(),
                last_seen: None,
                link: Link::Offline,
                storage: None,
            },
            Peer {
                id: peer(3),
                label: "Desk".into(),
                last_seen: None,
                link: Link::Lan,
                storage: None,
            },
        ];
        let rows = device_rows(&peers);
        let labels: Vec<_> = rows.iter().map(|r| r.label.as_str()).collect();
        let unnamed = short(&peer(1));
        assert_eq!(labels, [unnamed.as_str(), "Desk", "laptop"]);
        assert_eq!(rows[2].storage, Some((5, 10)));
        assert_eq!(rows[2].link, Link::Relay);
        assert_eq!(rows[1].root.scheme, "node");
        assert_eq!(rows[1].root.authority, peer(3).0.to_string());
    }

    #[test]
    fn pair_dialog_state_machine() {
        let p = Pair::Choose.next(PairEvent::ShowCode);
        assert_eq!(p, Pair::Requesting);
        let shown = p.next(PairEvent::Code(Ok(("abc".into(), "full".into()))));
        assert_eq!(
            shown,
            Pair::Showing {
                short: "abc".into(),
                ticket: "full".into()
            }
        );
        // Unrelated events leave a state alone; the joiner arriving ends it.
        assert_eq!(shown.clone().next(PairEvent::Submit), shown);
        assert_eq!(
            shown.next(PairEvent::PairedWith("Desk".into())),
            Pair::Paired("Desk".into())
        );
        assert_eq!(
            Pair::Requesting.next(PairEvent::Code(Err("offline".into()))),
            Pair::Failed("offline".into())
        );
        let entering = Pair::Choose.next(PairEvent::EnterCode);
        assert!(matches!(
            entering.clone().next(PairEvent::Submit),
            Pair::Entering { error: Some(_), .. }
        ));
        let typed = Pair::Entering {
            text: " not a code! ".into(),
            error: None,
        };
        assert!(matches!(
            typed.next(PairEvent::Submit),
            Pair::Entering { error: Some(_), .. }
        ));
        let code = "keel1-aaaaaaa-aaaaaaa-aaaaaa-aaaaaa";
        let joining = Pair::Entering {
            text: code.into(),
            error: None,
        }
        .next(PairEvent::Submit);
        assert_eq!(joining, Pair::Joining { text: code.into() });
        let failed = joining.next(PairEvent::Joined(Err("expired".into())));
        assert_eq!(
            failed,
            Pair::Entering {
                text: code.into(),
                error: Some("expired".into())
            }
        );
        let joining = Pair::Joining { text: "x".into() };
        assert_eq!(
            joining.next(PairEvent::Joined(Ok("Desk".into()))),
            Pair::Paired("Desk".into())
        );
        assert_eq!(Pair::Paired("x".into()).next(PairEvent::Back), Pair::Choose);
    }

    #[test]
    fn qr_image_is_square_with_a_quiet_zone() {
        let img = qr_image("keel-ticket-example").unwrap();
        assert_eq!(img.size[0], img.size[1]);
        assert_eq!(img.pixels[0], egui::Color32::WHITE);
        assert!(img.pixels.contains(&egui::Color32::BLACK));
    }

    fn source(id: &str, label: &str, kind: SourceKind) -> SourceSummary {
        SourceSummary {
            id: SourceId(id.into()),
            label: label.into(),
            root: VPath::local("x"),
            kind,
            status: SourceStatus::Online { indexed_at: None },
            generation: 0,
        }
    }

    #[test]
    fn share_rows_list_one_device_by_label() {
        let grant = |p: u8, source: &str, subtree: &str, access| Grant {
            peer: peer(p),
            source: source.into(),
            subtree: subtree.into(),
            access,
            created: 0,
        };
        let grants = vec![
            grant(1, "b", "", Access::Read),
            grant(1, "a", "photos", Access::ReadWrite),
            grant(2, "a", "", Access::Read),
            grant(1, "gone", "x", Access::Read),
        ];
        let sources = vec![
            source("a", "Archive", SourceKind::Folder),
            source("b", "Books", SourceKind::Drive),
            source("c", "Phone", SourceKind::Device),
        ];
        let rows = share_rows(&grants, &sources, &peer(1));
        let shown: Vec<_> = rows
            .iter()
            .map(|r| (r.label.as_str(), r.subtree.as_str(), r.access))
            .collect();
        assert_eq!(
            shown,
            [
                ("(removed source)", "x", Access::Read),
                ("Archive", "photos", Access::ReadWrite),
                ("Books", "", Access::Read),
            ]
        );
        let ids: Vec<_> = shareable(&sources).iter().map(|s| s.id.0.clone()).collect();
        assert_eq!(ids, ["a", "b"], "device sources are never re-shared");
        assert_eq!(clean_subtree(" \\photos\\2024/ "), "photos/2024");
    }

    #[test]
    fn devices_are_off_until_turned_on() {
        assert!(!DeviceSettings::default().enabled);
        let saved: DeviceSettings = toml::from_str("relay = false").unwrap();
        assert!(!saved.enabled, "a config without the switch stays off");
        let on: DeviceSettings = toml::from_str("enabled = true").unwrap();
        assert!(on.enabled && on.relay);
    }

    #[test]
    fn a_config_saved_while_devices_defaulted_on_is_turned_off_once() {
        let parse = |text: &str| crate::settings::parse_lenient(text).unwrap().0.devices;
        let old = parse("[devices]\nenabled = true\n");
        assert!(!old.enabled && old.explicit, "{old:?}");
        let chosen = parse("[devices]\nenabled = true\nexplicit = true\n");
        assert!(chosen.enabled);
        assert!(!parse("theme = 'dark'").explicit, "nothing to migrate");
        // The migrated value is saved: the next start keeps it off, and turning it on
        // in Settings (explicit) sticks.
        let mut s = crate::settings::Settings {
            devices: old,
            ..Default::default()
        };
        let text = toml::to_string(&s).unwrap();
        assert!(!parse(&text).enabled);
        s.devices.enabled = true;
        let text = toml::to_string(&s).unwrap();
        assert!(parse(&text).enabled);
    }

    #[test]
    fn the_inbox_falls_back_to_the_data_folder() {
        let (dl, data) = (PathBuf::from("dl"), PathBuf::from("data"));
        let at = |set: &str, dl: &Option<PathBuf>, data: &Option<PathBuf>| {
            inbox_in(set, dl.clone(), data.clone())
        };
        let (some_dl, some_data) = (Some(dl.clone()), Some(data.clone()));
        assert_eq!(at("", &some_dl, &some_data), Some(dl.join("Keel Drops")));
        assert_eq!(at(" ", &None, &some_data), Some(data.join("inbox")));
        assert_eq!(at("", &None, &None), None);
        assert_eq!(at(" mine ", &None, &None), Some(PathBuf::from("mine")));
    }

    #[test]
    fn the_offer_prompt_names_files_and_never_overflows() {
        let files = vec![
            ("a.txt".to_string(), u64::MAX),
            ("b.txt".to_string(), u64::MAX),
            ("dir/c.txt".to_string(), 1),
            ("d.txt".to_string(), 1),
            ("e.txt".to_string(), 1),
        ];
        let text = offer_text("Desk", &files);
        assert!(text.starts_with("Desk wants to send 5 file(s) ("), "{text}");
        assert!(
            text.contains("• a.txt") && text.contains("• dir/c.txt"),
            "{text}"
        );
        assert!(
            !text.contains("d.txt") && text.ends_with("…and 2 more"),
            "{text}"
        );
        let one = offer_text("Desk", &files[2..3]);
        assert!(one.ends_with("• dir/c.txt"), "{one}");
    }

    #[test]
    fn node_events_are_forwarded_with_a_repaint_and_none_are_lost() {
        let (tx, events) = crossbeam_channel::unbounded();
        let ctx = egui::Context::default();
        let rx = forward(events, ctx);
        for i in 0..1_000u32 {
            tx.send(NetEvent::Request {
                peer: peer(1),
                what: i.to_string(),
            })
            .unwrap();
        }
        tx.send(NetEvent::Paired(Peer {
            id: peer(2),
            label: "Desk".into(),
            last_seen: None,
            link: Link::Offline,
            storage: None,
        }))
        .unwrap();
        let got: Vec<NetEvent> = (0..1_001)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        assert!(matches!(got.last(), Some(NetEvent::Paired(p)) if p.label == "Desk"));
        drop(tx);
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_err(),
            "ends with the node"
        );
    }

    #[test]
    fn forgetting_a_device_takes_its_sources_along() {
        let at = |id: &str, root: &str| SourceSummary {
            root: VPath::parse(root).unwrap(),
            ..source(id, id, SourceKind::Device)
        };
        let sources = vec![
            at("mine", &format!("node://{}/docs", peer(1).0)),
            at("also", &format!("node://{}/photos", peer(1).0)),
            at("other", &format!("node://{}/docs", peer(2).0)),
            source("local", "Local", SourceKind::Folder),
        ];
        let ids: Vec<_> = device_sources_of(&sources, &peer(1))
            .into_iter()
            .map(|s| s.0)
            .collect();
        assert_eq!(ids, ["mine", "also"]);
    }
}
