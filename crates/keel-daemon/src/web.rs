//! `--web`: the browser client (crate keel-web), off by default and loopback only unless
//! `--ws-allow-remote`. One listener serves:
//!
//! - `/` and the bundle's files, embedded at build time (build.rs);
//! - `/rpc`: JSON-RPC over a WebSocket. Browsers cannot send an `Authorization` header on
//!   a WebSocket, so the first message must be `auth` with the token from
//!   `<config dir>/daemon.token`; the token never travels in a URL (a query string on
//!   `/rpc` is refused), so it cannot leak through history, referrers or logs;
//! - `/file/<token>`: a download made by `file.get`, valid once for 60 seconds;
//! - `POST /share`: the PWA's Web Share Target; the files are parked until the signed-in
//!   client claims them (`crate::share`). The only request that is not a `GET`;
//! - `/everything` (and `/` with Everything's parameters) and `/everything/file`: the
//!   system search for tools written for Everything's HTTP server (`crate::everything`),
//!   signed in with the token or the HTTP login on every request.
//!
//! Every answer is `no-store` with `Referrer-Policy: no-referrer` and a CSP that allows
//! only this origin (no CDN, no external fonts; the manifest and the service worker from
//! this origin too). The `Host` header must name a loopback
//! host on a loopback bind or the bound IP on a remote one, or a `--web-host` name (DNS
//! rebinding); a cross-origin WebSocket is refused. As on `--ws` (`crate::ws`), the
//! request head must be in within `HANDSHAKE_WAIT` in all, at most `MAX_CONNECTIONS` are
//! served at once and writes time out after `WRITE_WAIT`; of those, at most
//! `MAX_UNAUTHENTICATED` have not signed in yet (a message before `auth` is at most
//! `ws::AUTH_MAX` bytes) and at most `MAX_PER_IP` come from one remote address.
//!
//! There is no TLS. A non-loopback bind is therefore refused unless the profile's
//! config.toml says `[web] allow_plaintext = true` (`server::Daemon::start`), and even then
//! the Everything endpoint takes no Basic auth there (only `Authorization: Bearer`): put
//! a TLS proxy in front of a loopback bind to use the HTTP login, or keep a plain bind on
//! a private network (a tailnet).

use crate::everything;
use crate::server::Shared;
use crate::ws;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::protocol::Role;
use tungstenite::WebSocket;

include!(concat!(env!("OUT_DIR"), "/web_bundle.rs"));

/// Longest request head read.
const MAX_HEAD: usize = 16 * 1024;
/// Connections that have not signed in (or are fetching a page) at once: the rest of the
/// `MAX_CONNECTIONS` stay for signed-in sessions.
pub(crate) const MAX_UNAUTHENTICATED: usize = 16;
/// Connections at once from one remote address (loopback is not limited this way).
pub(crate) const MAX_PER_IP: usize = 8;

/// One of `MAX_PER_IP` places of a remote address; given back on drop.
struct IpSlot(Arc<Mutex<HashMap<IpAddr, usize>>>, IpAddr);

impl IpSlot {
    fn take(map: &Arc<Mutex<HashMap<IpAddr, usize>>>, ip: IpAddr) -> Option<IpSlot> {
        let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(ip).or_default();
        (*n < MAX_PER_IP).then(|| {
            *n += 1;
            IpSlot(map.clone(), ip)
        })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.1);
            }
        }
    }
}

/// Failed Everything sign-ins an address may make before it is locked out.
pub(crate) const FREE_FAILS: u32 = 5;
/// The first lockout; each further failure doubles it.
const LOCK_BASE: Duration = Duration::from_secs(5);
/// The longest lockout.
const LOCK_MAX: Duration = Duration::from_secs(15 * 60);
/// An address that has not failed for this long starts again.
const FORGET_AFTER: Duration = Duration::from_secs(30 * 60);
/// Failed sign-ins from all addresses together per `GLOBAL_WINDOW` before every request
/// that carries credentials is refused for the rest of the window (many addresses
/// guessing at once).
const GLOBAL_FAILS: usize = 100;
const GLOBAL_WINDOW: Duration = Duration::from_secs(60);
/// Addresses remembered at once.
const MAX_TRACKED: usize = 4096;

