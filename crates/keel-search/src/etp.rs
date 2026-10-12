//! ETP client: searches an ETP server (Everything's ETP/FTP server, or keel-daemon's
//! `--etp`) with Everything's `SITE EVERYTHING` commands on the FTP control connection.
//!
//! A search sets the options (`SITE EVERYTHING SEARCH text`, `CASE 1`, `COUNT 100`, the
//! `PATH_COLUMN`, `SIZE_COLUMN`, ... switches; only the ones that changed since the last
//! search on the connection are sent again) and runs `SITE EVERYTHING QUERY`, whose reply
//! is:
//!
//! ```text
//! 200-Query results
//!  RESULT_COUNT 2
//!  PATH C:\Users\me
//!  SIZE 1234
//!  DATE_MODIFIED 133444736000000000
//!  FILE notes.txt
//!  PATH C:\Users
//!  SIZE 18446744073709551615
//!  FOLDER me
//! 200 End.
//! ```
//!
//! (the columns of a result come before its `FILE`/`FOLDER` line; sizes and dates
//! `u64::MAX` mean unknown; dates are Windows FILETIMEs). No data connection is opened:
//! hits are `ftp://<authority>/<path>` paths, which the FTP remote whose id is the
//! authority opens. With `tls`, the connection asks for explicit TLS (`AUTH TLS`) and never
//! falls back to plain FTP; the certificate is checked against the OS trust store, or
//! pinned by its SHA-256 fingerprint (`ftps_known_hosts`, as FTPS remotes).

use crate::{Hit, Query, Searcher};
use anyhow::{bail, Context, Result};
use keel_vfs::ftp::tls::{self, Connector};
use keel_vfs::VPath;
use parking_lot::Mutex;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use suppaftp::{TlsConnector, TlsStream};

/// Everything's ETP server port.
pub const DEFAULT_PORT: u16 = 21;
/// How long a connect, a read or a write may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Results one `QUERY` asks for (Everything's server gives a limited window anyway).
pub const PAGE: u32 = 1000;
/// Most hits one [`Searcher::query`] collects, over several pages.
pub const MAX_FETCH: usize = 10_000;
/// Longest reply line read.
const MAX_LINE: u64 = 64 << 10;
/// Longest reply read (all its lines).
const MAX_REPLY: usize = 16 << 20;
/// Most results one reply may hold.
const MAX_ITEMS: usize = 100_000;

/// Where an ETP server is and how to sign in.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Empty: `anonymous`.
    pub user: String,
    pub password: String,
    /// Explicit TLS (`AUTH TLS`), never falling back to plain FTP.
    pub tls: bool,
    /// The SHA-256 fingerprint (`AB:CD:...`) of a certificate trusted although the OS does
    /// not; None: the one pinned in `ftps_known_hosts` for this host and port, if any.
    pub pin: Option<String>,
    /// The authority of the hits' `ftp://` paths: the id of the FTP remote that opens
    /// them. None: `host:port`.
    pub authority: Option<String>,
    pub timeout: Duration,
}

impl std::fmt::Debug for Target {
    /// Never shows the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("tls", &self.tls)
            .field("pin", &self.pin)
            .field("authority", &self.authority)
            .finish_non_exhaustive()
    }
}

