use super::*;
use crate::state::Msg;
use keel_search::{Hit, Query, Searcher};
use keel_vfs::{Router, VPath};
use parking_lot::Mutex;
use std::time::Duration;

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("keel-bm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn state(dir: &std::path::Path) -> AppState {
    AppState::new(
        egui::Context::default(),
        Arc::new(Router::new()),
        VPath::local(dir),
    )
}

fn set_query(s: &mut AppState, text: &str) {
    if let TabKind::Search { query, due, .. } = &mut s.tab_mut(0).kind {
        *query = text.into();
        *due = Some(Instant::now());
    }
}

fn query_of(s: &AppState) -> String {
    match &s.tab(0).kind {
        TabKind::Search { query, .. } => query.clone(),
        _ => panic!("not a search tab"),
    }
}

fn pump_until(s: &mut AppState, what: &str, done: impl Fn(&AppState) -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done(s) {
        assert!(Instant::now() < end, "timed out: {what}");
        if let Ok(m) = s.rx.recv_timeout(Duration::from_millis(30)) {
            s.apply(m);
        }
        s.tick();
    }
}

#[test]
fn shortcuts_parse_print_and_conflict() {
    let (m, k) = parse_shortcut("Ctrl+Shift+1").unwrap();
    assert_eq!(
        (m.command, m.shift, m.alt, k),
        (true, true, false, Key::Num1)
    );
    assert_eq!(shortcut_text(m, k), "Ctrl+Shift+1");
    assert_eq!(parse_shortcut("Alt+F9"), Some((Modifiers::ALT, Key::F9)));
    assert_eq!(parse_shortcut("Hyper+X"), None);
    assert_eq!(parse_shortcut(""), None);

    let mine = View {
        id: 7,
        name: "Logs".into(),
        shortcut: "Ctrl+Shift+1".into(),
        ..View::default()
    };
    let all = vec![(Store::Profile, mine)];
    assert_eq!(shortcut_problem("", None, &all), None);
    assert_eq!(shortcut_problem("Ctrl+Alt+L", None, &all), None);
    assert_eq!(shortcut_problem("F9", None, &all), None);
    for (bad, why) in [
        ("L", "Use Ctrl or Alt"),
        ("Shift+L", "Use Ctrl or Alt"),
        ("Ctrl+D", "Keel's own"),
        ("Ctrl+F", "Keel's own"),
        ("F5", "Keel's own"),
        ("Ctrl+Shift+1", "\"Logs\" already"),
        ("Ctrl+Nope", "not a key"),
    ] {
        let p = shortcut_problem(bad, None, &all).unwrap_or_default();
        assert!(p.contains(why), "{bad}: {p}");
    }
    // A bookmark keeps its own shortcut when edited.
    assert_eq!(
        shortcut_problem("Ctrl+Shift+1", Some((Store::Profile, 7)), &all),
        None
    );
    // The same id in the other store is another bookmark.
    assert!(shortcut_problem("Ctrl+Shift+1", Some((Store::Library, 7)), &all).is_some());
}

#[test]
fn sort_and_suggestions() {
    for sort in [
        (SortKey::Name, true),
        (SortKey::Name, false),
        (SortKey::Size, false),
        (SortKey::Modified, true),
        (SortKey::Ext, false),
    ] {
        let text = sort_text(sort);
        assert!(keel_core::valid_sort(&text), "{text}");
        assert_eq!(parse_sort(&text).unwrap_or((SortKey::Name, true)), sort);
    }
    assert_eq!(sort_text((SortKey::Name, true)), "");
    assert_eq!(sort_text((SortKey::Size, false)), "size:desc");
    // A key this window cannot sort by yet is skipped for the next one.
    assert_eq!(
        parse_sort("run_count:desc,modified"),
        Some((SortKey::Modified, true))
    );
    let history: Vec<String> = ["report", "notes", "Invoice 2026"]
        .map(String::from)
        .to_vec();
    assert_eq!(suggestions(&history, "o", 8), history);
    assert_eq!(suggestions(&history, "INVOICE", 8), ["Invoice 2026"]);
    assert_eq!(suggestions(&history, "notes", 8), Vec::<String>::new());
    assert_eq!(suggestions(&history, "", 2), ["report", "notes"]);
}

