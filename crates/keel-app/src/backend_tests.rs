use super::*;

/// A started daemon that keeps running.
fn running() -> Result<Exited> {
    Ok(Box::new(|| None))
}

#[test]
fn a_running_daemon_is_used() {
    let mut connect = || Some("daemon");
    let mut spawned = false;
    let mut spawn = || {
        spawned = true;
        running()
    };
    let got = attach(Some(&mut connect), Some(&mut spawn), Duration::ZERO);
    assert_eq!(
        got,
        Attach::Daemon {
            remote: "daemon",
            spawned: false
        }
    );
    assert!(!spawned, "nothing is started while one runs");
}

#[test]
fn without_a_daemon_and_the_setting_off_the_library_opens_here() {
    let mut connect = || None::<&str>;
    let got = attach(Some(&mut connect), None, Duration::ZERO);
    assert_eq!(got, Attach::InProcess { note: None });
    // "Open in this window" never looks for one.
    let got = attach::<&str>(None, None, Duration::ZERO);
    assert_eq!(got, Attach::InProcess { note: None });
}

#[test]
fn the_setting_on_starts_a_daemon_and_waits_for_it() {
    let tries = std::cell::Cell::new(0);
    let started = std::cell::Cell::new(false);
    // It answers on the third try after starting.
    let mut connect = || {
        tries.set(tries.get() + 1);
        (started.get() && tries.get() >= 3).then_some("daemon")
    };
    let mut spawn = || {
        started.set(true);
        running()
    };
    let got = attach(Some(&mut connect), Some(&mut spawn), Duration::from_secs(5));
    assert_eq!(
        got,
        Attach::Daemon {
            remote: "daemon",
            spawned: true
        }
    );
}

#[test]
fn a_daemon_that_fails_to_start_or_answer_falls_back_with_a_note() {
    let mut connect = || None::<&str>;
    let mut spawn = || anyhow::bail!("keel-daemon was not found next to keel");
    match attach(Some(&mut connect), Some(&mut spawn), Duration::ZERO) {
        Attach::InProcess { note: Some(n) } => assert!(n.contains("not found"), "{n}"),
        other => panic!("{other:?}"),
    }
    let mut connect = || None::<&str>;
    let mut spawn = running;
    match attach(
        Some(&mut connect),
        Some(&mut spawn),
        Duration::from_millis(200),
    ) {
        Attach::InProcess { note: Some(n) } => assert!(n.contains("did not answer"), "{n}"),
        other => panic!("{other:?}"),
    }
}

