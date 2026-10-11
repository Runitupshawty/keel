//! The FTP provider against the in-process test server (`test_server`), and an ignored
//! live test against a real server (`KEEL_FTP_TEST`, `KEEL_FTP_TEST_PASS`).

use super::test_server::{self, put, Options, Server};
use super::*;
use crate::ops::{transfer, transfer_resumable, Conflict, Journal};

/// Tests that read or write certificate pins hold this.
static PINS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn memory_secrets() {
    std::env::set_var("KEEL_NET_SECRET", "memory");
}

fn host(id: &str, port: u16, protocol: RemoteProtocol, user: &str) -> RemoteHost {
    RemoteHost {
        id: id.into(),
        label: id.into(),
        host: "127.0.0.1".into(),
        port,
        user: user.into(),
        auth: crate::RemoteAuth::PasswordInKeyring,
        home: None,
        bookmarks: Vec::new(),
        use_ssh_config: false,
        protocol,
    }
}

/// A provider signed in to `server` as `tester` (password in the in-memory store).
fn provider_for(
    server: &Server,
    id: &str,
) -> (FtpProvider, crossbeam_channel::Receiver<RemoteEvent>) {
    memory_secrets();
    crate::sftp::auth::store_secret(id, "password", "secret pw").unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let p = FtpProvider::new(host(id, server.port, RemoteProtocol::Ftp, "tester"), tx);
    (p, rx)
}

fn options(mlsd: bool) -> Options {
    Options {
        user: "tester".into(),
        password: "secret pw".into(),
        mlsd,
        ..Options::default()
    }
}

fn at(id: &str, path: &str) -> VPath {
    VPath {
        scheme: "ftp".into(),
        authority: id.into(),
        path: path.into(),
    }
}

fn read_all(p: &FtpProvider, path: &VPath) -> Vec<u8> {
    let mut out = Vec::new();
    p.read(path).unwrap().read_to_end(&mut out).unwrap();
    out
}

fn names(p: &FtpProvider, dir: &VPath) -> Vec<String> {
    let mut n: Vec<String> = p.list(dir).unwrap().into_iter().map(|e| e.name).collect();
    n.sort();
    n
}

/// Everything a provider does, against a server with `MLSD` and one with `LIST` only.
#[test]
fn browse_read_write_rename_and_delete() {
    for mlsd in [true, false] {
        let server = Server::start(options(mlsd));
        let id = if mlsd { "mlsd-host" } else { "list-host" };
        let (p, events) = provider_for(&server, id);
        put(&server.root, "docs/a file.txt", b"hello world");
        put(&server.root, "docs/Ordner \u{fc}/inner.bin", &[7; 3000]);
        put(&server.root, "docs/.hidden", b"h");
        let docs = at(id, "/docs");

        assert_eq!(
            names(&p, &docs),
            [".hidden", "Ordner \u{fc}", "a file.txt"],
            "mlsd {mlsd}"
        );
        let file = p.stat(&docs.join("a file.txt")).unwrap();
        assert_eq!((file.kind.clone(), file.size), (Kind::File, 11));
        assert!(file.modified.is_some());
        assert_eq!(p.stat(&docs.join("Ordner \u{fc}")).unwrap().kind, Kind::Dir);
        assert_eq!(p.stat(&at(id, "/")).unwrap().kind, Kind::Dir);
        let missing = p.stat(&docs.join("nope")).unwrap_err();
        assert!(
            missing
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound),
            "{missing:#}"
        );

        assert_eq!(read_all(&p, &docs.join("a file.txt")), b"hello world");
        let mut part = String::new();
        p.read_range(&docs.join("a file.txt"), 6, 3)
            .unwrap()
            .unwrap()
            .read_to_string(&mut part)
            .unwrap();
        assert_eq!(part, "wor");

        // Uploads land under their name only on flush; nothing staged stays behind.
        let up = docs.join("up.txt");
        let mut w = p.write(&up).unwrap();
        w.write_all(b"uploaded").unwrap();
        assert!(p.stat(&up).is_err(), "not placed before flush");
        w.flush().unwrap();
        drop(w);
        assert_eq!(read_all(&p, &up), b"uploaded");
        let mut w = p.write(&up).unwrap();
        w.write_all(b"replaced").unwrap();
        w.flush().unwrap();
        drop(w);
        assert_eq!(read_all(&p, &up), b"replaced");
        let exclusive = p.create_new(&up).map(|_| ()).unwrap_err();
        assert!(
            exclusive
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists),
            "{exclusive:#}"
        );
        // Dropped without flush: discarded, the staging file removed.
        let mut w = p.create_new(&docs.join("dropped.txt")).unwrap();
        w.write_all(b"never").unwrap();
        drop(w);
        let left: Vec<_> = names(&p, &docs)
            .into_iter()
            .filter(|n| n.contains("keel-partial") || n == "dropped.txt")
            .collect();
        assert!(left.is_empty(), "{left:?}");

        p.mkdir(&docs.join("new dir")).unwrap();
        assert_eq!(p.stat(&docs.join("new dir")).unwrap().kind, Kind::Dir);
        let err = p.rename(&up, &docs.join("a file.txt")).unwrap_err();
        assert!(format!("{err:#}").contains("exists"), "{err:#}");
        p.rename(&up, &docs.join("new dir/moved.txt")).unwrap();
        p.rename_replace(&docs.join("new dir/moved.txt"), &docs.join("a file.txt"))
            .unwrap();
        assert_eq!(read_all(&p, &docs.join("a file.txt")), b"replaced");
        p.remove_empty_dir(&docs.join("new dir")).unwrap();
        p.remove(&docs.join("Ordner \u{fc}")).unwrap();
        assert_eq!(names(&p, &docs), [".hidden", "a file.txt"]);
        assert_eq!(p.remove_kind(), crate::RemoveKind::Permanent);

        let copy = p.local_copy(&docs.join("a file.txt")).unwrap();
        assert_eq!(std::fs::read(copy).unwrap(), b"replaced");
        assert_eq!(p.status(), ConnStatus::Connected);
        let statuses: Vec<_> = events
            .try_iter()
            .filter_map(|e| match e {
                RemoteEvent::Status { status, .. } => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses[..2],
            [ConnStatus::Connecting, ConnStatus::Connected]
        );
        p.disconnect();
        assert_eq!(p.status(), ConnStatus::Disconnected);
    }
}

