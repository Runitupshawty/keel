//! A small FTP server for the provider tests: passive mode (`PASV`, `EPSV`), UNIX `LIST`
//! output (dot files with `-a`), optional `MLSD`/`MLST`, `REST`, `APPE`, explicit TLS
//! (`AUTH TLS`, `PROT P` data connections), serving a temporary folder on 127.0.0.1.
//! One thread per control connection; it stops with the test process.

use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

/// A test CA and a certificate it issued for `localhost` and 127.0.0.1 (P-256, valid until
/// 2126). Generated for these tests only; the key protects nothing.
pub const CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBlTCCATugAwIBAgIUC2b4Al/kCST+9aT5yn/EYTFbNWkwCgYIKoZIzj0EAwIw
FzEVMBMGA1UEAwwMS2VlbCB0ZXN0IENBMCAXDTI2MTAxMTEwMjMzMloYDzIxMjYw
OTE3MTAyMzMyWjAXMRUwEwYDVQQDDAxLZWVsIHRlc3QgQ0EwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAATu+aE0BMIgt8Q1sMfI41eBmxUWxJaB/LpVQDlaExSE0PsI
sLRdozPMmEZUdZypfedsshGMYYGzElOWHLqlWjHKo2MwYTAdBgNVHQ4EFgQUnmBx
oEih733tN/HCcDKnFgQ8Xm0wHwYDVR0jBBgwFoAUnmBxoEih733tN/HCcDKnFgQ8
Xm0wDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAgQwCgYIKoZIzj0EAwID
SAAwRQIgDZFWJEN0NzVRie3gNKK36N3mU/TIMnRTYhubFFxWQaYCIQD0gReZY0jc
pcw3wUL21a0X6kxol+OKQ0sStuWWSbh6FQ==
-----END CERTIFICATE-----
";
const LEAF_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBvDCCAWKgAwIBAgIUDwUL4ILcX9hwbvFrMjJIIpMGiNowCgYIKoZIzj0EAwIw
FzEVMBMGA1UEAwwMS2VlbCB0ZXN0IENBMCAXDTI2MTAxMTEwMjMzMloYDzIxMjYw
OTE3MTAyMzMyWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggq
hkjOPQMBBwNCAATAFKKSu4jxm4TwkO+c1QWadr0jONeLSHo1Ms7Yyz+TDdG2qGYW
jlMk7L7xD2yF1usyYho06NsAJFuxWXD9FeVto4GMMIGJMBoGA1UdEQQTMBGCCWxv
Y2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDATBgNVHSUEDDAK
BggrBgEFBQcDATAdBgNVHQ4EFgQU75DAbC9nv9tKFfMxW4+AnjWyHaswHwYDVR0j
BBgwFoAUnmBxoEih733tN/HCcDKnFgQ8Xm0wCgYIKoZIzj0EAwIDSAAwRQIgQbQn
ylaQbvEx0n0GcihlXzrW73jjQfdQDFfMBJxpJIwCIQDZuEqqoQEbKZ7Ou1/L2bQE
kg9gbJMWqYyDU44L//TFpQ==
-----END CERTIFICATE-----
";
const LEAF_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgw6bIIB9/L5pGmZnb
c1k1ifMMrPPTAvw6PHO0Hl3AR46hRANCAATAFKKSu4jxm4TwkO+c1QWadr0jONeL
SHo1Ms7Yyz+TDdG2qGYWjlMk7L7xD2yF1usyYho06NsAJFuxWXD9FeVt
-----END PRIVATE KEY-----
";

pub fn ca() -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(CA_PEM.as_bytes()).unwrap()
}
pub fn leaf() -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(LEAF_PEM.as_bytes()).unwrap()
}

#[derive(Clone, Default)]
pub struct Options {
    pub user: String,
    pub password: String,
    /// Answer `MLSD`/`MLST` (and list `MLST` in `FEAT`); else `LIST` only.
    pub mlsd: bool,
    /// Offer `AUTH TLS` with the test certificate.
    pub tls: bool,
    /// `RETR` sends this many bytes, closes the data connection and replies 426.
    pub cut_retr: Option<usize>,
    /// `RETR` opens the data connection and sends nothing.
    pub stall_retr: bool,
    /// Names every `LIST`/`MLSD` also shows as 1-byte files that cannot be read: hostile
    /// names the disk under the root cannot hold.
    pub listed: Vec<String>,
}

pub struct Server {
    pub port: u16,
    pub root: PathBuf,
    _dir: tempfile::TempDir,
    /// Control connections accepted so far.
    pub connections: Arc<AtomicUsize>,
}

