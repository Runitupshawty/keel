//! Synchronous FTP and FTPS provider (`ftp://<host id>/<path>`). Call on a worker thread.
//! Passive mode only, the data connection always to the control connection's address.
//! Uploads stream to a staging file beside the target and are renamed into place on
//! `flush()` / `finish()`, like SFTP uploads; dropped before that, the staging file is
//! removed. There is no trash: deletes are permanent.
//!
//! Connections: a lease per operation from a small pool; a transfer holds its control
//! connection until it ends (an FTP control connection carries one transfer at a time), so
//! other operations meanwhile open another. A connection that saw a transport error, or a
//! transfer that stopped early, is closed rather than reused.
pub mod list;
#[cfg(test)]
mod test_server;
#[cfg(test)]
mod tests;
pub mod tls;

use crate::ops::is_partial;
use crate::sftp::{cached_download, partial_path, undecodable, UNDECODABLE};
use crate::{
    Caps, ConnStatus, Entry, Kind, Progress, Provider, RemoteEvent, RemoteHost, RemoteProtocol,
    VPath,
};
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use list::{Item, Type};
use std::{
    io::{self, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};
use suppaftp::{types::FileType, FtpError, FtpResult, Mode, Status, TransferStream};

type Ftp = suppaftp::ImplFtpStream<tls::Tls>;
type Transfer = TransferStream<tls::Tls>;

/// Connect, control and data timeouts (the SFTP default).
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long a "trust this certificate?" prompt may stay open (as for SSH host keys).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Idle connections kept for reuse.
const IDLE: usize = 2;
/// An idle connection older than this is checked with `NOOP` before reuse.
const STALE: Duration = Duration::from_secs(30);
/// Symbolic links in one listing whose target type is looked up (`CWD`); later ones list
/// as files.
const LINK_PROBES: usize = 256;
/// The password sent for `anonymous` (RFC 1635 asks for an email address).
const ANONYMOUS_PASSWORD: &str = "anonymous@example.com";
const STALE_PARTIAL: Duration = Duration::from_secs(24 * 3600);

/// One signed-in control connection and what its server supports.
struct Conn {
    ftp: Ftp,
    mlsd: bool,
    size: bool,
    rest: bool,
    /// `SYST` says UNIX: `LIST -a` shows dot files.
    unix: bool,
    used: Instant,
}

struct Pool {
    host: RemoteHost,
    events: Sender<RemoteEvent>,
    listener: Arc<AtomicBool>,
    idle: parking_lot::Mutex<Vec<Conn>>,
    status: parking_lot::Mutex<ConnStatus>,
    timeout: parking_lot::RwLock<Duration>,
    /// Tests: trust only these roots (empty: the OS trust store).
    roots: Vec<rustls::pki_types::CertificateDer<'static>>,
}

/// A connection taken from the pool: `release` puts it back; dropped otherwise, it closes.
struct Lease {
    pool: Arc<Pool>,
    conn: Option<Conn>,
}
impl Lease {
    fn conn(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("a leased connection")
    }
    fn release(mut self) {
        if let Some(mut conn) = self.conn.take() {
            conn.used = Instant::now();
            let mut idle = self.pool.idle.lock();
            if idle.len() < IDLE {
                idle.push(conn);
            }
        }
    }
    /// Unblocks anything waiting on the control connection (a transfer's final reply).
    fn shutdown(&mut self) {
        if let Some(conn) = &self.conn {
            let _ = conn.ftp.get_ref().shutdown(Shutdown::Both);
        }
    }
}

/// Server replies by code, for errors: the server's own text is never shown or logged.
fn reply_meaning(code: u32) -> &'static str {
    match code {
        421 => "the server closed the connection",
        425 | 426 => "the data connection failed",
        450 | 550 => "the file or folder is unavailable (missing or no permission)",
        451 => "the server failed while processing",
        452 | 552 => "the server is out of space or over quota",
        530 => "not logged in (wrong user name or password)",
        553 => "the server does not allow this name",
        500..=504 => "the server does not support this command",
        _ => "the server refused",
    }
}

/// A refusal by the server (the connection stays usable) as opposed to a transport failure.
fn refused(e: &FtpError) -> Option<u32> {
    match e {
        FtpError::UnexpectedResponse(r) if r.status.code() != 421 => Some(r.status.code()),
        _ => None,
    }
}

