use super::*;
use crate::state::AppState;
use keel_api::types::Copies;
use keel_core::{Change, OnConflict, Redundancy};
use std::path::PathBuf;

/// The in-process plan of a preview dialog.
fn local(p: &Planned) -> &Plan {
    match p {
        Planned::Local(p) => p,
        Planned::Daemon(p) => panic!("a daemon's plan: {p:?}"),
    }
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("keel-lib-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A library under `<tmp>/data` with one indexed (and, with `hash`, hashed) source `Docs`:
/// `a.txt` and `sub/b.txt` share their content, `c.txt` is unique.
fn fixture(name: &str, hash: bool) -> (PathBuf, Arc<Library>, SourceId) {
    let tmp = temp(name);
    let src = tmp.join("docs");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), "same").unwrap();
    std::fs::write(src.join("sub").join("b.txt"), "same").unwrap();
    std::fs::write(src.join("c.txt"), "other").unwrap();
    let lib = Library::open(&tmp.join("data"), "test").unwrap();
    // Deterministic stores: no hashing writes behind a test's back unless asked for.
    lib.set_hash_after_walk(hash);
    let id = lib
        .add_source(SourceDef {
            label: "Docs".into(),
            root: VPath::local(&src),
            kind: SourceKind::Folder,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap();
    let job = lib.index(&id).unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    if hash {
        let job = lib.hash().unwrap();
        assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    }
    (tmp, Arc::new(lib), id)
}

fn state_with(lib: &Arc<Library>, tmp: &std::path::Path) -> AppState {
    let mut s = AppState::new(
        egui::Context::default(),
        Arc::new(Router::new()),
        VPath::local(tmp),
    );
    s.library.set_open(lib.clone());
    s
}

fn pump_until(s: &mut AppState, what: &str, done: impl Fn(&AppState) -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done(s) {
        assert!(Instant::now() < end, "timed out: {what}");
        if let Ok(m) = s.rx.recv_timeout(Duration::from_millis(30)) {
            s.apply(m);
        }
        s.drain();
    }
}

fn names(s: &AppState) -> Vec<String> {
    s.tab(0).entries().iter().map(|e| e.name.clone()).collect()
}

fn summary(label: &str, status: SourceStatus) -> SourceSummary {
    SourceSummary {
        id: SourceId(label.to_lowercase()),
        label: label.into(),
        root: VPath::local(std::env::temp_dir().join(label)),
        kind: SourceKind::Folder,
        status,
        generation: 1,
    }
}

#[test]
fn sidebar_rows_follow_source_status_and_stats() {
    let rows = source_rows(&[
        summary("A", SourceStatus::Online { indexed_at: None }),
        summary(
            "B",
            SourceStatus::Indexing {
                done: 1_200,
                total: 5_000,
            },
        ),
        summary(
            "C",
            SourceStatus::Offline {
                last_seen: None,
                reason: OfflineReason::Unreachable,
            },
        ),
        summary("D", SourceStatus::Error("denied".into())),
        summary(
            "E",
            SourceStatus::Offline {
                last_seen: None,
                reason: OfflineReason::RootMismatch,
            },
        ),
    ]);
    let adopt: Vec<bool> = rows.iter().map(|r| r.adopt).collect();
    assert_eq!(adopt, [false, false, false, false, true]);
    assert!(
        rows[4].detail.contains("different folder"),
        "{}",
        rows[4].detail
    );
    let rows = &rows[..4];
    let got: Vec<(Dot, &str)> = rows.iter().map(|r| (r.dot, r.detail.as_str())).collect();
    assert_eq!(
        got,
        [
            (Dot::Online, "not indexed yet"),
            (Dot::Indexing, "indexing 1,200 / 5,000"),
            (Dot::Offline, "offline (last seen never)"),
            (Dot::Error, "denied"),
        ]
    );
    // From a real library: stats and the source's row.
    let (tmp, lib, id) = fixture("sidebar", false);
    let stats = lib.stats();
    assert_eq!(
        (stats.sources, stats.files, stats.offline_sources),
        (1, 3, 0)
    );
    let s = state_with(&lib, &tmp);
    let rows = source_rows(&s.library.sources);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].id.clone(), rows[0].label.as_str()), (id, "Docs"));
    assert_eq!(rows[0].dot, Dot::Online);
    assert!(rows[0].detail.starts_with("indexed "), "{}", rows[0].detail);
}

