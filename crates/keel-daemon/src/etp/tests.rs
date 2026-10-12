//! The ETP server on loopback with temp folders: Keel's ETP client against it, sign-in
//! failures, downloads off and on, browsing, path escapes, read-only commands, TLS.

use super::*;
use keel_search::etp::{self as client, EtpSearcher, Request, SortKey};
use std::io::BufRead;

/// A test CA and a certificate it issued for `localhost` and 127.0.0.1 (P-256, valid
/// until 2126; the pair keel-vfs's FTP tests use). Generated for tests only; the key
/// protects nothing.
const CA_PEM: &str = "-----BEGIN CERTIFICATE-----
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

const USER: &str = "tester";
const PASSWORD: &str = "correct horse battery";

/// A search over everything under one folder (the roots and what is beside them), as an
/// index of the whole disk would answer.
struct Walk(PathBuf);

impl Searcher for Walk {
    fn query(&self, q: &Query) -> Result<Vec<Hit>> {
        Ok(keel_search::walk(&self.0, q))
    }
    fn available(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "Walk"
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.addr);
    }
}

/// `media/` is served (as `/media`); `secret/` beside it is not.
fn folders() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let put = |rel: &str, bytes: &[u8]| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    };
    put("media/Music/notes one.mp3", b"music");
    put("media/Docs/notes.txt", b"hello notes");
    let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    put("media/Docs/big.bin", &big);
    put("secret/notes-secret.txt", b"top secret");
    dir
}

fn start(downloads: bool, tweak: impl FnOnce(&mut Config, &Path)) -> Fixture {
    let dir = folders();
    let mut cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        roots: vec![(String::new(), dir.path().join("media"))],
        allow_downloads: downloads,
        log: true,
        ..Config::default()
    };
    tweak(&mut cfg, dir.path());
    let stop = Arc::new(AtomicBool::new(false));
    let base = dir.path().to_owned();
    let flag = stop.clone();
    let addr = serve(
        cfg,
        (USER.into(), PASSWORD.into()),
        Arc::new(move || Arc::new(Walk(base.clone())) as Arc<dyn Searcher>),
        Arc::new(move || downloads),
        Arc::new(move || flag.load(Ordering::Acquire)),
    )
    .unwrap();
    Fixture { dir, addr, stop }
}

fn target(f: &Fixture) -> client::Target {
    let mut t = client::Target::parse(&format!("{USER}:{PASSWORD}@{}", f.addr)).unwrap();
    t.timeout = Duration::from_secs(10);
    t
}

/// A bare control connection.
struct Raw {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

impl Raw {
    fn connect(addr: SocketAddr) -> (Raw, String) {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut raw = Raw {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
        };
        let hello = raw.read();
        (raw, hello)
    }

    /// One reply, its lines joined with `\n`; empty at the end of the connection.
    fn read(&mut self) -> String {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if self.r.read_line(&mut line).unwrap_or(0) == 0 {
                return lines.join("\n");
            }
            let line = line.trim_end_matches(['\r', '\n']).to_owned();
            let done = lines.is_empty() && line.as_bytes().get(3) == Some(&b' ')
                || (!lines.is_empty() && line.get(..4) == lines[0].get(..3).map(|c: &str| format!("{c} ")).as_deref());
            lines.push(line);
            if done {
                return lines.join("\n");
            }
        }
    }

    fn cmd(&mut self, line: &str) -> String {
        self.w.write_all(format!("{line}\r\n").as_bytes()).unwrap();
        self.read()
    }

    fn login(&mut self) {
        assert!(self.cmd(&format!("USER {USER}")).starts_with("331"));
        assert!(self.cmd(&format!("PASS {PASSWORD}")).starts_with("230"));
    }

    /// `EPSV`, then `line` on its data connection: (the first reply, the data, the last).
    fn data(&mut self, line: &str) -> (String, Vec<u8>, String) {
        let r = self.cmd("EPSV");
        assert!(r.starts_with("229"), "{r}");
        let port: u16 = r.split("|||").nth(1).unwrap().trim_end_matches(['|', ')']).parse().unwrap();
        let first = self.cmd(line);
        if !first.starts_with("150") {
            return (first, Vec::new(), String::new());
        }
        let mut data = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut bytes = Vec::new();
        data.read_to_end(&mut bytes).unwrap();
        (first, bytes, self.read())
    }
}

