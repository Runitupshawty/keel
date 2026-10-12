//! `--etp` / `[etp]`: an ETP/FTP server compatible with Everything's ETP clients (FTP with
//! Everything's `SITE EVERYTHING` search extension, voidtools' ETP), over the system search
//! (`keel_api::system`) and limited to the configured folders (`fs`: each root a top folder
//! under its own name, SUBST-style).
//!
//! Safer than Everything's: off by default; a loopback bind by default; a user and password
//! always (`keel-daemon --set-etp-login USER`, kept in the OS keychain; Everything accepts
//! anyone when they are empty); read-only (no command that changes a file exists); files are
//! sent only while `[etp] allow_downloads = true` (`RETR` is refused otherwise; browsing and
//! searching still work); passive mode only, the data connection only from the control
//! connection's address; explicit FTPS (`AUTH TLS`, `PROT P`) with the certificate
//! `[etp] tls_cert` / `tls_key` names, required by default once configured; a non-loopback
//! bind without required TLS is refused unless `[etp] allow_unencrypted = true` says the
//! user accepts that the password, the searches and the files cross the network readable.
//! At most `MAX_CONNECTIONS` connections (`MAX_PER_IP` from one remote address), three
//! failed sign-ins per connection, a pause after each.

pub mod fs;
pub mod proto;
#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
use fs::{Roots, Target, Virtual};
use keel_api::system::{Hit, Query, Searcher};
use keel_api::types::SystemSearchParams;
use keel_vfs::cloud::SecretStore;
use proto::Command;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The welcome message when `[etp] welcome` is empty.
pub const DEFAULT_WELCOME: &str = "Keel ETP/FTP server";
/// `[etp] bind` by default: Everything's port 21 on Windows; elsewhere a port that needs
/// no administrator.
pub const DEFAULT_BIND: &str = if cfg!(windows) {
    "127.0.0.1:21"
} else {
    "127.0.0.1:2121"
};
/// Why a non-loopback bind without TLS needs `[etp] allow_unencrypted = true`.
pub const UNENCRYPTED: &str = "the ETP server is bound to a network address without \
required TLS: the user name, the password, the searches, the results and the files cross the \
network unencrypted (Everything's ETP clients cannot use TLS). Set [etp] tls_cert and tls_key \
for FTPS, or accept this with [etp] allow_unencrypted = true.";

/// Connections served at once.
pub const MAX_CONNECTIONS: usize = 32;
/// Connections at once from one remote address (loopback is not limited this way).
pub const MAX_PER_IP: usize = 8;
/// Failed sign-ins before the connection is closed.
pub const MAX_FAILED_LOGINS: u32 = 3;
/// How long a connection may wait between commands before signing in, and after.
const LOGIN_WAIT: Duration = Duration::from_secs(30);
const IDLE_WAIT: Duration = Duration::from_secs(300);
/// How long a client has to open the data connection, and to take each part of a transfer.
const DATA_WAIT: Duration = Duration::from_secs(30);
/// The pause after a failed sign-in.
const AUTH_FAIL_WAIT: Duration = Duration::from_millis(if cfg!(test) { 50 } else { 1000 });

/// `[etp]` in the profile's config.toml.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// `enabled`: start with the daemon (`--etp` starts it either way).
    pub enabled: bool,
    pub bind: SocketAddr,
    /// `[[etp.roots]]`: `path`, and `name` (default: the folder's name, `C:` for a drive).
    pub roots: Vec<(String, PathBuf)>,
    pub welcome: String,
    pub allow_downloads: bool,
    /// `log`: every command (passwords hidden) at info level; otherwise at debug level.
    pub log: bool,
    pub allow_unencrypted: bool,
    /// `tls_cert`, `tls_key`: PEM files (relative to the profile's folder): explicit FTPS.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// `require_tls` (default true when TLS is configured): sign-in and transfers only
    /// over TLS.
    pub require_tls: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: false,
            bind: DEFAULT_BIND.parse().expect("a socket address"),
            roots: Vec::new(),
            welcome: DEFAULT_WELCOME.to_owned(),
            allow_downloads: false,
            log: false,
            allow_unencrypted: false,
            tls: None,
            require_tls: false,
        }
    }
}

/// The profile's config.toml.
fn config_file(cfg: &keel_api::config::HostConfig) -> PathBuf {
    cfg.config_dir
        .join("profiles")
        .join(&cfg.profile)
        .join("config.toml")
}