/// What failed Everything sign-ins cost: an address that keeps sending wrong credentials is
/// answered `429` without any check (so a right guess during the lockout gains nothing) for
/// a time that doubles with each failure, and a flood of failures from many addresses
/// refuses credentials for everyone for a while. Nothing here sleeps, so a guesser holds no
/// connection slot. Behind a reverse proxy all users share the proxy's address.
#[derive(Default)]
pub(crate) struct AuthGuard(Mutex<GuardState>);

#[derive(Default)]
struct GuardState {
    by_addr: HashMap<IpAddr, Fails>,
    /// When the latest failures (all addresses) happened, oldest first.
    recent: std::collections::VecDeque<Instant>,
}

struct Fails {
    count: u32,
    until: Instant,
    last: Instant,
}

/// An IPv6 client owns a whole /64: lockouts follow the network, not the address.
fn network(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
        v4 => v4,
    }
}

impl AuthGuard {
    /// `Err(wait)` when `ip` may not sign in now.
    fn check(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        while g
            .recent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= GLOBAL_WINDOW)
        {
            g.recent.pop_front();
        }
        if g.recent.len() >= GLOBAL_FAILS {
            let oldest = g.recent.front().copied().unwrap_or(now);
            return Err(GLOBAL_WINDOW.saturating_sub(now.saturating_duration_since(oldest)));
        }
        match g.by_addr.get(&network(ip)) {
            Some(f) if f.until > now => Err(f.until - now),
            _ => Ok(()),
        }
    }

    /// Records wrong credentials from `ip`.
    fn failed(&self, ip: IpAddr, now: Instant) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        g.recent.push_back(now);
        while g.recent.len() > GLOBAL_FAILS * 2 {
            g.recent.pop_front();
        }
        if g.by_addr.len() >= MAX_TRACKED {
            g.by_addr
                .retain(|_, f| now.saturating_duration_since(f.last) < FORGET_AFTER);
            if g.by_addr.len() >= MAX_TRACKED {
                let oldest = g.by_addr.iter().min_by_key(|(_, f)| f.last).map(|(k, _)| *k);
                if let Some(k) = oldest {
                    g.by_addr.remove(&k);
                }
            }
        }
        let f = g.by_addr.entry(network(ip)).or_insert(Fails {
            count: 0,
            until: now,
            last: now,
        });
        if now.saturating_duration_since(f.last) >= FORGET_AFTER {
            f.count = 0;
        }
        f.count = f.count.saturating_add(1);
        f.last = now;
        if f.count >= FREE_FAILS {
            let doublings = (f.count - FREE_FAILS).min(16);
            f.until = now + (LOCK_BASE * 2u32.pow(doublings)).min(LOCK_MAX);
        }
    }

    /// `ip` signed in: its failures are forgotten.
    fn succeeded(&self, ip: IpAddr) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        g.by_addr.remove(&network(ip));
    }
}

/// What the Everything endpoint keeps between requests.
#[derive(Default)]
pub(crate) struct Gate {
    guard: AuthGuard,
    /// The files recent searches listed (downloads with the HTTP login are held to them).
    recent: everything::Recent,
}

/// Which `Host` headers a listener answers.
#[derive(Clone)]
pub(crate) struct Hosts {
    bound: SocketAddr,
    /// `--web-host` names (lowercase).
    names: Vec<String>,
}