fn describe(e: &FtpError) -> io::Error {
    match e {
        FtpError::UnexpectedResponse(r) => {
            let code = r.status.code();
            let kind = match code {
                550 | 450 => io::ErrorKind::NotFound,
                _ => io::ErrorKind::Other,
            };
            io::Error::new(kind, format!("FTP {code}: {}", reply_meaning(code)))
        }
        FtpError::ConnectionError(e) => io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("FTP connection failed: {e}"),
        ),
        FtpError::SecureError(e) => io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("FTPS (TLS) failed: {e}"),
        ),
        _ => io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "FTP connection or protocol failure",
        ),
    }
}

fn io_err(e: io::Error) -> FtpError {
    FtpError::ConnectionError(e)
}

impl Pool {
    fn event(&self, status: ConnStatus, detail: &str) {
        *self.status.lock() = status;
        let _ = self.events.try_send(RemoteEvent::Status {
            host_id: self.host.id.clone(),
            status,
            detail: detail.into(),
        });
    }
    fn failed(&self, detail: &str) {
        self.idle.lock().clear();
        self.event(
            ConnStatus::Failed,
            &format!("{detail}; reconnecting on next operation"),
        );
    }
    fn lease(self: &Arc<Self>) -> Result<Lease> {
        loop {
            let Some(mut conn) = self.idle.lock().pop() else {
                break;
            };
            if conn.used.elapsed() < STALE || conn.ftp.noop().is_ok() {
                return Ok(Lease {
                    pool: self.clone(),
                    conn: Some(conn),
                });
            }
        }
        let first = *self.status.lock() != ConnStatus::Connected;
        if first {
            self.event(ConnStatus::Connecting, "connecting");
        }
        match self.open() {
            Ok(conn) => {
                if first {
                    self.event(ConnStatus::Connected, "connected");
                }
                Ok(Lease {
                    pool: self.clone(),
                    conn: Some(conn),
                })
            }
            Err(e) => {
                self.failed(&format!("{e:#}"));
                Err(e)
            }
        }
    }
    /// Connects and signs in; an FTPS certificate that fails verification is prompted for
    /// (and pinned when trusted), then the connection starts over from scratch.
    fn open(&self) -> Result<Conn> {
        let h = &self.host;
        for attempt in 0..2 {
            let pin = match h.protocol {
                RemoteProtocol::Ftps => tls::pinned(&h.host, h.port)?,
                _ => None,
            };
            let connector = match h.protocol {
                RemoteProtocol::Ftps => Some(tls::Connector::new(
                    pin.clone(),
                    *self.timeout.read(),
                    &self.roots,
                )?),
                _ => None,
            };
            let rejected = connector.as_ref().map(|c| c.rejected.clone());
            let result = self.connect(connector);
            let Some(cert) = rejected.and_then(|r| r.lock().take()) else {
                return result;
            };
            let endpoint = format!("{}:{}", h.host, h.port);
            if let Some(pin) = pin {
                anyhow::bail!(
                    "the FTPS certificate of {endpoint} changed since it was trusted \
                     (now {}, trusted {pin}); refusing to connect. If the server has a new \
                     certificate, remove its line from ftps_known_hosts in Keel's \
                     configuration folder",
                    cert.fingerprint
                );
            }
            anyhow::ensure!(
                attempt == 0,
                "the FTPS certificate of {endpoint} was not accepted"
            );
            anyhow::ensure!(
                self.listener.load(Ordering::SeqCst),
                "the FTPS certificate of {endpoint} is not trusted ({}) and no Keel window \
                 is open to confirm it; connect from the Keel window first",
                cert.reason
            );
            let (reply, answer) = crossbeam_channel::bounded(1);
            self.events
                .try_send(RemoteEvent::CertificatePrompt {
                    host_id: h.id.clone(),
                    endpoint: endpoint.clone(),
                    fingerprint: cert.fingerprint.clone(),
                    problem: cert.reason.clone(),
                    reply,
                })
                .context("certificate approval unavailable")?;
            anyhow::ensure!(
                answer.recv_timeout(PROMPT_TIMEOUT).unwrap_or(false),
                "the FTPS certificate of {endpoint} was not trusted"
            );
            tls::pin(&h.host, h.port, &cert.fingerprint)?;
        }
        unreachable!("the loop returns or fails")
    }
    fn connect(&self, connector: Option<tls::Connector>) -> Result<Conn> {
        let h = &self.host;
        let timeout = *self.timeout.read();
        let addrs: Vec<SocketAddr> = (h.host.trim_matches(['[', ']']), h.port)
            .to_socket_addrs()
            .with_context(|| format!("cannot resolve {}", h.host))?
            .collect();
        let (mut ftp, explicit) = match connector {
            // ponytail: implicit FTPS connects through suppaftp without a connect timeout
            // (the OS one applies); the TLS handshake and every reply have ours.
            Some(connector) if h.port == 990 => {
                let mut ftp = Ftp::connect_secure_implicit(&addrs[..], connector, &h.host)
                    .map_err(|e| describe(&e))
                    .context("FTPS (implicit TLS) connection failed")?;
                // Protected data connections, as `into_secure` asks for explicit FTPS.
                for cmd in ["PBSZ 0", "PROT P"] {
                    ftp.custom_command(cmd, &[Status::CommandOk])
                        .map_err(|e| describe(&e))
                        .context("the server refused protected data connections")?;
                }
                (ftp, None)
            }
            connector => {
                let tcp = addrs
                    .iter()
                    .find_map(|a| TcpStream::connect_timeout(a, timeout).ok())
                    .with_context(|| format!("cannot connect to {}:{}", h.host, h.port))?;
                tcp.set_read_timeout(Some(timeout))?;
                tcp.set_write_timeout(Some(timeout))?;
                let ftp = Ftp::connect_with_stream(tcp)
                    .map_err(|e| describe(&e))
                    .context("FTP connection failed")?;
                (ftp, connector)
            }
        };
        ftp = ftp.passive_stream_builder(move |addr| {
            let s = TcpStream::connect_timeout(&addr, timeout).map_err(io_err)?;
            s.set_read_timeout(Some(timeout)).map_err(io_err)?;
            s.set_write_timeout(Some(timeout)).map_err(io_err)?;
            Ok(s)
        });
        {
            let control = ftp.get_ref();
            control.set_read_timeout(Some(timeout))?;
            control.set_write_timeout(Some(timeout))?;
        }
        if let Some(connector) = explicit {
            // Never falls back to plain FTP: a refused AUTH TLS or a failed handshake ends here.
            ftp = ftp.into_secure(connector, &h.host).map_err(|e| {
                anyhow::anyhow!(
                    "{}; Keel never falls back to plain FTP",
                    match refused(&e) {
                        Some(_) => "the server refused TLS (AUTH TLS)".to_owned(),
                        None => describe(&e).to_string(),
                    }
                )
            })?;
        }
        // The data connection always goes to the server's own address: a PASV reply naming
        // another host (a NAT's inside address, or a hostile server) is never followed.
        ftp.set_passive_nat_workaround(true);
        let v6 = ftp.get_ref().peer_addr().is_ok_and(|a| a.is_ipv6());
        ftp.set_mode(if v6 {
            Mode::ExtendedPassive
        } else {
            Mode::Passive
        });
        let anonymous = h.user.is_empty() || h.user.eq_ignore_ascii_case("anonymous");
        let (user, password) = if anonymous {
            ("anonymous".to_owned(), ANONYMOUS_PASSWORD.to_owned())
        } else {
            let password = crate::sftp::auth::load_secret(&h.id, "password")?;
            (h.user.clone(), password)
        };
        ftp.login(&user, &password)
            .map_err(|e| describe(&e))
            .context("FTP sign-in failed")?;
        ftp.transfer_type(FileType::Binary)
            .map_err(|e| describe(&e))?;
        let features = ftp.feat().unwrap_or_default();
        let has = |name: &str| features.keys().any(|k| k.eq_ignore_ascii_case(name));
        if has("UTF8") {
            let _ = ftp.opts("UTF8", Some("ON"));
        }
        let unix = ftp
            .custom_command("SYST", &[Status::Name])
            .is_ok_and(|r| String::from_utf8_lossy(&r.body).contains("UNIX"));
        Ok(Conn {
            mlsd: has("MLST"),
            size: has("SIZE"),
            rest: has("REST"),
            unix,
            ftp,
            used: Instant::now(),
        })
    }
}