impl Config {
    /// `[etp]` from `table` (paths relative to `base`). A wrong type or address is an
    /// error.
    pub fn from_table(table: &toml::Table, base: &Path) -> Result<Config> {
        let mut c = Config::default();
        let Some(etp) = table.get("etp") else {
            return Ok(c);
        };
        let etp = etp.as_table().context("[etp] must be a table")?;
        let flag = |k: &str| -> Result<Option<bool>> {
            etp.get(k)
                .map(|v| v.as_bool().with_context(|| format!("[etp] {k}: true or false")))
                .transpose()
        };
        let text = |k: &str| -> Result<Option<String>> {
            etp.get(k)
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .with_context(|| format!("[etp] {k}: a string"))
                })
                .transpose()
        };
        c.enabled = flag("enabled")?.unwrap_or(false);
        if let Some(b) = text("bind")? {
            c.bind = b
                .trim()
                .parse()
                .with_context(|| format!("[etp] bind: {b:?} is not an address:port"))?;
        }
        if let Some(w) = text("welcome")? {
            anyhow::ensure!(w.len() <= 4096, "[etp] welcome: at most 4096 bytes");
            c.welcome = w;
        }
        c.allow_downloads = flag("allow_downloads")?.unwrap_or(false);
        c.log = flag("log")?.unwrap_or(false);
        c.allow_unencrypted = flag("allow_unencrypted")?.unwrap_or(false);
        let path = |p: String| {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                base.join(p)
            }
        };
        c.tls = match (text("tls_cert")?, text("tls_key")?) {
            (Some(cert), Some(key)) => Some((path(cert), path(key))),
            (None, None) => None,
            _ => bail!("[etp] tls_cert and tls_key go together"),
        };
        c.require_tls = flag("require_tls")?.unwrap_or(c.tls.is_some());
        anyhow::ensure!(
            !c.require_tls || c.tls.is_some(),
            "[etp] require_tls needs tls_cert and tls_key"
        );
        if let Some(roots) = etp.get("roots") {
            for r in roots.as_array().context("[[etp.roots]] must be an array")? {
                let r = r.as_table().context("[[etp.roots]]: path = \"...\"")?;
                let p = r
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("[[etp.roots]] needs path = \"...\"")?;
                let name = match r.get("name") {
                    Some(n) => n.as_str().context("[[etp.roots]] name: a string")?,
                    None => "",
                };
                c.roots.push((name.to_owned(), PathBuf::from(p)));
            }
        }
        Ok(c)
    }

    /// `[etp]` of the profile (defaults when the file is missing).
    pub fn read(cfg: &keel_api::config::HostConfig) -> Result<Config> {
        let file = config_file(cfg);
        let table: toml::Table = match std::fs::read_to_string(&file) {
            Ok(s) => s
                .parse()
                .with_context(|| format!("{}", file.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e).with_context(|| format!("{}", file.display())),
        };
        let base = file.parent().unwrap_or(Path::new("."));
        Config::from_table(&table, base)
    }

    /// Sign-in and transfers go over TLS only.
    pub fn encrypted(&self) -> bool {
        self.tls.is_some() && self.require_tls
    }
}

/// The keychain entry of a profile's ETP login.
fn login_key(profile: &str) -> String {
    format!("keel-daemon/etp-login/{profile}")
}

/// The profile's ETP login (user, password), if one is set.
pub fn login(store: &dyn SecretStore, profile: &str) -> Result<Option<(String, String)>> {
    Ok(store
        .get(&login_key(profile))?
        .and_then(|v| v.split_once(':').map(|(u, p)| (u.to_owned(), p.to_owned()))))
}

/// Stores the profile's ETP login in the keychain (the rules of the HTTP login).
pub fn set_login(store: &dyn SecretStore, profile: &str, user: &str, password: &str) -> Result<()> {
    crate::everything::store_login(store, &login_key(profile), user, password)
}

/// Removes the profile's ETP login.
pub fn clear_login(store: &dyn SecretStore, profile: &str) -> Result<()> {
    store.delete(&login_key(profile))
}

/// The search backend, asked per query.
pub type SearchFn = Arc<dyn Fn() -> Arc<dyn Searcher> + Send + Sync>;
/// Whether files may be sent now (`[etp] allow_downloads`, read per `RETR`).
pub type DownloadsFn = Arc<dyn Fn() -> bool + Send + Sync>;
/// Whether the server should stop accepting.
pub type StopFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// What every session shares.
struct Server {
    cfg: Config,
    roots: Roots,
    login: (String, String),
    tls: Option<Arc<rustls::ServerConfig>>,
    search: SearchFn,
    downloads: DownloadsFn,
    stop: StopFn,
}