impl Hosts {
    /// Loopback bind: loopback names; remote bind: the bound IP. Either: a `--web-host`
    /// name (a tailnet name, or the name a TLS reverse proxy in front of a loopback bind
    /// passes on).
    fn allow(&self, authority: &str) -> bool {
        let name = host_name(authority).to_ascii_lowercase();
        if self.names.contains(&name) {
            return true;
        }
        if self.bound.ip().is_loopback() {
            return loopback_host(authority);
        }
        let ip = match self.bound.ip() {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        name == ip
    }
}

/// Shown when keel-daemon was built without the web client (`scripts/build-web.*`).
const PLACEHOLDER: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Keel</title></head>\
<body><h1>Keel</h1><p>This keel-daemon was built without the web client. Build it with \
<code>scripts/build-web.sh</code> (or <code>scripts\\build-web.ps1</code>), then rebuild keel-daemon.</p></body></html>";

/// Binds `addr` and serves browsers on their own threads; returns the bound address.
/// `names`: the `--web-host` names a remote bind also answers to.
pub(crate) fn serve(
    shared: &Arc<Shared>,
    addr: SocketAddr,
    token_path: &Path,
    names: &[String],
) -> anyhow::Result<SocketAddr> {
    let tokens = ws::Token(Arc::new(token_path.to_owned()));
    tokens.current()?;
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let hosts = Hosts {
        bound,
        names: names
            .iter()
            .map(|n| n.trim().to_ascii_lowercase())
            .collect(),
    };
    let s = shared.clone();
    // Unclaimed shares go after their wait even when no one posts or claims.
    let _ = std::thread::Builder::new()
        .name("keel-daemon-shares".into())
        .spawn(move || {
            while !s.stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_secs(5));
                s.uploads.sweep();
            }
        });
    let s = shared.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let unauthenticated = Arc::new(AtomicUsize::new(0));
    let per_ip = Arc::new(Mutex::new(HashMap::new()));
    let gate = Arc::new(Gate::default());
    std::thread::Builder::new()
        .name("keel-daemon-web".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if s.stop.load(Ordering::Acquire) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                // Dropping `stream` (any `continue`) closes it.
                let Some(slot) = ws::Slot::take(&count, ws::MAX_CONNECTIONS) else {
                    tracing::debug!("web: over {} connections, dropped", ws::MAX_CONNECTIONS);
                    continue;
                };
                let Some(pending) = ws::Slot::take(&unauthenticated, MAX_UNAUTHENTICATED) else {
                    tracing::debug!("web: over {MAX_UNAUTHENTICATED} unauthenticated, dropped");
                    continue;
                };
                let ip_slot = match stream.peer_addr() {
                    Ok(peer) if !peer.ip().is_loopback() => {
                        match IpSlot::take(&per_ip, peer.ip()) {
                            Some(s) => Some(s),
                            None => {
                                tracing::debug!("web: over {MAX_PER_IP} from {}", peer.ip());
                                continue;
                            }
                        }
                    }
                    _ => None,
                };
                let (s, tokens, hosts, gate) =
                    (s.clone(), tokens.clone(), hosts.clone(), gate.clone());
                let _ = std::thread::Builder::new()
                    .name("keel-daemon-web-client".into())
                    .spawn(move || {
                        let _slots = (slot, ip_slot);
                        if let Err(e) = client(&s, stream, &tokens, &hosts, &gate, pending) {
                            tracing::debug!("web client: {e}");
                        }
                    });
            }
        })?;
    tracing::info!("web client on http://{bound}/");
    if !bound.ip().is_loopback() {
        tracing::warn!("{}", everything::UNENCRYPTED);
    }
    Ok(bound)
}

struct Head {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    /// Bytes read past the head (the start of a body).
    rest: Vec<u8>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads and parses a request head (only `POST /share` has a body), all of it by `deadline`:
/// non-blocking, so a client dripping bytes cannot stretch it (a per-read timeout restarts
/// with every byte). Leaves the stream non-blocking; the caller makes it blocking again.
fn read_head(stream: &mut TcpStream, deadline: Instant) -> io::Result<Option<Head>> {
    stream.set_nonblocking(true)?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                crate::server::time_left(deadline)?;
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let used = match req.parse(&buf) {
        Ok(httparse::Status::Complete(n)) => n,
        _ => return Ok(None),
    };
    let target = req.path.unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q.to_owned())),
        None => (target, None),
    };
    Ok(Some(Head {
        method: req.method.unwrap_or("").to_owned(),
        path: path.to_owned(),
        query,
        headers: req
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_owned(),
                    String::from_utf8_lossy(h.value).into_owned(),
                )
            })
            .collect(),
        rest: buf[used..].to_vec(),
    }))
}