pub struct FtpProvider {
    pool: Arc<Pool>,
}

impl FtpProvider {
    pub fn new(host: RemoteHost, events: Sender<RemoteEvent>) -> Self {
        Self::with_prompt_listener(host, events, Arc::new(AtomicBool::new(true)))
    }
    /// `listener`: whether anything answers certificate prompts (see `Router`).
    pub fn with_prompt_listener(
        host: RemoteHost,
        events: Sender<RemoteEvent>,
        listener: Arc<AtomicBool>,
    ) -> Self {
        Self::with_roots(host, events, listener, Vec::new())
    }
    fn with_roots(
        host: RemoteHost,
        events: Sender<RemoteEvent>,
        listener: Arc<AtomicBool>,
        roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    ) -> Self {
        Self {
            pool: Arc::new(Pool {
                host,
                events,
                listener,
                idle: parking_lot::Mutex::default(),
                status: parking_lot::Mutex::new(ConnStatus::Disconnected),
                timeout: parking_lot::RwLock::new(TIMEOUT),
                roots,
            }),
        }
    }
    pub fn status(&self) -> ConnStatus {
        *self.pool.status.lock()
    }
    /// Connects now (the sidebar's "Connect"); operations otherwise connect lazily.
    pub fn connect(&self) -> Result<()> {
        self.pool.lease()?.release();
        Ok(())
    }
    pub fn disconnect(&self) {
        for mut conn in self.pool.idle.lock().drain(..) {
            let _ = conn.ftp.quit();
        }
        self.pool.event(ConnStatus::Disconnected, "disconnected");
    }
    pub fn set_timeout(&self, timeout: Duration) {
        *self.pool.timeout.write() = timeout.max(Duration::from_millis(1));
    }
    fn validate(&self, p: &VPath) -> Result<()> {
        anyhow::ensure!(
            p.scheme == "ftp"
                && p.authority == self.pool.host.id
                && p.path.starts_with('/')
                && !p.path.contains(['\0', '\r', '\n']),
            "invalid FTP path: {}",
            p.display()
        );
        if p.path.contains(UNDECODABLE) {
            return Err(undecodable(p));
        }
        Ok(())
    }
    /// One operation on a leased connection: a refusal keeps the connection, a transport
    /// failure closes it and reports the host as failed.
    fn with<T>(&self, p: &VPath, f: impl FnOnce(&mut Conn) -> FtpResult<T>) -> Result<T> {
        self.validate(p)?;
        let mut lease = self.pool.lease().with_context(|| p.display())?;
        let result = f(lease.conn());
        self.settle(lease, result).with_context(|| p.display())
    }
    fn settle<T>(&self, lease: Lease, result: FtpResult<T>) -> Result<T> {
        match result {
            Ok(v) => {
                lease.release();
                Ok(v)
            }
            Err(e) => Err(self.fail(lease, e)),
        }
    }
    /// A refusal (or an error raised here) keeps the connection; anything else closes it.
    fn fail(&self, lease: Lease, e: FtpError) -> anyhow::Error {
        if keeps(&e) {
            lease.release();
        } else {
            drop(lease);
            self.pool.failed("connection lost");
        }
        to_io(e).into()
    }
    fn entry(&self, path: VPath, item: &Item, is_link: bool) -> Entry {
        let name = path.name().to_owned();
        let ext = name
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, ext)| ext.to_lowercase())
            .unwrap_or_default();
        Entry {
            kind: match item.kind {
                Type::Dir => Kind::Dir,
                Type::File | Type::Link => Kind::File,
            },
            size: item.size,
            modified: item.modified,
            hidden: name.starts_with('.'),
            ext,
            is_link,
            encrypted: false,
            name,
            path,
        }
    }
    fn rename_with(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.validate(to)?;
        self.with(from, |c| {
            if !replace && stat_on(c, &to.path)?.is_some() {
                return Err(exists());
            }
            c.ftp.rename(&from.path, &to.path)
        })
        .with_context(|| to.display())
    }
    /// Starts an upload to `p` (replacing it on `finish()` unless `exclusive`), streamed to
    /// a uniquely named staging file.
    pub fn upload(&self, p: &VPath, exclusive: bool) -> Result<FtpUpload> {
        self.validate(p)?;
        let dir = parent_of(&p.path);
        let mut lease = self.pool.lease().with_context(|| p.display())?;
        // Best effort: a day-old staging file here was left by a crash or a lost connection.
        let now = SystemTime::now();
        let items = match list_on(lease.conn(), &dir) {
            Ok(items) => items,
            Err(e) if keeps(&e) => Vec::new(),
            Err(e) => return Err(self.fail(lease, e).context(p.display())),
        };
        {
            for (item, _) in items {
                let old = item
                    .modified
                    .and_then(|t| now.duration_since(t).ok())
                    .is_some_and(|age| age > STALE_PARTIAL);
                if old && item.kind == Type::File && is_partial(&item.name) {
                    let _ = lease.conn().ftp.rm(join(&dir, &item.name));
                }
            }
        }
        let partial = partial_path(&p.path);
        let result = (|| {
            if exclusive && stat_on(lease.conn(), &p.path)?.is_some() {
                return Err(exists());
            }
            lease.conn().ftp.put_with_stream(&partial)
        })();
        let stream = match result {
            Ok(s) => s,
            Err(e) => {
                return Err(self
                    .fail(lease, e)
                    .context("cannot stage the upload")
                    .context(p.display()))
            }
        };
        Ok(FtpUpload {
            provider: self.pool.clone(),
            lease: Some(lease),
            stream: Some(stream),
            partial,
            target: p.clone(),
            exclusive,
            written: 0,
            committed: false,
            failed: false,
        })
    }
    /// `Provider::write_at`: offset 0 creates or empties `p`; a later offset continues it
    /// with `APPE` when it is exactly that long, else with `REST` + `STOR` (None when the
    /// server cannot, and the caller starts over).
    fn direct(&self, p: &VPath, offset: u64) -> Result<Option<FtpDirect>> {
        self.validate(p)?;
        let mut lease = self.pool.lease().with_context(|| p.display())?;
        let result = (|| -> FtpResult<Option<Transfer>> {
            let c = lease.conn();
            if offset == 0 {
                return c.ftp.put_with_stream(&p.path).map(Some);
            }
            if !c.size {
                return Ok(None);
            }
            let len = c.ftp.size(&p.path)? as u64;
            if len < offset {
                return Err(FtpError::ConnectionError(io::Error::other(format!(
                    "only {len} of {offset} bytes are there"
                ))));
            }
            if len == offset {
                return c.ftp.append_with_stream(&p.path).map(Some);
            }
            if !c.rest {
                return Ok(None);
            }
            c.ftp.resume_transfer(offset as usize)?;
            c.ftp.put_with_stream(&p.path).map(Some)
        })();
        match result {
            Ok(Some(stream)) => Ok(Some(FtpDirect {
                lease: Some(lease),
                stream: Some(stream),
                path: p.path.clone(),
            })),
            Ok(None) => {
                lease.release();
                Ok(None)
            }
            Err(e) => Err(self.fail(lease, e).context(p.display())),
        }
    }
    /// A reader from byte `offset` (`REST`).
    fn read_from(&self, p: &VPath, offset: u64) -> Result<FtpReader> {
        self.validate(p)?;
        let mut lease = self.pool.lease().with_context(|| p.display())?;
        let result = (|| {
            let c = lease.conn();
            if offset > 0 {
                c.ftp.resume_transfer(offset as usize)?;
            }
            c.ftp.retr_as_stream(&p.path)
        })();
        match result {
            Ok(stream) => Ok(FtpReader {
                pool: self.pool.clone(),
                lease: Some(lease),
                stream: Some(stream),
            }),
            Err(e) => Err(self.fail(lease, e).context(p.display())),
        }
    }
    fn download(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let h = &self.pool.host;
        let port = h.port.to_string();
        cached_download(
            self,
            p,
            &h.id,
            &[h.protocol.name(), &h.host, &h.user, &port],
            progress,
            cancel,
        )
    }
    /// Post-order (children before their folder): `(path, is a real folder)`.
    fn collect_tree(
        &self,
        entry: &Entry,
        out: &mut Vec<(VPath, bool)>,
        depth: usize,
    ) -> Result<()> {
        anyhow::ensure!(
            depth < 256,
            "directory nesting limit: {}",
            entry.path.display()
        );
        let dir = entry.kind == Kind::Dir && !entry.is_link;
        if dir {
            for child in self.list(&entry.path)? {
                if child.name.contains(UNDECODABLE) {
                    return Err(undecodable(&child.path).context("nothing was deleted"));
                }
                self.collect_tree(&child, out, depth + 1)?;
            }
        }
        out.push((entry.path.clone(), dir));
        Ok(())
    }
}

