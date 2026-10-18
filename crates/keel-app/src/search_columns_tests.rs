use super::*;
use crate::tab::test_entry;
use std::time::{Duration, UNIX_EPOCH};

fn dir() -> VPath {
    VPath::parse("mem://t/").unwrap()
}

fn entries() -> Vec<Entry> {
    let d = dir();
    let mut v = vec![
        test_entry(&d, "b.txt", Kind::File, 30),
        test_entry(&d, "a.txt", Kind::File, 30),
        test_entry(&d, "sub", Kind::Dir, 0),
        test_entry(&d, "c.pdf", Kind::File, 10),
        test_entry(&d, "z", Kind::Dir, 0),
    ];
    for (i, e) in v.iter_mut().enumerate() {
        e.modified = Some(UNIX_EPOCH + Duration::from_secs(100 * i as u64));
    }
    v
}

fn names(entries: &[Entry], rows: &[usize]) -> Vec<String> {
    rows.iter()
        .map(|&i| entries[i].path.name().to_owned())
        .collect()
}

fn sorted(entries: &[Entry], sort: &[(ResultCol, bool)], folders: FoldersFirst) -> Vec<String> {
    let meta = MetaCache::default();
    let bytes = |e: &Entry| (e.kind != Kind::Dir).then_some(e.size);
    let when = |_: &Entry| None;
    let get = |c: ResultCol, e: &Entry| value(c, e, &meta, &bytes, &when);
    names(entries, &order(entries, sort, folders, &get, &|_| true))
}

#[test]
fn a_click_makes_the_primary_sort_and_keeps_two_more() {
    let s = click(&[(ResultCol::Name, true)], ResultCol::Size, false);
    assert_eq!(s, [(ResultCol::Size, true), (ResultCol::Name, true)]);
    // The primary again: only its direction turns.
    let s = click(&s, ResultCol::Size, false);
    assert_eq!(s, [(ResultCol::Size, false), (ResultCol::Name, true)]);
    let s = click(&s, ResultCol::Path, false);
    let s = click(&s, ResultCol::Type, false);
    assert_eq!(
        s,
        [
            (ResultCol::Type, true),
            (ResultCol::Path, true),
            (ResultCol::Size, false)
        ]
    );
    // Descending first: sizes and dates start biggest and newest first, names do not.
    assert_eq!(click(&[], ResultCol::Modified, true)[0], (ResultCol::Modified, false));
    assert_eq!(click(&[], ResultCol::Name, true)[0], (ResultCol::Name, true));
}

#[test]
fn the_secondary_sort_breaks_ties() {
    let e = entries();
    // a.txt and b.txt have the same size: Name decides; c.pdf is smaller.
    let s = [(ResultCol::Size, false), (ResultCol::Name, true)];
    assert_eq!(
        sorted(&e, &s, FoldersFirst::Never),
        ["a.txt", "b.txt", "c.pdf", "sub", "z"]
    );
    let s = [(ResultCol::Size, false), (ResultCol::Name, false)];
    assert_eq!(
        sorted(&e, &s, FoldersFirst::Never),
        ["b.txt", "a.txt", "c.pdf", "z", "sub"]
    );
    // By date, then the tertiary key never matters (every date differs).
    let s = [(ResultCol::Modified, false)];
    assert_eq!(
        sorted(&e, &s, FoldersFirst::Never),
        ["z", "c.pdf", "sub", "a.txt", "b.txt"]
    );
}

#[test]
fn folders_first_modes() {
    let e = entries();
    let asc = [(ResultCol::Name, true)];
    let desc = [(ResultCol::Name, false)];
    assert_eq!(
        sorted(&e, &asc, FoldersFirst::Always),
        ["sub", "z", "a.txt", "b.txt", "c.pdf"]
    );
    assert_eq!(
        sorted(&e, &desc, FoldersFirst::Always),
        ["z", "sub", "c.pdf", "b.txt", "a.txt"]
    );
    assert_eq!(
        sorted(&e, &desc, FoldersFirst::Ascending),
        ["c.pdf", "b.txt", "a.txt", "z", "sub"]
    );
    assert_eq!(
        sorted(&e, &asc, FoldersFirst::Descending),
        ["a.txt", "b.txt", "c.pdf", "sub", "z"]
    );
    assert_eq!(
        sorted(&e, &desc, FoldersFirst::Descending),
        ["z", "sub", "c.pdf", "b.txt", "a.txt"]
    );
    // Sort mix: folders among the files.
    assert_eq!(
        sorted(&e, &asc, FoldersFirst::Never),
        ["a.txt", "b.txt", "c.pdf", "sub", "z"]
    );
}

#[test]
fn unknown_values_go_last_both_ways() {
    let e = entries();
    // Folders have no known size here.
    for asc in [true, false] {
        let got = sorted(&e, &[(ResultCol::Size, asc)], FoldersFirst::Never);
        assert_eq!(&got[3..], ["sub", "z"], "{asc}");
    }
    // Nothing read from the disk yet: every created date unknown, the name decides.
    let got = sorted(&e, &[(ResultCol::Created, false)], FoldersFirst::Never);
    assert_eq!(got, ["a.txt", "b.txt", "c.pdf", "sub", "z"]);
}