/// The host name of a `Host` header (or an `Origin`'s authority): `[::1]:7421` -> `[::1]`.
fn host_name(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.split_inclusive(']').next().unwrap_or(authority);
    }
    authority.split(':').next().unwrap_or(authority)
}

fn loopback_host(authority: &str) -> bool {
    matches!(
        host_name(authority).to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "[::1]"
    )
}

/// Characters a host (and so the CSP built from it) may contain.
fn plain_host(authority: &str) -> bool {
    !authority.is_empty()
        && authority
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
}

fn send(
    stream: &mut TcpStream,
    status: &str,
    host: &str,
    extra: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let csp = format!(
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
         connect-src 'self' ws://{host} wss://{host}; img-src 'self' blob: data:; \
         style-src 'self' 'unsafe-inline'; manifest-src 'self'; worker-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    send_csp(stream, status, &csp, extra, body)
}

/// The Everything pages' policy: no script, inline style, the form to this origin.
const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
                        base-uri 'none'; frame-ancestors 'none'";

fn send_csp(
    stream: &mut TcpStream,
    status: &str,
    csp: &str,
    extra: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\nCross-Origin-Resource-Policy: same-origin\r\n\
         Content-Security-Policy: {csp}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn refuse(stream: &mut TcpStream, status: &str, host: &str, why: &str) -> anyhow::Result<()> {
    let ctype = [("Content-Type", "text/plain; charset=utf-8".to_owned())];
    send(stream, status, host, &ctype, why.as_bytes())?;
    Ok(())
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// RFC 5987 `filename*` value.
fn attachment(name: &str) -> String {
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename*=UTF-8''{encoded}")
}

fn client(
    shared: &Arc<Shared>,
    mut stream: TcpStream,
    tokens: &ws::Token,
    hosts: &Hosts,
    gate: &Gate,
    pending: ws::Slot,
) -> anyhow::Result<()> {
    let head = read_head(&mut stream, Instant::now() + ws::HANDSHAKE_WAIT)?;
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(ws::WRITE_WAIT))?;
    let Some(head) = head else {
        return refuse(&mut stream, "400 Bad Request", "localhost", "bad request");
    };
    let host = head.header("host").unwrap_or_default().to_owned();
    if !plain_host(&host) {
        return refuse(
            &mut stream,
            "400 Bad Request",
            "localhost",
            "bad Host header",
        );
    }
    if !hosts.allow(&host) {
        let why = if hosts.bound.ip().is_loopback() {
            "use a loopback host name"
        } else {
            "unknown host name (keel-daemon --web-host <name>)"
        };
        return refuse(&mut stream, "403 Forbidden", &host, why);
    }
    if head.method == "POST" && head.path == "/share" {
        return share(shared, stream, head, &host);
    }
    if head.method != "GET" {
        return refuse(&mut stream, "405 Method Not Allowed", &host, "GET only");
    }
    if head.path == "/rpc" {
        return rpc(shared, stream, &head, &host, tokens, pending);
    }
    if head.path == "/everything/file" {
        let ev = Everything {
            shared,
            hosts,
            tokens,
            gate,
        };
        return everything_file(&ev, &mut stream, &head, &host, pending);
    }
    if matches!(head.path.as_str(), "/everything" | "/everything/")
        || (head.path == "/" && head.query.as_deref().is_some_and(everything::is_search))
    {
        let ev = Everything {
            shared,
            hosts,
            tokens,
            gate,
        };
        return everything_search(&ev, &mut stream, &head, &host, pending);
    }
    if let Some(link) = head.path.strip_prefix("/file/") {
        return download(shared, &mut stream, &host, link);
    }
    let name = match head.path.trim_start_matches('/') {
        "" => "index.html",
        n => n,
    };
    match FILES.iter().find(|(n, _)| *n == name) {
        Some((n, body)) => send(
            &mut stream,
            "200 OK",
            &host,
            &[("Content-Type", content_type(n).into())],
            body,
        )?,
        None if name == "index.html" => send(
            &mut stream,
            "200 OK",
            &host,
            &[("Content-Type", content_type(name).into())],
            PLACEHOLDER.as_bytes(),
        )?,
        None => return refuse(&mut stream, "404 Not Found", &host, "not found"),
    }
    Ok(())
}

/// What the Everything handlers share.
struct Everything<'a> {
    shared: &'a Arc<Shared>,
    hosts: &'a Hosts,
    tokens: &'a ws::Token,
    gate: &'a Gate,
}

impl Everything<'_> {
    /// Plain http on a network address (not loopback; behind a TLS proxy the bind is
    /// loopback).
    fn plaintext(&self) -> bool {
        !self.hosts.bound.ip().is_loopback()
    }
}

