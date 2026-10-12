//! Scans of temp fixture trees.

use super::*;
use std::fs;
use std::time::SystemTime;

fn scan_with(root: &Path, opts: &ScanOptions) -> Tree {
    scan(&[Root::Path(root.to_owned())], opts, &ScanHandle::new()).unwrap()
}

fn scan_default(root: &Path) -> Tree {
    scan_with(root, &ScanOptions::default())
}

fn write(path: &Path, bytes: usize) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

fn id_of(t: &Tree, path: &Path) -> NodeId {
    t.find(path)
        .unwrap_or_else(|| panic!("{} not in the scan", path.display()))
}

/// The live children's names of `id`, sorted.
fn names(t: &Tree, id: NodeId) -> Vec<String> {
    let mut v: Vec<String> = t.children(id).map(|c| t.node(c).name.to_string()).collect();
    v.sort();
    v
}

/// A link to a folder: a junction on Windows (no privilege needed), a symlink elsewhere.
/// Returns the flag such a link gets.
fn dir_link(target: &Path, link: &Path) -> u16 {
    #[cfg(windows)]
    {
        crate::fsattr::make_link(target, link, crate::fsattr::LinkKind::Junction).unwrap();
        flag::JUNCTION
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        flag::SYMLINK
    }
}

/// Options that follow what `dir_link` makes.
fn following() -> ScanOptions {
    ScanOptions {
        follow_junctions: cfg!(windows),
        follow_symlinks: !cfg!(windows),
        ..ScanOptions::default()
    }
}

#[test]
fn totals_counts_extensions_and_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("a/one.txt"), 1);
    write(&root.join("a/b/two.TXT"), 22);
    write(&root.join("top.txt"), 333);
    write(&root.join("big.bin"), 10_000);
    write(&root.join(".bashrc"), 4);
    fs::create_dir(root.join("empty")).unwrap();
    let t = scan_default(root);
    assert!(t.info.complete);
    assert_eq!(t.roots().len(), 1);
    let r = t.roots()[0].node;
    let top = t.node(TOP);
    assert_eq!((top.size, top.files, top.folders), (10_360, 5, 4));
    // a, a/b, empty: 3 folders, 5 files, plus the root itself for the top.
    let rn = t.node(r);
    assert_eq!((rn.size, rn.files, rn.folders, rn.items), (10_360, 5, 3, 8));
    let a = id_of(&t, &root.join("a"));
    assert_eq!(
        (t.node(a).size, t.node(a).files, t.node(a).folders),
        (23, 2, 1)
    );
    assert_eq!(t.path(a).unwrap(), root.join("a"));
    let two = id_of(&t, &root.join("a/b/two.TXT"));
    assert_eq!(t.ext(two), "txt");
    assert_eq!(t.ext(id_of(&t, &root.join(".bashrc"))), "");
    assert_eq!(t.ext(id_of(&t, &root.join("big.bin"))), "bin");
    assert_eq!(t.node(id_of(&t, &root.join("empty"))).items, 0);
    // Biggest first.
    let order: Vec<&str> = (t.sorted_children(r).iter())
        .map(|&c| &*t.node(c).name)
        .collect();
    assert_eq!(order[..3], ["big.bin", "top.txt", "a"]);
    // The newest change below a folder is its newest file's (or its own).
    assert!(t.node(r).newest >= t.node(two).modified);
    assert!(t.node(two).modified > 1_600_000_000_000_000_000);
    // Size on disk: whole clusters on Windows, st_blocks elsewhere (may lag on Unix).
    let big = t.node(id_of(&t, &root.join("big.bin")));
    if cfg!(windows) {
        assert!(big.allocated >= 10_000, "{}", big.allocated);
        assert!(t.node(TOP).allocated >= 10_360);
    }
    // A path that is not a prefix match.
    assert_eq!(t.find(&root.join("a/b/two.TXTX")), None);
    let s = t.summary(2);
    assert_eq!(s.roots[0].children.len(), 2);
    assert!(s.roots[0].truncated);
    assert_eq!(s.files, 5);
}