/// Raised here (not by the server): the target is already there.
fn exists() -> FtpError {
    FtpError::ConnectionError(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "destination exists",
    ))
}

/// Errors after which the connection is still usable: the server's refusals and the
/// checks made here (`AlreadyExists`, `Other`).
fn keeps(e: &FtpError) -> bool {
    refused(e).is_some()
        || matches!(e, FtpError::ConnectionError(io)
            if matches!(io.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::Other))
}

fn to_io(e: FtpError) -> io::Error {
    match e {
        FtpError::ConnectionError(io)
            if matches!(
                io.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::Other
            ) =>
        {
            io
        }
        e => describe(&e),
    }
}

fn parent_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".to_owned(),
        Some((dir, _)) => dir.to_owned(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Every entry of `dir` (`MLSD`, else `LIST`) and whether it is a link; a link's kind
/// is its target's (a folder when `CWD` into it works, else a file).
fn list_on(c: &mut Conn, dir: &str) -> FtpResult<Vec<(Item, bool)>> {
    c.ftp.cwd(dir)?;
    let items = loop {
        let cmd = match (c.mlsd, c.unix) {
            (true, _) => "MLSD",
            (false, true) => "LIST -a",
            (false, false) => "LIST",
        };
        let (_, mut stream) = match c
            .ftp
            .custom_data_command(cmd, &[Status::AboutToSend, Status::AlreadyOpen])
        {
            // A server that says UNIX but takes no `ls` options.
            Err(e) if cmd == "LIST -a" && refused(&e).is_some_and(|c| c >= 500) => {
                c.unix = false;
                continue;
            }
            other => other?,
        };
        let lines = list::lines(BufReader::new(&mut stream)).map_err(io_err)?;
        stream.finish()?;
        let now = SystemTime::now();
        break lines
            .iter()
            .filter_map(|l| match c.mlsd {
                true => list::mlsd(l),
                false => list::list(l, now),
            })
            .collect::<Vec<_>>();
    };
    let mut probes = 0;
    let mut out = Vec::with_capacity(items.len());
    for mut item in items {
        let link = item.kind == Type::Link;
        if link {
            probes += 1;
            let dir = probes <= LINK_PROBES && c.ftp.cwd(join(dir, &item.name)).is_ok();
            item.kind = if dir { Type::Dir } else { Type::File };
        }
        out.push((item, link));
    }
    Ok(out)
}

/// `path`'s entry and whether it is a link (`MLST`, else its parent's listing); None when
/// it does not exist.
fn stat_on(c: &mut Conn, path: &str) -> FtpResult<Option<(Item, bool)>> {
    if path == "/" {
        let root = Item {
            name: "/".into(),
            kind: Type::Dir,
            size: 0,
            modified: None,
        };
        return Ok(Some((root, false)));
    }
    let gone = |e: &FtpError| refused(e).is_some_and(|c| c == 550 || c == 450);
    if c.mlsd {
        match c.ftp.mlst(Some(path)) {
            Ok(line) => {
                if let Some(mut item) = list::mlst(&line) {
                    let link = item.kind == Type::Link;
                    if link {
                        let dir = c.ftp.cwd(path).is_ok();
                        item.kind = if dir { Type::Dir } else { Type::File };
                    }
                    return Ok(Some((item, link)));
                }
            }
            Err(e) if gone(&e) => return Ok(None),
            // MLST refused for this path: the parent's listing below.
            Err(e) if refused(&e).is_some() => {}
            Err(e) => return Err(e),
        }
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    match list_on(c, &parent_of(path)) {
        Ok(items) => Ok(items.into_iter().find(|(i, _)| i.name == name)),
        Err(e) if gone(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

impl Provider for FtpProvider {
    fn scheme(&self) -> &'static str {
        "ftp"
    }
    fn caps(&self) -> Caps {
        Caps {
            write: true,
            rename: true,
            delete: true,
            watch: false,
        }
    }
    fn list(&self, p: &VPath) -> Result<Vec<Entry>> {
        let items = self.with(p, |c| list_on(c, &p.path))?;
        Ok(items
            .iter()
            .map(|(i, link)| self.entry(p.join(&i.name), i, *link))
            .collect())
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        match self.with(p, |c| stat_on(c, &p.path))? {
            Some((item, link)) => Ok(self.entry(p.clone(), &item, link)),
            None => Err(anyhow::Error::from(io::Error::new(
                io::ErrorKind::NotFound,
                "no such file or folder on the FTP server",
            ))
            .context(p.display())),
        }
    }
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(self.read_from(p, 0)?))
    }
    fn read_range(&self, p: &VPath, offset: u64, len: u64) -> Result<Option<Box<dyn Read + Send>>> {
        Ok(Some(Box::new(self.read_from(p, offset)?.take(len))))
    }
    /// An `FtpUpload`: `flush()` commits it (see `Provider::write`).
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.upload(p, false)?))
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.upload(p, true)?))
    }
    fn write_at(&self, p: &VPath, offset: u64) -> Result<Option<Box<dyn Write + Send>>> {
        Ok(self
            .direct(p, offset)?
            .map(|w| Box::new(w) as Box<dyn Write + Send>))
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.with(p, |c| c.ftp.mkdir(&p.path))
    }
    /// Never replaces an existing target (checked first: FTP's own rename replaces on most
    /// servers, so a file that appears between the check and the rename is replaced).
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename_with(from, to, false)
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename_with(from, to, false)
    }
    /// `RNFR`/`RNTO` over an existing file: atomic on POSIX servers; servers that refuse an
    /// existing target fail and leave it as it was.
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename_with(from, to, true)
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        self.with(p, |c| c.ftp.rmdir(&p.path))
    }
    /// Lists the whole tree first: an entry that cannot be addressed (a non-UTF-8 name)
    /// refuses the delete before anything is removed.
    fn remove(&self, p: &VPath) -> Result<()> {
        let entry = self.stat(p)?;
        let mut doomed = Vec::new();
        self.collect_tree(&entry, &mut doomed, 0)?;
        for (path, dir) in doomed {
            self.with(&path, |c| match dir {
                true => c.ftp.rmdir(&path.path),
                false => c.ftp.rm(&path.path),
            })?;
        }
        Ok(())
    }
    fn remove_kind(&self) -> crate::RemoveKind {
        crate::RemoveKind::Permanent
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        self.download(p, &|_| {}, &AtomicBool::new(false))
    }
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        self.download(p, progress, cancel)
    }
}