#[test]
fn paths_that_could_inject_commands_are_refused() {
    let server = Server::start(options(false));
    let (p, _) = provider_for(&server, "inject");
    for bad in ["/a\r\nDELE b", "/a\nb", "/a\0"] {
        assert!(p.stat(&at("inject", bad)).is_err(), "{bad:?}");
    }
    assert!(p.stat(&at("other", "/a")).is_err(), "another host's path");
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
}

#[test]
fn anonymous_and_wrong_password() {
    let server = Server::start(options(false));
    put(&server.root, "pub/readme", b"anon");
    let (tx, _) = crossbeam_channel::unbounded();
    let anon = FtpProvider::new(
        host("anon", server.port, RemoteProtocol::Ftp, "anonymous"),
        tx,
    );
    assert_eq!(read_all(&anon, &at("anon", "/pub/readme")), b"anon");

    memory_secrets();
    crate::sftp::auth::store_secret("wrong-pw", "password", "nope").unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let wrong = FtpProvider::new(
        host("wrong-pw", server.port, RemoteProtocol::Ftp, "tester"),
        tx,
    );
    let err = wrong.list(&at("wrong-pw", "/")).unwrap_err();
    assert!(format!("{err:#}").contains("530"), "{err:#}");
    assert_eq!(wrong.status(), ConnStatus::Failed);
    assert!(rx.try_iter().any(|e| matches!(
        e,
        RemoteEvent::Status {
            status: ConnStatus::Failed,
            ..
        }
    )));
}

/// A download whose data connection closes early fails (no hang, no silent short file);
/// one whose server never sends fails at the timeout. The provider works on afterwards.
#[test]
fn early_close_and_stalled_data_connections_fail() {
    let server = Server::start(Options {
        cut_retr: Some(1000),
        ..options(false)
    });
    put(&server.root, "big.bin", &[1; 100_000]);
    let (p, _) = provider_for(&server, "cut");
    let start = Instant::now();
    let mut out = Vec::new();
    let err = p
        .read(&at("cut", "/big.bin"))
        .unwrap()
        .read_to_end(&mut out)
        .unwrap_err();
    assert!(err.to_string().contains("426"), "{err}");
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(p.local_copy(&at("cut", "/big.bin")).is_err());
    assert_eq!(names(&p, &at("cut", "/")), ["big.bin"]);

    let server = Server::start(Options {
        stall_retr: true,
        ..options(false)
    });
    put(&server.root, "slow.bin", &[1; 10]);
    let (p, _) = provider_for(&server, "stall");
    p.set_timeout(Duration::from_millis(500));
    let start = Instant::now();
    let mut out = Vec::new();
    assert!(p
        .read(&at("stall", "/slow.bin"))
        .unwrap()
        .read_to_end(&mut out)
        .is_err());
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
}