#[test]
fn several_roots_make_one_tree() {
    let (x, y) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(&x.path().join("one"), 10);
    write(&y.path().join("sub/two"), 20);
    let roots = [
        Root::Path(x.path().into()),
        Root::Path(y.path().into()),
        // The same root twice is scanned once.
        Root::Path(x.path().into()),
    ];
    let t = scan(&roots, &ScanOptions::default(), &ScanHandle::new()).unwrap();
    assert_eq!(t.roots().len(), 2);
    assert_eq!((t.node(TOP).size, t.node(TOP).files), (30, 2));
    assert_eq!(t.node(id_of(&t, y.path())).size, 20);
    assert!(t.find(&y.path().join("sub/two")).is_some());
    // A root that is missing or a file is refused before walking.
    let missing = Root::Path(x.path().join("nope"));
    assert!(scan(&[missing], &ScanOptions::default(), &ScanHandle::new()).is_err());
    let file = Root::Path(x.path().join("one"));
    let e = scan(&[file], &ScanOptions::default(), &ScanHandle::new()).unwrap_err();
    assert!(format!("{e:#}").contains("not a folder"), "{e:#}");
}

#[test]
fn hard_links_count_once_unless_turned_off() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("x/data.bin"), 1000);
    fs::create_dir(root.join("y")).unwrap();
    fs::hard_link(root.join("x/data.bin"), root.join("y/same.bin")).unwrap();
    write(&root.join("y/other.bin"), 7);
    let t = scan_default(root);
    assert_eq!((t.node(TOP).size, t.node(TOP).files), (1007, 3));
    let (a, b) = (
        id_of(&t, &root.join("x/data.bin")),
        id_of(&t, &root.join("y/same.bin")),
    );
    let (na, nb) = (t.node(a), t.node(b));
    assert!(na.has(flag::HARDLINK) && nb.has(flag::HARDLINK));
    assert_eq!(
        u8::from(na.has(flag::HARDLINK_DUP)) + u8::from(nb.has(flag::HARDLINK_DUP)),
        1,
        "exactly one name is the duplicate"
    );
    assert!(t.link_id(a).is_some());
    assert_eq!(t.link_id(a), t.link_id(b));
    assert_eq!(t.link_id(id_of(&t, &root.join("y/other.bin"))), None);
    // Both names keep their own size; only one counts in the totals.
    assert_eq!((na.size, nb.size), (1000, 1000));
    let x = t.node(id_of(&t, &root.join("x"))).size;
    let y = t.node(id_of(&t, &root.join("y"))).size;
    assert_eq!(x + y, 1007);
    let off = ScanOptions {
        count_hard_links_once: false,
        ..ScanOptions::default()
    };
    let t = scan_with(root, &off);
    assert_eq!(t.node(TOP).size, 2007);
    assert!(!t
        .nodes()
        .any(|(_, n)| n.has(flag::HARDLINK | flag::HARDLINK_DUP)));
}

#[test]
fn links_are_listed_not_followed_and_followed_loops_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("a/file.txt"), 5);
    let kind = dir_link(root, &root.join("a/loop"));
    dir_link(&root.join("a"), &root.join("b"));

    let t = scan_default(root);
    assert_eq!((t.node(TOP).size, t.node(TOP).files), (5, 1));
    for p in ["a/loop", "b"] {
        let n = t.node(id_of(&t, &root.join(p)));
        assert_eq!(n.kind, NodeKind::Link, "{p}");
        assert!(n.has(kind) && !n.has(flag::FOLLOWED), "{p}: {:x}", n.flags);
        assert_eq!(n.size, 0);
    }
    // Links count as items, not as files or folders.
    let r = t.node(t.roots()[0].node);
    assert_eq!((r.files, r.folders, r.items), (1, 1, 4));

    let t = scan_with(root, &following());
    assert!(t.info.complete);
    let b = t.node(id_of(&t, &root.join("b")));
    assert_eq!(b.kind, NodeKind::Dir);
    assert!(b.has(flag::FOLLOWED | kind));
    // b/file.txt is a/file.txt: counted once.
    assert_eq!((t.node(TOP).size, t.node(TOP).files), (5, 2));
    for p in ["a/loop", "b/loop"] {
        let n = t.node(id_of(&t, &root.join(p)));
        assert_eq!(n.kind, NodeKind::Link, "{p}");
        assert!(n.has(flag::LOOP), "{p}: {:x}", n.flags);
    }

    let none = ScanOptions {
        exclude_reparse_points: true,
        ..ScanOptions::default()
    };
    let t = scan_with(root, &none);
    assert!(!t.nodes().any(|(_, n)| n.kind == NodeKind::Link));
    assert_eq!(t.info.excluded, 2);
}

