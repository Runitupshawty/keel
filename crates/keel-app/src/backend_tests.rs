use super::*;

#[test]
fn a_running_daemon_is_used() {
    let mut connect = || Some("daemon");
    let mut spawned = false;
    let mut spawn = || {
        spawned = true;
        Ok(())
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
        Ok(())
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
    let mut spawn = || Ok(());
    match attach(
        Some(&mut connect),
        Some(&mut spawn),
        Duration::from_millis(200),
    ) {
        Attach::InProcess { note: Some(n) } => assert!(n.contains("did not answer"), "{n}"),
        other => panic!("{other:?}"),
    }
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
    wait_job(&b, b.hash().unwrap());
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
    let from = keel_vfs::library::path(&id.0, "docs/renamed.txt");
    let to = "again.txt".to_owned();
    st.run(0, Action::RenameTo { from, to });
    assert!(st.library.plan.is_none(), "no preview while lost");
    // Closing the window lets go without stopping anything.
    st.library.close_now();
    assert!(!st.library.is_open());
}

/// `LibraryUi::open` finds the profile's daemon by its socket name and attaches; the
/// window then never opens the library itself.
#[test]
fn open_attaches_to_the_profiles_running_daemon() {
    let _env = crate::settings::TEST_ENV.lock();
    let cfg = host_config(&crate::cli::profile()).unwrap();
    let daemon = keel_daemon::server::Daemon::start(keel_daemon::server::Options {
        cfg: cfg.clone(),
        ws: None,
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .unwrap();
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
    st.library
        .open(&cfg.library, router, crate::library::Via::Here);
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