#[test]
fn keels_client_searches_only_the_roots() {
    let f = start(false, |_, _| {});
    let etp = EtpSearcher::new(target(&f));
    assert!(etp.probe(), "signs in");
    let hits = etp
        .query(&Query {
            text: "notes".into(),
            ..Query::default()
        })
        .unwrap();
    let mut paths: Vec<String> = hits.iter().map(|h| h.path.path.clone()).collect();
    paths.sort();
    assert_eq!(paths, ["/media/Docs/notes.txt", "/media/Music/notes one.mp3"]);
    let authority = f.addr.to_string();
    assert!(hits.iter().all(|h| h.path.scheme == "ftp" && h.path.authority == authority));
    let txt = hits.iter().find(|h| h.path.name() == "notes.txt").unwrap();
    assert_eq!(txt.size, 11);
    assert!(txt.modified.is_some());
    // The whole request: paging, sorting, the total; secret/ never shows.
    let all = etp
        .search(&Request {
            search: String::new(),
            sort: SortKey::Size,
            descending: true,
            count: 2,
            ..Request::default()
        })
        .unwrap();
    // media, Docs, Music, notes.txt, big.bin, notes one.mp3.
    assert_eq!(all.total, 6, "{all:?}");
    assert_eq!(all.items.len(), 2);
    assert_eq!(all.items[0].name, "big.bin");
    assert_eq!(all.items[0].path.as_deref(), Some("media\\Docs"));
    assert_eq!(all.items[0].size, Some(100_000));
    assert_eq!(all.items[0].attributes.map(|a| a & 16), Some(0));
    let next = etp
        .search(&Request {
            sort: SortKey::Size,
            descending: true,
            offset: 2,
            count: 100,
            ..Request::default()
        })
        .unwrap();
    assert_eq!(next.items.len(), 4);
    let top = next.items.iter().find(|i| i.name == "media").unwrap();
    assert!(top.is_dir && top.path.as_deref() == Some("") && top.size.is_none());
    assert!(next.items.iter().all(|i| !i.name.contains("secret")));
    // Folders only, as the folder list asks.
    let folders = etp
        .query(&Query {
            folders_only: true,
            ..Query::default()
        })
        .unwrap();
    assert_eq!(folders.len(), 3);
    // A bad regular expression is an error reply, not a dead connection.
    let bad = etp.search(&Request {
        search: "(".into(),
        regex: true,
        ..Request::default()
    });
    assert!(bad.is_err());
    assert!(etp.search(&Request::default()).is_ok());
}

#[test]
fn everything_commands_on_the_wire() {
    let f = start(false, |c, _| c.welcome = "Line one\nLine two".into());
    let (mut c, hello) = Raw::connect(f.addr);
    assert_eq!(hello, "220-Line one\n220 Line two");
    c.login();
    assert_eq!(c.cmd("SITE EVERYTHING SEARCH notes"), "200 Search set to (notes).");
    assert_eq!(c.cmd("SITE EVERYTHING PATH_COLUMN 1"), "200 Path column set to (1).");
    assert_eq!(c.cmd("site everything sort name_ascending"), "200 Sort set.");
    assert!(c.cmd("SITE EVERYTHING SORT SIDEWAYS").starts_with("500"));
    assert!(c.cmd("SITE EVERYTHING FROB 1").starts_with("500"));
    let r = c.cmd("SITE EVERYTHING QUERY");
    assert_eq!(
        r,
        "200-Query results\n RESULT_COUNT 2\n PATH media\\Music\n FILE notes one.mp3\n PATH media\\Docs\n FILE notes.txt\n200 End."
    );
    // The filter search narrows the results; Everything's bare form works too.
    assert!(c.cmd("EVERYTHING FILTER_SEARCH .mp3").starts_with("200"));
    let r = c.cmd("EVERYTHING QUERY");
    assert!(r.contains("RESULT_COUNT 1") && r.contains("FILE notes one.mp3"), "{r}");
    // Count 0: only the total.
    c.cmd("SITE EVERYTHING FILTER_SEARCH ");
    c.cmd("SITE EVERYTHING COUNT 0");
    assert_eq!(
        c.cmd("SITE EVERYTHING QUERY"),
        "200-Query results\n RESULT_COUNT 2\n200 End."
    );
    let feat = c.cmd("FEAT");
    assert!(feat.contains(" EVERYTHING") && !feat.contains("AUTH TLS"), "{feat}");
    assert!(c.cmd("SYST").contains("UNIX"));
    assert!(c.cmd("HELP").contains("EVERYTHING"));
    assert!(c.cmd("SITE CHMOD 777 x").starts_with("500"));
    assert_eq!(c.cmd("QUIT"), "221 Goodbye.");
}