#[test]
fn library_tab_lists_from_the_index_and_works_offline() {
    let (tmp, lib, id) = fixture("tab", false);
    let mut s = state_with(&lib, &tmp);
    let root = vlib::path(&id.0, "");
    s.run(0, Action::Navigate(root.clone()));
    pump_until(&mut s, "library root listed", |s| {
        s.tab(0).listed_dir.as_ref() == Some(&root)
    });
    assert_eq!(names(&s), ["sub", "a.txt", "c.txt"], "folders first");
    assert_eq!(s.tab(0).title(), "Docs");
    assert_eq!(s.tab(0).entries()[1].ext, "txt");

    // Into `sub` and back up, inside the library tree.
    s.tab_mut(0).click("sub", false, false);
    s.run(0, Action::Enter);
    let sub = vlib::path(&id.0, "sub");
    pump_until(&mut s, "sub listed", |s| {
        s.tab(0).listed_dir.as_ref() == Some(&sub)
    });
    assert_eq!(names(&s), ["b.txt"]);
    s.run(0, Action::Up);
    pump_until(&mut s, "back at the root", |s| {
        s.tab(0).listed_dir.as_ref() == Some(&root)
    });

    // Offline: still listed (last generation); reading says why it cannot.
    *lib.source(&id).unwrap().status.write() = SourceStatus::Offline {
        last_seen: Some(0),
        reason: OfflineReason::Unreachable,
    };
    s.run(0, Action::Refresh);
    let before = s.tab(0).listed_req;
    pump_until(&mut s, "relisted offline", |s| s.tab(0).listed_req > before);
    assert_eq!(names(&s).len(), 3);
    assert!(s.tab(0).error.is_none());
    let a = vlib::path(&id.0, "a.txt");
    let err = s
        .router
        .provider_for(&a)
        .unwrap()
        .local_copy(&a)
        .unwrap_err();
    assert!(format!("{err:#}").contains("offline (last seen"), "{err:#}");
}

#[test]
fn tags_and_favorites_round_trip_through_the_ui_model() {
    let (tmp, lib, id) = fixture("tags", false);
    let tag = lib.create_tag("work", Some("#e5484d"), None).unwrap();
    let mut s = state_with(&lib, &tmp);
    let root = vlib::path(&id.0, "");
    s.run(0, Action::Navigate(root.clone()));
    pump_until(&mut s, "listed", |s| {
        s.tab(0).listed_dir.as_ref() == Some(&root)
    });
    s.tab_mut(0).click("a.txt", false, false);
    let real = VPath::local(tmp.join("docs").join("a.txt"));
    let has = |s: &AppState, t: TagId| s.library.tagged.get(&real).is_some_and(|v| v.contains(&t));
    let settle = |s: &mut AppState, what: &str, want: bool, t: TagId| {
        let end = Instant::now() + Duration::from_secs(20);
        while has(s, t) != want {
            assert!(Instant::now() < end, "timed out: {what}");
            s.library.refresh_meta();
            pump_until(s, "meta", |_| true);
            std::thread::sleep(Duration::from_millis(50));
            s.drain();
        }
    };

    s.run(0, Action::Library(LibCmd::SetTag { tag, on: true }));
    settle(&mut s, "tagged", true, tag);
    assert_eq!(
        s.library
            .tags
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["work"]
    );
    s.run(0, Action::Library(LibCmd::SetTag { tag, on: false }));
    settle(&mut s, "untagged", false, tag);

    // Ctrl+D toggles the favorite star; Favorites lists it.
    s.run(0, Action::Library(LibCmd::ToggleFavorite));
    settle(&mut s, "favorite", true, FAVORITES);
    let favs = LibSearch(LibraryBackend::InProcess(lib.clone()))
        .query(&Query {
            text: FAVORITES_QUERY.into(),
            max: 10,
            ..Query::default()
        })
        .unwrap();
    assert_eq!(
        favs.iter().map(|h| h.path.clone()).collect::<Vec<_>>(),
        std::slice::from_ref(&real)
    );
    s.run(0, Action::Library(LibCmd::ToggleFavorite));
    settle(&mut s, "not favorite", false, FAVORITES);
}