#[test]
fn the_folders_sidebar_keeps_a_subtree() {
    let root = VPath::parse("mem://t/").unwrap();
    let sub = root.join("sub");
    let d = vec![
        test_entry(&sub, "x.txt", Kind::File, 1),
        test_entry(&root, "y.txt", Kind::File, 1),
        test_entry(&sub.join("deep"), "w.txt", Kind::File, 1),
        test_entry(&root, "subway.txt", Kind::File, 1),
    ];
    let meta = MetaCache::default();
    let get = |c: ResultCol, e: &Entry| value(c, e, &meta, &|_| None, &|_| None);
    let keep = |e: &Entry| is_within(&e.path, &sub);
    let rows = order(&d, &[(ResultCol::Name, true)], FoldersFirst::Always, &get, &keep);
    assert_eq!(names(&d, &rows), ["w.txt", "x.txt"]);
    assert!(is_within(&sub, &sub));
    assert!(!is_within(&root, &sub));
    assert!(is_within(&sub, &root));
}

#[test]
fn bookmark_sort_and_columns_round_trip() {
    let s = vec![
        (ResultCol::Created, false),
        (ResultCol::Path, true),
        (ResultCol::Name, true),
    ];
    let text = sort_text(&s);
    assert_eq!(text, "created:desc,path,name");
    assert!(keel_core::valid_sort(&text));
    assert_eq!(parse_sort(&text), s);
    assert_eq!(sort_text(&[(ResultCol::Name, true)]), "");
    // Everything's names, unknown words skipped, each column once, three at most.
    assert_eq!(
        parse_sort("date_modified:desc,run_count,ext,date_modified,size,name"),
        [
            (ResultCol::Modified, false),
            (ResultCol::Extension, true),
            (ResultCol::Size, true)
        ]
    );
    let cols = vec![ResultCol::SizeOnDisk, ResultCol::Attributes];
    let words = column_words(&cols);
    assert_eq!(parse_columns(&words), Some(cols));
    assert_eq!(parse_columns(&["name".into(), "nope".into()]), None);
}

#[test]
fn the_tab_sort_leads_until_a_column_without_one() {
    let mut r = TabResults::default();
    assert_eq!(r.sort((SortKey::Name, true)), [(ResultCol::Name, true)]);
    // A header click on Created: the tab's own sort stays Name.
    let s = click(&r.sort((SortKey::Name, true)), ResultCol::Created, false);
    r.set_sort(s, (SortKey::Name, true));
    assert_eq!(r.sort((SortKey::Name, true))[0], (ResultCol::Created, true));
    // The View menu sorts by size: that leads, the earlier keys follow.
    assert_eq!(
        r.sort((SortKey::Size, false)),
        [
            (ResultCol::Size, false),
            (ResultCol::Created, true),
            (ResultCol::Name, true)
        ]
    );
}

#[test]
fn chosen_columns_keep_the_chooser_order() {
    let mut o = ResultsOptions::default();
    o.set(ResultCol::Attributes, true);
    o.set(ResultCol::Extension, true);
    o.set(ResultCol::Size, false);
    assert_eq!(
        o.columns,
        [
            ResultCol::Path,
            ResultCol::Extension,
            ResultCol::Modified,
            ResultCol::Attributes
        ]
    );
    let back: ResultsOptions = toml::from_str(&toml::to_string(&o).unwrap()).unwrap();
    assert_eq!(back, o);
    let old: ResultsOptions = toml::from_str("folders_first = \"never\"").unwrap();
    assert_eq!(old.folders_first, FoldersFirst::Never);
    assert_eq!(old.columns, ResultsOptions::default().columns);
}

#[test]
fn attribute_text() {
    if cfg!(windows) {
        assert_eq!(attributes_text(0x1 | 0x2 | 0x20, false), "RHA");
        assert_eq!(attributes_text(0x10 | 0x800, true), "DC");
    } else {
        assert_eq!(attributes_text(0o755, true), "drwxr-xr-x");
        assert_eq!(attributes_text(0o640, false), "-rw-r-----");
    }
}

/// The worker reads created, accessed, attributes and size on disk of local results.
#[test]
fn meta_is_read_off_the_ui_thread() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
    let path = VPath::local(tmp.path().join("a.txt"));
    let mut tab = Tab::search(VPath::local(tmp.path()));
    let mut e = test_entry(&VPath::local(tmp.path()), "a.txt", Kind::File, 5);
    e.path = path.clone();
    e.name = path.display();
    tab.set_entries(vec![e]);
    let ctx = egui::Context::default();
    let wait = std::time::Instant::now();
    while tab.results.meta.gen == 0 && wait.elapsed() < Duration::from_secs(10) {
        want_meta(&mut tab, &ctx);
        std::thread::sleep(Duration::from_millis(10));
    }
    let fixed = keel_vfs::is_fixed_disk(tmp.path());
    let m = tab.results.meta.by_name.get(&path.display()).copied();
    if fixed {
        let m = m.expect("read");
        assert!(m.attributes.is_some() && m.on_disk.is_some());
        assert!(m.accessed.is_some());
    } else {
        assert!(m.is_none());
    }
}
