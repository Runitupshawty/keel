//! Live: a watched SFTP source finds a file uploaded after its walk within the cheap poll
//! (folder times), without another walk. Needs `KEEL_SFTP_TEST=user@host[:port]` (key
//! auth, host key already in `~/.ssh/known_hosts`) and `KEEL_SFTP_TEST_DIR` (a remote
//! folder for tests); skips without them.

use keel_core::{Library, SourceDef, SourceKind};
use keel_vfs::{Provider, RemoteAuth, RemoteHost, Router, SftpProvider, VPath};
use std::{
    io::Write,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

/// Seconds between the source's polls in this test (the default is 120).
const POLL: Duration = Duration::from_secs(2);

#[test]
fn live_sftp_source_finds_an_upload_within_the_poll() {
    let (Ok(target), Ok(dir)) = (
        std::env::var("KEEL_SFTP_TEST"),
        std::env::var("KEEL_SFTP_TEST_DIR"),
    ) else {
        eprintln!("SKIP live SFTP: KEEL_SFTP_TEST and KEEL_SFTP_TEST_DIR must both be set");
        return;
    };
    assert!(
        dir.starts_with('/') && dir != "/",
        "dedicated remote test directory required"
    );
    let (user, address) = target.split_once('@').expect("user@host[:port]");
    let (host, port) = address
        .rsplit_once(':')
        .map(|(h, p)| (h.to_owned(), p.parse().expect("port")))
        .unwrap_or((address.to_owned(), 22));
    let host = RemoteHost {
        id: "live-test".into(),
        label: "Integration test".into(),
        host,
        port,
        user: user.into(),
        auth: RemoteAuth::KeyFile {
            path: std::path::PathBuf::new(),
            passphrase_in_keyring: false,
        },
        home: None,
        bookmarks: vec![],
        use_ssh_config: false,
    };
    let sftp = Arc::new(SftpProvider::new(host, crossbeam_channel::unbounded().0));
    let base = VPath {
        scheme: "sftp".into(),
        authority: "live-test".into(),
        path: dir,
    };
    let made_base = sftp.stat(&base).is_err() && sftp.mkdir(&base).is_ok();
    let root = base.join(&format!("livechg-{}", std::process::id()));
    sftp.mkdir(&root)
        .expect("create the run folder (base must exist and the host key be trusted)");
    let put = |name: &str, bytes: &[u8]| {
        let mut w = sftp.write(&root.join(name)).unwrap();
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
    };
    put("first.txt", b"there before the walk");
    let cleanup = || {
        for name in ["first.txt", "uploaded.txt", "sub/inner.txt"] {
            let _ = sftp.remove(&root.join(name));
        }
        let _ = sftp.remove_empty_dir(&root.join("sub"));
        let _ = sftp.remove_empty_dir(&root);
        if made_base {
            let _ = sftp.remove_empty_dir(&base);
        }
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let data = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "live").unwrap();
        lib.set_hash_after_walk(false);
        let router = Arc::new(Router::new());
        router.register_remote_provider("live-test".into(), sftp.clone());
        lib.set_router(router);
        lib.set_remote_poll(POLL);
        let id = lib
            .add_source(SourceDef {
                label: "server".into(),
                root: root.clone(),
                kind: SourceKind::Folder,
                include_hidden: false,
                ignore: Vec::new(),
                poll_secs: None,
                hash_shares: false,
            })
            .unwrap();
        lib.watch(&id).unwrap();
        let src = lib.source(&id).unwrap();
        let until = |what: &str, limit: Duration, check: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !check() {
                assert!(start.elapsed() < limit, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(100));
            }
            start.elapsed()
        };
        until("first walk", Duration::from_secs(60), &|| {
            src.generation.load(Ordering::SeqCst) >= 1
        });
        let generation = src.generation.load(Ordering::SeqCst);
        let names = |rel: &str| -> Vec<String> {
            (lib.list_children(&id, rel).unwrap_or_default().into_iter())
                .map(|h| h.name)
                .collect()
        };
        assert_eq!(names(""), ["first.txt"]);
        // SFTP folder times are whole seconds: a change in the second the walk listed the
        // folder would not move its time (the 6-hour walk would find it).
        std::thread::sleep(Duration::from_millis(1100));
        put("uploaded.txt", b"uploaded through the provider");
        sftp.mkdir(&root.join("sub")).unwrap();
        put("sub/inner.txt", b"in a new folder");
        let took = until("upload indexed", Duration::from_secs(30), &|| {
            names("").contains(&"uploaded.txt".to_owned()) && names("sub") == ["inner.txt"]
        });
        eprintln!("upload indexed after {took:?} (poll {POLL:?})");
        sftp.remove(&root.join("first.txt")).unwrap();
        let took = until("removal indexed", Duration::from_secs(30), &|| {
            !names("").contains(&"first.txt".to_owned())
        });
        eprintln!("removal indexed after {took:?}");
        assert_eq!(
            src.generation.load(Ordering::SeqCst),
            generation,
            "found by the folder-time poll, not a walk"
        );
        if std::env::var_os("CI").is_none() {
            assert!(took < POLL * 5, "{took:?}");
        }
        lib.unwatch(&id);
    }));
    cleanup();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e);
    }
}