#[test]
fn preview_dialog_lists_changes_and_warnings() {
    let srcs = [summary("NAS", SourceStatus::Online { indexed_at: None })];
    let in_nas = srcs[0].root.join("x.bin");
    let texts: Vec<String> = [
        Warning::LastCopy {
            path: in_nas.clone(),
            files: 1,
        },
        Warning::LastCopy {
            path: in_nas.clone(),
            files: 3,
        },
        Warning::OfflineSource {
            source: SourceId("nas".into()),
            label: "NAS".into(),
        },
        Warning::Permanent {
            path: in_nas.clone(),
        },
        Warning::Exists {
            path: in_nas.clone(),
            on_conflict: OnConflict::Skip,
        },
        Warning::ContentUnverified {
            path: in_nas.clone(),
            files: 2,
        },
    ]
    .iter()
    .map(|w| warning_text(w, &srcs))
    .collect();
    assert_eq!(texts[0], "1 file is the last copy of its content (x.bin)");
    assert_eq!(
        texts[1],
        "3 files are the last copies of their content (x.bin)"
    );
    assert!(texts[2].starts_with("Source offline: NAS"));
    assert_eq!(texts[3], "Permanent delete on NAS: there is no trash there");
    assert_eq!(texts[4], "x.bin already exists there: skipped");
    assert!(texts[5].starts_with("2 files are not hashed yet"));

    // Del in a library tab plans through validate/preview (a worker) and shows it.
    let (tmp, lib, id) = fixture("plan", true);
    let mut s = state_with(&lib, &tmp);
    let root = vlib::path(&id.0, "");
    s.run(0, Action::Navigate(root.clone()));
    pump_until(&mut s, "listed", |s| {
        s.tab(0).listed_dir.as_ref() == Some(&root)
    });
    s.tab_mut(0).click("c.txt", false, false);
    s.run(0, Action::Delete);
    assert!(s.dialog.is_none(), "no trash confirm: the plan dialog asks");
    pump_until(&mut s, "planned", |s| s.library.plan.is_some());
    let plan = local(&s.library.plan.as_ref().unwrap().plan);
    let c = VPath::local(tmp.join("docs").join("c.txt"));
    assert_eq!(
        plan.op,
        Op::Delete {
            paths: vec![c.clone()]
        }
    );
    assert!(matches!(
        plan.changes[..],
        [Change {
            files: 1,
            bytes: 5,
            ..
        }]
    ));
    assert_eq!(plan_summary(plan), "Delete 1 file (5 B)");
    let lines: Vec<String> = plan
        .warnings
        .iter()
        .map(|w| warning_text(w, &s.library.sources))
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l == "1 file is the last copy of its content (c.txt)"),
        "{lines:?}"
    );
    // The dialog draws; Cancel (Esc) closes it without touching the file.
    let ctx = s.ctx.clone();
    let _ = ctx.run(Default::default(), |ctx| {
        crate::library_ui::windows(ctx, &mut s, &mut Vec::new());
    });
    assert!(s.library.plan.is_some());
    let esc = egui::RawInput {
        events: vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }],
        ..Default::default()
    };
    let _ = ctx.run(esc, |ctx| {
        crate::library_ui::windows(ctx, &mut s, &mut Vec::new());
    });
    assert!(s.library.plan.is_none());
    assert!(tmp.join("docs").join("c.txt").exists());
}