#[test]
fn a_followed_symlink_to_a_file_counts_once() {
    if !crate::fsattr::symlinks_allowed() {
        return eprintln!("skipped: symbolic links need Developer Mode here");
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("real.dat"), 300);
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(root.join("real.dat"), root.join("alias.dat")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("real.dat"), root.join("alias.dat")).unwrap();
    let t = scan_default(root);
    let alias = t.node(id_of(&t, &root.join("alias.dat")));
    assert_eq!(alias.kind, NodeKind::Link);
    assert!(alias.has(flag::SYMLINK));
    assert_eq!(t.node(TOP).size, 300);
    let opts = ScanOptions {
        follow_symlinks: true,
        ..ScanOptions::default()
    };
    let t = scan_with(root, &opts);
    let alias = t.node(id_of(&t, &root.join("alias.dat")));
    assert_eq!((alias.kind, alias.size), (NodeKind::File, 300));
    assert!(alias.has(flag::FOLLOWED));
    assert_eq!((t.node(TOP).size, t.node(TOP).files), (300, 2));
}

#[test]
fn filters_leave_items_out() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("keep.txt"), 100);
    write(&root.join("skip.tmp"), 100);
    write(&root.join("tiny.txt"), 1);
    write(&root.join(".secret"), 100);
    write(&root.join("node_modules/x.js"), 100);
    write(&root.join("logs/a.log"), 100);
    write(&root.join("sub/gone/z.txt"), 100);
    write(&root.join("sub/stays.txt"), 100);
    write(&root.join("old.txt"), 100);
    let two_years = Duration::from_secs(2 * 365 * 86_400);
    fs::File::options()
        .write(true)
        .open(root.join("old.txt"))
        .unwrap()
        .set_modified(SystemTime::now() - two_years)
        .unwrap();
    #[cfg(windows)]
    crate::fsattr::set_attributes(
        &root.join(".secret"),
        &crate::fsattr::Attributes {
            hidden: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    let opts = ScanOptions {
        filters: Filters {
            exclude_paths: vec![root.join("sub").join("gone")],
            exclude_names: vec!["*.TMP".into(), "node_modules".into()],
            exclude_path_regex: Some(r"[\\/]logs$".into()),
            min_size: Some(10),
            exclude_older_than: Some(365 * 86_400),
            exclude_newer_than: None,
            exclude_hidden: true,
            exclude_system: false,
        },
        ..ScanOptions::default()
    };
    let t = scan_with(root, &opts);
    let r = t.roots()[0].node;
    let mut want = vec!["keep.txt", "sub"];
    if !cfg!(windows) {
        // Names compare with case elsewhere: "*.TMP" does not match "skip.tmp".
        want.push("skip.tmp");
        want.sort();
    }
    assert_eq!(names(&t, r), want);
    assert_eq!(names(&t, id_of(&t, &root.join("sub"))), ["stays.txt"]);
    // tmp, tiny, hidden, node_modules, logs, gone, old (and skip.tmp on Windows).
    assert_eq!(t.info.excluded, if cfg!(windows) { 7 } else { 6 });

    // Only what is older than a year.
    let opts = ScanOptions {
        filters: Filters {
            exclude_newer_than: Some(365 * 86_400),
            ..Filters::default()
        },
        ..ScanOptions::default()
    };
    let t = scan_with(root, &opts);
    assert_eq!((t.node(TOP).files, t.node(TOP).size), (1, 100));
    assert!(t.find(&root.join("old.txt")).is_some());
}

#[cfg(windows)]
#[test]
fn system_files_can_be_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("sys.dat"), 10);
    write(&root.join("user.dat"), 10);
    let ok = std::process::Command::new("attrib")
        .arg("+s")
        .arg(root.join("sys.dat"))
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return eprintln!("skipped: attrib failed");
    }
    let opts = ScanOptions {
        filters: Filters {
            exclude_system: true,
            ..Filters::default()
        },
        ..ScanOptions::default()
    };
    let t = scan_with(root, &opts);
    assert_eq!(names(&t, t.roots()[0].node), ["user.dat"]);
    let _ = std::process::Command::new("attrib")
        .arg("-s")
        .arg(root.join("sys.dat"))
        .status();
}

