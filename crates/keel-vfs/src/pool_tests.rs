//! The worker pool of local copies (`transfer_local`): one worker or eight give the same
//! result, and a copy stopped by a cancel or an error leaves no staging file and lists
//! exactly what landed.
use super::{transfer_local, Conflict, Landed, Progress, TEST_THREADS};
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// Every file and folder under `root` (relative, `/`-separated), with the files' bytes.
fn tree(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out: Vec<_> = walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .map(|e| {
            let e = e.unwrap();
            let rel = e.path().strip_prefix(root).unwrap().to_string_lossy();
            let bytes = e.file_type().is_file().then(|| fs::read(e.path()).unwrap());
            (rel.replace('\\', "/"), bytes)
        })
        .collect();
    out.sort();
    out
}

fn names(root: &Path) -> Vec<String> {
    tree(root).into_iter().map(|(name, _)| name).collect()
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// What a transfer did: its result, the top-level sources it listed (source path, target
/// name, new, complete) and its last progress report.
struct Run {
    result: anyhow::Result<()>,
    landed: Vec<(String, String, bool, bool)>,
    last: Progress,
}

/// Copies (or moves) `src` into `dst` with `threads` workers; `stop` cancels it.
fn run(
    threads: usize,
    src: &[PathBuf],
    dst: &Path,
    conflict: Conflict,
    moving: bool,
    stop: &dyn Fn(&Progress) -> bool,
) -> Run {
    TEST_THREADS.set(Some(threads));
    let cancel = AtomicBool::new(false);
    let last = RefCell::new(Progress::default());
    let mut landed = Vec::new();
    let result = transfer_local(
        src,
        dst,
        conflict,
        &|p| {
            if stop(&p) {
                cancel.store(true, Ordering::Relaxed);
            }
            *last.borrow_mut() = p;
        },
        &cancel,
        moving,
        None,
        Some(&mut landed),
    );
    TEST_THREADS.set(None);
    let landed = landed
        .into_iter()
        .map(|l: Landed| {
            (
                l.source.display(),
                l.target.name().to_owned(),
                l.new,
                l.complete,
            )
        })
        .collect();
    Run {
        result,
        landed,
        last: last.into_inner(),
    }
}

/// Folders, small files, one file over the pool's size limit and a second source with the
/// name of an earlier one; returns the sources in copy order.
fn fixture(root: &Path) -> Vec<PathBuf> {
    let src = root.join("src");
    for i in 0..300 {
        write(&src.join(format!("a/{i:03}.txt")), format!("a{i}"));
    }
    for i in 0..50 {
        write(&src.join(format!("a/sub/s{i:02}.txt")), format!("s{i}"));
    }
    fs::create_dir_all(src.join("a/empty")).unwrap();
    write(&src.join("big.bin"), vec![7u8; 2 << 20]);
    for i in 0..200 {
        write(&src.join(format!("f{i:03}.txt")), vec![i as u8; i]);
    }
    write(&root.join("other/f005.txt"), "other");
    let mut sources = vec![src.join("a")];
    sources.extend((0..100).map(|i| src.join(format!("f{i:03}.txt"))));
    sources.push(src.join("big.bin"));
    sources.extend((100..200).map(|i| src.join(format!("f{i:03}.txt"))));
    sources.push(root.join("other/f005.txt"));
    sources
}

/// `KEEL_COPY_THREADS=1` and the pool make the same tree, list the same sources and count
/// the same, for every conflict policy (a name taken in the destination and a second
/// source with the name of an earlier one).
#[test]
fn one_worker_or_eight_copy_the_same() {
    let tmp = tempfile::tempdir().unwrap();
    let sources = fixture(tmp.path());
    for conflict in [Conflict::Skip, Conflict::RenameNew, Conflict::Overwrite] {
        let results: Vec<_> = [1, 8]
            .into_iter()
            .map(|threads| {
                let dst = tmp.path().join(format!("{conflict:?}-{threads}"));
                write(&dst.join("f010.txt"), "old");
                write(&dst.join("a/sub/s05.txt"), "old");
                let r = run(threads, &sources, &dst, conflict, false, &|_| false);
                r.result.unwrap();
                let p = r.last;
                let counts = (p.done_items, p.done_bytes, p.skipped, p.total_items);
                (tree(&dst), r.landed, counts)
            })
            .collect();
        assert_eq!(results[0], results[1], "{conflict:?}");
        let (tree, landed, (done, _, skipped, total)) = &results[1];
        assert!(tree.iter().all(|(n, _)| !n.contains(".keel-partial")));
        assert_eq!(done + skipped, *total, "{conflict:?}");
        let f005 = |t: &str| tree.iter().find(|(n, _)| n == t).unwrap().1.clone();
        match conflict {
            Conflict::Skip => assert_eq!(f005("f005.txt").unwrap(), [5u8; 5]),
            Conflict::RenameNew => assert_eq!(f005("f005 (2).txt").unwrap(), b"other"),
            Conflict::Overwrite => assert_eq!(f005("f005.txt").unwrap(), b"other"),
        }
        assert_eq!(
            landed.len(),
            if conflict == Conflict::Skip { 201 } else { 203 }
        );
    }
}

/// 2,000 small files and three of 8 MiB, cancelled after 1,200: no staging file is left,
/// exactly the files counted are there, whole, and listed as complete.
#[test]
fn a_cancel_part_way_leaves_no_staging_file_and_lists_what_landed() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let mut sources = Vec::new();
    for i in 0..2000 {
        if i % 500 == 250 {
            let big = src.join(format!("b{i:04}.bin"));
            write(&big, vec![i as u8; 8 << 20]);
            sources.push(big);
        }
        let small = src.join(format!("f{i:04}.txt"));
        write(&small, format!("{i}"));
        sources.push(small);
    }
    for threads in [1, 8] {
        let dst = tmp.path().join(format!("dst{threads}"));
        fs::create_dir(&dst).unwrap();
        let r = run(threads, &sources, &dst, Conflict::Skip, false, &|p| {
            p.done_items >= 1200
        });
        let err = r.result.unwrap_err().to_string();
        assert!(err.contains("cancel"), "{err}");
        let there = tree(&dst);
        assert_eq!(there.len(), 1200, "{threads} workers");
        assert_eq!(r.last.done_items, 1200);
        for (name, bytes) in &there {
            assert!(!name.contains(".keel-partial"), "{name}");
            assert_eq!(bytes.as_ref().unwrap(), &fs::read(src.join(name)).unwrap());
        }
        let mut listed: Vec<_> = r.landed.iter().map(|l| l.1.clone()).collect();
        listed.sort();
        assert_eq!(listed, names(&dst), "{threads} workers");
        assert!(r.landed.iter().all(|l| l.2 && l.3));
    }
}