#[test]
fn search_tab_switches_between_platform_and_library_backends() {
    struct Fixed;
    impl Searcher for Fixed {
        fn query(&self, _: &Query) -> anyhow::Result<Vec<Hit>> {
            Ok(vec![Hit {
                path: VPath::local(std::env::temp_dir().join("platform.txt")),
                is_dir: false,
                size: 1,
                modified: None,
            }])
        }
        fn available(&self) -> bool {
            true
        }
    }
    let (tmp, lib, _) = fixture("search", false);
    let mut s = state_with(&lib, &tmp);
    s.searcher = Some(Arc::new(Fixed));
    s.run(0, Action::Search);
    let run = |s: &mut AppState, library: bool| {
        let tab = s.tab_mut(0);
        tab.library_search = library;
        if let TabKind::Search { query, due, .. } = &mut tab.kind {
            *query = "c".into();
            *due = Some(Instant::now());
        }
        s.tick();
        pump_until(s, "searched", |s| !s.tab(0).loading);
        s.tab(0)
            .entries()
            .iter()
            .map(|e| e.path.name().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(run(&mut s, false), ["platform.txt"]);
    assert_eq!(run(&mut s, true), ["c.txt"], "library index");
    assert_eq!(s.tab(0).title(), "Library: c");
    // Ctrl+Enter on a library hit opens its library folder with it selected.
    let hit = s.tab(0).entries()[0].name.clone();
    s.tab_mut(0).click(&hit, false, false);
    s.run(0, Action::OpenLocation);
    let q = s.active;
    assert_eq!(s.tab(q).dir.scheme, vlib::SCHEME);
    assert_eq!(s.tab(q).cursor.as_deref(), Some("c.txt"));
}

#[test]
fn duplicate_finder_groups_copies_and_keeps_one() {
    let (tmp, lib, _) = fixture("dups", true);
    let groups = load_dups(&lib, 1).unwrap();
    assert_eq!(groups.len(), 1);
    let g = &groups[0];
    assert_eq!((g.size, g.records.len()), (4, 2));
    assert_eq!(reclaimable(&groups), 4);
    let mut paths: Vec<String> = g.records.iter().map(|r| r.path.name().to_owned()).collect();
    paths.sort();
    assert_eq!(paths, ["a.txt", "b.txt"]);
    assert!(g.records.iter().all(|r| r.source == "Docs" && !r.offline));
    assert_eq!(
        keep_one(g, 0),
        Op::Delete {
            paths: vec![g.records[1].path.clone()]
        }
    );

    // Through the UI model: the finder loads, Keep this one previews the delete.
    let mut s = state_with(&lib, &tmp);
    s.run(0, Action::Library(LibCmd::Duplicates));
    pump_until(&mut s, "groups", |s| {
        s.library.dups.as_ref().is_some_and(|d| d.groups.is_some())
    });
    assert_eq!(s.library.dup_summary, Some((1, 4)));
    s.run(0, Action::Library(LibCmd::KeepOne { group: 0, keep: 1 }));
    pump_until(&mut s, "keep-one plan", |s| s.library.plan.is_some());
    let plan = local(&s.library.plan.as_ref().unwrap().plan);
    assert_eq!(
        plan.op,
        Op::Delete {
            paths: vec![g.records[0].path.clone()]
        }
    );
    // The other copy remains: no last-copy warning.
    assert!(!plan
        .warnings
        .iter()
        .any(|w| matches!(w, Warning::LastCopy { .. })));
}

#[test]
fn paths_map_between_library_and_real() {
    let srcs = [summary("Pics", SourceStatus::Online { indexed_at: None })];
    let root = srcs[0].root.clone();
    let deep = root.join("2026").join("x.jpg");
    assert_eq!(
        locate(&srcs, &deep),
        Some((SourceId("pics".into()), "2026/x.jpg".into()))
    );
    assert_eq!(
        real_of(&srcs, &vlib::path("pics", "2026/x.jpg")),
        Some(deep)
    );
    assert_eq!(real_of(&srcs, &vlib::path("pics", "")), Some(root.clone()));
    let sibling = VPath::local(format!("{}2", root.display()));
    assert_eq!(locate(&srcs, &sibling), None, "whole components only");
    assert_eq!(count(1_234_567), "1,234,567");
    assert_eq!(label_for(r"D:\Photos\"), "Photos");
}

#[test]
fn first_run_offers_documents_as_a_source() {
    let (tmp, lib, _) = fixture("firstrun", false);
    let mut s = AppState::new(
        egui::Context::default(),
        Arc::new(Router::new()),
        VPath::local(&tmp),
    );
    s.library.opening = true;
    s.library_msg(LibMsg::Opened {
        result: Ok(Opened {
            events: Some(lib.jobs().subscribe()),
            backend: LibraryBackend::InProcess(lib),
            jobs: Vec::new(),
            spawned: false,
            note: None,
        }),
        first_run: true,
    });
    assert!(s.library.is_open());
    let offer = s
        .toasts
        .list
        .iter()
        .find(|t| t.action.is_some())
        .expect("offer toast");
    assert_eq!(
        offer.action.as_ref().unwrap().1,
        Action::Library(LibCmd::AddDocuments)
    );
}

/// Manual end-to-end check (GPU): `KEEL_LIVE_ROOT=C:\KeelDemo KEEL_LIVE_DATA=<data dir>
/// KEEL_SHOT_DIR=<dir> cargo test -p keel-app -- --ignored library_live --nocapture`. Adds the
/// folder as a source through the UI model, waits for indexing and hashing, tags a file and
/// renders the Overview, the library tab, the tag picker and the duplicate finder to
/// `library-*.png`. Prints the source id (for a session that reopens these tabs).
#[test]
#[ignore]
fn library_live() {
    use crate::app::{App, Boot};
    use egui_kittest::Harness;
    let (Some(root), Some(data), Some(shots)) = (
        std::env::var_os("KEEL_LIVE_ROOT").map(PathBuf::from),
        std::env::var_os("KEEL_LIVE_DATA").map(PathBuf::from),
        std::env::var_os("KEEL_SHOT_DIR").map(PathBuf::from),
    ) else {
        return;
    };
    let lib = Arc::new(Library::open(&data, "main").unwrap());
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 860.0))
        .wgpu()
        .build_eframe(|cc| App::new(cc, Boot::at(VPath::local(&root))));
    lib.set_router(h.state().state.router.clone());
    h.state_mut().state.library.set_open(lib.clone());
    let wait = |h: &mut Harness<App>, what: &str, done: &dyn Fn(&AppState) -> bool| {
        let end = Instant::now() + Duration::from_secs(60);
        while !done(&h.state().state) {
            assert!(Instant::now() < end, "timed out: {what}");
            h.step();
            std::thread::sleep(Duration::from_millis(30));
        }
        h.run_steps(3);
    };
    let shot = |h: &mut Harness<App>, name: &str| {
        h.run_steps(3);
        h.render()
            .unwrap()
            .save(shots.join(format!("library-{name}.png")))
            .unwrap();
    };
    let run = |h: &mut Harness<App>, a: Action| h.state_mut().state.run(0, a);
    run(
        &mut h,
        Action::Library(LibCmd::Register(SourceDef {
            label: "KeelDemo".into(),
            root: VPath::local(&root),
            kind: SourceKind::Folder,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })),
    );
    wait(&mut h, "source added", &|s| !s.library.sources.is_empty());
    let id = h.state().state.library.sources[0].id.clone();
    println!("source id: {}", id.0);
    // Indexing, then hashing (started when the index job ends).
    wait(&mut h, "indexed and hashed", &|s| {
        s.library.jobs.values().any(|j| j.kind == "hash")
            && !s.library.jobs.values().any(|j| j.active())
            && s.library.stats.files > 0
    });
    run(&mut h, Action::Library(LibCmd::Overview));
    wait(&mut h, "overview numbers", &|s| {
        s.library.dup_summary.is_some_and(|(n, _)| n > 0) && s.library.stats.unique_content > 0
    });
    shot(&mut h, "overview");

    // The library tab, with a tagged file.
    run(&mut h, Action::Library(LibCmd::OpenSource(id.clone())));
    let dir = vlib::path(&id.0, "");
    wait(&mut h, "library tab", &|s| {
        s.tab(0).listed_dir.as_ref() == Some(&dir)
    });
    h.state_mut()
        .state
        .tab_mut(0)
        .click("todo.txt", false, false);
    run(&mut h, Action::Library(LibCmd::TagPicker));
    run(
        &mut h,
        Action::Library(LibCmd::CreateTag {
            name: "work".into(),
            color: "#e5484d".into(),
        }),
    );
    run(&mut h, Action::Library(LibCmd::ToggleFavorite));
    let todo = VPath::local(root.join("todo.txt"));
    wait(&mut h, "tagged", &|s| {
        s.library.tagged.get(&todo).is_some_and(|t| t.len() == 2)
    });
    shot(&mut h, "tag-picker");
    h.state_mut().state.library.picker = None;
    shot(&mut h, "tab");

    run(&mut h, Action::Library(LibCmd::Duplicates));
    wait(&mut h, "duplicate groups", &|s| {
        s.library.dups.as_ref().is_some_and(|d| d.groups.is_some())
    });
    shot(&mut h, "duplicates");
    h.state_mut().state.library.close_now();
}

// --- Task 33 ---

fn volume(id: &str, state: VolumeState, backup: bool, capacity: Option<(u64, u64)>) -> Volume {
    Volume {
        id: id.into(),
        label: id.to_uppercase(),
        kind: VolumeKind::Removable,
        failure_domain: format!("disk:{id}"),
        domain_set: id == "b",
        state,
        last_seen: if capacity.is_some() { 1_700_000_000 } else { 0 },
        backup,
        capacity,
    }
}

#[test]
fn per_source_table_model() {
    let stats = |label: &str, files, hashed, last_walk, offline| keel_core::SourceStats {
        id: SourceId(label.to_lowercase()),
        label: label.into(),
        files,
        folders: 1_200,
        bytes: 3_000_000,
        hashed_files: hashed,
        last_walk,
        offline,
    };
    let rows = source_count_rows(&[
        stats("Photos", 12_345, 9_876, Some(1_700_000_000), false),
        stats("Empty", 0, 0, None, true),
    ]);
    let r = &rows[0];
    assert_eq!(
        (
            r.label.as_str(),
            r.files.as_str(),
            r.folders.as_str(),
            r.size.as_str(),
            r.hashed.as_str(),
            r.offline
        ),
        ("Photos", "12,345", "1,200", "3 MB", "80 %", false)
    );
    assert_eq!(r.last_walk, when(Some(1_700_000_000)));
    assert_eq!(
        (
            rows[1].hashed.as_str(),
            rows[1].last_walk.as_str(),
            rows[1].offline
        ),
        ("—", "never", true)
    );
}

#[test]
fn volume_table_and_protection_card_models() {
    let rows = volume_rows(&[
        volume("a", VolumeState::Online, true, Some((1_000_000, 4_000_000))),
        volume("b", VolumeState::Lost, false, None),
    ]);
    assert_eq!(
        (rows[0].label.as_str(), rows[0].kind, rows[0].backup),
        ("A", "Removable", true)
    );
    assert_eq!(rows[0].usage, "1 MB / 4 MB");
    assert_eq!(
        (rows[0].domain.as_str(), rows[0].domain_set),
        ("disk:a", false)
    );
    assert!(rows[1].domain_set);
    assert_ne!(rows[0].last_seen, "never");
    assert_eq!(
        (rows[1].usage.as_str(), rows[1].last_seen.as_str()),
        ("", "never")
    );
    assert_eq!(state_text(rows[1].state), "lost");

    let mut p = ProtectionSummary {
        single_copy: 1,
        single_domain: 1_200,
        unbacked: 3,
        drifted: 0,
        unchecked: 0,
        offline_volumes: 2,
        capacity: Vec::new(),
    };
    let lines = protection_lines(&p);
    let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "1 file with one copy only",
            "1,200 files with every copy on one disk",
            "3 files not backed up",
            "0 files changed since last check",
            "2 volumes offline",
        ]
    );
    let warns: Vec<bool> = lines.iter().map(|l| l.warn).collect();
    assert_eq!(warns, [true, true, false, false, true]);
    // Every number says how it was computed.
    assert!(lines.iter().all(|l| l.how.len() > 40));
    // Nothing hashed yet: the unknown is said first, and the counts say they are partial.
    p.single_copy = 0;
    p.unchecked = 5;
    let lines = protection_lines(&p);
    assert_eq!(
        (lines[0].text.as_str(), lines[0].warn),
        ("5 files not checked yet", true)
    );
    assert_eq!(
        lines[1].text,
        "0 files with one copy only (of those checked)"
    );
}

