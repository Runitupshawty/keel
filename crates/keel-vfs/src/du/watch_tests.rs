//! The File Watcher: changes applied to a scanned temp tree, the change list and its
//! filter, and the watcher itself on a temp folder.

use super::watch::{Change, ChangeKind, ChangeLog, PathFilter, Watcher};
use super::*;
use std::fs;

fn write(path: &Path, bytes: usize) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

fn scan_of(root: &Path) -> Tree {
    scan(
        &[Root::Path(root.to_owned())],
        &ScanOptions::default(),
        &ScanHandle::new(),
    )
    .unwrap()
}

/// The tree's totals agree with a fresh scan of the same folder.
fn same_as_fresh(t: &Tree, root: &Path) {
    let fresh = scan_of(root);
    let (a, b) = (t.node(TOP), fresh.node(TOP));
    assert_eq!(
        (a.size, a.files, a.folders, a.items),
        (b.size, b.files, b.folders, b.items),
        "watched totals differ from a new scan"
    );
    for (id, n) in fresh.nodes().filter(|(_, n)| n.kind == NodeKind::Dir) {
        let path = fresh.path(id).unwrap();
        let mine = t.find(&path).unwrap_or_else(|| panic!("{} missing", path.display()));
        let m = t.node(mine);
        assert_eq!((m.size, m.files, m.items), (n.size, n.files, n.items), "{path:?}");
    }
}

fn fixture() -> (tempfile::TempDir, PathBuf, Tree) {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path().join("root");
    write(&r.join("a/one.txt"), 100);
    write(&r.join("a/two.bin"), 200);
    write(&r.join("b/deep/three.txt"), 300);
    write(&r.join("top.dat"), 50);
    let t = scan_of(&r);
    (tmp, r, t)
}

#[test]
fn a_created_file_is_added_and_counted_above() {
    let (_tmp, r, mut t) = fixture();
    let before = t.node(TOP).size;
    let deep = t.find(&r.join("b/deep")).unwrap();
    let three = t.find(&r.join("b/deep/three.txt")).unwrap();
    write(&r.join("a/new.txt"), 1000);
    let applied = t.apply_changes(&[r.join("a/new.txt")]).unwrap();
    assert_eq!((applied.added, applied.removed), (1, 0));
    assert_eq!(t.node(TOP).size, before + 1000);
    let new = t.find(&r.join("a/new.txt")).expect("added");
    assert_eq!(t.node(new).size, 1000);
    assert_eq!(t.ext(new), "txt");
    let a = t.find(&r.join("a")).unwrap();
    assert_eq!((t.node(a).size, t.node(a).files), (1300, 3));
    // Ids moved: the remap says where every old item went.
    assert!(applied.remap.is_some(), "ids changed");
    assert_eq!(applied.map(deep), t.find(&r.join("b/deep")));
    assert_eq!(applied.map(three), t.find(&r.join("b/deep/three.txt")));
    same_as_fresh(&t, &r);
}

#[test]
fn a_deleted_file_and_a_deleted_folder_are_dropped() {
    let (_tmp, r, mut t) = fixture();
    let one = t.find(&r.join("a/one.txt")).unwrap();
    fs::remove_file(r.join("a/one.txt")).unwrap();
    fs::remove_dir_all(r.join("b")).unwrap();
    // One notification per item, the folder's children included, in any order.
    let paths = [
        r.join("b/deep/three.txt"),
        r.join("a/one.txt"),
        r.join("b"),
        r.join("b/deep"),
    ];
    let applied = t.apply_changes(&paths).unwrap();
    assert_eq!(applied.added, 0);
    assert!(applied.removed >= 2, "{applied:?}");
    assert_eq!(applied.map(one), None);
    assert!(t.find(&r.join("b")).is_none());
    assert!(t.find(&r.join("a/one.txt")).is_none());
    assert_eq!(t.node(TOP).size, 250);
    same_as_fresh(&t, &r);
}

#[test]
fn a_changed_file_is_read_again_in_place() {
    let (_tmp, r, mut t) = fixture();
    let two = t.find(&r.join("a/two.bin")).unwrap();
    write(&r.join("a/two.bin"), 5000);
    let applied = t.apply_changes(&[r.join("a/two.bin"), r.join("a/two.bin")]).unwrap();
    assert_eq!((applied.added, applied.removed, applied.updated), (0, 0, 1));
    assert!(applied.remap.is_none(), "ids stay");
    assert_eq!(t.node(two).size, 5000);
    assert_eq!(t.node(t.find(&r.join("a")).unwrap()).size, 5100);
    same_as_fresh(&t, &r);
}

