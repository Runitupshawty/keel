use keel_vfs::{ops::transfer, Conflict, Provider, Router, SftpProvider};
use keel_vfs::{
    sftp::{RemoteAuth, RemoteHost},
    VPath,
};
use std::{
    io::{Read, Write},
    sync::atomic::{AtomicBool, Ordering},
};

fn live() -> Option<(RemoteHost, VPath)> {
    let (Ok(target), Ok(dir)) = (
        std::env::var("KEEL_SFTP_TEST"),
        std::env::var("KEEL_SFTP_TEST_DIR"),
    ) else {
        eprintln!("SKIP live SFTP: KEEL_SFTP_TEST and KEEL_SFTP_TEST_DIR must both be set");
        return None;
    };
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
    };
    assert!(
        dir.starts_with('/') && dir != "/",
        "dedicated remote test directory required"
    );
    Some((
        host,
        VPath {
            scheme: "sftp".into(),
            authority: "live-test".into(),
            path: dir,
        },
    ))
}

#[test]
fn live_operations_transfers_cache_and_cleanup() {
    let Some((host, base)) = live() else {
        return;
    };
    let (tx, rx) = crossbeam_channel::unbounded();
    // Live tests require an existing trusted key; they never silently approve TOFU.
    let provider = std::sync::Arc::new(SftpProvider::new(host.clone(), tx));
    let root = base.join(&format!(
        "run-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    provider
        .mkdir(&root)
        .expect("create unique test directory (base must exist and key must be trusted)");
    let file = root.join("space ü.txt");
    let payload = vec![42; 2 * 1024 * 1024 + 17];
    {
        let mut writer = provider.write(&file).unwrap();
        writer.write_all(&payload).unwrap();
        writer.flush().unwrap();
    }
    assert!(provider.create_new(&file).is_err());
    {
        let mut writer = provider.write(&file).unwrap();
        writer.write_all(b"unfinished").unwrap();
    }
    assert_eq!(provider.stat(&file).unwrap().size, payload.len() as u64);
    assert_eq!(provider.list(&root).unwrap().len(), 1);
    let mut actual = Vec::new();
    provider
        .read(&file)
        .unwrap()
        .read_to_end(&mut actual)
        .unwrap();
    assert_eq!(actual, payload);
    let cache = provider.local_copy_with_progress(&file, &|_| {}).unwrap();
    assert_eq!(std::fs::read(&cache).unwrap(), payload);
    assert_eq!(provider.local_copy(&file).unwrap(), cache);
    // rename never replaces; rename_replace does.
    let other_file = root.join("b.txt");
    {
        let mut w = provider.create_new(&other_file).unwrap();
        w.write_all(b"b").unwrap();
        w.flush().unwrap();
    }
    assert!(provider.rename(&file, &other_file).is_err());
    assert_eq!(provider.stat(&other_file).unwrap().size, 1);
    provider.remove(&other_file).unwrap();
    // Throughput (pipelined 32 KiB requests), printed for the record.
    let big = root.join("throughput.bin");
    let blob = vec![5u8; 16 * 1024 * 1024];
    let start = std::time::Instant::now();
    {
        let mut w = provider.write(&big).unwrap();
        w.write_all(&blob).unwrap();
        w.flush().unwrap();
    }
    let up = start.elapsed();
    let start = std::time::Instant::now();
    let mut back = Vec::new();
    provider.read(&big).unwrap().read_to_end(&mut back).unwrap();
    let down = start.elapsed();
    assert!(back == blob);
    eprintln!(
        "live SFTP 16 MiB: upload {:.1} MB/s, download {:.1} MB/s",
        16.8 / up.as_secs_f64(),
        16.8 / down.as_secs_f64()
    );
    provider.remove(&big).unwrap();
    provider.disconnect();
    assert_eq!(provider.stat(&file).unwrap().size, payload.len() as u64);
    let mut router = Router::new();
    router.register_remote_provider(host.id.clone(), provider.clone());
    let local = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    transfer(
        std::slice::from_ref(&file),
        &VPath::local(local.path()),
        false,
        Conflict::Overwrite,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(local.path().join(file.name())).unwrap(),
        payload
    );
    let incoming = VPath::local(local.path().join(file.name()));
    transfer(
        std::slice::from_ref(&incoming),
        &root,
        true,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert!(incoming.to_local_path().unwrap().exists());
    transfer(
        std::slice::from_ref(&incoming),
        &root,
        false,
        Conflict::Overwrite,
        &|p| {
            if p.done_bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
        &router,
    )
    .unwrap_err();
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(provider.list(&root).unwrap().len(), 1);
    assert_eq!(provider.stat(&file).unwrap().size, payload.len() as u64);
    transfer(
        std::slice::from_ref(&incoming),
        &root,
        true,
        Conflict::RenameNew,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert!(!incoming.to_local_path().unwrap().exists());
    let other = root.join("other");
    provider.mkdir(&other).unwrap();
    transfer(
        std::slice::from_ref(&file),
        &other,
        false,
        Conflict::Overwrite,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(
        provider.stat(&other.join(file.name())).unwrap().size,
        payload.len() as u64
    );
    assert!(transfer(
        std::slice::from_ref(&root),
        &other,
        false,
        Conflict::Overwrite,
        &|_| {},
        &cancel,
        &router
    )
    .is_err());
    provider.remove(&root).unwrap();
    assert!(provider.stat(&root).is_err());
    assert!(
        !rx.try_iter()
            .any(|event| matches!(event, keel_vfs::RemoteEvent::HostKeyPrompt { .. })),
        "live host must already be trusted"
    );
}

/// Review focus 5: a 50,000-entry remote folder lists in one call. Needs a prepared
/// `$KEEL_SFTP_TEST_DIR/big` (e.g. `mkdir big && cd big && seq 50000 | xargs touch`).
#[test]
fn live_list_large_directory() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = SftpProvider::new(host, crossbeam_channel::unbounded().0);
    let big = base.join("big");
    if provider.stat(&big).is_err() {
        eprintln!("SKIP live 50k listing: {} not prepared", big.display());
        return;
    }
    let start = std::time::Instant::now();
    let entries = provider.list(&big).unwrap();
    eprintln!(
        "live SFTP list: {} entries in {:.2} s",
        entries.len(),
        start.elapsed().as_secs_f64()
    );
    assert!(entries.len() >= 50_000);
}

#[test]
fn remote_config_roundtrip_and_path() {
    let host = RemoteHost {
        id: "test-remote".into(),
        label: "Test".into(),
        host: String::new(),
        port: 22,
        user: String::new(),
        auth: RemoteAuth::Agent,
        home: Some("/".into()),
        bookmarks: vec![("Files".into(), "/files".into())],
    };
    for auth in [
        RemoteAuth::Agent,
        RemoteAuth::KeyFile {
            path: "id_ed25519".into(),
            passphrase_in_keyring: true,
        },
        RemoteAuth::PasswordInKeyring,
    ] {
        let host = RemoteHost {
            auth,
            ..host.clone()
        };
        let encoded = toml::to_string(&host).unwrap();
        assert_eq!(toml::from_str::<RemoteHost>(&encoded).unwrap(), host);
    }
    let path = VPath::parse("sftp://test-remote/space and ü/file").unwrap();
    assert_eq!(path.authority, host.id);
    assert_eq!(path.parent().unwrap().path, "/space and ü");
    assert!(path.to_local_path().is_none());
}
