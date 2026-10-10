//! A real mount through the compiled backend: mounts a temp source, lists, reads, writes,
//! checks times, sizes and free space, renames a file while it is written and unmounts.
//! Needs the driver (WinFsp on Windows, FUSE on Linux, macFUSE on macOS), so it runs only
//! when `KEEL_MOUNT_TEST` (or `KEEL_FUSE_TEST`, as the CI `mount` job sets it) is set:
//!
//! ```text
//! KEEL_MOUNT_TEST=1 cargo test -p keel-mount --features winfsp   # Windows
//! KEEL_FUSE_TEST=1 cargo test -p keel-mount --features fuse      # Linux/macOS
//! ```
//!
//! `KEEL_MOUNT_TARGET` picks the target (a drive letter such as `K:`); by default it is a
//! folder in the temp directory.
#![cfg(any(all(windows, feature = "winfsp"), all(unix, feature = "fuse")))]

use keel_core::{Library, SourceDef, SourceKind};
use keel_mount::Mounts;
use keel_vfs::{Router, VPath};
use std::fs;
use std::io::Write;
use std::sync::Arc;

#[test]
fn mount_list_read_write_unmount() {
    if std::env::var_os("KEEL_MOUNT_TEST").is_none() && std::env::var_os("KEEL_FUSE_TEST").is_none()
    {
        eprintln!("SKIP: needs WinFsp or FUSE; set KEEL_MOUNT_TEST=1 (or KEEL_FUSE_TEST=1)");
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

    // Times and sizes are the source's.
    let shown = fs::metadata(root.join("docs/a.txt")).unwrap();
    let real = fs::metadata(files.path().join("docs/a.txt")).unwrap();
    assert_eq!(shown.len(), 10);
    let secs =
        |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(
        secs(shown.modified().unwrap()),
        secs(real.modified().unwrap())
    );
    assert!(!shown.permissions().readonly());

    // Free space: the source volume's (`df`).
    #[cfg(unix)]
    {
        let space = |p: &std::path::Path| {
            let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: zeroed is a valid statvfs; `c` is NUL-terminated.
            let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::statvfs(c.as_ptr(), &mut v) }, 0);
            (
                v.f_blocks as u64 * v.f_frsize as u64,
                v.f_bavail as u64 * v.f_frsize as u64,
            )
        };
        let (total, free) = space(&root);
        let (real_total, _) = space(files.path());
        assert!(total > 0 && free <= total, "{total} {free}");
        // Whole 4 KiB blocks of the same volume.
        assert!(real_total.abs_diff(total) < 4096, "{real_total} vs {total}");
    }

    // A file renamed while it is written lands under the new name, also in another folder,
    // and its size is the written length the whole time.
    fs::create_dir(root.join("moved")).unwrap();
    {
        let mut w = fs::File::create(root.join("docs/live.txt")).unwrap();
        w.write_all(b"first,").unwrap();
        assert_eq!(w.metadata().unwrap().len(), 6);
        fs::rename(root.join("docs/live.txt"), root.join("docs/live2.txt")).unwrap();
        w.write_all(b"second,").unwrap();
        fs::rename(root.join("docs/live2.txt"), root.join("moved/live3.txt")).unwrap();
        w.write_all(b"third").unwrap();
        assert_eq!(
            fs::metadata(root.join("moved/live3.txt")).unwrap().len(),
            18
        );
        assert!(
            !files.path().join("moved/live3.txt").exists(),
            "not published yet"
        );
    }
    assert_eq!(
        fs::read(files.path().join("moved/live3.txt")).unwrap(),
        b"first,second,third"
    );
    assert!(!files.path().join("docs/live.txt").exists());
    assert!(!files.path().join("docs/live2.txt").exists());
    // A descriptor closed before anything is written (a shell's `>`, `dd of=`: open, dup2,
    // close) publishes no empty file.
    {
        let mut w = fs::File::create(root.join("docs/dup.txt")).unwrap();
        drop(w.try_clone().unwrap());
        assert!(!files.path().join("docs/dup.txt").exists());
        w.write_all(b"dup").unwrap();
    }
    assert_eq!(fs::read(files.path().join("docs/dup.txt")).unwrap(), b"dup");
    // Appending keeps the written length (the OS appends at the size the mount reports).
    {
        let mut a = fs::OpenOptions::new()
            .append(true)
            .open(root.join("moved/live3.txt"))
            .unwrap();
        a.write_all(b"+1").unwrap();
        let _ = fs::metadata(root.join("moved/live3.txt")).unwrap();
        a.write_all(b"+2").unwrap();
    }
    assert_eq!(
        fs::read(files.path().join("moved/live3.txt")).unwrap(),
        b"first,second,third+1+2"
    );

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
    for dir in ["docs", "moved"] {
        let staged = fs::read_dir(files.path().join(dir))
            .unwrap()
            .filter(|e| {
                keel_vfs::ops::is_partial(&e.as_ref().unwrap().file_name().to_string_lossy())
            })
            .count();
        assert_eq!(staged, 0, "{dir}");
    }

    mounts.remove(&info.target).unwrap();
    assert!(mounts.list().is_empty());
    assert!(fs::read(root.join("docs/a.txt")).is_err(), "unmounted");
}
