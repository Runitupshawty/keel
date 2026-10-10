use keel_vfs::{ops::transfer, Conflict, ConnStatus, Kind, Provider, Router, SftpProvider};
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
        use_ssh_config: false,
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
    let router = Router::new();
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
    // Same-host move: renamed on the server (file and folder), source gone.
    let moved = root.join("moved");
    provider.mkdir(&moved).unwrap();
    for source in [&other, &root.join(file.name())] {
        transfer(
            std::slice::from_ref(source),
            &moved,
            true,
            Conflict::Skip,
            &|_| {},
            &cancel,
            &router,
        )
        .unwrap();
        assert!(provider.stat(source).is_err());
    }
    assert_eq!(
        provider
            .stat(&moved.join("other").join(file.name()))
            .unwrap()
            .size,
        payload.len() as u64
    );
    assert_eq!(
        provider.stat(&moved.join(file.name())).unwrap().size,
        payload.len() as u64
    );
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
        use_ssh_config: true,
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
    // Remotes saved before the setting existed read ~/.ssh/config.
    let older = toml::to_string(&host)
        .unwrap()
        .replace("use_ssh_config = true\n", "");
    assert!(!older.contains("use_ssh_config"));
    assert!(toml::from_str::<RemoteHost>(&older).unwrap().use_ssh_config);
    let path = VPath::parse("sftp://test-remote/space and ü/file").unwrap();
    assert_eq!(path.authority, host.id);
    assert_eq!(path.parent().unwrap().path, "/space and ü");
    assert!(path.to_local_path().is_none());
}

/// Runs `cmd` on the live host over `ssh` (key auth, never prompts); returns stdout.
fn ssh(cmd: &str) -> String {
    let target = std::env::var("KEEL_SFTP_TEST").unwrap();
    let (login, port) = match target.rsplit_once(':') {
        Some((login, port)) => (login.to_owned(), port.to_owned()),
        None => (target.clone(), "22".to_owned()),
    };
    let out = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-p", &port, &login, cmd])
        .output()
        .expect("ssh");
    assert!(
        out.status.success(),
        "ssh {cmd}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
/// A fresh folder under the test dir, removed over ssh when dropped (also on failure).
struct RunDir(VPath);
impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::panic::catch_unwind(|| ssh(&format!("rm -rf '{}'", self.0.path)));
    }
}
fn run_dir(base: &VPath, tag: &str) -> RunDir {
    let dir = base.join(&format!("{tag}-{}", std::process::id()));
    ssh(&format!("rm -rf '{0}' && mkdir -p '{0}'", dir.path));
    RunDir(dir)
}
fn names(provider: &SftpProvider, dir: &VPath) -> Vec<String> {
    let mut names: Vec<_> = provider
        .list(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    names
}
fn mode(path: &VPath) -> String {
    ssh(&format!("stat -c %a '{}'", path.path))
}

/// M10: new uploads get the server's default mode (not 0600); replacing a file keeps its
/// permission bits but never setuid/setgid, for direct uploads and for transfers.
#[test]
fn live_upload_modes_follow_umask_and_replaced_files() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = std::sync::Arc::new(SftpProvider::new(
        host.clone(),
        crossbeam_channel::unbounded().0,
    ));
    let run = run_dir(&base, "modes");
    let file = run.0.join("a.txt");
    let mut up = provider.upload(&file, true).unwrap();
    up.write_all(b"one").unwrap();
    up.finish().unwrap();
    let fresh = u32::from_str_radix(&mode(&file), 8).unwrap();
    assert_ne!(fresh, 0o600, "staged with an explicit 0600");
    assert_eq!(fresh & 0o044, 0o044 & !umask(), "server umask applies");
    ssh(&format!("chmod 6640 '{}'", file.path));
    assert_eq!(mode(&file), "6640", "setuid/setgid could not be set");
    let mut up = provider.upload(&file, false).unwrap();
    up.write_all(b"two").unwrap();
    up.finish().unwrap();
    assert_eq!(mode(&file), "640");
    // Through ops::transfer (staged as .keel-partial, then rename_replace).
    ssh(&format!("chmod 4604 '{}'", file.path));
    let local = tempfile::tempdir().unwrap();
    std::fs::write(local.path().join("a.txt"), b"three").unwrap();
    let router = Router::new();
    router.register_remote_provider(host.id.clone(), provider.clone());
    transfer(
        &[VPath::local(local.path().join("a.txt"))],
        &run.0,
        false,
        Conflict::Overwrite,
        &|_| {},
        &AtomicBool::new(false),
        &router,
    )
    .unwrap();
    assert_eq!(mode(&file), "604");
    assert_eq!(names(&provider, &run.0), ["a.txt"]);
}
/// The sftp-server's umask, as seen by a new file it creates for us.
fn umask() -> u32 {
    let mask = ssh("umask");
    u32::from_str_radix(&mask, 8).unwrap()
}