/// The server's TLS configuration from PEM files: the certificate chain and its key.
fn tls_config(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let pem = std::fs::read(cert).with_context(|| format!("{}", cert.display()))?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("{}: {e:?}", cert.display()))?;
    anyhow::ensure!(!certs.is_empty(), "{}: no certificate", cert.display());
    let pem = std::fs::read(key).with_context(|| format!("{}", key.display()))?;
    let key = PrivateKeyDer::from_pem_slice(&pem)
        .map_err(|e| anyhow::anyhow!("{}: no private key ({e:?})", key.display()))?;
    // graviola needs a CPU with AES and carry-less multiply; it panics without them.
    let provider = std::panic::catch_unwind(|| Arc::new(rustls_graviola::default_provider()))
        .map_err(|_| anyhow::anyhow!("TLS is not available on this CPU"))?;
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Arc::new(config))
}

/// Starts the server on `cfg.bind` with `login`; returns the bound address. Sessions run
/// on their own threads; the accept loop ends once `stop` says so (and a connection wakes
/// it).
pub fn serve(
    cfg: Config,
    login: (String, String),
    search: SearchFn,
    downloads: DownloadsFn,
    stop: StopFn,
) -> Result<SocketAddr> {
    let roots = Roots::new(cfg.roots.clone())?;
    anyhow::ensure!(
        !roots.is_empty(),
        "the ETP server has no folders to serve: add [[etp.roots]] path = \"...\" to the \
         profile's config.toml"
    );
    anyhow::ensure!(
        !login.0.is_empty() && !login.1.is_empty(),
        "the ETP server needs a user and password (keel-daemon --set-etp-login USER)"
    );
    let loopback = cfg.bind.ip().is_loopback();
    if !loopback && !cfg.encrypted() && !cfg.allow_unencrypted {
        bail!("refusing to start: {UNENCRYPTED}");
    }
    let tls = match &cfg.tls {
        Some((cert, key)) => Some(tls_config(cert, key)?),
        None => None,
    };
    let listener = TcpListener::bind(cfg.bind)
        .with_context(|| format!("ETP server: cannot listen on {}", cfg.bind))?;
    let bound = listener.local_addr()?;
    if !loopback && !cfg.encrypted() {
        tracing::warn!("{UNENCRYPTED}");
    }
    tracing::info!(
        "ETP server on {bound}{} serving {} folder(s), downloads {}",
        if cfg.encrypted() {
            " (TLS required)"
        } else if tls.is_some() {
            " (TLS offered)"
        } else {
            ""
        },
        roots.list().len(),
        if cfg.allow_downloads { "on" } else { "off" }
    );
    let server = Arc::new(Server {
        cfg,
        roots,
        login,
        tls,
        search,
        downloads,
        stop,
    });
    let count = Arc::new(AtomicUsize::new(0));
    let per_ip: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    std::thread::Builder::new()
        .name("keel-daemon-etp".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if (server.stop)() {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let Some(slot) = crate::ws::Slot::take(&count, MAX_CONNECTIONS) else {
                    let _ = (&stream).write_all(b"421 Too many connections.\r\n");
                    continue;
                };
                let peer = match stream.peer_addr() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let ip_slot = if peer.ip().is_loopback() {
                    None
                } else {
                    match IpSlot::take(&per_ip, peer.ip()) {
                        Some(s) => Some(s),
                        None => {
                            let _ = (&stream).write_all(b"421 Too many connections.\r\n");
                            continue;
                        }
                    }
                };
                let server = server.clone();
                let _ = std::thread::Builder::new()
                    .name("keel-daemon-etp-client".into())
                    .spawn(move || {
                        let _slots = (slot, ip_slot);
                        if let Err(e) = session(&server, stream, peer) {
                            tracing::debug!("ETP {peer}: {e:#}");
                        }
                    });
            }
        })?;
    Ok(bound)
}

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

