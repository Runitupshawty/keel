use super::*;
use crate::state::AppState;
use keel_core::{Change, OnConflict};
use std::path::PathBuf;

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
    let id = lib
        .add_source(SourceDef {
            label: "Docs".into(),
            root: VPath::local(&src),
            kind: SourceKind::Folder,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
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
        summary("C", SourceStatus::Offline { last_seen: None }),
        summary("D", SourceStatus::Error("denied".into())),
    ]);
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
    *lib.source(&id).unwrap().status.write() = SourceStatus::Offline { last_seen: Some(0) };
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
    let favs = LibSearch(lib.clone())
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
    let plan = &s.library.plan.as_ref().unwrap().plan;
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
    let plan = &s.library.plan.as_ref().unwrap().plan;
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
        result: Ok(lib),
        first_run: true,
        jobs: Vec::new(),
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
    let lib = Arc::new(Library::open(&data, "james").unwrap());
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