/// M11: a dropped upload leaves neither a file nor a staging file and never replaces the
/// target; `finish()` commits.
#[test]
fn live_dropped_upload_is_discarded_and_finish_commits() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = SftpProvider::new(host, crossbeam_channel::unbounded().0);
    let run = run_dir(&base, "upload");
    let file = run.0.join("new.bin");
    {
        let mut up = provider.upload(&file, false).unwrap();
        up.write_all(&[1; 100_000]).unwrap();
    }
    assert!(names(&provider, &run.0).is_empty());
    let mut up = provider.upload(&file, false).unwrap();
    up.write_all(b"kept").unwrap();
    up.finish().unwrap();
    {
        let mut up = provider.write(&file).unwrap(); // Box<dyn Write>, never flushed
        up.write_all(b"lost").unwrap();
    }
    assert_eq!(names(&provider, &run.0), ["new.bin"]);
    let mut back = String::new();
    provider
        .read(&file)
        .unwrap()
        .read_to_string(&mut back)
        .unwrap();
    assert_eq!(back, "kept");
}

/// m27: day-old staging files are swept, fresh ones (another upload in progress) and
/// look-alikes stay, a leftover never blocks an upload, and long names still upload.
#[test]
fn live_stale_partials_are_swept_and_never_block() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = SftpProvider::new(host, crossbeam_channel::unbounded().0);
    let run = run_dir(&base, "partials");
    ssh(&format!(
        "cd '{}' && touch x.bin.keel-partial-1-1 && touch -d '2 days ago' \
         x.bin.keel-partial y.keel-partial-99-3 notes.keel-partial-draft",
        run.0.path
    ));
    let file = run.0.join("x.bin");
    let mut up = provider.upload(&file, true).unwrap();
    up.write_all(b"x").unwrap();
    up.finish().unwrap();
    assert_eq!(
        names(&provider, &run.0),
        [
            "notes.keel-partial-draft",
            "x.bin",
            "x.bin.keel-partial-1-1"
        ]
    );
    let long = run.0.join(&format!("{}.txt", "ü".repeat(122))); // 248 bytes
    let mut up = provider.upload(&long, true).unwrap();
    up.write_all(b"long").unwrap();
    up.finish().unwrap();
    assert_eq!(provider.stat(&long).unwrap().size, 4);
}

/// M9: thousands of symlinks resolve concurrently; a slow listing is never a lost
/// connection even with a short per-request timeout.
#[test]
fn live_many_symlinks_list_without_losing_the_connection() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = SftpProvider::new(host, crossbeam_channel::unbounded().0);
    provider.set_timeout(std::time::Duration::from_secs(2));
    let run = run_dir(&base, "links");
    ssh(&format!(
        "cd '{}' && touch target && ln -s missing dangling && \
         perl -e 'symlink(\"target\", \"l$_\") or die for 1..3000'",
        run.0.path
    ));
    let start = std::time::Instant::now();
    let entries = provider.list(&run.0).unwrap();
    eprintln!(
        "live SFTP list: 3002 entries / 3001 links in {:.2} s",
        start.elapsed().as_secs_f64()
    );
    assert_eq!(entries.len(), 3002);
    let links: Vec<_> = entries.iter().filter(|e| e.is_link).collect();
    assert_eq!(links.len(), 3001);
    assert!(links
        .iter()
        .all(|e| (e.name == "dangling") == (e.kind == Kind::Symlink)));
    assert_eq!(provider.status(), ConnStatus::Connected);
}