#[test]
fn sign_in_failures() {
    let f = start(false, |_, _| {});
    // Nothing before signing in.
    let (mut c, _) = Raw::connect(f.addr);
    assert!(c.cmd("SITE EVERYTHING QUERY").starts_with("530"));
    assert!(c.cmd("LIST").starts_with("530"));
    assert!(c.cmd("PASS x").starts_with("503"));
    assert!(c.cmd("FEAT").starts_with("211"), "FEAT works before");
    // A wrong user or password, three times: the connection ends.
    assert!(c.cmd(&format!("USER {USER}")).starts_with("331"));
    assert!(c.cmd("PASS wrong").starts_with("530"));
    assert!(c.cmd("USER nobody").starts_with("331"), "an unknown user looks the same");
    assert!(c.cmd(&format!("PASS {PASSWORD}")).starts_with("530"));
    c.cmd(&format!("USER {USER}"));
    assert!(c.cmd("PASS wrong again").starts_with("421"));
    assert_eq!(c.read(), "", "closed");
    // The client says why.
    let mut t = target(&f);
    t.password = "wrong".into();
    let e = EtpSearcher::new(t).search(&Request::default()).unwrap_err();
    assert!(format!("{e:#}").contains("refused the sign-in"), "{e:#}");
    // A server without Everything's extension is not an ETP server: covered by FEAT.
    // TLS asked of a server without it: refused, never plain.
    let mut t = target(&f);
    t.tls = true;
    let e = EtpSearcher::new(t).search(&Request::default()).unwrap_err();
    assert!(format!("{e:#}").contains("never falls back"), "{e:#}");
}

#[test]
fn downloads_off_and_on() {
    let f = start(false, |_, _| {});
    let (mut c, _) = Raw::connect(f.addr);
    c.login();
    let (first, data, _) = c.data("RETR /media/Docs/notes.txt");
    assert_eq!(first, "550 Downloads are turned off on this server.");
    assert!(data.is_empty());
    // Browsing still works.
    let (first, data, last) = c.data("LIST -la /media/Docs");
    assert!(first.starts_with("150") && last.starts_with("226"), "{first} {last}");
    let text = String::from_utf8(data).unwrap();
    assert!(text.contains(" notes.txt\r\n") && text.contains(" big.bin\r\n"), "{text}");
    assert!(text.starts_with("-rw-r--r-- 1 owner group"), "{text}");
    drop(c);

    let f = start(true, |_, _| {});
    let (mut c, _) = Raw::connect(f.addr);
    c.login();
    assert!(c.cmd("TYPE I").starts_with("200"));
    let (_, data, last) = c.data("RETR /media/Docs/notes.txt");
    assert_eq!((data.as_slice(), last.as_str()), (&b"hello notes"[..], "226 Transfer complete."));
    let (_, data, _) = c.data("RETR /media/Docs/big.bin");
    assert_eq!(data, std::fs::read(f.dir.path().join("media/Docs/big.bin")).unwrap());
    assert!(c.cmd("REST 99990").starts_with("350"));
    let (_, data, _) = c.data("RETR /media/Docs/big.bin");
    assert_eq!(data.len(), 10);
    // Outside the roots: not there, however it is named.
    for path in [
        "/media/../secret/notes-secret.txt",
        "../../secret/notes-secret.txt",
        "/secret/notes-secret.txt",
        "/media/Docs/../../secret/notes-secret.txt",
    ] {
        let (first, data, _) = c.data(&format!("RETR {path}"));
        assert!(first.starts_with("550") && data.is_empty(), "{path}: {first}");
    }
    let outside = f.dir.path().join("secret/notes-secret.txt");
    let (first, _, _) = c.data(&format!("RETR {}", outside.display()));
    assert!(first.starts_with("550"), "{first}");
    // A data connection is needed first.
    assert!(c.cmd("RETR /media/Docs/notes.txt").starts_with("425"));
}