/// `write_at` continues a staging file: with `APPE` at its end, with `REST` + `STOR`
/// before it; a transfer cancelled midway leaves no file and no staging file; a journaled
/// one stopped midway continues where it stopped.
#[test]
fn resume_and_cancel() {
    let server = Server::start(options(true));
    let (p, _) = provider_for(&server, "resume");
    let p = Arc::new(p);
    let file = at("resume", "/part.bin");
    let mut w = p.write_at(&file, 0).unwrap().unwrap();
    w.write_all(b"hello").unwrap();
    w.flush().unwrap();
    w.write_all(b" wor").unwrap();
    drop(w);
    let mut w = p.write_at(&file, 9).unwrap().unwrap();
    w.write_all(b"ld").unwrap();
    drop(w);
    assert_eq!(read_all(&p, &file), b"hello world");
    let mut w = p.write_at(&file, 6).unwrap().unwrap();
    w.write_all(b"WORLD").unwrap();
    drop(w);
    assert_eq!(read_all(&p, &file), b"hello WORLD");
    assert!(p.write_at(&file, 100).is_err(), "shorter than the offset");
    p.remove(&file).unwrap();

    let router = crate::Router::new();
    router.register_remote_provider("resume".into(), p.clone());
    let local = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..10 << 20).map(|i| (i % 251) as u8).collect();
    std::fs::write(local.path().join("big.bin"), &big).unwrap();
    let src = [VPath::local(local.path().join("big.bin"))];
    let dst = at("resume", "/");

    // Cancelled without a journal: nothing stays on the server.
    let cancel = AtomicBool::new(false);
    let stop = |pr: Progress| {
        if pr.done_bytes >= 3 << 20 {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    assert!(transfer(&src, &dst, false, Conflict::Skip, &stop, &cancel, &router).is_err());
    assert!(names(&p, &dst).is_empty(), "{:?}", names(&p, &dst));

    // Journaled: stopped at 6 MiB, then continued.
    cancel.store(false, Ordering::SeqCst);
    let recorded = std::cell::RefCell::new(None);
    let mut journal = Journal::new(Default::default(), None, |_, u| {
        *recorded.borrow_mut() = u.cloned();
        Ok(())
    });
    let stop = |pr: Progress| {
        if pr.done_bytes >= 6 << 20 {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let result = transfer_resumable(
        &src,
        &dst,
        false,
        Conflict::Skip,
        &stop,
        &cancel,
        &router,
        &mut journal,
    );
    assert!(result.is_err(), "stopped");
    drop(journal);
    let unfinished = recorded
        .borrow()
        .clone()
        .expect("the partial copy is recorded");
    assert!(unfinished.bytes >= 4 << 20, "{}", unfinished.bytes);
    assert!(p.stat(&unfinished.staging).unwrap().size >= unfinished.bytes);
    cancel.store(false, Ordering::SeqCst);
    let mut journal = Journal::new(Default::default(), Some(unfinished), |_, _| Ok(()));
    transfer_resumable(
        &src,
        &dst,
        false,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
        &mut journal,
    )
    .unwrap();
    assert!(
        journal.notes[0].starts_with("continued"),
        "{:?}",
        journal.notes
    );
    assert!(read_all(&p, &dst.join("big.bin")) == big);
    assert_eq!(names(&p, &dst), ["big.bin"]);

    // And back down, then a same-host move is a server-side rename.
    let down = tempfile::tempdir().unwrap();
    let local_dst = VPath::local(down.path());
    transfer(
        &[dst.join("big.bin")],
        &local_dst,
        false,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
        &router,
    )
    .unwrap();
    assert!(std::fs::read(down.path().join("big.bin")).unwrap() == big);
    p.mkdir(&at("resume", "/moved")).unwrap();
    transfer(
        &[dst.join("big.bin")],
        &at("resume", "/moved"),
        true,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
        &router,
    )
    .unwrap();
    assert_eq!(names(&p, &at("resume", "/moved")), ["big.bin"]);
}

fn ftps(
    server: &Server,
    id: &str,
    roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    listener: bool,
) -> (FtpProvider, crossbeam_channel::Receiver<RemoteEvent>) {
    memory_secrets();
    crate::sftp::auth::store_secret(id, "password", "secret pw").unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    // 127.0.0.1, which the test certificate names: "localhost" would try ::1 first.
    let h = host(id, server.port, RemoteProtocol::Ftps, "tester");
    let p = FtpProvider::with_roots(h, tx, Arc::new(AtomicBool::new(listener)), roots);
    (p, rx)
}

/// FTPS: a verified certificate connects; an unverified one is prompted for, pinned when
/// trusted and refused otherwise; a changed one is refused; a server without TLS is
/// refused (never plain FTP).
#[test]
fn ftps_verifies_prompts_pins_and_never_falls_back() {
    let _pins = PINS.lock();
    let dir = tempfile::tempdir().unwrap();
    *tls::TEST_FILE.lock() = Some(dir.path().join("ftps_known_hosts"));
    let server = Server::start(Options {
        tls: true,
        ..options(true)
    });
    put(&server.root, "secret.txt", b"over tls");

    // Verified against the test CA: no prompt, data connections protected too.
    let (p, events) = ftps(&server, "tls-ok", vec![test_server::ca()], false);
    assert_eq!(read_all(&p, &at("tls-ok", "/secret.txt")), b"over tls");
    let mut w = p.write(&at("tls-ok", "/up.txt")).unwrap();
    w.write_all(b"up").unwrap();
    w.flush().unwrap();
    drop(w);
    assert_eq!(std::fs::read(server.path("up.txt")).unwrap(), b"up");
    assert!(!events
        .try_iter()
        .any(|e| matches!(e, RemoteEvent::CertificatePrompt { .. })));

    // The OS trust store does not know the test CA. Without a window: refused at once.
    let (p, _) = ftps(&server, "tls-alone", vec![], false);
    let err = p.connect().unwrap_err();
    assert!(format!("{err:#}").contains("no Keel window"), "{err:#}");

    // With a window: prompted; a "no" refuses.
    let (p, events) = ftps(&server, "tls-no", vec![], true);
    let answer = std::thread::spawn(move || {
        for e in events {
            if let RemoteEvent::CertificatePrompt {
                reply, fingerprint, ..
            } = e
            {
                let _ = reply.send(false);
                return fingerprint;
            }
        }
        String::new()
    });
    let err = p.connect().unwrap_err();
    assert!(format!("{err:#}").contains("not trusted"), "{err:#}");
    assert_eq!(
        answer.join().unwrap(),
        tls::fingerprint(&test_server::leaf())
    );
    assert_eq!(tls::pinned("127.0.0.1", server.port).unwrap(), None);

    // A "yes" pins the fingerprint; the next provider connects without a prompt.
    let (p, events) = ftps(&server, "tls-yes", vec![], true);
    let answer = std::thread::spawn(move || {
        for e in events {
            if let RemoteEvent::CertificatePrompt { reply, .. } = e {
                let _ = reply.send(true);
                return;
            }
        }
    });
    assert_eq!(read_all(&p, &at("tls-yes", "/secret.txt")), b"over tls");
    answer.join().unwrap();
    let (p, events) = ftps(&server, "tls-pinned", vec![], false);
    assert_eq!(read_all(&p, &at("tls-pinned", "/secret.txt")), b"over tls");
    assert!(!events
        .try_iter()
        .any(|e| matches!(e, RemoteEvent::CertificatePrompt { .. })));

    // A different certificate than the pinned one: refused, no prompt.
    tls::pin("127.0.0.1", server.port, "00:11").unwrap();
    let (p, _) = ftps(&server, "tls-changed", vec![], true);
    let err = p.connect().unwrap_err();
    assert!(
        format!("{err:#}").contains("changed since it was trusted"),
        "{err:#}"
    );

    // A server without TLS: refused, never plain FTP.
    let plain = Server::start(options(false));
    let (p, _) = ftps(&plain, "tls-missing", vec![test_server::ca()], false);
    let err = p.list(&at("tls-missing", "/")).unwrap_err();
    assert!(format!("{err:#}").contains("never falls back"), "{err:#}");
    *tls::TEST_FILE.lock() = None;
}

#[test]
fn router_and_saved_remotes() {
    let router = crate::Router::new();
    router.register_remote(host("ftp-box", 21, RemoteProtocol::Ftp, ""));
    router.register_remote(host("sftp-box", 22, RemoteProtocol::Sftp, ""));
    let scheme = |p: &str| {
        router
            .provider_for(&VPath::parse(p).unwrap())
            .map(|p| p.scheme())
    };
    assert_eq!(scheme("ftp://ftp-box/a"), Some("ftp"));
    assert_eq!(scheme("sftp://sftp-box/a"), Some("sftp"));
    assert_eq!(scheme("ftp://nobody/a"), None);

    let old = "id = \"nas\"\nlabel = \"NAS\"\nhost = \"example.invalid\"\nport = 22\nuser = \"me\"\nauth = \"Agent\"\nbookmarks = []\n";
    let saved: RemoteHost = toml::from_str(old).unwrap();
    assert_eq!(saved.protocol, RemoteProtocol::Sftp);
    let ftps: RemoteHost = toml::from_str(&format!("{old}protocol = \"ftps\"\n")).unwrap();
    assert_eq!(ftps.protocol, RemoteProtocol::Ftps);
    assert!(toml::to_string(&ftps)
        .unwrap()
        .contains("protocol = \"ftps\""));
    assert_eq!(RemoteProtocol::Ftps.scheme(), "ftp");
}

/// Live, against a real server: `KEEL_FTP_TEST=user@host:port`, `KEEL_FTP_TEST_PASS`
/// (optional `KEEL_FTP_TEST_TLS=1` for FTPS, `KEEL_FTP_TEST_FINGERPRINT` to trust a
/// certificate that does not verify). Lists, uploads, downloads and deletes in a folder it
/// creates and removes.
#[test]
#[ignore = "needs a live FTP server (KEEL_FTP_TEST)"]
fn live_ftp_round_trip() {
    let Ok(target) = std::env::var("KEEL_FTP_TEST") else {
        eprintln!("KEEL_FTP_TEST not set; skipping");
        return;
    };
    let (user, address) = target.split_once('@').expect("user@host:port");
    let (server, port) = address.rsplit_once(':').expect("host:port");
    memory_secrets();
    let password = std::env::var("KEEL_FTP_TEST_PASS").unwrap_or_default();
    crate::sftp::auth::store_secret("live-ftp", "password", &password).unwrap();
    let protocol = match std::env::var("KEEL_FTP_TEST_TLS").as_deref() {
        Ok("1") => RemoteProtocol::Ftps,
        _ => RemoteProtocol::Ftp,
    };
    let mut h = host("live-ftp", port.parse().unwrap(), protocol, user);
    h.host = server.into();
    // FTPS: a certificate that does not verify is trusted only when its fingerprint is
    // `KEEL_FTP_TEST_FINGERPRINT`; the pin goes to a temp file.
    let _pins = PINS.lock();
    let pins = tempfile::tempdir().unwrap();
    *tls::TEST_FILE.lock() = Some(pins.path().join("ftps_known_hosts"));
    let expected = std::env::var("KEEL_FTP_TEST_FINGERPRINT").ok();
    let (tx, events) = crossbeam_channel::unbounded();
    std::thread::spawn(move || {
        for e in events {
            if let RemoteEvent::CertificatePrompt {
                fingerprint,
                problem,
                reply,
                ..
            } = e
            {
                eprintln!("certificate prompt: {problem}; {fingerprint}");
                let _ = reply.send(expected.as_deref() == Some(fingerprint.as_str()));
            }
        }
    });
    let p = FtpProvider::new(h, tx);
    let base = at("live-ftp", &format!("/keel-live-{}", std::process::id()));
    let start = Instant::now();
    p.mkdir(&base).unwrap();
    let body: Vec<u8> = (0..3 << 20).map(|i| (i % 253) as u8).collect();
    let mut w = p.write(&base.join("up load.bin")).unwrap();
    w.write_all(&body).unwrap();
    w.flush().unwrap();
    drop(w);
    let listed = p.list(&base).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].name.as_str(), listed[0].size),
        ("up load.bin", body.len() as u64)
    );
    assert!(read_all(&p, &base.join("up load.bin")) == body);
    let mut tail = Vec::new();
    p.read_range(&base.join("up load.bin"), 1 << 20, 10)
        .unwrap()
        .unwrap()
        .read_to_end(&mut tail)
        .unwrap();
    assert_eq!(tail, &body[1 << 20..(1 << 20) + 10]);
    p.remove(&base).unwrap();
    assert!(p.stat(&base).is_err());
    *tls::TEST_FILE.lock() = None;
    eprintln!("live {protocol:?} round trip in {:?}", start.elapsed());
}
