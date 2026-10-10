//! The daemon: the profile's library (and keel-net) behind JSON-RPC 2.0 on a per-user
//! local socket, optionally a token-authenticated WebSocket too.
//!
//! Local socket: one JSON message per line, at most `rpc::MAX_REQUEST` bytes (a longer
//! one is read to its end, answered with an error and dropped). A connection may stay
//! idle for as long as it likes, but once a request has started it must arrive within
//! `READ_WAIT`; each request runs for at most `REQUEST_WAIT` before it is answered with a
//! timeout (it keeps running). Besides the registered operations (`keel_api::OPS`) the
//! daemon answers `subscribe` / `unsubscribe` (job progress, library and device events as
//! JSON-RPC notifications on that connection) and `daemon.shutdown`.

use crate::ws;
use anyhow::{bail, Context};
use crossbeam_channel::{Receiver, Sender};
use interprocess::local_socket::{prelude::*, Stream};
use keel_api::config::HostConfig;
use keel_api::host::{Host, NetSetup};
use keel_api::rpc::{self, Line, Request};
use keel_api::{socket, ApiError, Ctx};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Once a request has started, the rest of it must arrive within this.
pub const READ_WAIT: Duration = Duration::from_secs(30);
/// Longest a request runs before it is answered with a timeout.
pub const REQUEST_WAIT: Duration = Duration::from_secs(120);
/// Notifications a slow subscriber may fall behind by before it misses some.
const BACKLOG: usize = 1024;

pub struct Options {
    pub cfg: HostConfig,
    /// `--ws`: also serve JSON-RPC over a WebSocket on this address.
    pub ws: Option<SocketAddr>,
    /// `--web`: serve the browser client (and its WebSocket) on this address.
    pub web: Option<SocketAddr>,
    /// `--ws-allow-remote`: allow a non-loopback `ws` or `web` address.
    pub ws_allow_remote: bool,
    /// `--web-host`: host names a remote `web` bind answers to (besides its IP).
    pub web_hosts: Vec<String>,
    /// keel-net identity store and options (when `cfg.net` is on); the OS keychain and
    /// public discovery by default.
    pub net: Option<NetSetup>,
}

/// Another daemon already serves this profile.
#[derive(Debug)]
pub struct AlreadyRunning(pub String);

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "keel-daemon is already running for profile {}", self.0)
    }
}

impl std::error::Error for AlreadyRunning {}

/// Subscribed connections.
#[derive(Default)]
pub(crate) struct Hub {
    subs: Mutex<Vec<(u64, Sender<Value>)>>,
    next: AtomicU64,
}

impl Hub {
    fn subscribe(&self) -> (u64, Receiver<Value>) {
        let (tx, rx) = crossbeam_channel::bounded(BACKLOG);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.subs.lock().push((id, tx));
        (id, rx)
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        self.subs.lock().retain(|(i, _)| *i != id);
    }