impl Target {
    /// `[user[:password]@]host[:port]`, as Everything's `-connect` (an IPv6 address in
    /// brackets when a port follows). Plain FTP, no pin, `host:port` as the authority.
    pub fn parse(spec: &str) -> Result<Target> {
        let spec = spec.trim();
        let (creds, hostport) = match spec.rsplit_once('@') {
            Some((c, h)) => (Some(c), h),
            None => (None, spec),
        };
        let (user, password) = match creds {
            Some(c) => match c.split_once(':') {
                Some((u, p)) => (u.to_owned(), p.to_owned()),
                None => (c.to_owned(), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
            let (host, after) = rest
                .split_once(']')
                .with_context(|| format!("{spec}: an IPv6 address needs a closing ']'"))?;
            match after {
                "" => (host, None),
                p => (
                    host,
                    Some(p.strip_prefix(':').with_context(|| {
                        format!("{spec}: expected :port after the IPv6 address")
                    })?),
                ),
            }
        } else {
            match hostport.rsplit_once(':') {
                // One colon: host:port; more: a bare IPv6 address.
                Some((h, p)) if !h.contains(':') => (h, Some(p)),
                _ => (hostport, None),
            }
        };
        let port = match port {
            None => DEFAULT_PORT,
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .with_context(|| format!("{spec}: {p:?} is not a port"))?,
        };
        anyhow::ensure!(
            !host.is_empty()
                && host.len() <= 255
                && !host
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || "/\\@[]".contains(c)),
            "{spec}: not a host name or address"
        );
        for (what, s) in [("user name", &user), ("password", &password)] {
            anyhow::ensure!(
                !s.chars().any(char::is_control) && s.len() <= 1024,
                "the {what} may not hold control characters"
            );
        }
        Ok(Target {
            host: host.to_owned(),
            port,
            user,
            password,
            tls: false,
            pin: None,
            authority: None,
            timeout: TIMEOUT,
        })
    }

    /// `host:port` (`[v6]:port`).
    pub fn endpoint(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// What results are sorted by (Everything's sort names).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Extension,
    DateCreated,
    DateModified,
    Attributes,
}

/// A search: Everything's options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub search: String,
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub match_diacritics: bool,
    pub prefix: bool,
    pub suffix: bool,
    pub ignore_punctuation: bool,
    pub ignore_whitespace: bool,
    pub regex: bool,
    pub sort: SortKey,
    pub descending: bool,
    pub offset: u32,
    pub count: u32,
}

impl Request {
    /// The `SITE EVERYTHING` lines that set it (always the same lines in the same order),
    /// with the columns a hit needs.
    pub fn commands(&self) -> Result<Vec<String>> {
        anyhow::ensure!(
            !self.search.contains(['\r', '\n', '\0']),
            "the search may not hold line breaks"
        );
        anyhow::ensure!(self.search.len() <= 8192, "the search is too long");
        let flag = |name: &str, on: bool| format!("SITE EVERYTHING {name} {}", u8::from(on));
        let sort = match self.sort {
            SortKey::Name => "NAME",
            SortKey::Path => "PATH",
            SortKey::Size => "SIZE",
            SortKey::Extension => "EXTENSION",
            SortKey::DateCreated => "DATE_CREATED",
            SortKey::DateModified => "DATE_MODIFIED",
            SortKey::Attributes => "ATTRIBUTES",
        };
        let order = if self.descending {
            "DESCENDING"
        } else {
            "ASCENDING"
        };
        Ok(vec![
            format!("SITE EVERYTHING SEARCH {}", self.search),
            flag("CASE", self.match_case),
            flag("WHOLE_WORD", self.whole_word),
            flag("PATH", self.match_path),
            flag("DIACRITICS", self.match_diacritics),
            flag("PREFIX", self.prefix),
            flag("SUFFIX", self.suffix),
            flag("IGNORE_PUNCTUATION", self.ignore_punctuation),
            flag("IGNORE_WHITESPACE", self.ignore_whitespace),
            flag("REGEX", self.regex),
            format!("SITE EVERYTHING SORT {sort}_{order}"),
            format!("SITE EVERYTHING OFFSET {}", self.offset),
            format!("SITE EVERYTHING COUNT {}", self.count),
            flag("PATH_COLUMN", true),
            flag("SIZE_COLUMN", true),
            flag("DATE_MODIFIED_COLUMN", true),
            flag("ATTRIBUTES_COLUMN", true),
        ])
    }
}

/// One result of a `QUERY`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    /// The folder it is in, as the server writes it (`C:\Users\me`; empty for a top
    /// folder). None: the server sent no `PATH` column.
    pub path: Option<String>,
    pub size: Option<u64>,
    /// Windows FILETIMEs (100 ns since 1601).
    pub modified: Option<u64>,
    pub created: Option<u64>,
    pub attributes: Option<u32>,
}

/// A `QUERY`'s results: the total matching and the window asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    pub total: u64,
    pub items: Vec<Item>,
}

