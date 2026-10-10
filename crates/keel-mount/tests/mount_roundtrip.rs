//! A real mount through the compiled backend: mounts a temp source, lists, reads, writes
//! and unmounts. Needs the driver (WinFsp on Windows, FUSE on Linux, macFUSE on macOS),
//! so it is ignored and gated on an env var (CI runners have no driver):
//!
//! ```text
//! KEEL_MOUNT_TEST=1 cargo test -p keel-mount --features winfsp -- --ignored   # Windows
//! KEEL_MOUNT_TEST=1 cargo test -p keel-mount --features fuse -- --ignored     # Linux/macOS
//! ```
//!
//! `KEEL_MOUNT_TARGET` picks the target (a drive letter such as `K:`); by default it is a
//! folder in the temp directory.
#![cfg(any(all(windows, feature = "winfsp"), all(unix, feature = "fuse")))]

use keel_core::{Library, SourceDef, SourceKind};
use keel_mount::Mounts;
use keel_vfs::{Router, VPath};
use std::fs;
use std::sync::Arc;

#[test]
#[ignore = "needs WinFsp or FUSE: set KEEL_MOUNT_TEST=1 and run with --ignored"]
fn mount_list_read_write_unmount() {
    if std::env::var_os("KEEL_MOUNT_TEST").is_none() {
        eprintln!("KEEL_MOUNT_TEST is not set: skipped");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    fs::create_dir(files.path().join("docs")).unwrap();
    fs::write(files.path().join("docs/a.txt"), b"0123456789").unwrap();
    let lib = Library::open(data.path(), "mount-test").unwrap();
    lib.set_hash_after_walk(false);
    let router = Arc::new(Router::new());
    lib.set_router(router.clone());
    let id = lib
        .add_source(SourceDef {
            label: "Files".into(),
            root: VPath::local(files.path()),
            kind: SourceKind::Folder,
            include_hidden: true,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap();
    let lib = Arc::new(lib);
    let job = lib.index(&id).unwrap();
    lib.jobs().wait(job).unwrap();

    let holder = tempfile::tempdir().unwrap();
    let target = std::env::var("KEEL_MOUNT_TARGET").unwrap_or_else(|_| {
        let dir = holder.path().join("mnt");
        if cfg!(unix) {
            fs::create_dir(&dir).unwrap();
        }
        dir.display().to_string()
    });
    let spool = data.path().join("spool");
    let mounts = Mounts::new(spool);
    let info = mounts.add(&lib, &router, &id, "", &target).unwrap();
    let root = std::path::PathBuf::from(if info.target.ends_with(':') {
        format!("{}\\", info.target)
    } else {
        info.target.clone()
    });

    let mut listed: Vec<_> = fs::read_dir(root.join("docs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    listed.sort();
    assert_eq!(listed, vec!["a.txt"]);
    assert_eq!(fs::read(root.join("docs/a.txt")).unwrap(), b"0123456789");

    // A new file reaches the source when it is closed, with no staging file left.
    fs::write(root.join("docs/new.txt"), b"written through the mount").unwrap();
    assert_eq!(
        fs::read(files.path().join("docs/new.txt")).unwrap(),
        b"written through the mount"
    );
    fs::rename(root.join("docs/new.txt"), root.join("docs/renamed.txt")).unwrap();
    assert!(files.path().join("docs/renamed.txt").is_file());
    fs::create_dir(root.join("made")).unwrap();
    assert!(files.path().join("made").is_dir());
    fs::remove_dir(root.join("made")).unwrap();
    let staged = fs::read_dir(files.path().join("docs"))
        .unwrap()
        .filter(|e| keel_vfs::ops::is_partial(&e.as_ref().unwrap().file_name().to_string_lossy()))
        .count();
    assert_eq!(staged, 0);

    mounts.remove(&info.target).unwrap();
    assert!(mounts.list().is_empty());
    assert!(fs::read(root.join("docs/a.txt")).is_err(), "unmounted");
}