/// A control or data connection, plain or TLS.
enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }

    /// The TLS handshake on `tcp` (as the server).
    fn accept(tcp: TcpStream, config: &Arc<rustls::ServerConfig>) -> Result<Stream> {
        let conn = rustls::ServerConnection::new(config.clone())?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock)?;
        }
        Ok(Stream::Tls(Box::new(s)))
    }

    /// Ends the stream: TLS's `close_notify`, then the socket.
    fn finish(self) {
        if let Stream::Tls(mut boxed) = self {
            let s = &mut *boxed;
            s.conn.send_close_notify();
            while s.conn.wants_write() {
                if s.conn.write_tls(&mut s.sock).is_err() {
                    break;
                }
            }
            let _ = s.sock.shutdown(std::net::Shutdown::Write);
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// What to do after a command.
#[derive(PartialEq)]
enum Flow {
    Go,
    Close,
}

struct Session<'a> {
    srv: &'a Server,
    ctrl: BufReader<Stream>,
    peer: SocketAddr,
    local: SocketAddr,
    user: Option<String>,
    logged_in: bool,
    failures: u32,
    tls: bool,
    private_data: bool,
    cwd: Virtual,
    rest: u64,
    passive: Option<TcpListener>,
    opts: proto::Options,
}

fn session(srv: &Server, tcp: TcpStream, peer: SocketAddr) -> Result<()> {
    let local = tcp.local_addr()?;
    tcp.set_write_timeout(Some(DATA_WAIT))?;
    let _ = tcp.set_nodelay(true);
    let log = |what: &str| {
        if srv.cfg.log {
            tracing::info!("ETP {peer}: {what}");
        } else {
            tracing::debug!("ETP {peer}: {what}");
        }
    };
    log("connected");
    let mut s = Session {
        srv,
        ctrl: BufReader::new(Stream::Plain(tcp)),
        peer,
        local,
        user: None,
        logged_in: false,
        failures: 0,
        tls: false,
        private_data: false,
        cwd: Virtual::default(),
        rest: 0,
        passive: None,
        opts: proto::Options::default(),
    };
    s.send(&proto::welcome(&srv.cfg.welcome))?;
    loop {
        if (srv.stop)() {
            let _ = s.reply("421 The server is shutting down.");
            break;
        }
        let wait = if s.logged_in { IDLE_WAIT } else { LOGIN_WAIT };
        s.ctrl.get_ref().tcp().set_read_timeout(Some(wait))?;
        let line = match s.read_line() {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                let _ = s.reply("421 Timeout.");
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = s.reply("500 Line too long.");
                break;
            }
            Err(e) => return Err(e.into()),
        };
        let Ok(text) = String::from_utf8(line) else {
            s.reply("501 Commands must be UTF-8.")?;
            continue;
        };
        let cmd = match proto::parse(&text) {
            Ok(c) => c,
            Err(proto::Bad::Empty) => continue,
            Err(_) => {
                s.reply("501 Syntax error in parameters or arguments.")?;
                continue;
            }
        };
        let shown = match &cmd {
            Command::Pass(_) => "PASS ****".to_owned(),
            _ => text.chars().take(300).collect(),
        };
        log(&shown);
        if s.handle(cmd)? == Flow::Close {
            break;
        }
    }
    log("disconnected");
    s.ctrl.into_inner().finish();
    Ok(())
}