#[test]
fn sparse_files_take_less_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sparse.img");
    let f = fs::File::create(&path).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;
        use windows::Win32::System::IO::DeviceIoControl;
        let mut got = 0u32;
        // SAFETY: the handle is the open file's; no buffers are passed.
        let r = unsafe {
            DeviceIoControl(
                HANDLE(f.as_raw_handle()),
                FSCTL_SET_SPARSE,
                None,
                0,
                None,
                0,
                Some(&mut got),
                None,
            )
        };
        if r.is_err() {
            return eprintln!("skipped: this volume has no sparse files");
        }
    }
    f.set_len(64 << 20).unwrap();
    drop(f);
    let t = scan_default(tmp.path());
    let n = t.node(id_of(&t, &path));
    assert_eq!(n.size, 64 << 20);
    if cfg!(windows) {
        assert!(n.has(flag::SPARSE), "{:x}", n.flags);
    }
    assert!(n.allocated < 1 << 20, "{}", n.allocated);
}

#[test]
fn pause_resume_and_cancel() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    for i in 0..20 {
        write(&root.join(format!("d{i}/f.txt")), 10);
    }
    let handle = ScanHandle::new();
    handle.pause();
    let (tx, rx) = std::sync::mpsc::channel();
    let (h, r) = (handle.clone(), root.clone());
    std::thread::spawn(move || {
        let _ = tx.send(scan(&[Root::Path(r)], &ScanOptions::default(), &h));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "paused"
    );
    let p = handle.progress();
    assert!(p.paused && !p.done);
    assert_eq!(p.files, 0);
    handle.resume();
    let t = rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
    assert!(t.info.complete);
    assert_eq!(t.node(TOP).files, 20);
    let p = handle.progress();
    assert!(p.done && !p.paused);
    assert_eq!((p.files, p.folders, p.bytes), (20, 20, 200));
    assert!(p.files_per_sec > 0.0);

    // Cancelled while paused: nothing below the root, flagged as not scanned.
    let handle = ScanHandle::new();
    handle.pause();
    let (tx, rx) = std::sync::mpsc::channel();
    let (h, r) = (handle.clone(), root.clone());
    std::thread::spawn(move || {
        let _ = tx.send(scan(&[Root::Path(r)], &ScanOptions::default(), &h));
    });
    std::thread::sleep(Duration::from_millis(100));
    handle.cancel();
    let t = rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
    assert!(!t.info.complete);
    let rn = t.node(t.roots()[0].node);
    assert!(rn.has(flag::INCOMPLETE));
    assert_eq!(rn.files, 0);
}