/// A download. Read to the end, it reads the server's final reply and its connection goes
/// back to the pool; dropped before that, the connection is closed.
pub struct FtpReader {
    pool: Arc<Pool>,
    lease: Option<Lease>,
    stream: Option<Transfer>,
}
impl Read for FtpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(0);
        };
        let result = stream
            .read(buf)
            .map_err(FtpError::ConnectionError)
            .and_then(|n| {
                if n == 0 && !buf.is_empty() {
                    self.stream.take().expect("a transfer").finish()?;
                }
                Ok(n)
            });
        match result {
            Ok(0) if !buf.is_empty() => {
                if let Some(lease) = self.lease.take() {
                    lease.release();
                }
                Ok(0)
            }
            Ok(n) => Ok(n),
            Err(e) => {
                self.abort();
                if refused(&e).is_none() {
                    self.pool.failed("transfer interrupted");
                }
                Err(describe(&e))
            }
        }
    }
}
impl FtpReader {
    fn abort(&mut self) {
        if let Some(lease) = self.lease.as_mut() {
            lease.shutdown();
        }
        self.stream = None;
        self.lease = None;
    }
}
impl Drop for FtpReader {
    fn drop(&mut self) {
        if self.stream.is_some() {
            self.abort();
        }
    }
}

/// One upload, streamed as it is written to a staging file beside the target. `finish()`
/// (or `flush()`, for `Box<dyn Write>` users) ends the transfer, checks the size where the
/// server reports it and renames the file into place. Dropped before that, the staging
/// file is removed: the target is never left half-written.
pub struct FtpUpload {
    provider: Arc<Pool>,
    lease: Option<Lease>,
    stream: Option<Transfer>,
    partial: String,
    target: VPath,
    exclusive: bool,
    written: u64,
    committed: bool,
    failed: bool,
}
impl FtpUpload {
    /// Commits the upload. Must be called (or `flush()`) for the file to appear.
    pub fn finish(mut self) -> Result<()> {
        Ok(self.commit()?)
    }
    fn commit(&mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        if self.failed {
            return Err(io::Error::other("upload failed"));
        }
        let (partial, target, exclusive, written) = (
            self.partial.clone(),
            self.target.path.clone(),
            self.exclusive,
            self.written,
        );
        let mut lease = self.lease.take().expect("an open upload");
        let result = (|| {
            self.stream.take().expect("an open upload").finish()?;
            let c = lease.conn();
            if c.size {
                let size = c.ftp.size(&partial)? as u64;
                if size != written {
                    return Err(FtpError::ConnectionError(io::Error::other(
                        "upload size mismatch",
                    )));
                }
            }
            if exclusive && stat_on(c, &target)?.is_some() {
                return Err(exists());
            }
            c.ftp.rename(&partial, &target)
        })();
        match result {
            Ok(()) => {
                lease.release();
                self.committed = true;
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                if keeps(&e) {
                    lease.release();
                }
                let e = to_io(e);
                Err(io::Error::new(
                    e.kind(),
                    format!("{}: {e}", self.target.display()),
                ))
            }
        }
    }
}
impl Write for FtpUpload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.committed || self.failed {
            return Err(io::Error::other("upload finished or failed"));
        }
        let stream = self.stream.as_mut().expect("an open upload");
        match stream.write(bytes) {
            Ok(n) => {
                self.written += n as u64;
                Ok(n)
            }
            Err(e) => {
                self.failed = true;
                Err(io::Error::new(
                    e.kind(),
                    format!("{}: FTP upload interrupted: {e}", self.target.display()),
                ))
            }
        }
    }
    /// Commits, like `finish()`.
    fn flush(&mut self) -> io::Result<()> {
        self.commit()
    }
}
impl Drop for FtpUpload {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if !self.failed {
            tracing::error!(
                target = %self.target.display(),
                "upload dropped without finish(); discarding it"
            );
        }
        if let Some(lease) = self.lease.as_mut() {
            lease.shutdown();
        }
        self.stream = None;
        self.lease = None;
        // Only our own staging file is removed, on a fresh connection.
        if let Ok(mut lease) = self.provider.lease() {
            if lease.conn().ftp.rm(&self.partial).is_ok() {
                lease.release();
            }
        }
    }
}