#[test]
fn browsing_and_read_only() {
    let f = start(false, |_, _| {});
    let (mut c, _) = Raw::connect(f.addr);
    c.login();
    assert_eq!(c.cmd("PWD"), "257 \"/\" is current directory.");
    let (_, data, _) = c.data("NLST");
    assert_eq!(data, b"media\r\n");
    assert!(c.cmd("CWD /MEDIA/Music").starts_with("250"));
    assert_eq!(c.cmd("PWD"), "257 \"/MEDIA/Music\" is current directory.");
    let (_, data, _) = c.data("MLSD");
    let text = String::from_utf8(data).unwrap();
    assert!(text.starts_with("type=file;modify=") && text.ends_with(";size=5; notes one.mp3\r\n"), "{text}");
    assert!(c.cmd("CDUP").starts_with("250"));
    assert!(c.cmd("CWD Docs").starts_with("250"));
    assert_eq!(c.cmd("SIZE notes.txt"), "213 11");
    assert!(c.cmd("MDTM notes.txt").starts_with("213 2"));
    assert!(c.cmd("SIZE /media/Docs").starts_with("550"));
    let r = c.cmd("MLST notes.txt");
    assert!(r.starts_with("250-Listing /media/Docs/notes.txt") || r.starts_with("250-Listing /MEDIA/Docs/notes.txt"), "{r}");
    assert!(c.cmd("CWD ../../..").starts_with("250"));
    assert_eq!(c.cmd("PWD"), "257 \"/\" is current directory.");
    assert!(c.cmd("CWD /secret").starts_with("550"));
    assert!(c.cmd("CWD /media/Docs/notes.txt").starts_with("550"));
    for write in ["STOR x", "DELE /media/Docs/notes.txt", "MKD /media/new", "RNFR /media/Docs", "APPE x"] {
        assert!(c.cmd(write).starts_with("550"), "{write}");
    }
    assert!(c.cmd("PORT 127,0,0,1,4,1").starts_with("502"));
    assert!(c.cmd("AUTH TLS").starts_with("502"), "no certificate configured");
    assert!(c.cmd("NOOP").starts_with("200"));
    assert!(c.cmd("XYZZY").starts_with("500"));
    assert!(std::fs::read(f.dir.path().join("media/Docs/notes.txt")).is_ok());
}