/// A started daemon that exits at once (its library is held, a bad profile, ...) is
/// noticed at once, with the end of its log, instead of after the whole wait.
#[test]
fn a_started_daemon_that_exits_is_noticed_at_once() {
    let mut connect = || None::<&str>;
    let mut spawn = || -> Result<Exited> {
        Ok(Box::new(|| {
            Some("keel-daemon exited (exit code: 1): library main is already open".into())
        }))
    };
    let t = Instant::now();
    match attach(Some(&mut connect), Some(&mut spawn), SPAWN_WAIT) {
        Attach::InProcess { note: Some(n) } => assert!(n.contains("already open"), "{n}"),
        other => panic!("{other:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[test]
fn a_spawned_process_reports_its_exit_and_the_end_of_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(
        &log,
        "earlier run\n\nkeel-daemon: library main is already open\n",
    )
    .unwrap();
    #[cfg(windows)]
    let child = std::process::Command::new("cmd")
        .args(["/C", "exit 3"])
        .spawn();
    #[cfg(unix)]
    let child = std::process::Command::new("sh")
        .args(["-c", "exit 3"])
        .spawn();
    let mut s = Spawned {
        child: Some(child.unwrap()),
        log,
    };
    let end = Instant::now() + Duration::from_secs(10);
    let why = loop {
        if let Some(why) = s.exited() {
            break why;
        }
        assert!(Instant::now() < end, "no exit seen");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(why.contains('3') && why.contains("already open"), "{why}");
    assert!(why.contains("earlier run |"), "two lines: {why}");
    assert_eq!(s.exited(), None, "reported once");
}

/// keel-app starts keel-daemon with `--profile=<name>`.
#[test]
fn the_daemon_gets_the_profile_as_one_argument() {
    assert_eq!(crate::commands::daemon_args("work"), ["--profile=work"]);
}

// --- against a real keel-daemon, started in this process ---

struct Served {
    _config: tempfile::TempDir,
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
    daemon: keel_daemon::server::Daemon,
}

/// A daemon over a temp profile (never the user's folders) and a folder of files.
fn served() -> Served {
    let (config, data, files) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::create_dir(files.path().join("docs")).unwrap();
    std::fs::write(files.path().join("docs").join("invoice-2026.pdf"), b"pdf").unwrap();
    std::fs::write(files.path().join("docs").join("notes.txt"), b"notes").unwrap();
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let profile = format!("app-test-{}-{}", std::process::id(), u64::from_le_bytes(b));
    let cfg = HostConfig::read(&profile, config.path().into(), data.path().into());
    let daemon = keel_daemon::server::Daemon::start(keel_daemon::server::Options {
        cfg,
        ws: None,
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .unwrap();
    Served {
        _config: config,
        _data: data,
        files,
        daemon,
    }
}

fn wait_job(b: &LibraryBackend, id: JobId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let jobs = b.jobs().unwrap();
        let row = jobs.iter().find(|(j, _)| *j == id).map(|(_, r)| r.clone());
        match row {
            Some(r) if r.status == JobStatus::Done => return,
            Some(r) if !r.active() => panic!("job {id}: {:?}", r.status),
            _ => {}
        }
        assert!(Instant::now() < deadline, "job {id} did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn def(root: &std::path::Path) -> SourceDef {
    SourceDef {
        label: "Docs".into(),
        root: VPath::local(root),
        kind: SourceKind::Folder,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
        hash_shares: false,
    }
}

#[test]
fn the_daemon_backend_lists_searches_tags_plans_and_follows_jobs() {
    let s = served();
    let remote = Remote::connect(s.daemon.name()).unwrap();
    assert_eq!(remote.version.pid, std::process::id());
    let b = LibraryBackend::Daemon(remote.clone());
    let events = remote.subscribe(|_| {}).unwrap();
    let job = b.add_source(def(s.files.path())).unwrap();
    wait_job(&b, job);
    // The subscription saw the job.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !events
        .try_iter()
        .any(|e| e.id == job && e.status == JobStatus::Done)
    {
        assert!(Instant::now() < deadline, "no job.progress for {job}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let sources = b.sources().unwrap();
    assert_eq!(sources.len(), 1);
    let id = sources[0].id.clone();
    assert!(matches!(sources[0].status, SourceStatus::Online { .. }));
    assert_eq!(sources[0].root, VPath::local(s.files.path()));

    // library:// listings come from the daemon's index; reads go to the real file.
    let names: Vec<String> = (b.children(&id.0, "docs").unwrap())
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["invoice-2026.pdf", "notes.txt"]);
    let invoice = VPath::local(s.files.path().join("docs").join("invoice-2026.pdf"));
    assert_eq!(b.resolve(&id.0, "docs/invoice-2026.pdf").unwrap(), invoice);

    let query = |text: &str| keel_search::Query {
        text: text.into(),
        max: 10,
        ..keel_search::Query::default()
    };
    let hits = b.search(&query("invoice")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, invoice);

    // Tags, favorites and recents.
    let lib_path = keel_vfs::library::path(&id.0, "docs/invoice-2026.pdf");
    b.create_tag(
        &sources,
        "receipts",
        "#e5484d",
        Some(std::slice::from_ref(&lib_path)),
    )
    .unwrap();
    let (tags, _, tagged) = b.meta().unwrap();
    let receipts = tags.iter().find(|t| t.name == "receipts").unwrap().clone();
    assert_eq!(receipts.color.as_deref(), Some("#e5484d"));
    assert_eq!(tagged.get(&invoice), Some(&vec![receipts.id]));
    let one = std::slice::from_ref(&invoice);
    b.set_tag(&sources, &tags, one, receipts.id, false).unwrap();
    b.set_tag(&sources, &tags, one, FAVORITES, true).unwrap();
    // A tag made outside the picker: created, on nothing.
    b.create_tag(&sources, "later", "#3b82f6", None).unwrap();
    assert!(b.meta().unwrap().0.iter().any(|t| t.name == "later"));
    let (_, _, tagged) = b.meta().unwrap();
    assert_eq!(tagged.get(&invoice), Some(&vec![FAVORITES]));
    use crate::library::{FAVORITES_QUERY, RECENTS_QUERY};
    assert_eq!(b.search(&query(FAVORITES_QUERY)).unwrap()[0].path, invoice);
    b.note_open(&sources, &lib_path).unwrap();
    assert_eq!(b.search(&query(RECENTS_QUERY)).unwrap()[0].path, invoice);

    // A plan previews in the window's dialog, then runs as a daemon job.
    let notes = VPath::local(s.files.path().join("docs").join("notes.txt"));
    let planned = b
        .plan(Op::Rename {
            path: notes.clone(),
            new_name: "notes-2026.txt".into(),
        })
        .unwrap();
    assert!(matches!(planned, Planned::Daemon(_)));
    assert!(
        planned.summary().starts_with("Rename"),
        "{}",
        planned.summary()
    );
    let changes = planned.changes();
    assert!(changes[0].starts_with("Rename"), "{changes:?}");
    assert!(
        notes.to_local_path().unwrap().exists(),
        "a preview changes nothing"
    );
    match b.execute(planned).unwrap() {
        Executed::Job(job) => wait_job(&b, job),
        Executed::Changed(_) => panic!("nothing changed"),
    }
    assert!(s.files.path().join("docs").join("notes-2026.txt").exists());
    // A delete previews with its warnings (it is not run: no trash in tests).
    let planned = b
        .plan(Op::Delete {
            paths: vec![invoice.clone()],
        })
        .unwrap();
    assert!(!planned.warnings(&sources).is_empty(), "the last copy");

    // The Overview and the background work the window drives.
    let stats = b.stats().unwrap();
    assert_eq!((stats.sources, stats.files), (1, 2), "{stats:?}");
    let (_, volumes) = b.protection().unwrap();
    let volume = volumes[0].id.clone();
    b.set_volume(&volume, VolumeChange::Backup(true)).unwrap();
    assert!(b.protection().unwrap().1[0].backup);
    let docs = VPath::local(s.files.path().join("docs"));
    let badges = b.badges(&sources, &[docs]);
    let badge = badges.get(&invoice).map(|b| b.text.as_str());
    assert_eq!(badge, Some("1× · 1"), "{badges:?}");
    assert!(b.dups(1).unwrap().is_empty());
    wait_job(&b, b.index(&id, false).unwrap());
    // "Hash now" keeps the daemon's idle-only policy.
    b.set_hashing(true, true).unwrap();
    wait_job(&b, b.hash(true).unwrap());
    assert!(s.daemon.ctx().lib.hash_idle_only());
    assert_eq!(b.set_hashing(false, true).unwrap(), None);
    wait_job(&b, b.integrity(1.0).unwrap());
    wait_job(&b, b.media_job(&id).unwrap());
    let kinds = b.job_kinds().unwrap();
    for kind in ["index", "hash", "integrity", "sidecar"] {
        assert!(kinds.iter().any(|(_, k)| k == kind), "{kind}: {kinds:?}");
    }
    b.remove_source(&id).unwrap();
    assert!(b.sources().unwrap().is_empty());
}

fn pump_until(
    s: &mut crate::state::AppState,
    what: &str,
    done: impl Fn(&crate::state::AppState) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(30);
    while !done(s) {
        assert!(Instant::now() < end, "timed out: {what}");
        if let Ok(m) = s.rx.recv_timeout(Duration::from_millis(30)) {
            s.apply(m);
        }
        s.drain();
        s.library_tick();
    }
}

/// The window attached to a daemon: library tabs list the daemon's index, a rename goes
/// through the preview dialog and runs there; when the daemon stops, nothing is written
/// until Reconnect or Open in this window.
#[test]
fn the_window_works_through_the_daemon_and_stops_writing_when_it_goes() {
    use crate::keys::Action;
    use crate::library::{LibCmd, LOST};
    let s = served();
    let remote = Remote::connect(s.daemon.name()).unwrap();
    let b = LibraryBackend::Daemon(remote.clone());
    wait_job(&b, b.add_source(def(s.files.path())).unwrap());
    let mut st = crate::state::AppState::new(
        egui::Context::default(),
        Arc::new(keel_vfs::Router::new()),
        VPath::local(s.files.path()),
    );
    st.library.set_attached(remote);
    assert!(st.library.is_open() && st.library.lib.is_none());
    let id = st.library.sources[0].id.clone();
    let docs = keel_vfs::library::path(&id.0, "docs");
    st.run(0, Action::Navigate(docs.clone()));
    pump_until(&mut st, "library folder listed", |s| {
        s.tab(0).listed_dir.as_ref() == Some(&docs) && s.tab(0).entries().len() == 2
    });

    // Renaming in a library tab: the daemon's preview in the window's dialog.
    let from = keel_vfs::library::path(&id.0, "docs/notes.txt");
    let to = "renamed.txt".to_owned();
    st.run(0, Action::RenameTo { from, to });
    pump_until(&mut st, "preview", |s| s.library.plan.is_some());
    let shown = &st.library.plan.as_ref().unwrap().plan;
    assert!(matches!(shown, Planned::Daemon(_)));
    st.run(0, Action::Library(LibCmd::Execute));
    let renamed = s.files.path().join("docs").join("renamed.txt");
    pump_until(&mut st, "renamed by the daemon", |_| renamed.exists());

    // The daemon stops: the banner's state, and changes are refused.
    s.daemon.shutdown();
    pump_until(&mut st, "lost", |s| s.library.lost);
    st.toasts.list.clear();
    st.run(0, Action::Library(LibCmd::TagPicker));
    let texts: Vec<&str> = st.toasts.list.iter().map(|t| t.text.as_str()).collect();
    assert!(texts.contains(&LOST), "{texts:?}");
    // Nor is the mount list asked for.
    st.mount.asked = None;
    st.mount_tick();
    assert!(!st.mount.busy, "no mounts.list while lost");
    let from = keel_vfs::library::path(&id.0, "docs/renamed.txt");
    let to = "again.txt".to_owned();
    st.run(0, Action::RenameTo { from, to });
    assert!(st.library.plan.is_none(), "no preview while lost");
    // Closing the window lets go without stopping anything.
    st.library.close_now();
    assert!(!st.library.is_open());
}

/// The test profile's daemon (`TEST_ENV` held). An earlier test's daemon lets go of the
/// socket and library a moment after it was dropped (once its clients are gone).
fn profile_daemon(cfg: &HostConfig) -> keel_daemon::server::Daemon {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let started = keel_daemon::server::Daemon::start(keel_daemon::server::Options {
            cfg: cfg.clone(),
            ws: None,
            web: None,
            ws_allow_remote: false,
            web_hosts: Vec::new(),
            net: None,
        });
        match started {
            Ok(d) => return d,
            Err(e) if Instant::now() < end => {
                tracing::debug!("profile daemon: {e:#}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("{e:#}"),
        }
    }
}

/// `LibraryUi::open` finds the profile's daemon by its socket name and attaches; the
/// window then never opens the library itself.
#[test]
fn open_attaches_to_the_profiles_running_daemon() {
    let _env = crate::settings::TEST_ENV.lock();
    let cfg = host_config(&crate::cli::profile()).unwrap();
    let daemon = profile_daemon(&cfg);
    let mut st = crate::state::AppState::new(
        egui::Context::default(),
        Arc::new(keel_vfs::Router::new()),
        VPath::local(std::env::temp_dir()),
    );
    let router = st.router.clone();
    let via = crate::library::Via::Auto { spawn: false };
    st.library.open(&cfg.library, router.clone(), via);
    pump_until(&mut st, "attached", |s| s.library.is_open());
    let remote = st.library.remote().expect("attached").clone();
    assert_eq!(remote.version.pid, std::process::id());
    assert!(st.library.lib.is_none() && !st.library.spawned);
    // The daemon holds the library: opening it here as well is refused.
    st.library.close_now();
    st.library.open(
        &cfg.library,
        router,
        crate::library::Via::Replace {
            here: true,
            wait: Duration::ZERO,
        },
    );
    pump_until(&mut st, "refused", |s| !s.library.opening);
    assert!(!st.library.is_open());
    assert!(st.library.error.is_some());
    drop(daemon);
}

/// A daemon with devices on (identity in memory, no network lookups).
fn served_net() -> Served {
    let (config, data, files) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::write(files.path().join("notes.txt"), b"notes").unwrap();
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let profile = format!("app-net-{}-{}", std::process::id(), u64::from_le_bytes(b));
    let mut cfg = HostConfig::read(&profile, config.path().into(), data.path().into());
    cfg.net = true;
    let daemon = keel_daemon::server::Daemon::start(keel_daemon::server::Options {
        cfg,
        ws: None,
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: Some(keel_api::host::NetSetup {
            secrets: Arc::new(keel_vfs::cloud::MemoryStore::default()),
            options: keel_net::NodeOptions::offline(),
        }),
    })
    .unwrap();
    Served {
        _config: config,
        _data: data,
        files,
        daemon,
    }
}

fn pump_devices(
    s: &mut crate::state::AppState,
    what: &str,
    done: impl Fn(&crate::state::AppState) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(30);
    while !done(s) {
        assert!(Instant::now() < end, "timed out: {what}");
        if let Ok(m) = s.rx.recv_timeout(Duration::from_millis(30)) {
            s.apply(m);
        }
        s.drain();
        s.devices_tick();
    }
}

/// Attached, the window's Devices use the daemon's node: its identity, pairing (the code
/// it shows, the paired event), shares and the sidebar rows.
#[test]
fn devices_go_through_the_daemons_node() {
    use crate::devices::{DevCmd, Pair, PairEvent};
    let (a, other) = (served_net(), served_net());
    let remote = Remote::connect(a.daemon.name()).unwrap();
    let b = LibraryBackend::Daemon(remote.clone());
    wait_job(&b, b.add_source(def(a.files.path())).unwrap());
    let mut st = crate::state::AppState::new(
        egui::Context::default(),
        Arc::new(keel_vfs::Router::new()),
        VPath::local(a.files.path()),
    );
    st.library.set_attached(remote.clone());
    pump_devices(&mut st, "the daemon's device", |s| {
        s.devices.remote_self.is_some()
    });
    assert!(st.devices.node.is_none(), "the window opens no node");
    assert_eq!(st.sidebar.devices.as_deref().map(<[_]>::len), Some(0));

    // Show a code here; the other device pairs with it.
    st.devices_cmd(0, DevCmd::Pair);
    st.devices_cmd(0, DevCmd::PairStep(PairEvent::ShowCode));
    pump_devices(&mut st, "a code", |s| {
        matches!(s.devices.pair, Some(Pair::Showing { .. }))
    });
    let Some(Pair::Showing { ticket, .. }) = st.devices.pair.clone() else {
        unreachable!()
    };
    let mut c = Client::connect(other.daemon.name()).unwrap();
    let p: api::PlanPreview = serde_json::from_value(
        c.call("devices.pair_with", json!({ "code": ticket }))
            .unwrap(),
    )
    .unwrap();
    c.call(
        "execute",
        json!({"plan_id": p.plan_id, "input_hash": p.input_hash}),
    )
    .unwrap();
    pump_devices(&mut st, "paired", |s| {
        matches!(s.devices.pair, Some(Pair::Paired(_)))
    });
    pump_devices(&mut st, "the peer row", |s| {
        s.sidebar.devices.as_ref().is_some_and(|r| r.len() == 1)
    });
    let peer = st.devices.peers[0].id;

    // A grant made in the shares dialog lands in the daemon.
    let source = st.library.sources[0].id.0.clone();
    st.devices_cmd(
        0,
        DevCmd::Grant {
            peer,
            source: source.clone(),
            subtree: String::new(),
            access: keel_net::Access::Read,
        },
    );
    pump_devices(&mut st, "the grant", |s| s.devices.grants.len() == 1);
    assert_eq!(st.devices.grants[0].source, source);
    // node:// paths go through the daemon: the other device shares nothing yet.
    let root = crate::devices::root_of(&peer);
    let listed = st.router.provider_for(&root).unwrap().list(&root);
    assert!(listed.unwrap().is_empty());
}

#[test]
fn the_window_ignores_changes_that_need_no_refresh() {
    use crate::library::{refresh_for, Refresh};
    let jobs = [
        "integrity.check",
        "hashing.set",
        "jobs.cancel",
        "media.index",
        "sources.index",
    ];
    for kind in jobs {
        assert_eq!(refresh_for(kind), Refresh::Nothing, "{kind}");
    }
    assert_eq!(refresh_for("volumes.set"), Refresh::Protection);
    assert_eq!(refresh_for("recents.note"), Refresh::Recents);
    assert_eq!(refresh_for("tags.add"), Refresh::All);
    assert_eq!(refresh_for(""), Refresh::All, "an older daemon names none");
}

#[test]
fn at_most_four_idle_connections_are_kept() {
    let s = served();
    let remote = Remote::connect(s.daemon.name()).unwrap();
    for _ in 0..6 {
        remote.put_back(Client::connect(s.daemon.name()).unwrap());
    }
    assert_eq!(remote.idle.lock().len(), MAX_IDLE);
}

/// Letting go of a daemon (library off, switching, Reconnect) ends its subscription: the
/// reader thread goes (its callback is dropped) without reporting the daemon lost.
#[test]
fn closing_the_events_ends_the_subscription() {
    let s = served();
    let remote = Remote::connect(s.daemon.name()).unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let _jobs = remote
        .subscribe(move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(200)); // the reader waits in its read
    remote.close_events();
    match rx.recv_timeout(Duration::from_secs(10)) {
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {}
        other => panic!("the subscription is still there: {other:?}"),
    }
}

/// "Open in this window" and "Reconnect" right after `daemon.stopping`, while the daemon
/// still closes the library (here: the test holds `library.lock`): the lost connection and
/// its banner stay until the library opens; it opens once the lock is free.
#[test]
fn reconnect_and_open_here_wait_for_a_stopping_daemon() {
    use crate::keys::Action;
    use crate::library::LibCmd;
    let _env = crate::settings::TEST_ENV.lock();
    let cfg = host_config(&crate::cli::profile()).unwrap();
    let start = || profile_daemon(&cfg);
    let lock_path = cfg
        .data_dir
        .join("library")
        .join(&cfg.library)
        .join("library.lock");
    // The stopped daemon lets go of the library once its clients are gone (a real one
    // when its process exits); the test takes the lock over, as a slow close would hold it.
    let stop = |st: &mut crate::state::AppState, daemon: keel_daemon::server::Daemon| {
        daemon.shutdown();
        pump_until(st, "lost", |s| s.library.lost);
        let r = st.library.remote().unwrap();
        r.close_events();
        r.idle.lock().clear();
        drop(daemon);
    };
    let hold = || {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        while f.try_lock().is_err() {
            assert!(Instant::now() < end, "the daemon kept the library");
            std::thread::sleep(Duration::from_millis(20));
        }
        f
    };
    let daemon = start();
    let mut st = crate::state::AppState::new(
        egui::Context::default(),
        Arc::new(keel_vfs::Router::new()),
        VPath::local(std::env::temp_dir()),
    );
    st.settings.library.name = cfg.library.clone();
    let router = st.router.clone();
    st.library.open(
        &cfg.library,
        router,
        crate::library::Via::Auto { spawn: false },
    );
    pump_until(&mut st, "attached", |s| s.library.is_open());
    stop(&mut st, daemon);
    let held = hold();

    // Still held: both give up, and the window stays as it was (read-only, its banner).
    st.library.release_wait = Duration::from_millis(300);
    for cmd in [LibCmd::OpenHere, LibCmd::Reconnect] {
        st.run(0, Action::Library(cmd.clone()));
        assert!(st.library.opening, "{cmd:?}");
        pump_until(&mut st, "gave up", |s| !s.library.opening);
        assert!(st.library.lost && st.library.remote().is_some(), "{cmd:?}");
        assert!(st.library.error.is_some(), "{cmd:?}");
    }

    // The daemon is back: Reconnect attaches to it.
    drop(held);
    let daemon = start();
    st.run(0, Action::Library(LibCmd::Reconnect));
    pump_until(&mut st, "reconnected", |s| !s.library.opening);
    assert!(!st.library.lost && st.library.remote().is_some());

    // It stops again; Open in this window waits until the library is let go.
    stop(&mut st, daemon);
    let held = hold();
    st.library.release_wait = Duration::from_secs(20);
    st.run(0, Action::Library(LibCmd::OpenHere));
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        if let Ok(m) = st.rx.recv_timeout(Duration::from_millis(30)) {
            st.apply(m);
        }
    }
    assert!(
        st.library.opening && st.library.lost,
        "waits, read-only meanwhile"
    );
    drop(held);
    pump_until(&mut st, "opened here", |s| !s.library.opening);
    assert!(!st.library.lost && st.library.remote().is_none() && st.library.lib.is_some());
    st.library.close_now();
}

/// The media entry of a local file.
fn media_entry(path: &std::path::Path) -> Entry {
    let md = std::fs::metadata(path).unwrap();
    Entry {
        path: VPath::local(path),
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        kind: keel_vfs::Kind::File,
        size: md.len(),
        modified: md.modified().ok(),
        hidden: false,
        is_link: false,
        encrypted: false,
        ext: "png".into(),
    }
}

/// Waits until `m` shows `key` (handing it the sidecar news, as the window does).
fn tile(
    m: &mut crate::media::Media,
    ctx: &egui::Context,
    key: &crate::media::TexKey,
    news: &crossbeam_channel::Receiver<(JobId, bool)>,
) -> egui::Vec2 {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        for (job, done) in news.try_iter() {
            m.news(job, done);
        }
        m.upload(ctx);
        if let crate::media::Tex::Ready(_, size) = m.get(key) {
            return size;
        }
        assert!(
            Instant::now() < until,
            "no thumbnail for {}",
            key.path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Attached, the media grid shows the daemon's sidecars: the first ask finds none, so the
/// source's sidecar job is started; its news brings the thumbnail, which is kept on disk
/// by content id. A file in no source is made in the window.
#[test]
fn attached_media_tiles_come_from_the_daemons_sidecars() {
    use crate::media::{tile_key, Media, Req};
    let s = served();
    let photo = s.files.path().join("docs").join("photo.png");
    image::RgbImage::from_pixel(600, 300, image::Rgb([10, 200, 30]))
        .save(&photo)
        .unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let loose = elsewhere.path().join("loose.png");
    image::RgbImage::from_pixel(300, 300, image::Rgb([200, 10, 30]))
        .save(&loose)
        .unwrap();
    let remote = Remote::connect(s.daemon.name()).unwrap();
    let b = LibraryBackend::Daemon(remote.clone());
    wait_job(&b, b.add_source(def(s.files.path())).unwrap());
    b.set_hashing(true, false).unwrap();
    wait_job(&b, b.hash(false).unwrap());
    b.sources().unwrap();
    let (tx, news) = crossbeam_channel::unbounded();
    let _jobs = remote
        .subscribe(move |e| {
            if let Event::Sidecars { job, done } = e {
                let _ = tx.send((job, done));
            }
        })
        .unwrap();
    let ctx = egui::Context::default();
    let mut m = Media::new(ctx.clone(), Arc::new(keel_vfs::Router::new()));
    m.sync_daemon(Some(&remote));

    let e = media_entry(&photo);
    let key = tile_key(&e, false, 96);
    let req = Req {
        real: e.path.clone(),
        entry: e,
    };
    m.want(0, vec![(key.clone(), 0, req)]);
    // 600x300 -> Thumb256 (256x128) -> shorter side 96.
    assert_eq!(tile(&mut m, &ctx, &key, &news), egui::vec2(192.0, 96.0));
    let kinds = b.job_kinds().unwrap();
    assert!(kinds.iter().any(|(_, k)| k == "sidecar"), "{kinds:?}");
    // Kept by content id and size (the alias names the file's content id).
    let dir = keel_vfs::cache_dir().join("daemon-thumbs");
    let cas = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter_map(|f| std::fs::read_to_string(f.path()).ok())
        .find(|c| c.len() == 64)
        .expect("an alias");
    assert!(dir.join(format!("{cas}-256.webp")).is_file());
    // The viewer's 1024 px thumbnail (no job makes those, never enlarged): the daemon
    // makes it on request.
    let e = media_entry(&photo);
    let big = crate::media::TexKey::of(&e, keel_core::SidecarKind::Thumb1024, 0);
    let req = Req {
        real: e.path.clone(),
        entry: e,
    };
    m.want(crate::media::VIEWER_SLOT, vec![(big.clone(), 0, req)]);
    assert_eq!(tile(&mut m, &ctx, &big, &news), egui::vec2(600.0, 300.0));
    assert!(dir.join(format!("{cas}-1024.webp")).is_file());

    // Not in a source: no job would make it, so the window does.
    let e = media_entry(&loose);
    let key = tile_key(&e, false, 96);
    let req = Req {
        real: e.path.clone(),
        entry: e,
    };
    m.want(0, vec![(key.clone(), 0, req)]);
    assert_eq!(tile(&mut m, &ctx, &key, &news), egui::vec2(96.0, 96.0));
}