#[test]
fn copies_badge_model() {
    let copy = |label: &str, state: VolumeState, backup: bool| keel_core::CopyAt {
        record: RecordRef {
            source: SourceId("s".into()),
            id: 1,
        },
        path: VPath::local(std::env::temp_dir().join(label).join("x.jpg")),
        source_label: label.into(),
        volume: volume(label, state, backup, None),
        claimed: label == "f",
    };
    let b = badge_of(&Copies::from(Redundancy {
        copies: 2,
        failure_domains: 1,
        backed_up: false,
        offline_copies: 0,
        locations: vec![
            copy("c", VolumeState::Online, false),
            copy("d", VolumeState::Online, false),
        ],
    }));
    assert_eq!(b.text, "2× · 1");
    assert!(b.risk, "one domain");
    assert!(
        b.hover
            .starts_with("2 copies in 1 failure domain; not backed up"),
        "{}",
        b.hover
    );
    assert!(b.hover.contains("x.jpg on C (disk:c)"), "{}", b.hover);
    let b = badge_of(&Copies::from(Redundancy {
        copies: 3,
        failure_domains: 2,
        backed_up: true,
        offline_copies: 1,
        locations: vec![
            copy("e", VolumeState::Archived, true),
            copy("f", VolumeState::Online, false),
        ],
    }));
    assert_eq!(b.text, "3× · 2");
    assert!(!b.risk);
    assert!(b
        .hover
        .starts_with("3 copies in 2 failure domains (1 offline); backed up"));
    assert!(b.hover.contains("[archived] [backup]"), "{}", b.hover);
    assert!(
        b.hover.contains("[claimed by the device, not counted]"),
        "{}",
        b.hover
    );
    // The preview dialog's failure-domain warning.
    let text = warning_text(
        &Warning::SingleDomain {
            path: VPath::local(std::env::temp_dir().join("x.jpg")),
            files: 2,
        },
        &[],
    );
    assert!(
        text.starts_with("2 files are the only copy outside one failure domain"),
        "{text}"
    );
}

