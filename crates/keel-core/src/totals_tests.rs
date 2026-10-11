use super::*;
use crate::index::tests::{id_of, library_with, walk, write};
use crate::library::tests::folder;
use crate::{ChangeEvent, Indexer, Source};
use keel_vfs::{Router, VPath};
use std::path::Path;

/// Every folder's stored total equals the sum over the records below it, and every folder
/// has one.
fn assert_consistent(src: &Source) {
    let c = src.store.get().unwrap();
    let dirs: Vec<(i64, String)> = c
        .prepare("SELECT id, path FROM record WHERE kind = 1 AND flags & 4 = 0")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let dirty: i64 = c
        .query_row("SELECT count(*) FROM folder_dirty", [], |r| r.get(0))
        .unwrap();
    assert_eq!(dirty, 0, "marks left after a refresh");
    for (id, path) in dirs {
        let want: (i64, i64) = c
            .query_row(
                "WITH RECURSIVE sub(id) AS (
                     SELECT ?1 UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id
                     WHERE r.flags & 4 = 0)
                 SELECT coalesce(sum(size), 0), count(*) FROM record
                 WHERE id IN (SELECT id FROM sub) AND kind = 0 AND flags & 4 = 0",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let got: Option<(i64, i64)> = c
            .query_row(
                "SELECT bytes, files FROM folder_total WHERE record = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .unwrap();
        assert_eq!(got, Some(want), "folder {path:?}");
    }
}

fn total(src: &Source, rel: &str) -> Total {
    let c = src.store.get().unwrap();
    let id = id_of(src, rel).unwrap();
    c.query_row(
        "SELECT bytes, files FROM folder_total WHERE record = ?1",
        [id],
        |r| {
            Ok(Total {
                bytes: r.get::<_, i64>(0)? as u64,
                files: r.get::<_, i64>(1)? as u64,
            })
        },
    )
    .unwrap()
}

fn t(bytes: u64, files: u64) -> Total {
    Total { bytes, files }
}

fn changed(src: &Source, paths: &[&Path]) {
    let paths: Vec<VPath> = paths.iter().map(VPath::local).collect();
    Indexer::apply_paths(src, &paths, false).unwrap();
}

#[test]
fn totals_follow_adds_removes_renames_and_moves() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("root");
    write(&root.join("a/one.txt"), "1");
    write(&root.join("a/deep/two.txt"), "22");
    write(&root.join("b/three.txt"), "333");
    std::fs::create_dir_all(root.join("empty")).unwrap();
    let (_data, lib, src) = library_with(folder("root", &root));
    walk(&src, &Router::new()).unwrap();
    assert_consistent(&src);
    assert_eq!(total(&src, ""), t(6, 3));
    assert_eq!(total(&src, "a"), t(3, 2));
    assert_eq!(total(&src, "empty"), t(0, 0));

    // Add: one chain.
    write(&root.join("a/deep/four.txt"), "4444");
    changed(&src, &[&root.join("a/deep/four.txt")]);
    assert_eq!(total(&src, "a/deep"), t(6, 2));
    assert_eq!(total(&src, ""), t(10, 4));
    assert_consistent(&src);

    // Remove.
    std::fs::remove_file(root.join("a/one.txt")).unwrap();
    changed(&src, &[&root.join("a/one.txt")]);
    assert_eq!(total(&src, "a"), t(6, 2));
    assert_consistent(&src);

    // Rename within a folder: no total changes.
    std::fs::rename(root.join("b/three.txt"), root.join("b/drei.txt")).unwrap();
    changed(&src, &[&root.join("b/drei.txt"), &root.join("b/three.txt")]);
    assert_eq!(total(&src, "b"), t(3, 1));
    assert_consistent(&src);

    // A file and a folder moved across folders: two chains each.
    std::fs::rename(root.join("b/drei.txt"), root.join("empty/drei.txt")).unwrap();
    changed(
        &src,
        &[&root.join("empty/drei.txt"), &root.join("b/drei.txt")],
    );
    assert_eq!((total(&src, "b"), total(&src, "empty")), (t(0, 0), t(3, 1)));
    std::fs::rename(root.join("a/deep"), root.join("empty/deep")).unwrap();
    changed(&src, &[&root.join("empty/deep"), &root.join("a/deep")]);
    assert_eq!(total(&src, "a"), t(0, 0));
    assert_eq!(total(&src, "empty"), t(9, 3));
    assert_eq!(total(&src, ""), t(9, 3));
    assert_consistent(&src);

    // A file that grew.
    write(&root.join("empty/drei.txt"), "3333333");
    changed(&src, &[&root.join("empty/drei.txt")]);
    assert_eq!(total(&src, ""), t(13, 3));
    assert_consistent(&src);

    // The reader: biggest first, files with their length.
    let sizes = lib.folder_sizes(&src.id, "", false, usize::MAX).unwrap();
    assert_eq!(sizes.total, Some(t(13, 3)));
    let names: Vec<_> = sizes.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["empty", "a", "b"]);
    let inner = lib
        .folder_sizes(&src.id, "empty", false, usize::MAX)
        .unwrap();
    assert_eq!(inner.children[0].name, "drei.txt");
    assert_eq!(inner.children[0].total, Some(t(7, 1)));
    assert!(lib
        .folder_sizes(&src.id, "empty/drei.txt", false, usize::MAX)
        .is_err());
    // A cap keeps the biggest and says there were more; folders only leaves files out.
    let two = lib.folder_sizes(&src.id, "", false, 2).unwrap();
    let names: Vec<_> = two.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!((names, two.truncated), (vec!["empty", "a"], true));
    assert!(!sizes.truncated);
    write(&root.join("loose.txt"), "x");
    changed(&src, &[&root.join("loose.txt")]);
    let all = lib.folder_sizes(&src.id, "", false, usize::MAX).unwrap();
    assert!(all.children.iter().any(|c| c.name == "loose.txt"));
    let dirs = lib.folder_sizes(&src.id, "", true, usize::MAX).unwrap();
    assert!(dirs.children.iter().all(|c| c.is_dir), "{dirs:?}");
    assert_eq!(dirs.children.len(), 3);
    assert!(lib.folder_sizes(&src.id, "", false, 0).unwrap().truncated);

    // Removed with its source.
    lib.remove_source(&src.id, true).unwrap();
    assert!(lib.folder_sizes(&src.id, "", false, usize::MAX).is_err());
}