/// Signs in an Everything request: the daemon token (bearer, or the Basic password), or the
/// HTTP login as Basic auth. None when it was answered instead: 401, 403 for a request
/// another site's page made or Basic auth over plain http on a network address (the
/// password would cross the network readable), 429 for an address that sent too many wrong
/// credentials.
fn everything_auth(
    ev: &Everything,
    stream: &mut TcpStream,
    head: &Head,
    host: &str,
) -> anyhow::Result<Option<everything::Who>> {
    // A page on another site may link or embed these URLs; the browser says so.
    if matches!(
        head.header("sec-fetch-site"),
        Some("cross-site" | "same-site")
    ) {
        refuse(stream, "403 Forbidden", host, "cross-site request refused")?;
        return Ok(None);
    }
    let header = head.header("authorization");
    let basic = everything::is_basic(header);
    if basic && ev.plaintext() {
        refuse(
            stream,
            "403 Forbidden",
            host,
            "Basic sign-in is refused over plain http on a network address: send \
             `Authorization: Bearer <token>`, or reach keel-daemon through a TLS proxy \
             (--web on a loopback address)",
        )?;
        return Ok(None);
    }
    let ip = stream
        .peer_addr()
        .map_or(IpAddr::from([0, 0, 0, 0]), |a| a.ip());
    // Credentials are not even looked at while the address is locked out.
    if header.is_some() {
        if let Err(wait) = ev.gate.guard.check(ip, Instant::now()) {
            let secs = wait.as_secs().max(1);
            send_csp(
                stream,
                "429 Too Many Requests",
                PAGE_CSP,
                &[
                    ("Retry-After", secs.to_string()),
                    ("Content-Type", "text/plain; charset=utf-8".into()),
                ],
                format!("Too many wrong sign-ins: try again in {secs} seconds.").as_bytes(),
            )?;
            return Ok(None);
        }
    }
    let token = ev.tokens.current()?;
    let login = if basic { ev.shared.http_login() } else { None };
    if let Some(who) = everything::signed_in(header, &token, login.as_ref()) {
        ev.gate.guard.succeeded(ip);
        return Ok(Some(who));
    }
    if header.is_some() {
        ev.gate.guard.failed(ip, Instant::now());
    }
    send_csp(
        stream,
        "401 Unauthorized",
        PAGE_CSP,
        &[
            (
                "WWW-Authenticate",
                "Basic realm=\"Keel\", charset=\"UTF-8\"".into(),
            ),
            ("Content-Type", "text/plain; charset=utf-8".into()),
        ],
        b"Sign in with the daemon token, or the HTTP login (keel-daemon --set-http-login).",
    )?;
    Ok(None)
}