#[test]
fn a_new_folder_is_walked_with_what_is_in_it() {
    let (_tmp, r, mut t) = fixture();
    write(&r.join("b/deep/new/x/1.txt"), 10);
    write(&r.join("b/deep/new/x/2.txt"), 20);
    write(&r.join("b/deep/new/3.txt"), 30);
    // Notifications for the files only (the folders' own may come late or not at all).
    let applied = t
        .apply_changes(&[r.join("b/deep/new/x/2.txt"), r.join("b/deep/new/3.txt")])
        .unwrap();
    assert_eq!(applied.added, 1, "the new folder, with what is in it");
    let new = t.find(&r.join("b/deep/new")).unwrap();
    assert_eq!(t.node(new).kind, NodeKind::Dir);
    assert_eq!((t.node(new).size, t.node(new).files, t.node(new).folders), (60, 3, 1));
    same_as_fresh(&t, &r);
}

#[test]
fn a_renamed_item_moves_and_a_file_replaced_by_a_folder_changes_kind() {
    let (_tmp, r, mut t) = fixture();
    fs::rename(r.join("a"), r.join("c")).unwrap();
    fs::remove_file(r.join("top.dat")).unwrap();
    write(&r.join("top.dat/inside.txt"), 7);
    let applied = t
        .apply_changes(&[r.join("a"), r.join("c"), r.join("top.dat")])
        .unwrap();
    assert!(applied.added >= 2 && applied.removed >= 2, "{applied:?}");
    assert!(t.find(&r.join("a")).is_none());
    let c = t.find(&r.join("c")).unwrap();
    assert_eq!(t.node(c).size, 300);
    let top = t.find(&r.join("top.dat")).unwrap();
    assert_eq!(t.node(top).kind, NodeKind::Dir);
    same_as_fresh(&t, &r);
}

#[test]
fn paths_outside_the_scan_and_unchanged_items_change_nothing() {
    let (tmp, r, mut t) = fixture();
    let before = t.clone();
    write(&tmp.path().join("elsewhere.txt"), 10);
    let applied = t
        .apply_changes(&[tmp.path().join("elsewhere.txt"), r.clone(), r.join("a")])
        .unwrap();
    assert!(applied.is_empty() && applied.remap.is_none(), "{applied:?}");
    assert_eq!(t.node(TOP), before.node(TOP));
    // A file notified but unchanged is read again (same values).
    let applied = t.apply_changes(&[r.join("top.dat")]).unwrap();
    assert_eq!(applied.updated, 1);
    assert_eq!(t.node(TOP).size, before.node(TOP).size);
}

#[test]
fn filters_still_apply_to_watched_items() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path().join("root");
    write(&r.join("keep.txt"), 10);
    let opts = ScanOptions {
        filters: Filters {
            min_size: Some(100),
            ..Filters::default()
        },
        ..ScanOptions::default()
    };
    let mut t = scan(&[Root::Path(r.clone())], &opts, &ScanHandle::new()).unwrap();
    write(&r.join("small.txt"), 5);
    write(&r.join("big.txt"), 500);
    let applied = t
        .apply_changes(&[r.join("small.txt"), r.join("big.txt")])
        .unwrap();
    assert_eq!(applied.added, 1);
    assert!(t.find(&r.join("small.txt")).is_none());
    assert_eq!(t.node(TOP).size, 500);
}

#[test]
fn the_change_log_keeps_the_latest_and_filters_by_path() {
    let mut log = ChangeLog::new(3);
    for (i, name) in ["a.txt", "B.TXT", "c.log", "d.txt"].iter().enumerate() {
        log.push(Change {
            path: PathBuf::from("/data").join(name),
            kind: ChangeKind::Created,
            at: i as i64,
        });
    }
    assert_eq!((log.len(), log.total), (3, 4));
    let all = PathFilter::new("").unwrap();
    let names: Vec<i64> = log.newest_first(&all).map(|c| c.at).collect();
    assert_eq!(names, [3, 2, 1], "newest first, the oldest dropped");
    let txt = PathFilter::new(r"\.txt$").unwrap();
    let hits: Vec<i64> = log.newest_first(&txt).map(|c| c.at).collect();
    assert_eq!(hits, [3, 1], "any case");
    assert!(PathFilter::new("(").is_err());
    log.clear();
    assert!(log.is_empty() && log.total == 0);
}

#[test]
fn the_watcher_reports_a_new_file() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Watcher::new(&[tmp.path().to_owned()]).unwrap();
    assert_eq!(w.roots().len(), 1);
    // Some systems start delivering a moment after the watch is set.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut seen = Vec::new();
    let mut n = 0;
    while std::time::Instant::now() < deadline {
        write(&tmp.path().join(format!("new{n}.txt")), 3);
        n += 1;
        std::thread::sleep(std::time::Duration::from_millis(300));
        seen.extend(w.drain(1000));
        if seen.iter().any(|c| {
            (c.path.file_name()).is_some_and(|f| f.to_string_lossy().starts_with("new"))
        }) {
            return;
        }
    }
    panic!("no change reported: {seen:?}");
}

#[test]
fn a_missing_folder_cannot_be_watched() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(Watcher::new(&[tmp.path().join("missing")]).is_err());
}