#[test]
fn refresh_rescans_one_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("a/1.txt"), 1);
    write(&root.join("b/2.txt"), 2);
    let mut t = scan_default(root);
    let (a, b) = (id_of(&t, &root.join("a")), id_of(&t, &root.join("b")));
    let one = id_of(&t, &root.join("a/1.txt"));
    let two = id_of(&t, &root.join("b/2.txt"));
    assert_eq!(t.node(TOP).size, 3);

    write(&root.join("b/3.txt"), 30);
    write(&root.join("a/later.txt"), 100);
    let h = ScanHandle::new();
    t.refresh(b, &h).unwrap();
    assert!(h.progress().done);
    assert_eq!((t.node(b).size, t.node(b).files), (32, 2));
    // Only b was walked: a's new file is not seen yet.
    assert_eq!(t.node(TOP).size, 33);
    assert_eq!(t.node(a).name.as_ref(), "a");
    assert_eq!(t.node(two).kind, NodeKind::Dead);
    assert_ne!(id_of(&t, &root.join("b/2.txt")), two);

    write(&root.join("a/1.txt"), 11);
    t.refresh(one, &ScanHandle::new()).unwrap();
    assert_eq!(t.node(one).size, 11);
    assert_eq!(t.node(a).size, 11);
    assert_eq!(t.node(TOP).size, 43);

    fs::remove_file(root.join("b/3.txt")).unwrap();
    let three = id_of(&t, &root.join("b/3.txt"));
    t.refresh(three, &ScanHandle::new()).unwrap();
    assert_eq!(t.node(three).kind, NodeKind::Dead);
    assert_eq!(t.node(TOP).size, 13);
    assert_eq!(names(&t, b), ["2.txt"]);

    fs::remove_dir_all(root.join("a")).unwrap();
    t.refresh(a, &ScanHandle::new()).unwrap();
    assert_eq!(t.find(&root.join("a")), None);
    assert_eq!(
        (t.node(TOP).size, t.node(TOP).files, t.node(TOP).folders),
        (2, 1, 2)
    );

    assert!(t.live() < t.len());
    t.compact();
    assert_eq!(t.live(), t.len());
    assert_eq!(t.node(id_of(&t, &root.join("b/2.txt"))).size, 2);

    write(&root.join("c/4.txt"), 4);
    t.refresh(TOP, &ScanHandle::new()).unwrap();
    assert_eq!(t.node(TOP).size, 6);
}

#[test]
fn a_refreshed_branch_keeps_hard_links_counted_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("x/data.bin"), 1000);
    fs::create_dir(root.join("y")).unwrap();
    fs::hard_link(root.join("x/data.bin"), root.join("y/same.bin")).unwrap();
    let mut t = scan_default(root);
    assert_eq!(t.node(TOP).size, 1000);
    for dir in ["x", "y", "x"] {
        let id = id_of(&t, &root.join(dir));
        t.refresh(id, &ScanHandle::new()).unwrap();
        assert_eq!(t.node(TOP).size, 1000, "after refreshing {dir}");
    }
}

#[test]
fn save_and_load_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tree");
    write(&root.join("x/data.bin"), 1000);
    write(&root.join("x/notes.md"), 12);
    fs::create_dir(root.join("y")).unwrap();
    fs::hard_link(root.join("x/data.bin"), root.join("y/same.bin")).unwrap();
    dir_link(&root.join("x"), &root.join("jx"));
    let mut t = scan_default(&root);
    // A refreshed tree (with Dead items) saves compacted.
    let y = id_of(&t, &root.join("y"));
    t.refresh(y, &ScanHandle::new()).unwrap();
    let mut want = t.clone();
    want.compact();
    for compress in [true, false] {
        let file = tmp.path().join(format!("scan-{compress}.keeldu"));
        save(&t, &file, compress).unwrap();
        let bytes = fs::read(&file).unwrap();
        assert_eq!(bytes.starts_with(&[0x1f, 0x8b]), compress);
        let got = load(&file).unwrap();
        assert_eq!(got.nodes, want.nodes);
        assert_eq!(got.exts, want.exts);
        assert_eq!(got.links, want.links);
        assert_eq!(got.roots, want.roots);
        assert_eq!(got.options, want.options);
        assert_eq!(got.info.complete, want.info.complete);
        assert_eq!(got.info.started, want.info.started);
        assert_eq!(got.info.elapsed.as_millis(), want.info.elapsed.as_millis());
        assert_eq!(got.node(TOP).size, 1012);
        assert!(got.find(&root.join("y/same.bin")).is_some());
    }
}

fn doc(nodes: &str) -> String {
    format!(
        r#"{{"format":"keel-du-scan","version":1,"roots":[{{"node":1,"path":"/r","total":1000,"free":300}}],"exts":["","txt"],"nodes":{nodes}}}"#
    )
}