/// `/everything` (or `/?search=...`): the system search with Everything's parameters, as
/// JSON (`json=1`) or an HTML page. `pending` (an unauthenticated place) is given back once
/// the request has signed in.
fn everything_search(
    ev: &Everything,
    stream: &mut TcpStream,
    head: &Head,
    host: &str,
    pending: ws::Slot,
) -> anyhow::Result<()> {
    if everything_auth(ev, stream, head, host)?.is_none() {
        return Ok(());
    }
    drop(pending);
    let shared = ev.shared;
    let r = match everything::parse(head.query.as_deref().unwrap_or_default()) {
        Ok(r) => r,
        Err(why) => return refuse(stream, "400 Bad Request", host, &why),
    };
    let searcher = shared.ctx().system.get();
    let found = keel_api::system::run(&*searcher, &r.params, keel_api::system::MAX_SCAN);
    if let Ok(f) = &found {
        for hit in f.hits.iter().filter(|h| !h.is_dir) {
            ev.gate.recent.remember(&hit.path.display());
        }
    }
    let status = match &found {
        Ok(_) => "200 OK",
        Err(e) if e.code == keel_api::ApiError::INVALID_PARAMS => "400 Bad Request",
        Err(_) => "503 Service Unavailable",
    };
    let (ctype, body) = if r.json {
        let v = match &found {
            Ok(f) => everything::to_json(f, &r),
            Err(e) => serde_json::json!({ "error": e.message }),
        };
        ("application/json; charset=utf-8", v.to_string())
    } else {
        let page = everything::Page {
            base: "/everything",
            downloads: shared.allow_downloads(),
            unencrypted: ev.plaintext(),
        };
        let found = found.as_ref().map_err(|e| e.message.as_str());
        let html = everything::to_html(&r, found, &page);
        ("text/html; charset=utf-8", html)
    };
    send_csp(
        stream,
        status,
        PAGE_CSP,
        &[("Content-Type", ctype.into())],
        body.as_bytes(),
    )?;
    Ok(())
}

/// `/everything/file?path=...`: a file, while `[web] allow_downloads` is on. Signed in
/// with the HTTP login, only a file a recent search listed and outside Keel's own folders.
fn everything_file(
    ev: &Everything,
    stream: &mut TcpStream,
    head: &Head,
    host: &str,
    pending: ws::Slot,
) -> anyhow::Result<()> {
    let Some(who) = everything_auth(ev, stream, head, host)? else {
        return Ok(());
    };
    drop(pending);
    let shared = ev.shared;
    if !shared.allow_downloads() {
        return refuse(
            stream,
            "403 Forbidden",
            host,
            "downloads are off: [web] allow_downloads = true in the profile's config.toml turns them on",
        );
    }
    let query = head.query.as_deref().unwrap_or_default();
    let Some(path) = everything::param(query, "path") else {
        return refuse(stream, "400 Bad Request", host, "path=<the file> needed");
    };
    if who == everything::Who::Login
        && (!ev.gate.recent.contains(&path) || keel_api::files::in_keel_folders(shared.ctx(), &path))
    {
        return refuse(
            stream,
            "403 Forbidden",
            host,
            "the HTTP login downloads only files a recent search listed (and none in Keel's own folders)",
        );
    }
    // Through file.get: what the read operations may not reach stays out of reach.
    let link = keel_api::call(
        shared.ctx(),
        "file.get",
        serde_json::json!({ "path": path }),
    );
    let link = match link {
        Ok(v) => v["url"].as_str().unwrap_or_default().to_owned(),
        Err(e) => return refuse(stream, "404 Not Found", host, &e.message),
    };
    download(shared, stream, host, link.trim_start_matches("/file/"))
}

/// How long a share upload may stall between reads.
const SHARE_IDLE: Duration = Duration::from_secs(30);

/// `POST /share` (the Web Share Target): parks the files and sends the browser to the
/// client with the upload id, which only the signed-in client can claim.
fn share(
    shared: &Arc<Shared>,
    mut stream: TcpStream,
    head: Head,
    host: &str,
) -> anyhow::Result<()> {
    // A page on another site may post a form here: browsers say where it came from. (The
    // share sheet's own request is same-origin or user-initiated, `none`.)
    let site = head.header("sec-fetch-site").unwrap_or("none");
    let origin_ok = match head.header("origin") {
        None | Some("null") => true,
        Some(o) => o.split_once("://").map_or("", |(_, a)| a) == host,
    };
    if !origin_ok || !matches!(site, "none" | "same-origin") {
        return refuse(
            &mut stream,
            "403 Forbidden",
            host,
            "cross-site share refused",
        );
    }
    let Some(len) = head
        .header("content-length")
        .and_then(|l| l.trim().parse::<u64>().ok())
    else {
        return refuse(
            &mut stream,
            "411 Length Required",
            host,
            "Content-Length needed",
        );
    };
    let ctype = head.header("content-type").unwrap_or_default().to_owned();
    stream.set_read_timeout(Some(SHARE_IDLE))?;
    let body = io::Cursor::new(head.rest).chain(&stream);
    match shared.uploads.receive(body, len, &ctype) {
        Ok(id) => send(
            &mut stream,
            "303 See Other",
            host,
            &[
                // The counts are only for the client's question before it claims.
                ("Location", {
                    let (n, bytes) = shared.uploads.summary(&id).unwrap_or_default();
                    format!("/?share={id}&files={n}&bytes={bytes}")
                }),
                ("Content-Type", "text/plain; charset=utf-8".into()),
            ],
            b"Shared: open Keel to send it.",
        )?,
        Err(crate::share::Refused(status, why)) => refuse(&mut stream, status, host, &why)?,
    }
    Ok(())
}

