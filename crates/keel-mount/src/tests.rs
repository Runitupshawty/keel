use super::*;
use keel_core::{SourceDef, SourceKind};
use keel_vfs::VPath;
use std::fs;
use std::io::{self, Read, Write};

struct Fixture {
    _data: tempfile::TempDir,
    files: tempfile::TempDir,
    spool: tempfile::TempDir,
    lib: Arc<Library>,
    router: Arc<Router>,
}

fn fixture() -> Fixture {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    fs::create_dir(files.path().join("docs")).unwrap();
    fs::write(files.path().join("docs/a.txt"), b"0123456789").unwrap();
    fs::write(files.path().join("docs/b.bin"), b"bee").unwrap();
    fs::write(files.path().join("docs/b.bin.keel-partial-9-9"), b"junk").unwrap();
    let lib = Library::open(data.path(), "test").unwrap();
    lib.set_hash_after_walk(false);
    let router = Arc::new(Router::new());
    lib.set_router(router.clone());
    Fixture {
        _data: data,
        files,
        spool: tempfile::tempdir().unwrap(),
        lib: Arc::new(lib),
        router,
    }
}

fn add(f: &Fixture, root: VPath, kind: SourceKind) -> SourceId {
    f.lib
        .add_source(SourceDef {
            label: "Files".into(),
            root,
            kind,
            include_hidden: true,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap()
}

fn indexed(f: &Fixture) -> SourceId {
    let id = add(f, VPath::local(f.files.path()), SourceKind::Folder);
    let job = f.lib.index(&id).unwrap();
    let info = f.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, keel_core::JobStatus::Done, "{}", info.log);
    id
}

fn mount_fs(f: &Fixture, id: &SourceId, subtree: &str, windows: bool) -> MountFs {
    MountFs::new(
        f.lib.clone(),
        f.router.clone(),
        id,
        subtree,
        windows,
        f.spool.path().to_owned(),
    )
    .unwrap()
}

fn mp(s: &str) -> MountPath {
    MountPath::parse(s).unwrap()
}

fn names(entries: &[DirEntry]) -> Vec<String> {
    let mut v: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    v.sort();
    v
}

fn on_disk(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn lists_stats_and_reads_ranges_online() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    assert_eq!(names(&m.list(&MountPath::root()).unwrap()), vec!["docs"]);
    let docs = m.list(&mp("docs")).unwrap();
    assert_eq!(
        names(&docs),
        vec!["a.txt", "b.bin"],
        "staging files are hidden"
    );
    let a = m.stat(&mp("docs/a.txt")).unwrap();
    assert!(!a.is_dir);
    assert_eq!(a.size, 10);
    assert!(m.stat(&mp("docs")).unwrap().is_dir);
    assert_eq!(
        m.stat(&mp("docs/missing")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        m.stat(&mp("docs/b.bin.keel-partial-9-9"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );

    let (h, _) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    let mut buf = [0; 4];
    assert_eq!(m.read(h, 6, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"6789");
    assert_eq!(m.read(h, 2, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"2345");
    assert_eq!(m.read(h, 10, &mut buf).unwrap(), 0, "end of file");
    assert!(m.write(h, 0, b"x").is_err(), "opened read-only");
    m.release(h).unwrap();

    // A subtree mount sees only its folder.
    let sub = mount_fs(&f, &id, "docs", false);
    assert_eq!(
        names(&sub.list(&MountPath::root()).unwrap()),
        vec!["a.txt", "b.bin"]
    );
}

#[test]
fn new_files_appear_outside_the_mount_only_when_closed() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let docs = f.files.path().join("docs");
    let (h, attr) = m.create(&mp("docs/new.txt")).unwrap();
    assert_eq!(attr.size, 0);
    m.write(h, 0, b"hello ").unwrap();
    m.write(h, 6, b"world").unwrap();
    // The mount shows the file being written; the folder does not have it yet.
    let listed = m.list(&mp("docs")).unwrap();
    let new = listed.iter().find(|e| e.name == "new.txt").unwrap();
    assert_eq!(new.attr.size, 11);
    assert_eq!(m.stat(&mp("docs/new.txt")).unwrap().size, 11);
    assert!(!docs.join("new.txt").exists());
    let mut buf = [0; 5];
    m.read(h, 6, &mut buf).unwrap();
    assert_eq!(&buf, b"world", "reads see the handle's own writes");
    assert_eq!(m.pending_writes(), 1);
    assert_eq!(
        m.create(&mp("docs/new.txt")).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    m.release(h).unwrap();
    assert_eq!(fs::read(docs.join("new.txt")).unwrap(), b"hello world");
    assert_eq!(m.pending_writes(), 0);
    let left: Vec<_> = on_disk(&docs)
        .into_iter()
        .filter(|n| keel_vfs::ops::is_partial(n))
        .collect();
    assert_eq!(
        left,
        vec!["b.bin.keel-partial-9-9"],
        "only the old leftover"
    );
}

#[test]
fn edits_keep_the_old_file_until_the_last_writer_closes() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let a = f.files.path().join("docs/a.txt");
    let (w1, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    let (w2, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    m.write(w1, 0, b"AB").unwrap();
    m.write(w2, 8, b"YZ").unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"0123456789");
    m.release(w1).unwrap();
    assert_eq!(
        fs::read(&a).unwrap(),
        b"0123456789",
        "a writer is still open"
    );
    m.release(w2).unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"AB234567YZ");

    // Truncating open: the new content replaces the old one on close.
    let (h, attr) = m.open(&mp("docs/a.txt"), true, true).unwrap();
    assert_eq!(attr.size, 0);
    m.write(h, 0, b"short").unwrap();
    m.release(h).unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"short");

    // A writer that never writes leaves the file alone.
    let (h, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    m.release(h).unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"short");
}

#[test]
fn unmount_and_delete_drop_unfinished_writes() {
    let f = fixture();
    let id = indexed(&f);
    let docs = f.files.path().join("docs");
    let before = on_disk(&docs);
    {
        let m = mount_fs(&f, &id, "", false);
        let (h, _) = m.create(&mp("docs/half.txt")).unwrap();
        m.write(h, 0, b"half").unwrap();
        let (e, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
        m.write(e, 0, b"zz").unwrap();
        assert_eq!(m.pending_writes(), 2);
        // Deleting a file still being created drops it.
        m.remove_file(&mp("docs/half.txt")).unwrap();
        assert!(m.write(h, 4, b"more").is_err());
        assert_eq!(m.pending_writes(), 1);
        m.release(h).unwrap();
        assert!(m.stat(&mp("docs/half.txt")).is_err());
        m.abort_all();
    }
    assert_eq!(on_disk(&docs), before, "no new or staging files");
    assert_eq!(fs::read(docs.join("a.txt")).unwrap(), b"0123456789");
}

#[test]
fn mkdir_rename_and_rmdir() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    m.mkdir(&mp("new")).unwrap();
    assert!(f.files.path().join("new").is_dir());
    assert_eq!(
        m.mkdir(&mp("new")).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    let (h, _) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    m.rename(&mp("docs/a.txt"), &mp("new/a2.txt"), false)
        .unwrap();
    assert!(f.files.path().join("new/a2.txt").is_file());
    assert_eq!(
        m.handle_path(h).unwrap(),
        mp("new/a2.txt"),
        "handles follow"
    );
    let mut buf = [0; 3];
    m.read(h, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"012");
    m.release(h).unwrap();
    // No replace unless asked; never over a folder.
    assert_eq!(
        m.rename(&mp("docs/b.bin"), &mp("new/a2.txt"), false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    m.rename(&mp("docs/b.bin"), &mp("new/a2.txt"), true)
        .unwrap();
    assert_eq!(fs::read(f.files.path().join("new/a2.txt")).unwrap(), b"bee");
    assert_eq!(
        m.remove_dir(&mp("new")).unwrap_err().kind(),
        io::ErrorKind::DirectoryNotEmpty
    );
    m.mkdir(&mp("empty")).unwrap();
    m.remove_dir(&mp("empty")).unwrap();
    assert!(!f.files.path().join("empty").exists());
    // A file being written cannot be renamed yet.
    let (w, _) = m.create(&mp("docs/w.txt")).unwrap();
    assert_eq!(
        m.rename(&mp("docs/w.txt"), &mp("docs/w2.txt"), false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ResourceBusy
    );
    m.release(w).unwrap();
    m.rename(&mp("docs/w.txt"), &mp("docs/w2.txt"), false)
        .unwrap();
}

#[test]
fn offline_listings_come_from_the_index() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let moved = f.files.path().with_extension("away");
    fs::rename(f.files.path(), &moved).unwrap();
    // The status poll has not run yet: the failed listing falls back to the index.
    assert_eq!(names(&m.list(&mp("docs")).unwrap()), vec!["a.txt", "b.bin"]);
    f.lib.refresh_status().join().unwrap();
    let status = f.lib.source(&id).unwrap().status.read().clone();
    assert!(
        matches!(status, keel_core::SourceStatus::Offline { .. }),
        "{status:?}"
    );
    assert_eq!(names(&m.list(&MountPath::root()).unwrap()), vec!["docs"]);
    let a = m.stat(&mp("docs/a.txt")).unwrap();
    assert_eq!(a.size, 10);
    assert!(a.modified.is_some());
    // Contents and writes need the source.
    let (h, _) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    let mut buf = [0; 2];
    assert_eq!(
        m.read(h, 0, &mut buf).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    m.release(h).unwrap();
    assert!(m.create(&mp("docs/x.txt")).is_err());
    assert!(m.open(&mp("docs/a.txt"), true, false).is_err());
    fs::rename(&moved, f.files.path()).unwrap();
}

fn memory_cloud() -> Arc<keel_vfs::CloudProvider> {
    let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    let account = keel_vfs::CloudAccount {
        id: "mem".into(),
        label: "Memory".into(),
        kind: keel_vfs::CloudKind::S3,
        root: None,
        client_id_override: None,
        s3: None,
    };
    Arc::new(
        keel_vfs::CloudProvider::with_operator(account, op, crossbeam_channel::unbounded().0)
            .unwrap(),
    )
}

#[test]
fn remote_sources_read_ranges_and_upload_on_close() {
    use keel_vfs::Provider;
    let f = fixture();
    let cloud = memory_cloud();
    f.router
        .register_cloud_provider("mem".into(), cloud.clone());
    let root = VPath::parse("cloud://mem/").unwrap();
    cloud.mkdir(&root.join("Docs")).unwrap();
    let mut w = cloud.write(&root.join("Docs/notes.txt")).unwrap();
    w.write_all(b"abcdefghij").unwrap();
    w.flush().unwrap();
    drop(w);
    let id = add(&f, root.clone(), SourceKind::Cloud);
    // Served on Windows: a case-sensitive source behind a case-insensitive mount.
    let m = mount_fs(&f, &id, "", true);
    assert!(m.map().fold_case);
    let (path, attr) = m.lookup(&mp("docs/NOTES.TXT")).unwrap();
    assert_eq!(path, mp("Docs/notes.txt"), "the source's spelling");
    assert_eq!(attr.size, 10);

    let (h, _) = m.open(&mp("DOCS/notes.txt"), false, false).unwrap();
    let mut buf = [0; 3];
    m.read(h, 2, &mut buf).unwrap();
    assert_eq!(&buf, b"cde");
    m.read(h, 5, &mut buf).unwrap();
    assert_eq!(&buf, b"fgh", "a sequential read continues the reader");
    m.read(h, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"abc", "a backward jump starts a new one");
    m.release(h).unwrap();

    let (h, _) = m.open(&mp("docs/notes.txt"), true, false).unwrap();
    m.write(h, 0, b"ABC").unwrap();
    let mut old = Vec::new();
    cloud
        .read(&root.join("Docs/notes.txt"))
        .unwrap()
        .read_to_end(&mut old)
        .unwrap();
    assert_eq!(old, b"abcdefghij");
    m.release(h).unwrap();
    let mut new = Vec::new();
    cloud
        .read(&root.join("Docs/notes.txt"))
        .unwrap()
        .read_to_end(&mut new)
        .unwrap();
    assert_eq!(new, b"ABCdefghij");
    let listed: Vec<_> = cloud
        .list(&root.join("Docs"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(listed, vec!["notes.txt"], "no staging file left");
    assert_eq!(fs::read_dir(f.spool.path()).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn targets_normalize() {
    for t in ["k", "K:", "k:\\", "K:/"] {
        assert_eq!(normalize_target(t).unwrap(), "K:");
    }
    assert_eq!(
        normalize_target(r"D:\Mounts\Photos\").unwrap(),
        r"D:\Mounts\Photos"
    );
    assert!(normalize_target("relative\\dir").is_err());
    assert!(normalize_target("KK:").is_err());
}

#[cfg(unix)]
#[test]
fn targets_normalize() {
    assert_eq!(normalize_target("/mnt/photos/").unwrap(), "/mnt/photos");
    assert_eq!(normalize_target("/").unwrap(), "/");
    assert!(normalize_target("K:").is_err());
    assert!(normalize_target("rel/dir").is_err());
}

#[test]
fn mounting_without_a_backend_says_how_to_get_one() {
    let f = fixture();
    let id = indexed(&f);
    let mounts = Mounts::new(f.spool.path().to_owned());
    let target = if cfg!(windows) {
        "Q:"
    } else {
        "/nonexistent-keel-mount"
    };
    let r = mounts.check(&f.lib, &f.router, &id, "", target);
    if backend().is_none() {
        assert!(format!("{:#}", r.err().unwrap()).contains("--features"));
    }
    assert!(mounts.list().is_empty());
    assert!(mounts.remove("Q:").is_err());
}