impl Session<'_> {
    /// One command line, without its line break; None at the end of the connection;
    /// `InvalidData` when it is longer than `proto::MAX_LINE`. Telnet interrupt bytes in
    /// front (sent before `ABOR`) are skipped.
    fn read_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        let limit = proto::MAX_LINE as u64 + 2;
        let n = (&mut self.ctrl).take(limit + 1).read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() != Some(&b'\n') && n as u64 > limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        let start = buf.iter().position(|b| *b < 0xF0).unwrap_or(buf.len());
        buf.drain(..start);
        Ok(Some(buf))
    }

    /// Sends `text` as it is (lines with their line breaks).
    fn send(&mut self, text: &str) -> io::Result<()> {
        let s = self.ctrl.get_mut();
        s.write_all(text.as_bytes())?;
        s.flush()
    }

    /// Sends one reply line.
    fn reply(&mut self, line: &str) -> io::Result<()> {
        self.send(&format!("{line}\r\n"))
    }

    fn handle(&mut self, cmd: Command) -> Result<Flow> {
        let before_login = matches!(
            cmd,
            Command::User(_)
                | Command::Pass(_)
                | Command::Auth(_)
                | Command::Pbsz(_)
                | Command::Prot(_)
                | Command::Feat
                | Command::Syst
                | Command::Noop
                | Command::Quit
                | Command::Opts(_)
                | Command::Help
        );
        if !self.logged_in && !before_login {
            self.reply("530 Not logged on.")?;
            return Ok(Flow::Go);
        }
        match cmd {
            Command::User(u) => self.user(u)?,
            Command::Pass(p) => return self.pass(p),
            Command::Auth(mech) => return self.auth(&mech),
            Command::Pbsz(_) => {
                if self.tls {
                    self.reply("200 PBSZ=0")?
                } else {
                    self.reply("503 Use AUTH TLS first.")?
                }
            }
            Command::Prot(level) => self.prot(&level)?,
            Command::Syst => self.reply("215 UNIX emulated by Keel")?,
            Command::Feat => {
                let mut f = String::from("211-Features:\r\n");
                for feat in [
                    "MDTM",
                    "REST STREAM",
                    "SIZE",
                    "MLST type*;size*;modify*;",
                    "MLSD",
                    "UTF8",
                    "EPSV",
                    "PASV",
                ] {
                    f.push_str(&format!(" {feat}\r\n"));
                }
                if self.srv.tls.is_some() {
                    f.push_str(" AUTH TLS\r\n PBSZ\r\n PROT\r\n");
                }
                f.push_str(" EVERYTHING\r\n211 End\r\n");
                self.send(&f)?;
            }
            Command::Help => self.send("214-Site commands:\r\n EVERYTHING\r\n214 End\r\n")?,
            Command::Noop => self.reply("200 OK.")?,
            Command::Quit => {
                self.reply("221 Goodbye.")?;
                return Ok(Flow::Close);
            }
            Command::Pwd => {
                let line = format!("257 {} is current directory.", proto::quoted(&self.cwd.display()));
                self.reply(&line)?;
            }
            Command::Cwd(arg) => self.cwd(&arg)?,
            Command::Cdup => self.cwd("..")?,
            Command::Type(t) => {
                let t = t.trim().to_ascii_uppercase();
                let reply = match t.as_str() {
                    "A" | "A N" => "200 Type set to A.",
                    "I" | "L 8" => "200 Type set to I.",
                    _ => "504 Type not supported.",
                };
                self.reply(reply)?;
            }
            Command::Mode(m) => self.reply(if m.trim().eq_ignore_ascii_case("S") {
                "200 Mode set to S."
            } else {
                "504 Only stream mode is supported."
            })?,
            Command::Stru(s) => self.reply(if s.trim().eq_ignore_ascii_case("F") {
                "200 Structure set to F."
            } else {
                "504 Only file structure is supported."
            })?,
            Command::Opts(o) => {
                let o = o.trim().to_ascii_uppercase();
                let reply = if o == "UTF8 ON" || o == "UTF-8 ON" {
                    "200 UTF8 mode enabled."
                } else if o.starts_with("MLST") {
                    "200 MLST OPTS type;size;modify;"
                } else {
                    "501 Invalid option."
                };
                self.reply(reply)?;
            }
            Command::Pasv => self.pasv(false, "")?,
            Command::Epsv(arg) => self.pasv(true, &arg)?,
            Command::Active => {
                self.reply("502 Active mode is not supported: use PASV or EPSV.")?
            }
            Command::List(arg) => return self.list(proto::list_target(&arg), Listing::List),
            Command::Nlst(arg) => return self.list(proto::list_target(&arg), Listing::Names),
            Command::Mlsd(arg) => return self.list(&arg, Listing::Mlsd),
            Command::Mlst(arg) => self.mlst(&arg)?,
            Command::Size(arg) => {
                let reply = match self.real(&arg) {
                    Some(p) => match std::fs::metadata(&p) {
                        Ok(m) if m.is_file() => format!("213 {}", m.len()),
                        _ => "550 File not found.".to_owned(),
                    },
                    None => "550 File not found.".to_owned(),
                };
                self.reply(&reply)?;
            }
            Command::Mdtm(arg) => {
                let reply = match self.real(&arg).and_then(|p| std::fs::metadata(p).ok()) {
                    Some(m) => format!("213 {}", fs::mdtm(m.modified().ok())),
                    None => "550 File not found.".to_owned(),
                };
                self.reply(&reply)?;
            }
            Command::Rest(arg) => {
                let arg = arg.trim();
                match arg.parse::<u64>() {
                    Ok(n) => {
                        self.rest = n;
                        self.reply(&format!("350 Restarting at {n}."))?;
                    }
                    Err(_) => self.reply("501 Invalid restart point.")?,
                }
            }
            Command::Retr(arg) => return self.retr(&arg),
            Command::Abor => self.reply("226 No transfer to abort.")?,
            Command::Everything(e) => match self.opts.apply(&e) {
                Some(reply) => self.reply(&reply)?,
                None => {
                    let answer = self.query();
                    self.send(&answer)?;
                }
            },
            Command::Site(_) => self.reply("500 Unknown SITE command.")?,
            Command::Write(_) => self.reply("550 Permission denied: this server is read-only.")?,
            Command::Unknown(_) => self.reply("500 Syntax error, command unrecognized.")?,
        }
        Ok(Flow::Go)
    }

    fn user(&mut self, u: String) -> io::Result<()> {
        if self.srv.cfg.encrypted() && !self.tls {
            return self.reply("530 This server requires TLS: use AUTH TLS first.");
        }
        if self.logged_in {
            return self.reply("503 Already logged on.");
        }
        let shown: String = u.chars().take(64).collect();
        self.user = Some(u);
        self.reply(&format!("331 Password required for {shown}."))
    }

    fn pass(&mut self, p: String) -> Result<Flow> {
        if self.logged_in {
            self.reply("503 Already logged on.")?;
            return Ok(Flow::Go);
        }
        let Some(user) = self.user.take() else {
            self.reply("503 Login with USER first.")?;
            return Ok(Flow::Go);
        };
        let (u, pw) = &self.srv.login;
        // Both compared in full, whatever the first gives.
        let user_ok = crate::ws::same(user.as_bytes(), u.as_bytes());
        let pass_ok = crate::ws::same(p.as_bytes(), pw.as_bytes());
        if user_ok & pass_ok {
            self.logged_in = true;
            tracing::info!("ETP {}: signed in", self.peer);
            self.reply("230 Logged on.")?;
            return Ok(Flow::Go);
        }
        self.failures += 1;
        tracing::info!("ETP {}: failed sign-in", self.peer);
        std::thread::sleep(AUTH_FAIL_WAIT);
        if self.failures >= MAX_FAILED_LOGINS {
            self.reply("421 Too many failed sign-ins.")?;
            return Ok(Flow::Close);
        }
        self.reply("530 Login or password incorrect!")?;
        Ok(Flow::Go)
    }

    fn auth(&mut self, mech: &str) -> Result<Flow> {
        let mech = mech.trim().to_ascii_uppercase();
        let Some(config) = self.srv.tls.clone() else {
            self.reply("502 TLS is not configured on this server.")?;
            return Ok(Flow::Go);
        };
        if self.tls {
            self.reply("503 TLS is already on.")?;
            return Ok(Flow::Go);
        }
        if !matches!(mech.as_str(), "TLS" | "TLS-C" | "SSL") {
            self.reply("504 Unknown AUTH type.")?;
            return Ok(Flow::Go);
        }
        self.reply("234 AUTH TLS successful.")?;
        // A client that sent more before the handshake is not speaking FTPS.
        if !self.ctrl.buffer().is_empty() {
            return Ok(Flow::Close);
        }
        let placeholder = BufReader::new(Stream::Plain(self.ctrl.get_ref().tcp().try_clone()?));
        let Stream::Plain(tcp) = std::mem::replace(&mut self.ctrl, placeholder).into_inner() else {
            return Ok(Flow::Close);
        };
        self.ctrl = BufReader::new(Stream::accept(tcp, &config)?);
        self.tls = true;
        // RFC 4217: the security exchange starts the session over.
        self.user = None;
        self.logged_in = false;
        Ok(Flow::Go)
    }

    fn prot(&mut self, level: &str) -> io::Result<()> {
        if !self.tls {
            return self.reply("503 Use AUTH TLS first.");
        }
        match level.trim().to_ascii_uppercase().as_str() {
            "P" => {
                self.private_data = true;
                self.reply("200 Protection level set to P.")
            }
            "C" if self.srv.cfg.require_tls => {
                self.reply("534 PROT C is not allowed: this server requires TLS.")
            }
            "C" => {
                self.private_data = false;
                self.reply("200 Protection level set to C.")
            }
            _ => self.reply("504 Unsupported protection level."),
        }
    }

    /// The canonical file or folder `arg` names from the current folder, inside a root.
    fn real(&self, arg: &str) -> Option<PathBuf> {
        match self.srv.roots.real(&Virtual::resolve(&self.cwd, arg)) {
            Ok(Target::Real(p)) => Some(p),
            _ => None,
        }
    }

    fn cwd(&mut self, arg: &str) -> io::Result<()> {
        let v = Virtual::resolve(&self.cwd, arg);
        let ok = match self.srv.roots.real(&v) {
            Ok(Target::Top) => true,
            Ok(Target::Real(p)) => p.is_dir(),
            Err(_) => false,
        };
        if ok {
            self.cwd = v;
            let line = format!(
                "250 CWD successful. {} is current directory.",
                proto::quoted(&self.cwd.display())
            );
            self.reply(&line)
        } else {
            self.reply("550 Directory not found.")
        }
    }

    fn pasv(&mut self, extended: bool, arg: &str) -> io::Result<()> {
        let arg = arg.trim();
        if extended && arg.eq_ignore_ascii_case("ALL") {
            return self.reply("200 EPSV ALL ok.");
        }
        if extended && !matches!(arg, "" | "1" | "2") {
            return self.reply("522 Network protocol not supported, use (1,2)");
        }
        let ip = self.local.ip();
        if !extended && !ip.is_ipv4() {
            return self.reply("500 PASV is not available over IPv6: use EPSV.");
        }
        let listener = match TcpListener::bind((ip, 0)) {
            Ok(l) => l,
            Err(_) => return self.reply("425 Cannot open a data port."),
        };
        let port = listener.local_addr()?.port();
        self.passive = Some(listener);
        if extended {
            self.reply(&format!(
                "229 Entering Extended Passive Mode (|||{port}|)"
            ))
        } else {
            let IpAddr::V4(v4) = ip else {
                unreachable!("checked above")
            };
            let [a, b, c, d] = v4.octets();
            self.reply(&format!(
                "227 Entering Passive Mode ({a},{b},{c},{d},{},{}).",
                port >> 8,
                port & 0xff
            ))
        }
    }

    /// Opens the data connection (from the control connection's address only), runs
    /// `write` on it and replies how it went.
    fn transfer(&mut self, write: impl FnOnce(&mut Stream) -> io::Result<()>) -> Result<Flow> {
        let Some(listener) = self.passive.take() else {
            self.reply("425 Use PASV or EPSV first.")?;
            return Ok(Flow::Go);
        };
        if self.srv.cfg.encrypted() && !self.private_data {
            self.reply("521 Use PROT P: this server requires TLS on data connections.")?;
            return Ok(Flow::Go);
        }
        self.reply("150 Opening data connection.")?;
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + DATA_WAIT;
        let tcp = loop {
            match listener.accept() {
                // Another address connecting first (port theft) is dropped.
                Ok((s, from)) if from.ip() == self.peer.ip() => break Some(s),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };
        let Some(tcp) = tcp else {
            self.reply("425 Cannot open data connection.")?;
            return Ok(Flow::Go);
        };
        drop(listener);
        tcp.set_nonblocking(false)?;
        tcp.set_read_timeout(Some(DATA_WAIT))?;
        tcp.set_write_timeout(Some(DATA_WAIT))?;
        let stream = match (&self.srv.tls, self.private_data) {
            (Some(config), true) => Stream::accept(tcp, config),
            _ => Ok(Stream::Plain(tcp)),
        };
        let done = stream.and_then(|mut s| {
            write(&mut s)?;
            s.flush()?;
            s.finish();
            Ok(())
        });
        match done {
            Ok(()) => self.reply("226 Transfer complete.")?,
            Err(e) => {
                tracing::debug!("ETP {}: transfer: {e:#}", self.peer);
                self.reply("426 Connection closed; transfer aborted.")?
            }
        }
        Ok(Flow::Go)
    }

    fn list(&mut self, arg: &str, how: Listing) -> Result<Flow> {
        let v = Virtual::resolve(&self.cwd, arg);
        let entries = match self.srv.roots.real(&v) {
            Ok(Target::Top) => Ok(fs::top(&self.srv.roots)),
            Ok(Target::Real(p)) => match std::fs::metadata(&p) {
                Ok(m) if m.is_dir() => fs::read_dir(&p).map_err(|_| "550 Directory not found."),
                Ok(_) if how == Listing::Mlsd => Err("501 Not a folder."),
                Ok(m) => Ok(vec![fs::Entry::of(v.name().to_owned(), &m)]),
                Err(_) => Err("550 Directory not found."),
            },
            Err(_) => Err("550 Directory not found."),
        };
        let entries = match entries {
            Ok(e) => e,
            Err(reply) => {
                self.passive = None;
                self.reply(reply)?;
                return Ok(Flow::Go);
            }
        };
        let now = SystemTime::now();
        let mut text = String::new();
        for e in &entries {
            let line = match how {
                Listing::List => fs::list_line(e, now),
                Listing::Names => e.name.clone(),
                Listing::Mlsd => fs::mlsd_line(e),
            };
            text.push_str(&line);
            text.push_str("\r\n");
        }
        self.transfer(|s| s.write_all(text.as_bytes()))
    }

    fn mlst(&mut self, arg: &str) -> io::Result<()> {
        let v = Virtual::resolve(&self.cwd, arg);
        let entry = match self.srv.roots.real(&v) {
            Ok(Target::Top) => Some(fs::Entry {
                name: "/".into(),
                is_dir: true,
                size: 0,
                modified: None,
            }),
            Ok(Target::Real(p)) => std::fs::metadata(p)
                .ok()
                .map(|m| fs::Entry::of(v.display(), &m)),
            Err(_) => None,
        };
        match entry {
            Some(mut e) => {
                e.name = v.display();
                let text = format!(
                    "250-Listing {}\r\n {}\r\n250 End.\r\n",
                    v.display(),
                    fs::mlsd_line(&e)
                );
                self.send(&text)
            }
            None => self.reply("550 File or Directory not found."),
        }
    }

    fn retr(&mut self, arg: &str) -> Result<Flow> {
        let rest = std::mem::take(&mut self.rest);
        if !(self.srv.downloads)() {
            self.passive = None;
            self.reply("550 Downloads are turned off on this server.")?;
            return Ok(Flow::Go);
        }
        let file = self
            .real(arg)
            .filter(|p| p.is_file())
            .and_then(|p| std::fs::File::open(p).ok());
        let Some(mut file) = file else {
            self.passive = None;
            self.reply("550 File not found.")?;
            return Ok(Flow::Go);
        };
        if rest > 0 && file.seek(io::SeekFrom::Start(rest)).is_err() {
            self.passive = None;
            self.reply("451 Transfer aborted: cannot restart there.")?;
            return Ok(Flow::Go);
        }
        tracing::info!("ETP {}: sending {}", self.peer, arg.chars().take(300).collect::<String>());
        self.transfer(|s| io::copy(&mut file, s).map(|_| ()))
    }

    /// `SITE EVERYTHING QUERY`: the system search, limited to the roots, as Everything's
    /// reply.
    fn query(&self) -> String {
        let o = &self.opts;
        if o.has(proto::Flag::HideEmptySearchResults) && o.search.trim().is_empty() {
            return proto::results(0, &[], o);
        }
        let searcher = (self.srv.search)();
        let filter = o.filter_params();
        let scoped = Scoped {
            inner: &*searcher,
            roots: &self.srv.roots,
            filter: filter.as_ref(),
        };
        let found = keel_api::system::run(&scoped, &o.params(), keel_api::system::MAX_SCAN);
        match found {
            Ok(found) => {
                let items: Vec<proto::Item> =
                    found.hits.iter().filter_map(|h| self.item(h)).collect();
                proto::results(found.total, &items, o)
            }
            Err(e) => {
                let why: String = e
                    .message
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(300)
                    .collect();
                format!("500 {why}\r\n")
            }
        }
    }

    /// A hit (inside a root) as `QUERY` sends it.
    fn item(&self, h: &Hit) -> Option<proto::Item> {
        let real = h.path.to_local_path()?;
        let (path, name) = self.srv.roots.locate(&real)?;
        let o = &self.opts;
        let meta = (o.column(proto::Column::Attributes) || o.column(proto::Column::DateCreated))
            .then(|| std::fs::metadata(&real).ok())
            .flatten();
        Some(proto::Item {
            name,
            is_dir: h.is_dir,
            path,
            size: Some(h.size),
            modified: fs::filetime(h.modified),
            created: meta
                .as_ref()
                .and_then(|m| m.created().ok())
                .and_then(|t| fs::filetime(Some(t))),
            attributes: meta.as_ref().map_or(
                if h.is_dir { 16 } else { 32 },
                fs::attributes,
            ),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    List,
    Names,
    Mlsd,
}

/// The system search limited to the roots (and to the filter search, when one is set):
/// hits elsewhere never reach a client. With Everything as the backend, the roots also go
/// into its search, so the scan limit is spent on them.
struct Scoped<'a> {
    inner: &'a dyn Searcher,
    roots: &'a Roots,
    filter: Option<&'a SystemSearchParams>,
}

impl Searcher for Scoped<'_> {
    fn query(&self, q: &Query) -> Result<Vec<Hit>> {
        let mut q = q.clone();
        if self.inner.name() == "Everything" && !q.regex {
            let paths: Vec<String> = self
                .roots
                .list()
                .iter()
                .map(|r| {
                    let p = r.path.to_string_lossy();
                    let p = p.trim_end_matches(['\\', '/']);
                    format!("\"{p}\\\"")
                })
                .collect();
            q.text = format!("<{}> {}", paths.join("|"), q.text);
        }
        let hits: Vec<Hit> = self
            .inner
            .query(&q)?
            .into_iter()
            .filter(|h| {
                h.path
                    .to_local_path()
                    .is_some_and(|p| self.roots.locate(&p).is_some())
            })
            .collect();
        let Some(filter) = self.filter else {
            return Ok(hits);
        };
        let shown: Vec<String> = hits.iter().map(|h| h.path.display()).collect();
        let shown: Vec<&str> = shown.iter().map(String::as_str).collect();
        let keep = keel_api::system::check_paths(filter, &shown)
            .map_err(|e| anyhow::anyhow!("filter search: {}", e.message))?;
        Ok(hits
            .into_iter()
            .zip(keep)
            .filter_map(|(h, k)| k.then_some(h))
            .collect())
    }

    fn available(&self) -> bool {
        self.inner.available()
    }

    fn probe(&self) -> bool {
        self.inner.probe()
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

/// Whether `[etp] enabled = true` (false when the file or the value is not readable).
pub fn enabled(cfg: &keel_api::config::HostConfig) -> bool {
    Config::read(cfg).is_ok_and(|c| c.enabled)
}