fn rpc(
    shared: &Arc<Shared>,
    mut stream: TcpStream,
    head: &Head,
    host: &str,
    tokens: &ws::Token,
    pending: ws::Slot,
) -> anyhow::Result<()> {
    // The token goes in the first message, never in the address.
    if head.query.is_some() {
        return refuse(
            &mut stream,
            "400 Bad Request",
            host,
            "/rpc takes no query string",
        );
    }
    // A page from another origin may open a WebSocket here; only our own page may.
    if let Some(origin) = head.header("origin") {
        let authority = origin.split_once("://").map_or("", |(_, a)| a);
        if authority != host {
            return refuse(
                &mut stream,
                "403 Forbidden",
                host,
                "cross-origin WebSocket refused",
            );
        }
    }
    let upgrade = head
        .header("upgrade")
        .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    let (Some(key), true, Some("13")) = (
        head.header("sec-websocket-key"),
        upgrade,
        head.header("sec-websocket-version"),
    ) else {
        return refuse(&mut stream, "426 Upgrade Required", host, "WebSocket only");
    };
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let answer = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(answer.as_bytes())?;
    stream.flush()?;
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, Some(ws::config()));
    // A native client may still send the header; a browser authenticates in-band.
    let token = tokens.current()?;
    let bearer = head.header("authorization").unwrap_or_default();
    let header_ok = ws::same(bearer.as_bytes(), format!("Bearer {token}").as_bytes());
    ws::run(
        shared,
        &mut socket,
        &token,
        !header_ok,
        tokens,
        Some(pending),
    )
}