#[test]
fn overview_reads_protection_and_volumes_and_badges_load_off_the_ui_thread() {
    let (tmp, lib, _) = fixture("protection", true);
    let mut s = state_with(&lib, &tmp);
    s.library.refresh_stats();
    pump_until(&mut s, "protection", |s| s.library.protection.is_some());
    let p = s.library.protection.clone().unwrap();
    // a.txt and sub/b.txt share content on one disk; c.txt is alone.
    assert_eq!((p.single_copy, p.single_domain, p.unbacked), (1, 1, 2));
    assert_eq!(s.library.volumes.len(), 1);
    let vol = s.library.volumes[0].id.clone();

    // Badges: none at first (the folder is asked for), then from a worker.
    let a = VPath::local(tmp.join("docs").join("a.txt"));
    assert!(s.library.badge(&a).is_none());
    s.library.ask_badges();
    pump_until(&mut s, "badges", |s| s.library.badges.contains_key(&a));
    let b = s.library.badges[&a].clone();
    assert_eq!((b.text.as_str(), b.risk), ("2× · 1", true));
    assert_eq!(
        s.library.badges[&VPath::local(tmp.join("docs").join("c.txt"))].text,
        "1× · 1"
    );

    // State and backup edits go through workers and come back on the next read.
    s.library_cmd(
        0,
        LibCmd::SetVolumeState {
            volume: vol.clone(),
            state: VolumeState::Archived,
        },
    );
    s.library_cmd(
        0,
        LibCmd::SetBackup {
            volume: vol.clone(),
            on: true,
        },
    );
    s.library_cmd(
        0,
        LibCmd::SetDomain {
            volume: vol,
            domain: "disk:shelf".into(),
        },
    );
    assert!(s.library.badges.is_empty(), "badges are read again");
    let end = Instant::now() + Duration::from_secs(20);
    while !s.library.volumes.first().is_some_and(|v| {
        v.state == VolumeState::Archived && v.backup && v.failure_domain == "disk:shelf"
    }) {
        assert!(Instant::now() < end, "timed out: volume edits");
        s.library.refresh_stats();
        std::thread::sleep(Duration::from_millis(50));
        while let Ok(m) = s.rx.try_recv() {
            s.apply(m);
        }
        s.drain();
    }
}