#[test]
fn loading_checks_the_structure() {
    let ok = doc(
        r#"[["",4294967295,0,0,0,0,0,0,0,0],["/r",0,1,0,0,0,0,0,0,0],["a.txt",1,3,0,0,1,0,200,256,5],["<Free space>",1,5,0,0,0,0,0,0,0],["<Unknown>",1,6,0,0,0,0,0,0,0]]"#,
    );
    let t = load_from(ok.as_bytes()).unwrap();
    let r = t.node(1);
    assert_eq!((r.size, r.allocated, r.files), (200, 256, 1));
    // Free and unknown are not in the totals; unknown is used (700) − scanned (256).
    assert_eq!(t.node(3).size, 300);
    assert_eq!(t.node(4).size, 444);
    assert_eq!(t.node(TOP).size, 200);
    let s = t.summary(10);
    assert_eq!(
        (s.roots[0].free, s.roots[0].unknown),
        (Some(300), Some(444))
    );
    assert_eq!(s.roots[0].children.len(), 1);

    let bad = [
        // A child before its parent.
        doc(
            r#"[["",4294967295,0,0,0,0,0,0,0,0],["x",2,3,0,0,0,0,1,1,0],["/r",0,1,0,0,0,0,0,0,0]]"#,
        ),
        // A file below a file.
        doc(
            r#"[["",4294967295,0,0,0,0,0,0,0,0],["/r",0,1,0,0,0,0,0,0,0],["a",1,3,0,0,0,0,1,1,0],["b",2,3,0,0,0,0,1,1,0]]"#,
        ),
        // Siblings apart.
        doc(
            r#"[["",4294967295,0,0,0,0,0,0,0,0],["/r",0,1,0,0,0,0,0,0,0],["d",1,2,0,0,0,0,0,0,0],["e",2,3,0,0,0,0,1,1,0],["f",1,3,0,0,0,0,1,1,0]]"#,
        ),
        // An extension out of range, a kind that does not exist.
        doc(
            r#"[["",4294967295,0,0,0,0,0,0,0,0],["/r",0,1,0,0,0,0,0,0,0],["a",1,3,0,0,9,0,1,1,0]]"#,
        ),
        doc(r#"[["",4294967295,0,0,0,0,0,0,0,0],["/r",0,9,0,0,0,0,0,0,0]]"#),
        // No root item for the root.
        doc(r#"[["",4294967295,0,0,0,0,0,0,0,0]]"#),
        "not json".into(),
        r#"{"format":"something else","version":1,"roots":[],"exts":[""],"nodes":[]}"#.into(),
    ];
    for b in &bad {
        assert!(load_from(b.as_bytes()).is_err(), "{b}");
    }

    let tmp = tempfile::tempdir().unwrap();
    let newer = tmp.path().join("newer.keeldu");
    fs::write(
        &newer,
        r#"{"format":"keel-du-scan","version":99,"nodes":{"some":"new layout"}}"#,
    )
    .unwrap();
    let e = format!("{:#}", load(&newer).unwrap_err());
    assert!(e.contains("newer than this Keel"), "{e}");
}

#[test]
fn drive_roots_and_local_drives() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!sys::is_drive_root(tmp.path()));
    #[cfg(windows)]
    {
        let drives = local_drives();
        assert!(!drives.is_empty());
        assert!(drives.iter().all(|d| sys::is_drive_root(d)), "{drives:?}");
    }
    #[cfg(unix)]
    assert!(sys::is_drive_root(Path::new("/")));
}

/// 200,000 files in 400 folders: `cargo test -p keel-vfs --release du::tests::bench --
/// --ignored --nocapture`.
#[test]
#[ignore]
fn bench_200k_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let made = std::time::Instant::now();
    for a in 0..20 {
        for b in 0..20 {
            let dir = root.join(format!("d{a}/e{b}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..500 {
                fs::write(dir.join(format!("f{f}.dat")), b"x").unwrap();
            }
        }
    }
    eprintln!("made 200000 files in {:?}", made.elapsed());
    for run in 0..2 {
        let h = ScanHandle::new();
        let t = scan(&[Root::Path(root.into())], &ScanOptions::default(), &h).unwrap();
        let p = h.progress();
        eprintln!(
            "scan {run}: {} files in {:?} ({:.0} files/s, {} items)",
            t.node(TOP).files,
            p.elapsed,
            p.files_per_sec,
            t.len()
        );
        assert_eq!(t.node(TOP).files, 200_000);
        assert_eq!(t.node(TOP).folders, 421);
    }
}