fn download(
    shared: &Arc<Shared>,
    stream: &mut TcpStream,
    host: &str,
    link: &str,
) -> anyhow::Result<()> {
    let (name, size, mut body) = match keel_api::files::open_link(shared.ctx(), link) {
        Ok(found) => found,
        Err(e) => return refuse(stream, "404 Not Found", host, &e.message),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {size}\r\n\
         Content-Disposition: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        attachment(&name)
    );
    stream.write_all(head.as_bytes())?;
    // A file that grew since the stat is cut at the announced length.
    io::copy(&mut (&mut body).take(size), stream)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_remote_address_gets_at_most_max_per_ip() {
        let map = Arc::new(Mutex::new(HashMap::new()));
        let ip: IpAddr = "192.0.2.9".parse().unwrap();
        let held: Vec<IpSlot> = (0..MAX_PER_IP)
            .map(|_| IpSlot::take(&map, ip).unwrap())
            .collect();
        assert!(IpSlot::take(&map, ip).is_none());
        assert!(IpSlot::take(&map, "192.0.2.10".parse().unwrap()).is_some());
        drop(held);
        assert!(IpSlot::take(&map, ip).is_some());
        assert!(map.lock().unwrap().get(&ip).is_none(), "given back");
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Review 1: a wrong login only cost a one second sleep that held the connection's
    /// slots, with no counting. Now an address is locked out after a few failures, for a
    /// time that doubles, without sleeping.
    #[test]
    fn wrong_credentials_lock_an_address_out_for_a_growing_time() {
        let g = AuthGuard::default();
        let (a, b) = (ip("192.0.2.9"), ip("192.0.2.10"));
        let t = Instant::now();
        for _ in 0..FREE_FAILS - 1 {
            assert!(g.check(a, t).is_ok());
            g.failed(a, t);
        }
        assert!(g.check(a, t).is_ok(), "the last free try");
        g.failed(a, t);
        assert_eq!(g.check(a, t), Err(LOCK_BASE));
        assert_eq!(g.check(a, t + LOCK_BASE / 2), Err(LOCK_BASE / 2));
        assert!(g.check(b, t).is_ok(), "another address is untouched");
        // Each further failure doubles the wait, up to the cap.
        let t2 = t + LOCK_BASE;
        assert!(g.check(a, t2).is_ok());
        g.failed(a, t2);
        assert_eq!(g.check(a, t2), Err(LOCK_BASE * 2));
        let mut now = t2;
        for _ in 0..40 {
            now += LOCK_MAX;
            g.failed(a, now);
        }
        assert_eq!(g.check(a, now), Err(LOCK_MAX));
        // A right sign-in clears it.
        g.succeeded(a);
        assert!(g.check(a, now).is_ok());
        // An address that stopped failing is forgiven.
        for _ in 0..FREE_FAILS - 1 {
            g.failed(b, t);
        }
        g.failed(b, t + FORGET_AFTER);
        assert!(g.check(b, t + FORGET_AFTER).is_ok(), "started again at one");
    }

    #[test]
    fn an_ipv6_network_is_locked_out_as_one() {
        let g = AuthGuard::default();
        let t = Instant::now();
        for i in 0..FREE_FAILS {
            g.failed(ip(&format!("2001:db8:1:2::{}", i + 1)), t);
        }
        assert!(g.check(ip("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), t).is_err());
        assert!(g.check(ip("2001:db8:1:3::1"), t).is_ok());
    }

    #[test]
    fn many_addresses_guessing_at_once_lock_everyone_out_for_a_while() {
        let g = AuthGuard::default();
        let t = Instant::now();
        for i in 0..GLOBAL_FAILS as u32 {
            assert!(g.check(IpAddr::from([10, 0, (i / 250) as u8, (i % 250) as u8]), t).is_ok());
            g.failed(IpAddr::from([10, 0, (i / 250) as u8, (i % 250) as u8]), t);
        }
        let fresh = ip("203.0.113.7");
        assert_eq!(g.check(fresh, t), Err(GLOBAL_WINDOW));
        assert!(g.check(fresh, t + GLOBAL_WINDOW).is_ok(), "the window passed");
    }

    #[test]
    fn the_lockout_table_stays_bounded() {
        let g = AuthGuard::default();
        let t = Instant::now();
        for i in 0..(MAX_TRACKED as u32 + 50) {
            g.failed(IpAddr::from(i.to_be_bytes()), t);
        }
        assert!(g.0.lock().unwrap().by_addr.len() <= MAX_TRACKED);
    }

    #[test]
    fn hosts_and_filenames() {
        assert!(loopback_host("localhost:7421"));
        assert!(loopback_host("127.0.0.1"));
        assert!(loopback_host("[::1]:7421"));
        assert!(!loopback_host("evil.example:7421"));
        assert!(!loopback_host("127.0.0.1.evil.example"));
        let remote = Hosts {
            bound: "192.0.2.7:7421".parse().unwrap(),
            names: vec!["keel.example".into()],
        };
        assert!(remote.allow("192.0.2.7:7421"));
        assert!(remote.allow("KEEL.example:7421"));
        assert!(!remote.allow("evil.example:7421"));
        assert!(
            !remote.allow("localhost:7421"),
            "a remote bind lists its names"
        );
        let v6 = Hosts {
            bound: "[2001:db8::1]:7421".parse().unwrap(),
            names: Vec::new(),
        };
        assert!(v6.allow("[2001:db8::1]:7421"));
        let local = Hosts {
            bound: "127.0.0.1:7421".parse().unwrap(),
            names: vec!["keel.example".into()],
        };
        assert!(local.allow("localhost:7421") && local.allow("KEEL.example:443"));
        assert!(!local.allow("evil.example"), "only the names given");
        assert!(plain_host("host.example:7421"));
        assert!(!plain_host("a; script-src *"));
        assert!(!plain_host(""));
        assert_eq!(
            attachment("a b\"ü.txt"),
            "attachment; filename*=UTF-8''a%20b%22%C3%BC.txt"
        );
    }
}