#[test]
fn a_recount_after_watcher_changes_reads_protection_and_badges_again() {
    let (tmp, lib, id) = fixture("recount", true);
    let mut s = state_with(&lib, &tmp);
    s.library.follow_recounts();
    s.library.refresh_stats();
    pump_until(&mut s, "protection", |s| s.library.protection.is_some());
    let a = VPath::local(tmp.join("docs").join("a.txt"));
    assert!(s.library.badge(&a).is_none(), "its folder is asked for");
    s.library.ask_badges();
    pump_until(&mut s, "badges", |s| s.library.badges.contains_key(&a));
    assert_eq!(s.library.protection.as_ref().unwrap().single_copy, 1);
    // What a watcher does when sub/b.txt is deleted outside Keel.
    let b = tmp.join("docs").join("sub").join("b.txt");
    std::fs::remove_file(&b).unwrap();
    let revision = lib.protection_revision();
    let src = lib.source(&id).unwrap();
    Indexer::apply_change(&src, keel_core::ChangeEvent::Removed(VPath::local(&b))).unwrap();
    let end = Instant::now() + Duration::from_secs(60);
    while lib.protection_revision() == revision {
        assert!(Instant::now() < end, "timed out: recount");
        std::thread::sleep(Duration::from_millis(50));
    }
    s.library.follow_recounts();
    assert!(s.library.badges.is_empty(), "badges are read again");
    s.library.refresh_stats();
    pump_until(&mut s, "recounted card", |s| {
        s.library
            .protection
            .as_ref()
            .is_some_and(|p| p.single_copy == 2)
    });
}