impl Server {
    pub fn start(options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let tls = options.tls.then(server_config);
        let options = Arc::new(options);
        let (count, base) = (connections.clone(), root.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                count.fetch_add(1, Ordering::SeqCst);
                let (options, base, tls) = (options.clone(), base.clone(), tls.clone());
                std::thread::spawn(move || {
                    let _ = Session::new(stream, options, base, tls).run();
                });
            }
        });
        Self {
            port,
            root,
            _dir: dir,
            connections,
        }
    }
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel.trim_start_matches('/'))
    }
}

fn server_config() -> Arc<rustls::ServerConfig> {
    let provider = Arc::new(rustls_graviola::default_provider());
    let key = PrivateKeyDer::from_pem_slice(LEAF_KEY.as_bytes()).unwrap();
    Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![leaf(), ca()], key)
            .unwrap(),
    )
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
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
impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }
    fn upgrade(self, tls: &Arc<rustls::ServerConfig>) -> io::Result<Stream> {
        let Stream::Plain(mut tcp) = self else {
            return Ok(self);
        };
        let mut conn = rustls::ServerConnection::new(tls.clone()).map_err(io::Error::other)?;
        while conn.is_handshaking() {
            conn.complete_io(&mut tcp)?;
        }
        Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))))
    }
    fn close(self) {
        if let Stream::Tls(mut s) = self {
            s.conn.send_close_notify();
            let _ = s.conn.complete_io(&mut s.sock);
        }
    }
}

struct Session {
    control: Option<Stream>,
    options: Arc<Options>,
    root: PathBuf,
    tls: Option<Arc<rustls::ServerConfig>>,
    cwd: String,
    user: Option<String>,
    logged_in: bool,
    protect: bool,
    passive: Option<TcpListener>,
    rest: u64,
    rename_from: Option<PathBuf>,
}