#[test]
fn a_burst_of_ten_thousand_changes_keeps_totals_exact() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("root");
    write(&root.join("keep/k.txt"), "k");
    for d in 0..10 {
        for e in 0..7 {
            std::fs::create_dir_all(root.join(format!("burst/d{d}/e{e}"))).unwrap();
        }
    }
    let (_data, _lib, src) = library_with(folder("root", &root));
    walk(&src, &Router::new()).unwrap();
    let mut paths = Vec::new();
    for i in 0..10_000 {
        let p = root.join(format!("burst/d{}/e{}/f{i}.txt", i % 10, i % 7));
        write(&p, &"x".repeat(i % 13));
        paths.push(p);
    }
    // Watcher order: files, then removals of half of them.
    for p in paths.iter().step_by(2) {
        std::fs::remove_file(p).unwrap();
    }
    let all: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    changed(&src, &all);
    assert_consistent(&src);
    let left: u64 = (0..10_000u64).filter(|i| i % 2 == 1).map(|i| i % 13).sum();
    assert_eq!(total(&src, "burst"), t(left, 5_000));
    assert_eq!(total(&src, ""), t(left + 1, 5_001));

    // A small change after the burst takes the incremental path.
    std::fs::remove_file(root.join("keep/k.txt")).unwrap();
    changed(&src, &[&root.join("keep/k.txt")]);
    assert_eq!(total(&src, ""), t(left, 5_000));
    assert_consistent(&src);
}

#[test]
fn watcher_changes_during_a_walk_are_counted_when_it_ends() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("root");
    write(&root.join("a/one.txt"), "1");
    let (_data, _lib, src) = library_with(folder("root", &root));
    walk(&src, &Router::new()).unwrap();
    // As if a walk were running: the batch leaves its marks.
    src.pending_gen
        .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
    write(&root.join("a/two.txt"), "22");
    Indexer::apply_change(
        &src,
        ChangeEvent::Changed(VPath::local(root.join("a/two.txt"))),
    )
    .unwrap();
    assert_eq!(total(&src, "a"), t(1, 1), "not refreshed while a walk runs");
    src.pending_gen
        .store(0, std::sync::atomic::Ordering::SeqCst);
    walk(&src, &Router::new()).unwrap();
    assert_eq!(total(&src, "a"), t(3, 2));
    assert_consistent(&src);
}

#[test]
fn links_are_not_counted() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("root");
    write(&root.join("big/data.bin"), &"d".repeat(100));
    let linked = crate::index::tests::dir_link(&root.join("big"), &root.join("again"));
    let (_data, lib, src) = library_with(folder("root", &root));
    walk(&src, &Router::new()).unwrap();
    assert_consistent(&src);
    assert_eq!(total(&src, ""), t(100, 1));
    if linked {
        let sizes = lib.folder_sizes(&src.id, "", false, usize::MAX).unwrap();
        let again = sizes.children.iter().find(|c| c.name == "again").unwrap();
        assert_eq!(again.total, None, "a folder link has no total");
    }
}