/// Bookmark round trip in a profile: Ctrl+D's dialog takes the search tab's text,
/// backend, filter and toggles; saved, it is listed, opens in a search tab with all of
/// them, moves inside its folder, survives config.toml and is deleted.
#[test]
fn profile_bookmarks_round_trip() {
    let dir = temp("profile");
    let mut s = state(&dir);
    s.run(0, Action::Search);
    set_query(&mut s, "  invoice  ");
    s.tab_mut(0).search_filter = "Document".into();
    s.tab_mut(0).set_match_toggles(keel_core::MatchToggles {
        case: true,
        ..Default::default()
    });
    s.tab_mut(0).sort = (SortKey::Modified, false);
    s.run(0, Action::Find(FindCmd::AddBookmark));
    let ed = s.find.editor.as_ref().expect("the dialog");
    assert_eq!(ed.store, Store::Profile);
    assert_eq!(ed.replaces, None);
    let v = &ed.view;
    assert_eq!(
        (v.name.as_str(), v.query.as_str(), v.backend),
        ("invoice", "invoice", ViewBackend::System)
    );
    assert_eq!(
        (v.filter.as_str(), v.sort.as_str()),
        ("Document", "modified:desc")
    );
    assert!(v.matching.case && !v.matching.regex);
    let mut view = v.clone();
    view.folder = "Work".into();
    view.shortcut = "Ctrl+Alt+1".into();
    s.find.editor = None;
    for name in ["Invoices", "Receipts"] {
        let mut v = view.clone();
        v.name = name.into();
        if name == "Receipts" {
            v.query = "receipt".into();
            v.shortcut.clear();
        }
        s.run(
            0,
            Action::Find(FindCmd::Save {
                store: Store::Profile,
                view: Box::new(v),
                replaces: None,
            }),
        );
    }
    let names =
        |s: &AppState| -> Vec<String> { s.bookmarks().into_iter().map(|(_, v)| v.name).collect() };
    assert_eq!(names(&s), ["Invoices", "Receipts"]);
    let saved = s.settings.bookmarks[0].clone();
    assert_eq!((saved.id, saved.position), (1, 1));
    assert_eq!(s.settings.bookmarks[1].position, 2);

    // Opening it from a folder tab makes a search tab with everything restored.
    s.run(0, Action::NewTab);
    assert!(!s.tab(0).is_search());
    s.run(0, Action::Find(FindCmd::Open(Store::Profile, 1)));
    let tab = s.tab(0);
    assert!(tab.is_search() && !tab.library_search);
    assert_eq!(query_of(&s), "invoice");
    let tab = s.tab(0);
    assert_eq!(tab.search_filter, "Document");
    assert!(tab.match_toggles().case);
    assert_eq!(tab.sort, (SortKey::Modified, false));
    // Opened again in that search tab (no new tab); it counts as a search run.
    let tabs = s.panes[0].tabs.len();
    s.run(0, Action::Find(FindCmd::Open(Store::Profile, 2)));
    assert_eq!(s.panes[0].tabs.len(), tabs);
    assert_eq!(query_of(&s), "receipt");
    let ordered = s
        .find
        .history
        .history
        .ordered(crate::search_history::Order::MostRecent);
    assert_eq!(ordered.len(), 2);

    // Order inside the folder.
    s.run(0, Action::Find(FindCmd::Move(Store::Profile, 2, -1)));
    assert_eq!(names(&s), ["Receipts", "Invoices"]);
    s.run(0, Action::Find(FindCmd::Move(Store::Profile, 2, -1)));
    assert_eq!(names(&s), ["Receipts", "Invoices"], "already first");

    // Edit keeps the id; a shortcut taken by another bookmark is refused by the dialog's
    // check.
    s.run(0, Action::Find(FindCmd::Edit(Store::Profile, 2)));
    assert_eq!(s.find.editor.as_ref().unwrap().replaces, Some(2));
    let all = s.bookmarks();
    assert!(shortcut_problem("Ctrl+Alt+1", Some((Store::Profile, 2)), &all).is_some());
    s.find.editor = None;

    // config.toml keeps them (and filters and history settings).
    s.settings.filters.push(CustomFilter {
        name: "Designs".into(),
        search: "ext:psd".into(),
        matching: MatchToggles::default(),
    });
    s.settings.search_history.order = crate::search_history::Order::MostUsed;
    let text = toml::to_string_pretty(&s.settings).unwrap();
    let (back, dropped) = crate::settings::parse_lenient(&text).unwrap();
    assert!(dropped.is_empty(), "{dropped:?}");
    assert_eq!(back.bookmarks, s.settings.bookmarks);
    assert_eq!(back.filters, s.settings.filters);
    assert_eq!(back.search_history, s.settings.search_history);
    // A broken [[bookmarks]] entry drops only itself.
    let broken = format!("{text}\n[[bookmarks]]\nname = 5\n");
    let (back, dropped) = crate::settings::parse_lenient(&broken).unwrap();
    assert_eq!(back.bookmarks.len(), 2);
    assert_eq!(dropped, ["[[bookmarks]] #3"]);

    // Delete asks first.
    s.run(0, Action::Find(FindCmd::AskDelete(Store::Profile, 1)));
    let Some(crate::dialogs::Dialog::Confirm { on_yes, .. }) = s.dialog.take() else {
        panic!("asks first");
    };
    assert_eq!(s.settings.bookmarks.len(), 2);
    s.run(0, on_yes);
    assert_eq!(names(&s), ["Receipts"]);

    // Shortcut and Ctrl+D keys.
    let ctx = s.ctx.clone();
    let press = |mods: Modifiers, key: Key| egui::RawInput {
        events: vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: mods,
        }],
        modifiers: mods,
        ..Default::default()
    };
    s.settings.bookmarks[0].shortcut = "Ctrl+Alt+2".into();
    s.run(0, Action::NewTab);
    let _ = ctx.run(
        press(Modifiers::COMMAND | Modifiers::ALT, Key::Num2),
        |ctx| {
            s.bookmark_keys(ctx, true);
        },
    );
    assert_eq!(query_of(&s), "receipt");
    // Ctrl+Shift+Alt+2 is another key.
    s.run(0, Action::NewTab);
    let _ = ctx.run(
        press(
            Modifiers::COMMAND | Modifiers::ALT | Modifiers::SHIFT,
            Key::Num2,
        ),
        |ctx| s.bookmark_keys(ctx, true),
    );
    assert!(!s.tab(0).is_search());
    s.run(0, Action::Search);
    let _ = ctx.run(press(Modifiers::COMMAND, Key::D), |ctx| {
        s.bookmark_keys(ctx, true);
    });
    assert!(
        s.find.editor.is_some(),
        "Ctrl+D in a search tab adds a bookmark"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Library bookmarks are the library's saved views: saved, moved, opened and deleted
/// through the app's library backend.
#[test]
fn library_bookmarks_are_saved_views() {
    let dir = temp("library");
    let lib = Arc::new(keel_core::Library::open(&dir.join("data"), "test").unwrap());
    let mut s = state(&dir);
    s.library.set_open(lib.clone());
    s.run(0, Action::Search);
    s.tab_mut(0).library_search = true;
    set_query(&mut s, "ext:pdf");
    s.run(0, Action::Find(FindCmd::AddBookmark));
    let ed = s.find.editor.take().expect("the dialog");
    assert_eq!(
        (ed.store, ed.view.backend),
        (Store::Library, ViewBackend::Library)
    );
    for (n, (name, query)) in [("PDFs", "ext:pdf"), ("Docs", "ext:docx")]
        .into_iter()
        .enumerate()
    {
        // One after the other: each save runs on its own worker.
        pump_until(&mut s, "the save before", |_| {
            lib.views().unwrap().len() == n
        });
        let mut v = ed.view.clone();
        v.name = name.into();
        v.query = query.into();
        s.run(
            0,
            Action::Find(FindCmd::Save {
                store: Store::Library,
                view: Box::new(v),
                replaces: None,
            }),
        );
    }
    pump_until(&mut s, "both views", |s| s.library.views.len() == 2);
    assert_eq!(lib.views().unwrap().len(), 2);
    let names =
        |s: &AppState| -> Vec<String> { s.bookmarks().into_iter().map(|(_, v)| v.name).collect() };
    assert_eq!(names(&s), ["PDFs", "Docs"]);
    let docs = s.library.views[1].id;
    s.run(0, Action::Find(FindCmd::Move(Store::Library, docs, -1)));
    assert_eq!(names(&s), ["Docs", "PDFs"], "shown at once");
    pump_until(&mut s, "moved in the library", |_| {
        lib.views().unwrap()[0].id == docs
    });

    s.run(0, Action::NewTab);
    s.run(0, Action::Find(FindCmd::Open(Store::Library, docs)));
    assert!(s.tab(0).library_search);
    assert_eq!(query_of(&s), "ext:docx");

    s.run(0, Action::Find(FindCmd::Delete(Store::Library, docs)));
    pump_until(&mut s, "deleted", |s| s.library.views.len() == 1);
    assert_eq!(lib.views().unwrap()[0].name, "PDFs");
    drop(s);
    let _ = std::fs::remove_dir_all(dir);
}

/// A searcher that remembers what it was asked and answers fixed hits.
struct Fake {
    name: &'static str,
    asked: Mutex<Vec<Query>>,
    root: std::path::PathBuf,
}

impl Searcher for Fake {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        self.asked.lock().push(q.clone());
        let hit = |name: &str, is_dir| Hit {
            path: VPath::local(self.root.join(name)),
            is_dir,
            size: 1,
            modified: None,
            snippet: None,
        };
        Ok(vec![
            hit("song.MP3", false),
            hit("notes.txt", false),
            hit("music", true),
        ])
    }

    fn available(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

fn search_with(s: &mut AppState, fake: &Arc<Fake>, filter: &str, text: &str) -> Vec<String> {
    s.tab_mut(0).search_filter = filter.into();
    set_query(s, text);
    s.tick();
    let msg = loop {
        match s.rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            m @ Msg::Search { .. } => break m,
            other => s.apply(other),
        }
    };
    s.apply(msg);
    let _ = fake;
    let tab = s.tab(0);
    let mut names: Vec<String> = (tab.entries().iter())
        .map(|e| tab.shown_name(e).to_owned())
        .collect();
    names.sort();
    names
}

/// Built-in filters reach the backend as extension lists it understands and are checked
/// on the hits; a custom filter puts its search in front and adds its match toggles; a
/// filter with no text searches too.
#[test]
fn filters_narrow_the_search_through_the_worker() {
    let dir = temp("filters");
    for (name, expect_prefix) in [
        ("Everything", "ext:mp3;"),
        ("Keel index", "regex:\\.(mp3|"),
        ("Search", ""),
    ] {
        let mut s = state(&dir);
        let fake = Arc::new(Fake {
            name,
            asked: Mutex::default(),
            root: dir.clone(),
        });
        let searcher: Arc<dyn Searcher> = fake.clone();
        s.apply(Msg::Searcher {
            searcher,
            reason: None,
        });
        s.run(0, Action::Search);
        assert_eq!(search_with(&mut s, &fake, "Audio", "song"), ["song.MP3"]);
        let asked = fake.asked.lock().last().unwrap().clone();
        assert!(
            asked.text.starts_with(expect_prefix),
            "{name}: {}",
            asked.text
        );
        assert!(asked.text.ends_with("song"), "{name}: {}", asked.text);

        assert_eq!(search_with(&mut s, &fake, "Folder", "mu"), ["music"]);
        assert!(fake.asked.lock().last().unwrap().folders_only);

        assert_eq!(
            search_with(&mut s, &fake, "", "x"),
            ["music", "notes.txt", "song.MP3"]
        );

        s.settings.filters.push(CustomFilter {
            name: "Text".into(),
            search: "ext:txt".into(),
            matching: MatchToggles {
                case: true,
                ..MatchToggles::default()
            },
        });
        search_with(&mut s, &fake, "Text", "notes");
        let asked = fake.asked.lock().last().unwrap().clone();
        assert_eq!(asked.text, "ext:txt notes");
        assert!(asked.match_case && !asked.regex);

        // Only a filter: Everything and the Keel index list every audio file.
        if !expect_prefix.is_empty() {
            assert_eq!(search_with(&mut s, &fake, "Audio", ""), ["song.MP3"]);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Review focus 2: the history follows Do not save search history and Clear history.
#[test]
fn history_follows_the_privacy_switch() {
    let dir = temp("history");
    let mut s = state(&dir);
    s.run(0, Action::Find(FindCmd::Remember("budget".into())));
    s.run(0, Action::Find(FindCmd::Remember("budget".into())));
    s.run(0, Action::Find(FindCmd::Remember("plans".into())));
    let h = &s.find.history.history;
    assert_eq!(h.entries.len(), 2);
    assert_eq!(
        h.ordered(crate::search_history::Order::MostUsed)[0].text,
        "budget"
    );
    s.settings.search_history.order = crate::search_history::Order::MostUsed;
    s.find.publish(&s.ctx.clone(), &s.settings, None);
    assert_eq!(published(&s.ctx).history, ["budget", "plans"]);

    s.settings.search_history.save = false;
    s.run(0, Action::Find(FindCmd::Remember("secret".into())));
    s.tick();
    assert!(s.find.history.history.entries.is_empty());
    s.find.publish(&s.ctx.clone(), &s.settings, None);
    assert!(published(&s.ctx).history.is_empty());

    s.settings.search_history.save = true;
    s.run(0, Action::Find(FindCmd::Remember("again".into())));
    assert_eq!(s.find.history.history.entries.len(), 1);
    s.run(0, Action::Find(FindCmd::ClearHistory));
    assert!(s.find.history.history.entries.is_empty());
    // A smaller maximum trims at once.
    for t in ["a", "b", "c"] {
        s.run(0, Action::Find(FindCmd::Remember(t.into())));
    }
    s.settings.search_history.max = 2;
    s.tick();
    assert_eq!(s.find.history.history.entries.len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}