impl Session {
    fn new(
        stream: TcpStream,
        options: Arc<Options>,
        root: PathBuf,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> Self {
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        Self {
            control: Some(Stream::Plain(stream)),
            options,
            root,
            tls,
            cwd: "/".into(),
            user: None,
            logged_in: false,
            protect: false,
            passive: None,
            rest: 0,
            rename_from: None,
        }
    }
    fn reply(&mut self, text: &str) -> io::Result<()> {
        let c = self.control.as_mut().unwrap();
        c.write_all(text.as_bytes())?;
        c.write_all(b"\r\n")?;
        c.flush()
    }
    fn line(&mut self) -> io::Result<Option<String>> {
        let c = self.control.as_mut().unwrap();
        let mut out = Vec::new();
        let mut byte = [0];
        loop {
            match c.read(&mut byte)? {
                0 if out.is_empty() => return Ok(None),
                0 => break,
                _ if byte[0] == b'\n' => break,
                _ => out.push(byte[0]),
            }
        }
        if out.last() == Some(&b'\r') {
            out.pop();
        }
        Ok(Some(String::from_utf8_lossy(&out).into_owned()))
    }
    /// An FTP path (absolute or relative to the working folder) inside the root.
    fn local(&self, arg: &str) -> Option<PathBuf> {
        let full = if arg.starts_with('/') {
            arg.to_owned()
        } else {
            format!("{}/{arg}", self.cwd.trim_end_matches('/'))
        };
        let mut path = self.root.clone();
        for part in full.split('/').filter(|p| !p.is_empty() && *p != ".") {
            if part == ".." {
                return None;
            }
            path.push(part);
        }
        Some(path)
    }
    fn data(&mut self) -> io::Result<Stream> {
        let listener = self
            .passive
            .take()
            .ok_or_else(|| io::Error::other("no PASV"))?;
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
        let stream = Stream::Plain(tcp);
        match (&self.tls, self.protect) {
            (Some(tls), true) => stream.upgrade(tls),
            _ => Ok(stream),
        }
    }
    fn run(mut self) -> io::Result<()> {
        self.reply("220 keel test server")?;
        while let Some(line) = self.line()? {
            let (cmd, arg) = line.split_once(' ').unwrap_or((&line, ""));
            let cmd = cmd.to_ascii_uppercase();
            let open = matches!(
                cmd.as_str(),
                "USER" | "PASS" | "AUTH" | "PBSZ" | "PROT" | "QUIT" | "FEAT" | "SYST" | "NOOP"
            );
            if !open && !self.logged_in {
                self.reply("530 not logged in")?;
                continue;
            }
            match cmd.as_str() {
                "USER" => {
                    self.user = Some(arg.to_owned());
                    self.reply("331 password please")?;
                }
                "PASS" => {
                    let user = self.user.clone().unwrap_or_default();
                    self.logged_in = (user == self.options.user && arg == self.options.password)
                        || (user == "anonymous" && arg.contains('@'));
                    match self.logged_in {
                        true => self.reply("230 welcome")?,
                        false => self.reply("530 no")?,
                    }
                }
                "AUTH" => match self.tls.clone() {
                    Some(tls) => {
                        self.reply("234 go ahead")?;
                        let plain = self.control.take().unwrap();
                        match plain.upgrade(&tls) {
                            Ok(s) => self.control = Some(s),
                            Err(_) => return Ok(()),
                        }
                    }
                    None => self.reply("502 no TLS here")?,
                },
                "PBSZ" => self.reply("200 ok")?,
                "PROT" => {
                    self.protect = arg.eq_ignore_ascii_case("P");
                    self.reply("200 ok")?;
                }
                "SYST" => self.reply("215 UNIX Type: L8")?,
                "FEAT" => {
                    let mlst = match self.options.mlsd {
                        true => " MLST type*;size*;modify*;\r\n",
                        false => "",
                    };
                    let auth = match self.tls {
                        Some(_) => " AUTH TLS\r\n PBSZ\r\n PROT\r\n",
                        None => "",
                    };
                    let text = format!(
                        "211-Features:\r\n SIZE\r\n MDTM\r\n REST STREAM\r\n UTF8\r\n EPSV\r\n{mlst}{auth}211 End"
                    );
                    self.reply(&text)?;
                }
                "OPTS" | "TYPE" | "NOOP" => self.reply("200 ok")?,
                "PWD" => {
                    let text = format!("257 \"{}\"", self.cwd);
                    self.reply(&text)?;
                }
                "CWD" => match self.local(arg).filter(|p| p.is_dir()) {
                    Some(_) => {
                        let full = if arg.starts_with('/') {
                            arg.to_owned()
                        } else {
                            format!("{}/{arg}", self.cwd.trim_end_matches('/'))
                        };
                        self.cwd = full;
                        self.reply("250 ok")?;
                    }
                    None => self.reply("550 no such folder")?,
                },
                "PASV" | "EPSV" => {
                    let listener = TcpListener::bind("127.0.0.1:0")?;
                    let port = listener.local_addr()?.port();
                    self.passive = Some(listener);
                    let text = match cmd.as_str() {
                        "PASV" => format!(
                            "227 Entering Passive Mode (127,0,0,1,{},{})",
                            port / 256,
                            port % 256
                        ),
                        _ => format!("229 Entering Extended Passive Mode (|||{port}|)"),
                    };
                    self.reply(&text)?;
                }
                "LIST" | "MLSD" => {
                    if cmd == "MLSD" && !self.options.mlsd {
                        self.reply("500 unknown command")?;
                        continue;
                    }
                    let all = arg.starts_with("-a");
                    let path = arg.trim_start_matches("-a").trim();
                    let Some(dir) = self.local(if path.is_empty() { "." } else { path }) else {
                        self.reply("550 bad path")?;
                        continue;
                    };
                    let mut entries: Vec<_> = fs::read_dir(&dir)?.flatten().collect();
                    entries.sort_by_key(|e| e.file_name());
                    let mut text = String::new();
                    if cmd == "LIST" {
                        text.push_str("total 0\r\n");
                    }
                    for e in entries {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if cmd == "LIST" && name.starts_with('.') && !all {
                            continue;
                        }
                        let m = e.metadata()?;
                        text.push_str(&match cmd.as_str() {
                            "LIST" => unix_line(&name, &m),
                            _ => mlsd_line(&name, &m),
                        });
                    }
                    for name in &self.options.listed {
                        text.push_str(&match cmd.as_str() {
                            "LIST" => format!(
                                "-rw-r--r--    1 1000     1000            1 Jan  1  2020 {name}\r\n"
                            ),
                            _ => format!("type=file;size=1;modify=20200101000000; {name}\r\n"),
                        });
                    }
                    self.reply("150 here it comes")?;
                    let mut data = self.data()?;
                    data.write_all(text.as_bytes())?;
                    data.close();
                    self.reply("226 done")?;
                }
                "MLST" => {
                    if !self.options.mlsd {
                        self.reply("500 unknown command")?;
                        continue;
                    }
                    match self.local(arg).and_then(|p| fs::metadata(p).ok()) {
                        Some(m) => {
                            let line = mlsd_line(arg, &m);
                            let text = format!("250-facts\r\n {}250 End", line);
                            self.reply(&text)?;
                        }
                        None => self.reply("550 no such file")?,
                    }
                }
                "REST" => {
                    self.rest = arg.parse().unwrap_or(0);
                    self.reply("350 restarting")?;
                }
                "SIZE" => match self.local(arg).and_then(|p| fs::metadata(p).ok()) {
                    Some(m) if m.is_file() => {
                        let text = format!("213 {}", m.len());
                        self.reply(&text)?;
                    }
                    _ => self.reply("550 no such file")?,
                },
                "RETR" => {
                    let Some(path) = self.local(arg).filter(|p| p.is_file()) else {
                        self.reply("550 no such file")?;
                        continue;
                    };
                    let mut bytes = fs::read(path)?;
                    bytes.drain(..(self.rest as usize).min(bytes.len()));
                    self.rest = 0;
                    self.reply("150 sending")?;
                    let mut data = self.data()?;
                    if self.options.stall_retr {
                        std::thread::sleep(Duration::from_secs(5));
                        drop(data);
                        self.reply("426 stalled")?;
                        continue;
                    }
                    let cut = self.options.cut_retr;
                    let send = cut.unwrap_or(bytes.len()).min(bytes.len());
                    let ok = data.write_all(&bytes[..send]).is_ok();
                    match cut {
                        Some(_) => drop(data),
                        None => data.close(),
                    }
                    match ok && cut.is_none() {
                        true => self.reply("226 done")?,
                        false => self.reply("426 aborted")?,
                    }
                }
                "STOR" | "APPE" => {
                    let Some(path) = self.local(arg) else {
                        self.reply("553 bad name")?;
                        continue;
                    };
                    let mut file = match (cmd.as_str(), self.rest) {
                        ("APPE", _) => fs::OpenOptions::new().append(true).create(true).open(&path),
                        (_, 0) => fs::File::create(&path),
                        (_, _) => fs::OpenOptions::new().write(true).open(&path),
                    }?;
                    if cmd == "STOR" && self.rest > 0 {
                        io::Seek::seek(&mut file, io::SeekFrom::Start(self.rest))?;
                    }
                    self.rest = 0;
                    self.reply("150 send it")?;
                    let mut data = self.data()?;
                    let copied = io::copy(&mut data, &mut file);
                    data.close();
                    match copied {
                        Ok(_) => self.reply("226 stored")?,
                        Err(_) => self.reply("426 aborted")?,
                    }
                }
                "DELE" => match self.local(arg).map(fs::remove_file) {
                    Some(Ok(())) => self.reply("250 deleted")?,
                    _ => self.reply("550 cannot delete")?,
                },
                "RMD" => match self.local(arg).map(fs::remove_dir) {
                    Some(Ok(())) => self.reply("250 removed")?,
                    _ => self.reply("550 cannot remove")?,
                },
                "MKD" => match self.local(arg).map(fs::create_dir) {
                    Some(Ok(())) => self.reply("257 created")?,
                    _ => self.reply("550 cannot create")?,
                },
                "RNFR" => match self.local(arg).filter(|p| p.exists()) {
                    Some(p) => {
                        self.rename_from = Some(p);
                        self.reply("350 ready")?;
                    }
                    None => self.reply("550 no such file")?,
                },
                "RNTO" => {
                    let from = self.rename_from.take();
                    match (from, self.local(arg)) {
                        (Some(from), Some(to)) if fs::rename(&from, &to).is_ok() => {
                            self.reply("250 renamed")?
                        }
                        _ => self.reply("553 cannot rename")?,
                    }
                }
                "QUIT" => {
                    self.reply("221 bye")?;
                    break;
                }
                _ => self.reply("502 not implemented")?,
            }
        }
        if let Some(c) = self.control.take() {
            let _ = c.tcp().shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    }
}

fn unix_line(name: &str, m: &fs::Metadata) -> String {
    let t = chrono::DateTime::<chrono::Utc>::from(m.modified().unwrap());
    let kind = if m.is_dir() { 'd' } else { '-' };
    format!(
        "{kind}rw-r--r--    1 1000     1000     {:>8} {} {}\r\n",
        m.len(),
        t.format("%b %e %Y"),
        name
    )
}

fn mlsd_line(name: &str, m: &fs::Metadata) -> String {
    let t = chrono::DateTime::<chrono::Utc>::from(m.modified().unwrap());
    let kind = if m.is_dir() { "dir" } else { "file" };
    format!(
        "type={kind};size={};modify={}; {}\r\n",
        m.len(),
        t.format("%Y%m%d%H%M%S"),
        name
    )
}

/// Writes `bytes` at `rel` under the server's root (folders created).
pub fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel.trim_start_matches('/'));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