    pub(crate) fn broadcast(&self, method: &str, params: Value) {
        let note = rpc::notification(method, params);
        self.subs.lock().retain(|(_, tx)| {
            !matches!(
                tx.try_send(note.clone()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
    }
}

/// One connection's subscription.
#[derive(Default)]
pub(crate) struct Session {
    pub(crate) sub: Option<(u64, Receiver<Value>)>,
}

pub(crate) struct Shared {
    pub(crate) host: Host,
    pub(crate) hub: Hub,
    pub(crate) stop: AtomicBool,
    shutdown: Sender<()>,
    pub(crate) profile: String,
    /// Where the profile's settings are read again (the integrity schedule).
    cfg: HostConfig,
    /// `--web` share-target uploads waiting for `share.claim`.
    pub(crate) uploads: crate::share::Uploads,
}

impl Shared {
    pub(crate) fn ctx(&self) -> &Arc<Ctx> {
        &self.host.ctx
    }

    pub(crate) fn dispatch(&self, req: &Request, session: &mut Session) -> keel_api::Result<Value> {
        match req.method.as_str() {
            "subscribe" => {
                if session.sub.is_none() {
                    session.sub = Some(self.hub.subscribe());
                }
                Ok(
                    json!({"subscribed": ["job.progress", "library.changed", "net.event", "daemon.stopping"]}),
                )
            }
            "unsubscribe" => {
                if let Some((id, _)) = session.sub.take() {
                    self.hub.unsubscribe(id);
                }
                Ok(json!({"subscribed": []}))
            }
            "share.claim" => {
                let id = req
                    .params
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.uploads.claim(id).map_err(ApiError::not_found)
            }
            "daemon.shutdown" => {
                let _ = self.shutdown.try_send(());
                Ok(json!({"stopping": true}))
            }
            method => {
                let (ctx, m, params) = (self.ctx().clone(), method.to_owned(), req.params.clone());
                let (tx, rx) = crossbeam_channel::bounded(1);
                std::thread::Builder::new()
                    .name("keel-daemon-request".into())
                    .spawn(move || {
                        let _ = tx.send(keel_api::call(&ctx, &m, params));
                    })
                    .map_err(|e| ApiError::failed(e.to_string()))?;
                let result = rx.recv_timeout(REQUEST_WAIT).unwrap_or_else(|_| {
                    Err(ApiError::new(
                        ApiError::TIMEOUT,
                        format!("{method} did not finish within {REQUEST_WAIT:?}"),
                    ))
                });
                if let Some(kind) = changed(method, &result) {
                    self.hub
                        .broadcast("library.changed", json!({ "method": method, "kind": kind }));
                }
                result
            }
        }
    }
}

/// What a call changed, for `library.changed`: the operation (`execute`: the executed
/// plan's), or None when nothing changed (an `execute` that started no job and has no
/// result, such as an integrity check that was not due yet).
pub(crate) fn changed(method: &str, result: &keel_api::Result<Value>) -> Option<String> {
    let v = result.as_ref().ok()?;
    match method {
        "execute" => {
            let empty = |r: &Value| r.is_null() || r.as_object().is_some_and(|o| o.is_empty());
            let job = v.get("job").is_some_and(|j| !j.is_null());
            let result = v.get("result").is_some_and(|r| !empty(r));
            (job || result).then(|| v["operation"].as_str().unwrap_or_default().to_owned())
        }
        "shares.revoke" | "recents.note" => Some(method.to_owned()),
        _ => None,
    }
}

pub struct Daemon {
    shared: Arc<Shared>,
    name: String,
    ws_addr: Option<SocketAddr>,
    web_addr: Option<SocketAddr>,
    requests: Receiver<()>,
}

impl Daemon {
    /// Claims the profile's socket, opens its library (and keel-net) and serves.
    pub fn start(opts: Options) -> anyhow::Result<Daemon> {
        for (flag, addr) in [("--ws", opts.ws), ("--web", opts.web)] {
            if let Some(addr) = addr.filter(|a| !a.ip().is_loopback() && !opts.ws_allow_remote) {
                bail!("{flag} {addr} is not a loopback address; add --ws-allow-remote to bind it");
            }
        }
        let cfg = opts.cfg;
        let name = cfg.socket_name();
        // The socket first: a second daemon never touches the library.
        let listener = match socket::bind(&name) {
            Ok(l) => l,
            Err(e) => {
                if keel_api::client::Client::connect(&name)
                    .and_then(|mut c| {
                        c.call("version", Value::Null)
                            .map_err(|e| io::Error::other(e.to_string()))
                    })
                    .is_ok()
                {
                    return Err(AlreadyRunning(cfg.profile.clone()).into());
                }
                return Err(e).with_context(|| format!("listening on {name}"));
            }
        };
        let host = Host::open(&cfg, opts.net.or_else(|| Some(NetSetup::system())), true)?
            .with_utc_offset(chrono::Local::now().offset().local_minus_utc().into())
            .with_mounts(&cfg.data_dir);
        host.ctx.lib.poll_status(Duration::from_secs(60))?;
        let (shutdown, requests) = crossbeam_channel::bounded(1);
        let shared = Arc::new(Shared {
            host,
            hub: Hub::default(),
            stop: AtomicBool::new(false),
            shutdown,
            profile: cfg.profile.clone(),
            cfg: cfg.clone(),
            uploads: crate::share::Uploads::open(cfg.data_dir.join("shares")),
        });
        pump(&shared);
        keep_watched(&shared);
        let ws_addr = match opts.ws {
            Some(addr) => Some(ws::serve(&shared, addr, &cfg.token_path())?),
            None => None,
        };
        let web_addr = match opts.web {
            Some(addr) => Some(crate::web::serve(
                &shared,
                addr,
                &cfg.token_path(),
                &opts.web_hosts,
            )?),
            None => None,
        };
        let s = shared.clone();
        std::thread::Builder::new()
            .name("keel-daemon-accept".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if s.stop.load(Ordering::Acquire) {
                        break; // dropping the listener frees the name
                    }
                    match conn {
                        Ok(conn) => {
                            let s = s.clone();
                            let spawned = std::thread::Builder::new()
                                .name("keel-daemon-client".into())
                                .spawn(move || serve(&s, conn));
                            if let Err(e) = spawned {
                                tracing::warn!("spawn client thread: {e}");
                            }
                        }
                        Err(e) => tracing::warn!("accept: {e}"),
                    }
                }
            })?;
        Ok(Daemon {
            shared,
            name,
            ws_addr,
            web_addr,
            requests,
        })
    }

    /// The local socket name clients connect to.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The WebSocket's bound address (`--ws` with port 0 picks one).
    pub fn ws_addr(&self) -> Option<SocketAddr> {
        self.ws_addr
    }

    /// The web client's bound address (`--web`).
    pub fn web_addr(&self) -> Option<SocketAddr> {
        self.web_addr
    }

    /// The library and keel-net the daemon serves (tests).
    #[doc(hidden)]
    pub fn ctx(&self) -> &Arc<Ctx> {
        self.shared.ctx()
    }

    /// Fires when a client called `daemon.shutdown`.
    pub fn shutdown_requests(&self) -> &Receiver<()> {
        &self.requests
    }

    /// Stops accepting, unmounts, closes keel-net and the library (jobs resume on the next
    /// start).
    pub fn shutdown(&self) {
        if self.shared.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        // Subscribers (an attached window) learn it before their connection ends.
        self.shared
            .hub
            .broadcast("daemon.stopping", json!({ "pid": std::process::id() }));
        // Wakes the accept loops, which then see `stop` and let go of the socket.
        let _ = socket::connect(&self.name, Duration::from_millis(500));
        for addr in self.ws_addr.iter().chain(&self.web_addr) {
            let _ = std::net::TcpStream::connect_timeout(addr, Duration::from_millis(500));
        }
        if !self.shared.host.close() {
            tracing::warn!("a job was still busy when the library closed");
        }
        tracing::info!("keel-daemon for profile {} stopped", self.shared.profile);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// While a sidecar job runs, `library.changed` `{method: "job", kind: "media.index", job,
/// done}` goes out at most this often (and when it ends, `done: true`): attached windows
/// ask again for the thumbnails it made.
pub const SIDECARS_EVERY: Duration = Duration::from_secs(2);

/// Job and device events to subscribers, until the daemon stops.
fn pump(shared: &Arc<Shared>) {
    let jobs = shared.ctx().lib.jobs().subscribe();
    let s = Arc::downgrade(shared);
    let _ = std::thread::Builder::new()
        .name("keel-daemon-jobs".into())
        .spawn(move || {
            // Per job: whether it makes sidecars, and when it last said so (or started).
            let mut sidecars: std::collections::HashMap<i64, (bool, Instant)> =
                Default::default();
            loop {
                let ev = jobs.recv_timeout(Duration::from_millis(500));
                let Some(s) = s.upgrade() else { return };
                if s.stop.load(Ordering::Acquire) {
                    return;
                }
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => return,
                };
                s.hub.broadcast(
                    "job.progress",
                    json!({
                        "id": ev.id,
                        "status": format!("{:?}", ev.status).to_lowercase(),
                        "progress": ev.progress,
                    }),
                );
                let lib = &s.ctx().lib;
                let (makes, last) = sidecars.entry(ev.id).or_insert_with(|| {
                    let kind = lib.jobs().info(ev.id).map(|j| j.kind).unwrap_or_default();
                    (kind == keel_core::SidecarJob::KIND, Instant::now())
                });
                let ended = !matches!(
                    ev.status,
                    keel_core::JobStatus::Queued | keel_core::JobStatus::Running
                );
                if *makes && (ended || last.elapsed() >= SIDECARS_EVERY) {
                    *last = Instant::now();
                    s.hub.broadcast(
                        "library.changed",
                        json!({"method": "job", "kind": "media.index", "job": ev.id, "done": ended}),
                    );
                }
                if ended {
                    sidecars.remove(&ev.id);
                }
            }
        });
    let Some(node) = shared.ctx().node.clone() else {
        return;
    };
    let events = node.events();
    let s = Arc::downgrade(shared);
    let _ = std::thread::Builder::new()
        .name("keel-daemon-net".into())
        .spawn(move || loop {
            let ev = events.recv_timeout(Duration::from_millis(500));
            let Some(s) = s.upgrade() else { return };
            if s.stop.load(Ordering::Acquire) {
                return;
            }
            use keel_net::NetEvent as E;
            let params = match ev {
                Ok(E::PeerOnline(p, link)) => {
                    json!({"event": "peer_online", "peer": p.0.to_string(), "link": format!("{link:?}").to_lowercase()})
                }
                Ok(E::PeerOffline(p)) => json!({"event": "peer_offline", "peer": p.0.to_string()}),
                Ok(E::Paired(p)) => {
                    json!({"event": "paired", "peer": p.id.0.to_string(), "label": p.label})
                }
                Ok(E::GrantChanged) => json!({"event": "grants_changed"}),
                Ok(E::Request { peer, what }) => {
                    json!({"event": "request", "peer": peer.0.to_string(), "what": what})
                }
                Ok(E::DropReceived { peer, id, path }) => {
                    json!({"event": "drop_received", "peer": peer.0.to_string(), "drop": id, "path": path.display().to_string()})
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => return,
            };
            s.hub.broadcast("net.event", params);
        });
}

/// How often new sources are picked up for watching.
const WATCH_EVERY: Duration = Duration::from_secs(2);
/// How often the integrity schedule is looked at.
const INTEGRITY_EVERY: Duration = Duration::from_secs(60);

/// Keeps every source current, as the window does: each one is watched (`Library::watch`:
/// live changes for local folders, a periodic re-walk for the others; a completed walk
/// schedules hashing) once no index job walks it, until it is removed or the daemon stops.
/// Runs the scheduled integrity checks too (Settings → Library, read again each time: an
/// attached window leaves them to the daemon).
fn keep_watched(shared: &Arc<Shared>) {
    let s = Arc::downgrade(shared);
    let _ = std::thread::Builder::new()
        .name("keel-daemon-watch".into())
        .spawn(move || {
            let mut watched = std::collections::HashSet::new();
            let mut next_integrity = Instant::now();
            loop {
                let Some(s) = s.upgrade() else { return };
                if s.stop.load(Ordering::Acquire) {
                    return;
                }
                let lib = &s.ctx().lib;
                if Instant::now() >= next_integrity {
                    next_integrity = Instant::now() + INTEGRITY_EVERY;
                    let c = &s.cfg;
                    let c = HostConfig::read(&c.profile, c.config_dir.clone(), c.data_dir.clone());
                    if c.integrity_days > 0 {
                        let every = Duration::from_secs(u64::from(c.integrity_days) * 24 * 60 * 60);
                        if let Err(e) = lib.schedule_integrity(c.integrity_pct, every) {
                            tracing::warn!("integrity check: {e:#}");
                        }
                    }
                }
                let sources = lib.sources();
                watched.retain(|id| sources.iter().any(|x| &x.id == id));
                for src in sources {
                    let indexing = matches!(src.status, keel_core::SourceStatus::Indexing { .. });
                    if !indexing && !watched.contains(&src.id) {
                        match lib.watch(&src.id) {
                            Ok(()) => {
                                watched.insert(src.id);
                            }
                            Err(e) => tracing::warn!("watch {}: {e:#}", src.label),
                        }
                    }
                }
                drop(s);
                std::thread::sleep(WATCH_EVERY);
            }
        });
}

/// Reads one request: waits as long as it takes for its first byte, then `READ_WAIT` in
/// all for the rest (checked before every read; on Unix each read also times out when the
/// time is up, on Windows a watchdog cancels it). Over `MAX_REQUEST`: reads on to the end
/// of the line and reports `TooLarge`.
fn read_request(reader: &mut BufReader<&Stream>, conn: &Stream) -> io::Result<Line> {
    if reader.fill_buf()?.is_empty() {
        return Ok(Line::Eof);
    }
    #[cfg(windows)]
    let _watchdog = socket::Watchdog::arm(conn, READ_WAIT);
    let deadline = Instant::now() + READ_WAIT;
    let got = rpc::read_line(reader, || {
        let left = time_left(deadline)?;
        #[cfg(unix)]
        conn.set_recv_timeout(Some(left))?;
        #[cfg(windows)]
        let _ = left;
        Ok(())
    });
    #[cfg(unix)]
    conn.set_recv_timeout(None)?;
    #[cfg(windows)]
    let _ = conn;
    got
}

/// The time left until `deadline`; TimedOut once it has passed.
pub(crate) fn time_left(deadline: Instant) -> io::Result<Duration> {
    match deadline.saturating_duration_since(Instant::now()) {
        left if left.is_zero() => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the request did not arrive in time",
        )),
        left => Ok(left),
    }
}

struct Out {
    conn: Arc<Stream>,
    lock: Mutex<()>,
}

impl Out {
    fn send(&self, v: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        let _guard = self.lock.lock();
        (&*self.conn).write_all(&line)?;
        (&*self.conn).flush()
    }
}

/// Serves one local-socket client until it disconnects.
fn serve(shared: &Arc<Shared>, conn: Stream) {
    if let Err(e) = socket::check_client(&conn) {
        tracing::warn!("client refused: {e}");
        return;
    }
    let out = Arc::new(Out {
        conn: Arc::new(conn),
        lock: Mutex::new(()),
    });
    let mut reader = BufReader::with_capacity(64 * 1024, &*out.conn);
    let mut session = Session::default();
    let mut notifier: Option<u64> = None;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let answer = match read_request(&mut reader, &out.conn) {
            Ok(Line::Line(line)) => {
                rpc::handle(&line, &mut |req| shared.dispatch(req, &mut session))
            }
            Ok(Line::TooLarge) => Some(rpc::too_large()),
            Ok(Line::Eof) => break,
            Err(e) => {
                tracing::debug!("client read: {e}");
                break;
            }
        };
        // A new subscription: a thread forwards its notifications.
        match &session.sub {
            Some((id, rx)) if notifier != Some(*id) => {
                notifier = Some(*id);
                let (rx, out) = (rx.clone(), out.clone());
                let _ = std::thread::Builder::new()
                    .name("keel-daemon-notify".into())
                    .spawn(move || {
                        for note in rx {
                            if out.send(&note).is_err() {
                                break;
                            }
                        }
                    });
            }
            None => notifier = None,
            _ => {}
        }
        if let Some(answer) = answer {
            if out.send(&answer).is_err() {
                break;
            }
        }
    }
    if let Some((id, _)) = session.sub.take() {
        shared.hub.unsubscribe(id);
    }
}