/// Manual end-to-end check (GPU), like `library_live`: `KEEL_LIVE_ROOT=C:\KeelDemo
/// KEEL_LIVE_DATA=<data dir> KEEL_SHOT_DIR=<dir> KEEL_CONFIG_DIR=<config dir> cargo test -p
/// keel-app -- --ignored protection_live --nocapture`. Adds the folder as a source, waits for
/// indexing and hashing, renders the Overview (protection card, volume table) to
/// `protection-harness.png`, and leaves a session that opens the Overview for a release run.
#[test]
#[ignore]
fn protection_live() {
    use crate::app::{App, Boot};
    use egui_kittest::Harness;
    let (Some(root), Some(data), Some(shots)) = (
        std::env::var_os("KEEL_LIVE_ROOT").map(PathBuf::from),
        std::env::var_os("KEEL_LIVE_DATA").map(PathBuf::from),
        std::env::var_os("KEEL_SHOT_DIR").map(PathBuf::from),
    ) else {
        return;
    };
    let lib = Arc::new(Library::open(&data, "main").unwrap());
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .wgpu()
        .build_eframe(|cc| App::new(cc, Boot::at(VPath::local(&root))));
    lib.set_router(h.state().state.router.clone());
    h.state_mut().state.library.set_open(lib.clone());
    let wait = |h: &mut Harness<App>, what: &str, done: &dyn Fn(&AppState) -> bool| {
        let end = Instant::now() + Duration::from_secs(60);
        while !done(&h.state().state) {
            assert!(Instant::now() < end, "timed out: {what}");
            h.step();
            std::thread::sleep(Duration::from_millis(30));
        }
        h.run_steps(3);
    };
    let run = |h: &mut Harness<App>, a: Action| h.state_mut().state.run(0, a);
    run(
        &mut h,
        Action::Library(LibCmd::Register(SourceDef {
            label: "KeelDemo".into(),
            root: VPath::local(&root),
            kind: SourceKind::Folder,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })),
    );
    wait(&mut h, "indexed and hashed", &|s| {
        s.library.jobs.values().any(|j| j.kind == "hash")
            && !s.library.jobs.values().any(|j| j.active())
            && s.library.stats.files > 0
    });
    run(&mut h, Action::Library(LibCmd::Overview));
    wait(&mut h, "protection", &|s| {
        s.library
            .protection
            .as_ref()
            .is_some_and(|p| p.single_copy + p.single_domain > 0)
            && !s.library.volumes.is_empty()
    });
    let p = h.state().state.library.protection.clone().unwrap();
    println!("protection: {p:?}");
    println!("volumes: {:?}", h.state().state.library.volumes);
    h.run_steps(3);
    h.render()
        .unwrap()
        .save(shots.join("protection-harness.png"))
        .unwrap();
    h.state_mut().state.library.close_now();
    // A release run opens on the Overview (pane 1) beside the folder.
    if let Some(path) = crate::session::Session::path() {
        let session = crate::session::Session {
            panes: vec![vec![overview_path()], vec![VPath::local(&root)]],
            ..crate::session::Session::single(VPath::local(&root))
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&session).unwrap()).unwrap();
        println!("session: {}", path.display());
    }
}

/// Review M7: input pauses the library's background jobs whatever the hashing policy
/// (hashing itself only pauses when idle-only).
#[test]
fn input_notes_activity_whatever_the_hashing_policy() {
    let (tmp, lib, _) = fixture("activity", false);
    let mut s = state_with(&lib, &tmp);
    s.library.policy = Hashing::PauseOnBattery;
    s.library.sync_hashing();
    s.library_tick();
    assert!(!lib.user_active(), "no input yet");
    let mut input = egui::RawInput::default();
    input
        .events
        .push(egui::Event::PointerMoved(egui::pos2(10.0, 10.0)));
    let _ = s.ctx.run(input, |_| {});
    s.library_tick();
    assert!(
        lib.user_active(),
        "input pauses the media and integrity jobs"
    );
    s.library.close_now();
}