/// The lines of a `QUERY` reply between `200-Query results` and `200 End.` (without the
/// line breaks).
pub fn parse_results<S: AsRef<str>>(lines: &[S]) -> Result<Results> {
    let mut out = Results::default();
    let mut item = Item::default();
    let number = |key: &str, v: &str| -> Result<u64> {
        v.trim()
            .parse::<u64>()
            .with_context(|| format!("{key}: {v:?} is not a number"))
    };
    let known = |n: u64| (n != u64::MAX).then_some(n);
    for line in lines {
        let line = line.as_ref();
        let Some(line) = line.strip_prefix(' ') else {
            continue;
        };
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "RESULT_COUNT" => out.total = number(key, value)?,
            "PATH" => item.path = Some(value.to_owned()),
            "SIZE" => item.size = known(number(key, value)?),
            "DATE_MODIFIED" => item.modified = known(number(key, value)?),
            "DATE_CREATED" => item.created = known(number(key, value)?),
            "ATTRIBUTES" => {
                item.attributes = Some(
                    u32::try_from(number(key, value)?)
                        .with_context(|| format!("ATTRIBUTES: {value} is too large"))?,
                )
            }
            "FILE" | "FOLDER" => {
                anyhow::ensure!(
                    out.items.len() < MAX_ITEMS,
                    "more than {MAX_ITEMS} results in one reply"
                );
                anyhow::ensure!(!value.is_empty(), "a result without a name");
                item.name = value.to_owned();
                item.is_dir = key == "FOLDER";
                out.items.push(std::mem::take(&mut item));
            }
            // FILE_LIST_FILENAME, DATE_RECENTLY_CHANGED and what later servers add.
            _ => {}
        }
    }
    Ok(out)
}

/// A FILETIME as a time.
pub fn from_filetime(ft: u64) -> SystemTime {
    const EPOCH: u64 = 116_444_736_000_000_000;
    if ft >= EPOCH {
        UNIX_EPOCH + Duration::from_nanos((ft - EPOCH).saturating_mul(100))
    } else {
        UNIX_EPOCH - Duration::from_nanos((EPOCH - ft).saturating_mul(100))
    }
}

/// The FTP path of `item`: `/` + its folder with `/` for `\` + `/` + its name.
pub fn ftp_path(item: &Item) -> String {
    let folder = item.path.as_deref().unwrap_or_default().replace('\\', "/");
    let folder = folder.trim_matches('/');
    if folder.is_empty() {
        format!("/{}", item.name)
    } else {
        format!("/{folder}/{}", item.name)
    }
}

/// A server reply: its code and its text lines (the first and the last included).
#[derive(Debug)]
struct Reply {
    code: u16,
    lines: Vec<String>,
}

impl Reply {
    fn text(&self) -> String {
        self.lines.join(" | ")
    }
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<tls::Tls>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.mut_ref().read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.mut_ref().write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.mut_ref().flush(),
        }
    }
}

