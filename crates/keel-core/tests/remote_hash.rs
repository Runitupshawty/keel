//! Live: hashing an SFTP source finds the copy of a local file. Needs
//! `KEEL_SFTP_TEST=user@host[:port]` (key auth, host key already in `~/.ssh/known_hosts`)
//! and `KEEL_SFTP_TEST_DIR` (a remote folder for tests); skips without them.

use keel_core::{JobStatus, Library, SourceDef, SourceKind};
use keel_vfs::{Provider, RemoteAuth, RemoteHost, Router, SftpProvider, VPath};
use std::{io::Write, sync::Arc};

#[test]
fn live_sftp_source_is_hashed_and_its_local_copy_found() {
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
    // The test folder itself is made (and removed again) when it does not exist yet.
    let made_base = sftp.stat(&base).is_err() && sftp.mkdir(&base).is_ok();
    let root = base.join(&format!("rhash-{}", std::process::id()));
    sftp.mkdir(&root)
        .expect("create the run folder (base must exist and the host key be trusted)");
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 7).map(|i| (i % 253) as u8).collect();
    let files: [(&str, &[u8]); 3] = [
        ("small.txt", b"same bytes here and there"),
        ("big.bin", &big),
        ("only-remote.txt", b"no local copy"),
    ];
    for (name, bytes) in files {
        let mut w = sftp.write(&root.join(name)).unwrap();
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
    }
    let cleanup = || {
        for (name, _) in files {
            let _ = sftp.remove(&root.join(name));
        }
        let _ = sftp.remove_empty_dir(&root);
        if made_base {
            let _ = sftp.remove_empty_dir(&base);
        }
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let local = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("small.txt"), files[0].1).unwrap();
        std::fs::write(local.path().join("big.bin"), &big).unwrap();
        let data = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "live").unwrap();
        lib.set_pause_on_battery(false);
        lib.set_hash_after_walk(false);
        let router = Arc::new(Router::new());
        router.register_remote_provider("live-test".into(), sftp.clone());
        lib.set_router(router);
        let def = |label: &str, root: VPath| SourceDef {
            label: label.into(),
            root,
            kind: SourceKind::Folder,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        };
        for d in [
            def("pc", VPath::local(local.path())),
            def("server", root.clone()),
        ] {
            let id = lib.add_source(d).unwrap();
            let info = lib.jobs().wait(lib.index(&id).unwrap()).unwrap();
            assert_eq!(info.status, JobStatus::Done, "{}", info.log);
        }
        let info = lib.jobs().wait(lib.hash().unwrap()).unwrap();
        assert_eq!(info.status, JobStatus::Done, "{}", info.log);
        eprintln!("{}", info.log);
        let dups = lib.duplicates(0).unwrap();
        let sizes: Vec<u64> = dups.iter().map(|g| g.size).collect();
        assert_eq!(
            sizes,
            [big.len() as u64, files[0].1.len() as u64],
            "{dups:?}"
        );
        assert!(dups.iter().all(|g| g.records.len() == 2));
        let r = lib.redundancy(&dups[0].records[0]).unwrap();
        assert_eq!((r.copies, r.failure_domains), (2, 2), "{r:?}");
    }));
    cleanup();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e);
    }
}