/// A rustls client trusting the test CA.
fn tls_client() -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(CA_PEM.as_bytes()).unwrap())
        .unwrap();
    let provider = Arc::new(rustls_graviola::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn tls_files(dir: &Path) -> (PathBuf, PathBuf) {
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::write(&cert, format!("{LEAF_PEM}{CA_PEM}")).unwrap();
    std::fs::write(&key, LEAF_KEY).unwrap();
    (cert, key)
}

#[test]
fn ftps_required_when_configured() {
    let f = start(true, |c, dir| {
        c.tls = Some(tls_files(dir));
        c.require_tls = true;
    });
    // Plain sign-in is refused.
    let (mut c, _) = Raw::connect(f.addr);
    assert!(c.cmd("FEAT").contains("AUTH TLS"));
    assert!(c.cmd(&format!("USER {USER}")).starts_with("530"));
    assert!(c.cmd("PROT P").starts_with("503"));
    let e = EtpSearcher::new(target(&f)).search(&Request::default());
    assert!(e.is_err(), "plain FTP cannot sign in");
    // Keel's client over TLS, the certificate pinned by its fingerprint.
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let leaf = CertificateDer::from_pem_slice(LEAF_PEM.as_bytes()).unwrap();
    let mut t = target(&f);
    t.tls = true;
    t.pin = Some(keel_vfs::ftp::tls::fingerprint(&leaf));
    let etp = EtpSearcher::new(t.clone());
    let found = etp
        .search(&Request {
            search: "notes".into(),
            ..Request::default()
        })
        .unwrap();
    assert_eq!(found.total, 2);
    // Not trusted and not pinned: refused, with the fingerprint to check.
    t.pin = Some("00:11".into());
    let e = EtpSearcher::new(t).search(&Request::default()).unwrap_err();
    assert!(format!("{e:#}").contains("not trusted"), "{e:#}");

    // A transfer over TLS (PROT P), the data connection only so.
    let tcp = TcpStream::connect(f.addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut r = BufReader::new(tcp.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let mut w = tcp;
    w.write_all(b"AUTH TLS\r\n").unwrap();
    line.clear();
    r.read_line(&mut line).unwrap();
    assert!(line.starts_with("234"), "{line}");
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let config = tls_client();
    let conn = rustls::ClientConnection::new(config.clone(), name.clone()).unwrap();
    let mut s = BufReader::new(rustls::StreamOwned::new(conn, w));
    type Tls = BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;
    fn cmd(s: &mut Tls, l: &str) -> String {
        s.get_mut().write_all(format!("{l}\r\n").as_bytes()).unwrap();
        s.get_mut().flush().unwrap();
        let mut line = String::new();
        s.read_line(&mut line).unwrap();
        line
    }
    fn epsv(s: &mut Tls) -> u16 {
        let r = cmd(s, "EPSV");
        let port = r.split("|||").nth(1).unwrap();
        port.trim_end().trim_end_matches(['|', ')']).parse().unwrap()
    }
    assert!(cmd(&mut s, &format!("USER {USER}")).starts_with("331"));
    assert!(cmd(&mut s, &format!("PASS {PASSWORD}")).starts_with("230"));
    assert!(cmd(&mut s, "PBSZ 0").starts_with("200"));
    assert!(cmd(&mut s, "PROT C").starts_with("534"));
    epsv(&mut s);
    let r = cmd(&mut s, "RETR /media/Docs/notes.txt");
    assert!(r.starts_with("521"), "PROT P first: {r}");
    assert!(cmd(&mut s, "PROT P").starts_with("200"));
    let port = epsv(&mut s);
    assert!(cmd(&mut s, "RETR /media/Docs/notes.txt").starts_with("150"));
    let data = TcpStream::connect(("127.0.0.1", port)).unwrap();
    data.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let conn = rustls::ClientConnection::new(config, name).unwrap();
    let mut d = rustls::StreamOwned::new(conn, data);
    let mut bytes = Vec::new();
    let _ = d.read_to_end(&mut bytes);
    assert_eq!(bytes, b"hello notes");
    let mut line = String::new();
    s.read_line(&mut line).unwrap();
    assert!(line.starts_with("226"), "{line}");
}

#[test]
fn settings_and_refusals() {
    let base = Path::new(if cfg!(windows) { r"C:\profile" } else { "/profile" });
    let table: toml::Table = r#"
[etp]
enabled = true
bind = "127.0.0.1:2121"
welcome = "hi"
allow_downloads = true
log = true
tls_cert = "cert.pem"
tls_key = "key.pem"

[[etp.roots]]
path = "/srv/media"
name = "Media"

[[etp.roots]]
path = "/srv/docs"
"#
    .parse()
    .unwrap();
    let c = Config::from_table(&table, base).unwrap();
    assert!(c.enabled && c.allow_downloads && c.log && c.require_tls && c.encrypted());
    assert_eq!(c.bind, "127.0.0.1:2121".parse().unwrap());
    assert_eq!(c.tls, Some((base.join("cert.pem"), base.join("key.pem"))));
    assert_eq!(
        c.roots,
        [
            ("Media".to_owned(), PathBuf::from("/srv/media")),
            (String::new(), PathBuf::from("/srv/docs"))
        ]
    );
    // Off, loopback, no TLS by default.
    let d = Config::from_table(&toml::Table::new(), base).unwrap();
    assert!(!d.enabled && !d.allow_downloads && !d.encrypted() && d.bind.ip().is_loopback());
    for bad in [
        "[etp]\nbind = \"nowhere\"",
        "[etp]\nenabled = \"yes\"",
        "[etp]\ntls_cert = \"c.pem\"",
        "[etp]\nrequire_tls = true",
        "[[etp.roots]]\nname = \"x\"",
        "etp = 1",
    ] {
        let t: toml::Table = bad.parse().unwrap();
        assert!(Config::from_table(&t, base).is_err(), "{bad}");
    }
    // A network address without TLS needs the warning accepted; no roots or no login: no
    // server.
    let dir = folders();
    let run = |cfg: Config, login: (&str, &str)| {
        serve(
            cfg,
            (login.0.into(), login.1.into()),
            Arc::new(|| Arc::new(Walk(PathBuf::new())) as Arc<dyn Searcher>),
            Arc::new(|| false),
            Arc::new(|| true),
        )
    };
    let remote = Config {
        bind: "0.0.0.0:0".parse().unwrap(),
        roots: vec![(String::new(), dir.path().join("media"))],
        ..Config::default()
    };
    let e = run(remote.clone(), (USER, PASSWORD)).unwrap_err();
    assert!(format!("{e:#}").contains("allow_unencrypted"), "{e:#}");
    let e = run(
        Config {
            roots: Vec::new(),
            bind: "127.0.0.1:0".parse().unwrap(),
            ..remote.clone()
        },
        (USER, PASSWORD),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("[[etp.roots]]"), "{e:#}");
    let e = run(
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..remote
        },
        ("", ""),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("--set-etp-login"), "{e:#}");
    // The login lives in the keychain, per profile, with the HTTP login's rules.
    let store = keel_vfs::cloud::MemoryStore::default();
    assert!(login(&store, "p").unwrap().is_none());
    assert!(set_login(&store, "p", "a:b", PASSWORD).is_err());
    assert!(set_login(&store, "p", USER, "short").is_err());
    set_login(&store, "p", USER, PASSWORD).unwrap();
    assert_eq!(
        login(&store, "p").unwrap(),
        Some((USER.to_owned(), PASSWORD.to_owned()))
    );
    assert!(
        crate::everything::login(&store, "p").unwrap().is_none(),
        "not the HTTP login"
    );
    clear_login(&store, "p").unwrap();
    assert!(login(&store, "p").unwrap().is_none());
}