/// A file that cannot be read (locked by another program on Windows, no read permission
/// elsewhere) stops the copy with its error: the files before it are there and listed,
/// nothing after it, with one worker or eight.
#[test]
fn a_file_that_cannot_be_read_stops_the_copy_there() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let sources: Vec<_> = (0..300)
        .map(|i| {
            let p = src.join(format!("f{i:03}.txt"));
            write(&p, format!("{i}"));
            p
        })
        .collect();
    let locked = &sources[150];
    #[cfg(windows)]
    let _held = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(locked)
            .unwrap()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(locked).is_ok() {
            eprintln!("skipped: this user reads files without read permission (root)");
            return;
        }
    }
    let before: Vec<String> = (0..150).map(|i| format!("f{i:03}.txt")).collect();
    let mut listed = Vec::new();
    for threads in [1, 8] {
        let dst = tmp.path().join(format!("dst{threads}"));
        fs::create_dir(&dst).unwrap();
        let r = run(threads, &sources, &dst, Conflict::Skip, false, &|_| false);
        let err = format!("{:#}", r.result.unwrap_err());
        assert!(err.contains("f150.txt"), "{err}");
        assert_eq!(names(&dst), before, "{threads} workers");
        assert_eq!(r.last.done_items, 150);
        assert!(r.landed.iter().all(|l| l.3));
        listed.push(r.landed);
    }
    assert_eq!(listed[0], listed[1]);
    assert_eq!(listed[0].len(), 150);
}

/// A move to another volume copies small files on the pool and drops each source only
/// once its copy is in place, then the emptied folders.
/// Set KEEL_TEST_OTHER_VOLUME to a writable folder on a different drive than the temp folder.
#[test]
fn a_move_to_another_volume_drops_each_source_after_its_copy() {
    let Some(other) = std::env::var_os("KEEL_TEST_OTHER_VOLUME") else {
        eprintln!("KEEL_TEST_OTHER_VOLUME not set; skipping cross-volume move");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir_in(other).unwrap();
    let src = tmp.path().join("src");
    for i in 0..200 {
        write(&src.join(format!("d/{i:03}.txt")), format!("{i}"));
        write(&src.join(format!("d/sub/{i:03}.txt")), format!("sub {i}"));
    }
    let before = tree(&src);
    let r = run(
        8,
        &[src.join("d")],
        dst.path(),
        Conflict::Skip,
        true,
        &|_| false,
    );
    r.result.unwrap();
    assert!(!src.join("d").exists());
    assert_eq!(tree(dst.path()), before);
    let d = crate::VPath::local(src.join("d")).display();
    assert_eq!(r.landed, [(d, "d".into(), true, true)]);
}