/// m25: a name that is not UTF-8 on the server lists with U+FFFD, every operation on it
/// is refused clearly, and deleting its folder refuses before removing anything.
#[test]
fn live_undecodable_names_are_flagged_and_remove_refuses_upfront() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = SftpProvider::new(host, crossbeam_channel::unbounded().0);
    let run = run_dir(&base, "names");
    ssh(&format!(
        "cd '{}' && mkdir d && touch d/a.txt d/z.txt \"d/$(printf 'bad\\377')\"",
        run.0.path
    ));
    let dir = run.0.join("d");
    let bad = provider
        .list(&dir)
        .unwrap()
        .into_iter()
        .find(|e| e.name.contains('\u{FFFD}'))
        .expect("flagged entry");
    let err = provider.stat(&bad.path).unwrap_err();
    assert!(format!("{err:#}").contains("not valid UTF-8"), "{err:#}");
    let err = provider.remove(&dir).unwrap_err();
    assert!(
        format!("{err:#}").contains("nothing was deleted"),
        "{err:#}"
    );
    assert_eq!(names(&provider, &dir).len(), 3);
    assert_eq!(provider.status(), ConnStatus::Connected);
}

/// m26: remote downloads can be cancelled (archives on a remote go through this).
#[test]
fn live_download_cancel_then_cached_copy() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = SftpProvider::new(host, crossbeam_channel::unbounded().0);
    let run = run_dir(&base, "download");
    let file = run.0.join("big.bin");
    let payload = vec![9u8; 3 * 1024 * 1024];
    let mut up = provider.upload(&file, true).unwrap();
    up.write_all(&payload).unwrap();
    up.finish().unwrap();
    let cancel = AtomicBool::new(false);
    let dynamic: &dyn Provider = &provider;
    let err = dynamic
        .local_copy_cancellable(
            &file,
            &|p| {
                if p.done_bytes > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    let copy = dynamic
        .local_copy_cancellable(&file, &|_| {}, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(std::fs::read(copy).unwrap(), payload);
}

/// `user@host[:port]` from an environment variable as an ssh_config block for `alias`.
fn config_block(alias: &str, login: &str) -> String {
    let (user, address) = login.split_once('@').expect("user@host[:port]");
    let (host, port) = address.rsplit_once(':').unwrap_or((address, "22"));
    format!("Host {alias}\n HostName {host}\n Port {port}\n User {user}\n")
}

/// Lists, writes and reads back a file through `host` resolved from `config`.
fn roundtrip_through(host: RemoteHost, config: &std::path::Path, base: &VPath) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let provider = SftpProvider::new(host, tx);
    provider.set_ssh_config(config.to_owned());
    let file = base.join(&format!("sshcfg-{}.txt", std::process::id()));
    {
        let mut w = provider
            .write(&file)
            .expect("write through the resolved host");
        w.write_all(b"through ssh_config").unwrap();
        w.flush().unwrap();
    }
    let mut back = String::new();
    provider
        .read(&file)
        .unwrap()
        .read_to_string(&mut back)
        .unwrap();
    assert_eq!(back, "through ssh_config");
    provider.remove(&file).unwrap();
    assert_eq!(provider.status(), ConnStatus::Connected);
    provider.disconnect();
    assert!(
        !rx.try_iter()
            .any(|event| matches!(event, keel_vfs::RemoteEvent::HostKeyPrompt { .. })),
        "live hosts must already be trusted"
    );
}

/// An alias in a temp ssh_config pointing at the live host; Keel's user and port stay empty
/// and default, so both come from the config.
#[test]
fn live_ssh_config_alias() {
    let Some((host, base)) = live() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let login = std::env::var("KEEL_SFTP_TEST").unwrap();
    std::fs::write(&config, config_block("keel-live-alias", &login)).unwrap();
    let alias = RemoteHost {
        host: "keel-live-alias".into(),
        port: 22,
        user: String::new(),
        use_ssh_config: true,
        ..host
    };
    roundtrip_through(alias, &config, &base);
}

/// ProxyJump: the live host reached through `KEEL_SFTP_JUMP_TEST` (`user@host[:port]`;
/// jumping through the same host to itself is fine).
#[test]
fn live_proxy_jump() {
    let Ok(jump) = std::env::var("KEEL_SFTP_JUMP_TEST") else {
        eprintln!("SKIP live ProxyJump: KEEL_SFTP_JUMP_TEST is not set");
        return;
    };
    let Some((host, base)) = live() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let login = std::env::var("KEEL_SFTP_TEST").unwrap();
    std::fs::write(
        &config,
        config_block("keel-live-target", &login)
            .replace("\n User", "\n ProxyJump keel-live-jump\n User")
            + &config_block("keel-live-jump", &jump),
    )
    .unwrap();
    let target = RemoteHost {
        host: "keel-live-target".into(),
        port: 22,
        user: String::new(),
        use_ssh_config: true,
        ..host
    };
    let route = keel_vfs::sftp::describe(&target, Some(&config));
    assert!(route.contains(" via "), "{route}");
    roundtrip_through(target, &config, &base);
}

/// `write_at` writes in place and continues a file cut to its offset; a resumable transfer
/// stopped halfway keeps its staging file on the server and the next run finishes it.
#[test]
fn live_write_at_and_a_resumed_upload() {
    let Some((host, base)) = live() else {
        return;
    };
    let provider = std::sync::Arc::new(SftpProvider::new(
        host.clone(),
        crossbeam_channel::unbounded().0,
    ));
    let run = run_dir(&base, "resume");
    let file = run.0.join("w.txt");
    let mut w = provider.write_at(&file, 0).unwrap().unwrap();
    w.write_all(b"hello world").unwrap();
    w.flush().unwrap();
    drop(w);
    let mut w = provider.write_at(&file, 5).unwrap().unwrap();
    w.write_all(b" there").unwrap();
    drop(w);
    let mut back = String::new();
    provider
        .read(&file)
        .unwrap()
        .read_to_string(&mut back)
        .unwrap();
    assert_eq!(back, "hello there");
    assert!(
        provider.write_at(&file, 100).is_err(),
        "shorter than the offset"
    );

    let router = Router::new();
    router.register_remote_provider(host.id.clone(), provider.clone());
    let local = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..10 << 20).map(|i| (i % 249) as u8).collect();
    std::fs::write(local.path().join("big.bin"), &big).unwrap();
    let src = [VPath::local(local.path().join("big.bin"))];
    let recorded = std::cell::RefCell::new(None);
    let cancel = AtomicBool::new(false);
    let mut journal = keel_vfs::ops::Journal::new(Default::default(), None, |_, u| {
        *recorded.borrow_mut() = u.cloned();
        Ok(())
    });
    let stop = |p: keel_vfs::Progress| {
        if p.done_bytes >= 6 << 20 {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let result = keel_vfs::ops::transfer_resumable(
        &src,
        &run.0,
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
    assert!(unfinished.bytes >= 6 << 20, "{}", unfinished.bytes);
    assert_eq!(
        provider.stat(&unfinished.staging).unwrap().size,
        unfinished.bytes
    );
    cancel.store(false, Ordering::SeqCst);
    let mut journal =
        keel_vfs::ops::Journal::new(Default::default(), Some(unfinished), |_, _| Ok(()));
    keel_vfs::ops::transfer_resumable(
        &src,
        &run.0,
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
    let mut back = Vec::new();
    provider
        .read(&run.0.join("big.bin"))
        .unwrap()
        .read_to_end(&mut back)
        .unwrap();
    assert!(back == big);
    assert_eq!(names(&provider, &run.0), ["big.bin", "w.txt"]);
}

/// A zip extracted straight into a folder on the host (each entry streamed through the
/// SFTP provider), then listed back.
#[cfg(feature = "zip")]
#[test]
fn live_extract_zip_into_remote_folder() {
    let Some((host, base)) = live() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("fixture.zip");
    {
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
        for (name, body) in [
            ("readme.txt", "hello"),
            ("docs/a.txt", "alpha"),
            ("docs/deep/b.txt", "bravo"),
        ] {
            w.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.add_directory("empty", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.finish().unwrap();
    }
    let run = run_dir(&base, "extract");
    let router = Router::new();
    let provider = std::sync::Arc::new(SftpProvider::new(host, crossbeam_channel::unbounded().0));
    router.register_remote_provider("live-test".into(), provider.clone());
    let cancel = AtomicBool::new(false);
    keel_vfs::extract_to(
        &VPath::local(&zip),
        "",
        &[],
        &run.0,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(names(&provider, &run.0), ["docs", "empty", "readme.txt"]);
    assert_eq!(names(&provider, &run.0.join("docs")), ["a.txt", "deep"]);
    let mut body = String::new();
    provider
        .read(&run.0.join("docs/deep/b.txt"))
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "bravo");
    eprintln!(
        "live extract: {}",
        ssh(&format!(
            "cd '{}' && find . | sort | tr '\n' ' '",
            run.0.path
        ))
    );
    // Again with Keep both: every file lands a second time beside the first.
    keel_vfs::extract_to(
        &VPath::local(&zip),
        "",
        &[],
        &run.0,
        Conflict::RenameNew,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(
        names(&provider, &run.0),
        ["docs", "empty", "readme (2).txt", "readme.txt"]
    );
}