/// `Provider::write_at`: writes in place. `flush()` ends the transfer, so the server has
/// written (and reports the size of) every byte so far; the next write continues with
/// `APPE`. Dropping it ends the transfer too.
pub struct FtpDirect {
    lease: Option<Lease>,
    stream: Option<Transfer>,
    path: String,
}
impl Write for FtpDirect {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.stream.is_none() {
            let lease = self
                .lease
                .as_mut()
                .ok_or_else(|| io::Error::other("stream closed"))?;
            match lease.conn().ftp.append_with_stream(&self.path) {
                Ok(s) => self.stream = Some(s),
                Err(e) => {
                    if !keeps(&e) {
                        self.lease = None;
                    }
                    return Err(to_io(e));
                }
            }
        }
        let stream = self.stream.as_mut().expect("an open transfer");
        stream.write(bytes).inspect_err(|_| {
            if let Some(lease) = self.lease.as_mut() {
                lease.shutdown();
            }
            self.stream = None;
            self.lease = None;
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        let Some(stream) = self.stream.take() else {
            return Ok(());
        };
        stream.finish().map_err(|e| {
            if !keeps(&e) {
                self.lease = None;
            }
            to_io(e)
        })
    }
}
impl Drop for FtpDirect {
    fn drop(&mut self) {
        if self.flush().is_ok() {
            if let Some(lease) = self.lease.take() {
                lease.release();
            }
        }
    }
}