/// One line of a reply, without its line break; None at the end of the stream.
fn read_line(r: &mut impl BufRead, total: &mut usize) -> Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(MAX_LINE + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    anyhow::ensure!(n as u64 <= MAX_LINE, "a reply line is too long");
    *total += n;
    anyhow::ensure!(*total <= MAX_REPLY, "the reply is too long");
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn read_reply(r: &mut impl BufRead) -> Result<Reply> {
    let mut total = 0;
    let first = read_line(r, &mut total)?.context("the server closed the connection")?;
    let code = |l: &str| {
        let b = l.as_bytes();
        (b.len() >= 4 && b[..3].iter().all(u8::is_ascii_digit) && matches!(b[3], b' ' | b'-'))
            .then(|| (l[..3].parse::<u16>().unwrap_or(0), b[3] == b'-'))
    };
    let (status, mut more) = code(&first).with_context(|| {
        format!(
            "not an FTP reply: {:?}",
            first.chars().take(80).collect::<String>()
        )
    })?;
    let mut lines = vec![first];
    let end = format!("{status} ");
    while more {
        let line = read_line(r, &mut total)?.context("the server closed the connection")?;
        more = !line.starts_with(&end);
        lines.push(line);
    }
    Ok(Reply {
        code: status,
        lines,
    })
}

/// A signed-in control connection to an ETP server.
pub struct Client {
    reader: BufReader<Stream>,
    endpoint: String,
    /// The option lines sent last (resent only when they change).
    sent: Vec<String>,
    /// The welcome message (`220`).
    pub welcome: String,
}

impl Client {
    /// Connects, asks for TLS when `t.tls`, signs in and checks that the server has
    /// Everything's search (`FEAT` lists `EVERYTHING`).
    pub fn connect(t: &Target) -> Result<Client> {
        let endpoint = t.endpoint();
        let addrs: Vec<SocketAddr> = (t.host.as_str(), t.port)
            .to_socket_addrs()
            .with_context(|| format!("cannot resolve {}", t.host))?
            .collect();
        let tcp = addrs
            .iter()
            .find_map(|a| TcpStream::connect_timeout(a, t.timeout).ok())
            .with_context(|| format!("cannot connect to {endpoint}"))?;
        tcp.set_read_timeout(Some(t.timeout))?;
        tcp.set_write_timeout(Some(t.timeout))?;
        let _ = tcp.set_nodelay(true);
        let mut c = Client {
            reader: BufReader::new(Stream::Plain(tcp)),
            endpoint: endpoint.clone(),
            sent: Vec::new(),
            welcome: String::new(),
        };
        let hello = read_reply(&mut c.reader)?;
        anyhow::ensure!(
            hello.code == 220,
            "{endpoint} did not greet as an FTP server: {}",
            hello.text()
        );
        c.welcome = hello
            .lines
            .iter()
            .map(|l| l.get(4..).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        if t.tls {
            c = c.secure(t)?;
        }
        let user = if t.user.is_empty() {
            "anonymous"
        } else {
            &t.user
        };
        let r = c.command(&format!("USER {user}"))?;
        let r = match r.code {
            230 => r,
            331 => c.command(&format!("PASS {}", t.password))?,
            _ => bail!("{endpoint} refused the sign-in: {}", r.text()),
        };
        anyhow::ensure!(
            r.code == 230,
            "{endpoint} refused the sign-in: {}",
            r.text()
        );
        let feat = c.command("FEAT")?;
        let everything = feat.code == 211
            && feat
                .lines
                .iter()
                .any(|l| l.trim().eq_ignore_ascii_case("EVERYTHING"));
        anyhow::ensure!(
            everything,
            "{endpoint} is an FTP server without Everything's search (ETP)"
        );
        Ok(c)
    }

    /// `AUTH TLS` and the handshake; never falls back to plain FTP.
    fn secure(mut self, t: &Target) -> Result<Client> {
        let r = self.command("AUTH TLS")?;
        anyhow::ensure!(
            r.code == 234,
            "{} refused TLS (AUTH TLS): {}; Keel never falls back to plain FTP",
            self.endpoint,
            r.text()
        );
        anyhow::ensure!(
            self.reader.buffer().is_empty(),
            "{} sent data before the TLS handshake",
            self.endpoint
        );
        let pin = match &t.pin {
            Some(p) => Some(p.clone()),
            None => tls::pinned(&t.host, t.port)?,
        };
        let connector = Connector::new(pin, t.timeout, &[])?;
        let rejected = connector.rejected.clone();
        let endpoint = self.endpoint.clone();
        let Stream::Plain(tcp) = self.reader.into_inner() else {
            bail!("TLS is already on");
        };
        match connector.connect(&t.host, tcp) {
            Ok(s) => Ok(Client {
                reader: BufReader::new(Stream::Tls(Box::new(s))),
                endpoint,
                sent: self.sent,
                welcome: self.welcome,
            }),
            Err(e) => match rejected.lock().take() {
                Some(r) => bail!(
                    "the TLS certificate of {} is not trusted ({}); its SHA-256 fingerprint is \
                     {}. If that is the server's certificate, trust it with `etp_pin` in \
                     the [search] settings",
                    endpoint,
                    r.reason,
                    r.fingerprint
                ),
                None => bail!("TLS with {endpoint} failed: {e}"),
            },
        }
    }

    /// Sends `line` and reads the reply.
    fn command(&mut self, line: &str) -> Result<Reply> {
        let stream = self.reader.get_mut();
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        read_reply(&mut self.reader)
    }

    /// Runs `r` and returns the server's results.
    pub fn search(&mut self, r: &Request) -> Result<Results> {
        let lines = r.commands()?;
        for (i, line) in lines.iter().enumerate() {
            if self.sent.get(i) == Some(line) {
                continue;
            }
            let reply = self.command(line)?;
            if reply.code != 200 {
                self.sent.clear();
                bail!("{}: {}", self.endpoint, reply.text());
            }
        }
        self.sent = lines;
        let reply = self.command("SITE EVERYTHING QUERY")?;
        anyhow::ensure!(reply.code == 200, "{}: {}", self.endpoint, reply.text());
        parse_results(&reply.lines)
    }

    /// `QUIT`.
    pub fn quit(mut self) {
        let _ = self.command("QUIT");
    }
}

/// An ETP server as a search backend.
pub struct EtpSearcher {
    target: Target,
    client: Mutex<Option<Client>>,
    up: AtomicBool,
}

impl EtpSearcher {
    /// Does not connect yet: the first search (or [`Searcher::probe`]) does.
    pub fn new(target: Target) -> Self {
        Self {
            target,
            client: Mutex::new(None),
            up: AtomicBool::new(false),
        }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Runs `r` on the kept connection (a new one when there is none or it failed).
    pub fn search(&self, r: &Request) -> Result<Results> {
        let mut slot = self.client.lock();
        let reused = slot.is_some();
        let result = (|| {
            if slot.is_none() {
                *slot = Some(Client::connect(&self.target)?);
            }
            slot.as_mut().expect("connected").search(r)
        })();
        let result = match result {
            // A kept connection the server closed meanwhile: once more on a new one.
            Err(_) if reused => {
                *slot = None;
                Client::connect(&self.target).and_then(|mut c| {
                    let found = c.search(r);
                    *slot = Some(c);
                    found
                })
            }
            r => r,
        };
        if result.is_err() {
            *slot = None;
        }
        self.up.store(result.is_ok(), Ordering::Release);
        result
    }

    /// `item` as a hit: an `ftp://<authority>/<path>` path.
    pub fn hit(&self, item: &Item) -> Hit {
        Hit {
            path: VPath {
                scheme: "ftp".into(),
                authority: self
                    .target
                    .authority
                    .clone()
                    .unwrap_or_else(|| self.target.endpoint()),
                path: ftp_path(item),
            },
            is_dir: item.is_dir,
            size: if item.is_dir {
                0
            } else {
                item.size.unwrap_or(0)
            },
            modified: item.modified.map(from_filetime),
            snippet: None,
        }
    }
}

impl Searcher for EtpSearcher {
    fn query(&self, q: &Query) -> Result<Vec<Hit>> {
        let max = (q.max as usize).min(MAX_FETCH);
        let mut hits = Vec::new();
        let mut offset = 0u32;
        while hits.len() < max {
            let r = Request {
                search: q.text.clone(),
                match_case: q.match_case,
                regex: q.regex,
                offset,
                count: PAGE,
                ..Request::default()
            };
            let found = self.search(&r)?;
            let got = found.items.len();
            hits.extend(
                found
                    .items
                    .iter()
                    .filter(|i| i.is_dir || !q.folders_only)
                    .map(|i| self.hit(i)),
            );
            offset = offset.saturating_add(got as u32);
            if got == 0 || u64::from(offset) >= found.total {
                break;
            }
        }
        hits.truncate(max);
        Ok(hits)
    }

    fn available(&self) -> bool {
        self.up.load(Ordering::Acquire)
    }

    fn probe(&self) -> bool {
        let mut slot = self.client.lock();
        if slot.is_none() {
            *slot = Client::connect(&self.target).ok();
        }
        let up = slot.is_some();
        self.up.store(up, Ordering::Release);
        up
    }

    fn status(&self) -> Option<String> {
        Some(format!("ETP server {}", self.target.endpoint()))
    }

    fn name(&self) -> &'static str {
        "ETP"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_like_everythings_connect() {
        let t = Target::parse("me:se:cret@files.example:2121").unwrap();
        assert_eq!(
            (
                t.user.as_str(),
                t.password.as_str(),
                t.host.as_str(),
                t.port
            ),
            ("me", "se:cret", "files.example", 2121)
        );
        assert_eq!(t.endpoint(), "files.example:2121");
        let t = Target::parse("127.0.0.1").unwrap();
        assert_eq!((t.user.as_str(), t.port), ("", DEFAULT_PORT));
        let t = Target::parse("u@[::1]:21").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.user.as_str()), ("::1", 21, "u"));
        assert_eq!(t.endpoint(), "[::1]:21");
        assert_eq!(Target::parse("fe80::1").unwrap().host, "fe80::1");
        assert_eq!(Target::parse("[::1]").unwrap().port, DEFAULT_PORT);
        for bad in [
            "",
            "host:0",
            "host:99999",
            "host:x",
            "[::1",
            "a b",
            "u@",
            "[::1]x",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
        assert!(!format!("{:?}", Target::parse("u:hunter2@h").unwrap()).contains("hunter2"));
    }

    #[test]
    fn requests_are_everythings_commands() {
        let r = Request {
            search: "abc  def".into(),
            match_case: true,
            sort: SortKey::DateModified,
            descending: true,
            offset: 100,
            count: 50,
            ..Request::default()
        };
        let lines = r.commands().unwrap();
        assert_eq!(lines[0], "SITE EVERYTHING SEARCH abc  def");
        assert!(lines.contains(&"SITE EVERYTHING CASE 1".to_owned()));
        assert!(lines.contains(&"SITE EVERYTHING REGEX 0".to_owned()));
        assert!(lines.contains(&"SITE EVERYTHING SORT DATE_MODIFIED_DESCENDING".to_owned()));
        assert!(lines.contains(&"SITE EVERYTHING OFFSET 100".to_owned()));
        assert!(lines.contains(&"SITE EVERYTHING COUNT 50".to_owned()));
        assert!(lines.contains(&"SITE EVERYTHING PATH_COLUMN 1".to_owned()));
        let bad = Request {
            search: "a\r\nDELE x".into(),
            ..Request::default()
        };
        assert!(bad.commands().is_err());
    }

    #[test]
    fn results_parse() {
        let reply = [
            "200-Query results",
            " RESULT_COUNT 3",
            " PATH C:\\Users\\me",
            " ATTRIBUTES 32",
            " SIZE 1234",
            " DATE_MODIFIED 133444736000000000",
            " FILE  leading space.txt",
            " PATH ",
            " SIZE 18446744073709551615",
            " DATE_MODIFIED 18446744073709551615",
            " FOLDER C:",
            " FILE_LIST_FILENAME ",
            " FILE bare",
            "200 End.",
        ];
        let r = parse_results(&reply).unwrap();
        assert_eq!(r.total, 3);
        assert_eq!(r.items.len(), 3);
        let a = &r.items[0];
        assert_eq!(a.name, " leading space.txt");
        assert_eq!(a.path.as_deref(), Some("C:\\Users\\me"));
        assert_eq!(
            (a.size, a.attributes, a.is_dir),
            (Some(1234), Some(32), false)
        );
        assert_eq!(
            from_filetime(a.modified.unwrap()),
            UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
        assert_eq!(ftp_path(a), "/C:/Users/me/ leading space.txt");
        let b = &r.items[1];
        assert!(b.is_dir && b.size.is_none() && b.modified.is_none());
        assert_eq!(ftp_path(b), "/C:");
        assert_eq!(r.items[2].path, None);
        assert_eq!(ftp_path(&r.items[2]), "/bare");
        for bad in [
            &[" RESULT_COUNT x"][..],
            &[" SIZE -1"],
            &[" ATTRIBUTES 99999999999"],
            &[" FILE "],
        ] {
            assert!(parse_results(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            from_filetime(116_444_735_990_000_000),
            UNIX_EPOCH - Duration::from_secs(1)
        );
    }

    #[test]
    fn replies_read_to_their_end() {
        let text = b"200-Query results\r\n RESULT_COUNT 1\r\n FILE x\n201 not the code\r\n200 End.\r\n220 next\r\n";
        let mut r = BufReader::new(&text[..]);
        let reply = read_reply(&mut r).unwrap();
        assert_eq!(reply.code, 200);
        assert_eq!(reply.lines.len(), 5, "{reply:?}");
        assert_eq!(read_reply(&mut r).unwrap().code, 220);
        assert!(read_reply(&mut BufReader::new(&b"hello\r\n"[..])).is_err());
        assert!(read_reply(&mut BufReader::new(&b"200-open\r\n"[..])).is_err());
        let long = vec![b'a'; MAX_LINE as usize + 10];
        assert!(read_reply(&mut BufReader::new(&long[..])).is_err());
    }
}
