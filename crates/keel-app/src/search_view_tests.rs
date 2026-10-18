use super::*;
use crate::tab::test_entry;

#[test]
fn ctrl_space_completes_the_word_before_the_cursor() {
    let words = ["report", "Reports 2026", "readme.md", "repo"];
    // From the end of the text; any case; longer words only.
    assert_eq!(
        complete_word("ext:pdf rep", 11, &words, 0),
        Some(("ext:pdf report".into(), 14))
    );
    assert_eq!(
        complete_word("ext:pdf REP", 11, &words, 1),
        Some(("ext:pdf \"Reports 2026\"".into(), 22))
    );
    assert_eq!(complete_word("rep", 3, &words, 9), None);
    // In the middle of the text, after a function's colon, and inside quotes.
    assert_eq!(
        complete_word("rea x", 3, &words, 0),
        Some(("readme.md x".into(), 9))
    );
    assert_eq!(
        complete_word("file:rea", 8, &words, 0),
        Some(("file:readme.md".into(), 14))
    );
    assert_eq!(
        complete_word("\"Reports", 8, &words, 0),
        Some(("\"Reports 2026".into(), 13))
    );
    // Nothing typed, or nothing longer.
    assert_eq!(complete_word("x ", 2, &words, 0), None);
    assert_eq!(complete_word("readme.md", 9, &words, 0), None);
    // Non-ASCII words keep their characters.
    let words = ["café menu"];
    assert_eq!(
        complete_word("caf", 3, &words, 0),
        Some(("\"café menu\"".into(), 11))
    );
}

#[test]
fn words_come_from_the_history_then_the_names() {
    let history = vec!["ext:pdf invoice".to_owned(), "\"tax return\" Invoice".to_owned()];
    let names = ["invoice-2026.pdf", "taxes.xlsx"];
    let got = candidates(&history, names.iter().copied());
    assert_eq!(
        got,
        ["invoice", "tax", "return", "invoice-2026.pdf", "taxes.xlsx"]
    );
}

#[test]
fn ctrl_space_pressed_again_offers_the_next_word() {
    let ctx = egui::Context::default();
    let id = egui::Id::new("q");
    let words = ["alpha", "alps", "also"];
    let mut query = "al".to_owned();
    let mut seen = Vec::new();
    for _ in 0..4 {
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(complete_key(ui, id, &mut query, &words));
            });
        });
        seen.push(query.clone());
    }
    assert_eq!(seen, ["alpha", "alps", "also", "alpha"]);
}

#[test]
fn no_highlight_terms_are_left_out() {
    assert_eq!(without_no_highlight("foo bar", false), "foo bar");
    let t = without_no_highlight("foo no-highlight:bar baz", false);
    assert_eq!(t.split_whitespace().collect::<Vec<_>>(), ["foo", "baz"]);
    assert_eq!(t.len(), "foo no-highlight:bar baz".len());
    // A regular expression is not the search language.
    assert_eq!(
        without_no_highlight("no-highlight:x", true),
        "no-highlight:x"
    );
    // The highlighter then marks only the other words.
    let q = keel_search::Query {
        text: without_no_highlight("café no-highlight:menu", false),
        ..Default::default()
    };
    let h = keel_search::Highlighter::new(&q).expect("café is highlighted");
    let name = "café menu.txt";
    let marks = h.marks(name);
    assert_eq!(marks, [(0, "café".len())]);
}

#[test]
fn the_folders_tree_counts_results_and_folds_chains() {
    let root = VPath::parse("mem://t/").unwrap();
    let a = root.join("a");
    let deep = a.join("b").join("c");
    let entries = vec![
        test_entry(&deep, "x.txt", Kind::File, 1),
        test_entry(&deep, "y.txt", Kind::File, 1),
        test_entry(&root.join("z"), "w.txt", Kind::File, 1),
    ];
    let tree = FolderTree::build(entries.iter());
    assert_eq!(tree.roots.len(), 1);
    let top = &tree.nodes[tree.roots[0]];
    assert_eq!(top.path, root);
    assert_eq!(top.hits, 3);
    let names: Vec<&str> = (top.children.iter())
        .map(|&i| tree.nodes[i].path.name())
        .collect();
    assert_eq!(names, ["a", "z"]);
    // a → b → c is one row: c holds the results.
    let end = tree.chain_end(top.children[0]);
    assert_eq!(tree.nodes[end].path, deep);
    assert_eq!((tree.nodes[end].hits, tree.nodes[end].own), (2, 2));
}

/// Search current results: the results shown become the list searched; off, the search
/// before comes back.
#[test]
fn search_current_results_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = VPath::local(tmp.path());
    let mut tab = Tab::search(dir.clone());
    let mut rows = Vec::new();
    for name in ["report.pdf", "notes.txt"] {
        let mut e = test_entry(&dir, name, Kind::File, 3);
        e.name = e.path.display();
        rows.push(e);
    }
    tab.set_entries(rows);
    tab.visible(true);
    if let TabKind::Search { query, .. } = &mut tab.kind {
        *query = "o".into();
    }
    assert!(toggle_refine(&mut tab));
    let list = tab.filelist.clone().expect("the results as a list");
    assert_eq!(list.len(), 2);
    let TabKind::Search { query, due, .. } = &tab.kind else {
        unreachable!()
    };
    assert!(query.is_empty() && due.is_some());
    // The list answers the next query by itself.
    use keel_search::Searcher;
    let q = keel_search::Query {
        text: "report".into(),
        ..Default::default()
    };
    let hits = list.query(&q).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path.name(), "report.pdf");
    // Off: the earlier query and no list.
    assert!(toggle_refine(&mut tab));
    assert!(tab.filelist.is_none() && tab.results.refine.is_none());
    let TabKind::Search { query, .. } = &tab.kind else {
        unreachable!()
    };
    assert_eq!(query, "o");
    // Remote results cannot be listed: nothing to search in.
    let mut remote = Tab::search(VPath::parse("sftp://host/srv/").unwrap());
    remote.set_entries(vec![test_entry(
        &VPath::parse("sftp://host/srv/").unwrap(),
        "a.txt",
        Kind::File,
        1,
    )]);
    remote.visible(true);
    assert!(!toggle_refine(&mut remote));
}

/// Match Prefix, Match Suffix, Ignore Punctuation and Ignore White-space reach the
/// backend's query, and bookmarks and filters keep them.
#[test]
fn the_four_toggles_reach_the_query_and_bookmarks() {
    use crate::search_tab::{match_toggles, toggles_of, SearchSettings, Toggle, Toggles};
    let mut t = Toggles::default();
    for x in [
        Toggle::Prefix,
        Toggle::Suffix,
        Toggle::Punctuation,
        Toggle::Whitespace,
    ] {
        assert!(!x.get(t));
        *x.of(&mut t) = true;
        assert!(x.get(t));
        assert!(x.shortcut().is_empty(), "no default key");
        assert!(!Toggle::KEYED.contains(&x));
    }
    let q = SearchSettings::default().query("a".into(), t, 10);
    assert!(q.match_prefix && q.match_suffix && q.ignore_punctuation && q.ignore_whitespace);
    assert!(!q.match_case && !q.whole_word);
    let m = match_toggles(t);
    assert!(m.prefix && m.suffix && m.ignore_punctuation && m.ignore_whitespace);
    assert_eq!(toggles_of(m), t);
}
