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
    // Only the writer sees the file being written; listings show the published state.
    assert_eq!(m.handle_attr(h).unwrap().size, 11);
    let listed = m.list(&mp("docs")).unwrap();
    assert!(!listed.iter().any(|e| e.name == "new.txt"), "{listed:?}");
    assert_eq!(
        m.stat(&mp("docs/new.txt")).unwrap_err().kind(),
        io::ErrorKind::ResourceBusy,
        "being created: busy, not missing"
    );
    assert_eq!(
        m.open(&mp("docs/new.txt"), false, false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ResourceBusy
    );
    assert!(!docs.join("new.txt").exists());
    let mut buf = [0; 5];
    m.read(h, 6, &mut buf).unwrap();
    assert_eq!(&buf, b"world", "reads see the handle's own writes");
    assert_eq!(m.pending_writes(), 1);
    assert_eq!(
        m.create(&mp("docs/new.txt")).unwrap_err().kind(),
        io::ErrorKind::ResourceBusy
    );
    m.release(h).unwrap();
    assert_eq!(m.stat(&mp("docs/new.txt")).unwrap().size, 11);
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
        // Closes processed while the backend shuts down never publish.
        m.flush(e).unwrap();
        m.release(e).unwrap();
        assert_eq!(fs::read(docs.join("a.txt")).unwrap(), b"0123456789");
        assert!(m.open(&mp("docs/a.txt"), true, false).is_err(), "closing");
        assert!(m.create(&mp("docs/late.txt")).is_err(), "closing");
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
    // A file being written can be renamed (see the rename-while-written tests below).
    let (w, _) = m.create(&mp("docs/w.txt")).unwrap();
    m.rename(&mp("docs/w.txt"), &mp("docs/w2.txt"), false)
        .unwrap();
    m.release(w).unwrap();
    assert!(f.files.path().join("docs/w2.txt").is_file());
    assert!(!f.files.path().join("docs/w.txt").exists());
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
        webdav: None,
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

#[test]
fn a_pending_write_is_private_to_its_writers() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let (early, _) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    let (w, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    // A writer that has not written yet hides nothing.
    let (r, _) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    m.release(r).unwrap();
    m.write(w, 10, b"XYZ").unwrap();
    assert_eq!(
        m.handle_attr(w).unwrap().size,
        13,
        "the writer sees its write"
    );
    assert_eq!(
        m.open(&mp("docs/a.txt"), false, false).unwrap_err().kind(),
        io::ErrorKind::ResourceBusy,
        "a second open while the write is pending"
    );
    let listed = m.list(&mp("docs")).unwrap();
    let a = listed.iter().find(|e| e.name == "a.txt").unwrap();
    assert_eq!(a.attr.size, 10, "listings show the published size");
    assert_eq!(m.stat(&mp("docs/a.txt")).unwrap().size, 10);
    let mut buf = [0; 13];
    assert_eq!(m.read(early, 0, &mut buf).unwrap(), 10, "published content");
    assert_eq!(m.handle_attr(early).unwrap().size, 10);
    m.release(early).unwrap();
    m.release(w).unwrap();
    let (r, attr) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    assert_eq!(attr.size, 13);
    m.release(r).unwrap();
}

#[test]
fn flush_publishes_when_the_only_writer_closes() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let a = f.files.path().join("docs/a.txt");
    let (w1, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    let (w2, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    m.write(w1, 0, b"AB").unwrap();
    m.flush(w1).unwrap();
    assert_eq!(
        fs::read(&a).unwrap(),
        b"0123456789",
        "another writer is open"
    );
    m.release(w1).unwrap();
    m.flush(w2).unwrap();
    assert_eq!(
        fs::read(&a).unwrap(),
        b"AB23456789",
        "published by the flush, before release"
    );
    assert_eq!(m.pending_writes(), 0);
    let (r, attr) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    assert_eq!(attr.size, 10);
    m.release(r).unwrap();
    // A write after the flush (a dup'd descriptor) stages again from the new content.
    m.write(w2, 9, b"!").unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"AB23456789");
    m.release(w2).unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"AB2345678!");
    let partials = on_disk(&f.files.path().join("docs"))
        .into_iter()
        .filter(|n| keel_vfs::ops::is_partial(n))
        .count();
    assert_eq!(partials, 1, "only the fixture's leftover");
}

#[test]
fn a_failed_publish_keeps_the_data_under_an_unsaved_name() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let docs = f.files.path().join("docs");
    let (h, _) = m.create(&mp("docs/report.txt")).unwrap();
    m.write(h, 0, b"precious").unwrap();
    // Something else puts a folder there: the publishing rename fails.
    fs::create_dir(docs.join("report.txt")).unwrap();
    fs::write(docs.join("report.txt/inside"), b"x").unwrap();
    assert!(m.release(h).is_err(), "the failure is reported");
    let kept: Vec<_> = on_disk(&docs)
        .into_iter()
        .filter(|n| n.starts_with("report (unsaved "))
        .collect();
    assert_eq!(kept.len(), 1, "{:?}", on_disk(&docs));
    assert!(kept[0].ends_with(").txt"), "{kept:?}");
    assert!(!keel_vfs::ops::is_partial(&kept[0]), "not swept");
    assert_eq!(fs::read(docs.join(&kept[0])).unwrap(), b"precious");
    assert_eq!(m.pending_writes(), 0);
}

#[test]
fn a_mount_target_inside_the_source_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("sub")).unwrap();
    let inside = root.path().join("sub").join("mnt");
    assert!(check_outside(inside.to_str().unwrap(), root.path()).is_err());
    assert!(check_outside(root.path().to_str().unwrap(), root.path()).is_err());
    let outside = other.path().join("mnt");
    check_outside(outside.to_str().unwrap(), root.path()).unwrap();
    check_outside("Q:", root.path()).unwrap();
}

/// A memory cloud whose `read` and `rename_replace` can be held (a slow remote).
struct Gated {
    inner: Arc<keel_vfs::CloudProvider>,
    hold_read: std::sync::atomic::AtomicBool,
    hold_rename: std::sync::atomic::AtomicBool,
    entered: crossbeam_channel::Sender<&'static str>,
    gate: crossbeam_channel::Receiver<()>,
}

impl Gated {
    fn wait(&self, flag: &std::sync::atomic::AtomicBool, what: &'static str) {
        if flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered.send(what).unwrap();
            self.gate.recv().unwrap();
        }
    }
}

impl keel_vfs::Provider for Gated {
    fn scheme(&self) -> &'static str {
        self.inner.scheme()
    }
    fn caps(&self) -> keel_vfs::Caps {
        self.inner.caps()
    }
    fn list(&self, d: &VPath) -> anyhow::Result<Vec<keel_vfs::Entry>> {
        self.inner.list(d)
    }
    fn stat(&self, p: &VPath) -> anyhow::Result<keel_vfs::Entry> {
        self.inner.stat(p)
    }
    fn list_complete(&self, d: &VPath) -> anyhow::Result<Vec<keel_vfs::Entry>> {
        self.inner.list_complete(d)
    }
    fn read(&self, p: &VPath) -> anyhow::Result<Box<dyn Read + Send>> {
        self.wait(&self.hold_read, "read");
        self.inner.read(p)
    }
    fn write(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
        self.inner.write(p)
    }
    fn create_new(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
        self.inner.create_new(p)
    }
    fn mkdir(&self, p: &VPath) -> anyhow::Result<()> {
        self.inner.mkdir(p)
    }
    fn rename(&self, a: &VPath, b: &VPath) -> anyhow::Result<()> {
        self.inner.rename(a, b)
    }
    fn rename_replace(&self, a: &VPath, b: &VPath) -> anyhow::Result<()> {
        self.wait(&self.hold_rename, "rename");
        self.inner.rename_replace(a, b)
    }
    fn remove(&self, p: &VPath) -> anyhow::Result<()> {
        self.inner.remove(p)
    }
    fn remove_kind(&self) -> keel_vfs::RemoveKind {
        self.inner.remove_kind()
    }
    fn local_copy(&self, p: &VPath) -> anyhow::Result<std::path::PathBuf> {
        self.inner.local_copy(p)
    }
}

type Gate = (
    Arc<Gated>,
    crossbeam_channel::Receiver<&'static str>,
    crossbeam_channel::Sender<()>,
);

/// A remote source `cloud://mem/` holding `Docs/notes.txt` = "abcdefghij", behind a gate.
fn gated(f: &Fixture) -> (SourceId, Gate) {
    use keel_vfs::Provider;
    let cloud = memory_cloud();
    let root = VPath::parse("cloud://mem/").unwrap();
    cloud.mkdir(&root.join("Docs")).unwrap();
    let mut w = cloud.write(&root.join("Docs/notes.txt")).unwrap();
    w.write_all(b"abcdefghij").unwrap();
    w.flush().unwrap();
    drop(w);
    let (entered_tx, entered) = crossbeam_channel::unbounded();
    let (open, gate) = crossbeam_channel::unbounded();
    let g = Arc::new(Gated {
        inner: cloud,
        hold_read: false.into(),
        hold_rename: false.into(),
        entered: entered_tx,
        gate,
    });
    f.router.register_cloud_provider("mem".into(), g.clone());
    (add(f, root, SourceKind::Cloud), (g, entered, open))
}

const SOON: std::time::Duration = std::time::Duration::from_secs(10);

#[test]
fn a_slow_remote_stage_does_not_block_listings() {
    let f = fixture();
    let (id, (g, entered, open)) = gated(&f);
    let m = Arc::new(mount_fs(&f, &id, "", false));
    let (w, _) = m.open(&mp("Docs/notes.txt"), true, false).unwrap();
    g.hold_read.store(true, std::sync::atomic::Ordering::SeqCst);
    let writer = {
        let m = m.clone();
        std::thread::spawn(move || m.write(w, 0, b"ABC").map(drop))
    };
    assert_eq!(entered.recv_timeout(SOON).unwrap(), "read");
    // The download is held: the mount still answers.
    let (tx, rx) = crossbeam_channel::unbounded();
    {
        let m = m.clone();
        std::thread::spawn(move || {
            let listed = m.list(&mp("Docs")).map(|l| l.len());
            let stat = m.stat(&mp("Docs/notes.txt")).map(|a| a.size);
            let pending = m.pending_writes();
            tx.send((listed.ok(), stat.ok(), pending)).unwrap();
        });
    }
    assert_eq!(rx.recv_timeout(SOON).unwrap(), (Some(1), Some(10), 0));
    open.send(()).unwrap();
    writer.join().unwrap().unwrap();
    m.release(w).unwrap();
    assert_eq!(m.stat(&mp("Docs/notes.txt")).unwrap().size, 10);
}

#[test]
fn a_write_stays_pending_until_its_publish_returns() {
    let f = fixture();
    let (id, (g, entered, open)) = gated(&f);
    let m = Arc::new(mount_fs(&f, &id, "", false));
    let (w, _) = m.open(&mp("Docs/notes.txt"), true, false).unwrap();
    m.write(w, 0, b"ABC").unwrap();
    g.hold_rename
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let closer = {
        let m = m.clone();
        std::thread::spawn(move || m.release(w))
    };
    assert_eq!(entered.recv_timeout(SOON).unwrap(), "rename");
    // The upload is done but not placed: a new open waits for it instead of reading (or
    // staging from) the old content.
    let (tx, rx) = crossbeam_channel::unbounded();
    {
        let m = m.clone();
        std::thread::spawn(move || {
            let r = m
                .open(&mp("Docs/notes.txt"), false, false)
                .and_then(|(h, _)| {
                    let mut buf = [0; 10];
                    let n = m.read(h, 0, &mut buf)?;
                    m.release(h)?;
                    Ok(buf[..n].to_vec())
                });
            tx.send(r.map_err(|e| e.kind())).unwrap();
        });
    }
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the open waits while the publish runs"
    );
    open.send(()).unwrap();
    closer.join().unwrap().unwrap();
    assert_eq!(rx.recv_timeout(SOON).unwrap().unwrap(), b"ABCdefghij");
    assert_eq!(m.pending_writes(), 0);
}

// --- Attributes, free space and renames while written -------------------------------------

#[test]
fn file_times_are_the_sources_and_a_write_shows_its_staged_copy() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let a = f.files.path().join("docs/a.txt");
    let disk = fs::metadata(&a).unwrap().modified().unwrap();
    let attr = m.stat(&mp("docs/a.txt")).unwrap();
    assert_eq!(attr.modified, Some(disk));
    assert!(!attr.readonly);
    let listed = m.list(&mp("docs")).unwrap();
    let e = listed.iter().find(|e| e.name == "a.txt").unwrap();
    assert_eq!(e.attr.modified, Some(disk), "listings carry the same time");
    let root = m.stat(&MountPath::root()).unwrap();
    assert_eq!(
        root.modified,
        Some(fs::metadata(f.files.path()).unwrap().modified().unwrap()),
        "the root has the shown folder's time"
    );

    // An edit: the OS's view of the file (FUSE lookups and getattr) is the staged copy,
    // its length and its last write, while listings and stat show the published file.
    let (w, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    assert_eq!(
        m.attr(&mp("docs/a.txt")).unwrap().1,
        attr,
        "nothing written yet"
    );
    m.write(w, 10, b"+more").unwrap();
    let (path, staged) = m.attr(&mp("docs/a.txt")).unwrap();
    assert_eq!(path, mp("docs/a.txt"));
    assert_eq!(staged.size, 15);
    assert_eq!(m.handle_attr(w).unwrap(), staged);
    assert_eq!(
        m.handle_attr(w).unwrap().modified,
        staged.modified,
        "the staged time is its last write, not the time of asking"
    );
    assert_eq!(m.stat(&mp("docs/a.txt")).unwrap().size, 10);
    m.release(w).unwrap();
    let after = m.stat(&mp("docs/a.txt")).unwrap();
    assert_eq!(after.size, 15);
    assert_eq!(
        after.modified,
        Some(fs::metadata(&a).unwrap().modified().unwrap())
    );

    // A file being created: busy to look up by name, but the OS sees its staged size.
    let (c, _) = m.create(&mp("docs/new.txt")).unwrap();
    m.write(c, 0, b"abc").unwrap();
    assert_eq!(
        m.stat(&mp("docs/new.txt")).unwrap_err().kind(),
        io::ErrorKind::ResourceBusy
    );
    assert_eq!(m.attr(&mp("docs/new.txt")).unwrap().1.size, 3);
    m.release(c).unwrap();
    assert_eq!(m.attr(&mp("docs/new.txt")).unwrap().1.size, 3);
}

/// A source on an in-memory server (`sftp://box/srv`).
fn memory_source(f: &Fixture) -> (SourceId, Arc<keel_vfs::memory::MemoryProvider>) {
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.put("/srv/docs/a.txt", "0123456789");
    f.router.register_remote_provider("box".into(), mem.clone());
    let id = add(
        f,
        VPath::parse("sftp://box/srv").unwrap(),
        SourceKind::Share,
    );
    (id, mem)
}

#[test]
fn a_read_only_source_shows_read_only_files_and_refuses_changes() {
    let f = fixture();
    let (id, mem) = memory_source(&f);
    let m = mount_fs(&f, &id, "", false);
    assert!(!m.stat(&mp("docs/a.txt")).unwrap().readonly);
    mem.read_only
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let a = m.stat(&mp("docs/a.txt")).unwrap();
    assert!(a.readonly);
    assert_eq!(
        a.modified,
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000))
    );
    assert!(m.list(&mp("docs")).unwrap()[0].attr.readonly);
    assert!(m.stat(&mp("docs")).unwrap().readonly);
    assert!(m.stat(&MountPath::root()).unwrap().readonly);
    let rofs = io::ErrorKind::ReadOnlyFilesystem;
    assert_eq!(
        m.open(&mp("docs/a.txt"), true, false).unwrap_err().kind(),
        rofs
    );
    assert_eq!(m.create(&mp("docs/b.txt")).unwrap_err().kind(), rofs);
    assert_eq!(m.mkdir(&mp("more")).unwrap_err().kind(), rofs);
    assert_eq!(m.remove_file(&mp("docs/a.txt")).unwrap_err().kind(), rofs);
    assert_eq!(
        m.rename(&mp("docs/a.txt"), &mp("docs/b.txt"), false)
            .unwrap_err()
            .kind(),
        rofs
    );
    // Reading still works.
    let (h, _) = m.open(&mp("docs/a.txt"), false, false).unwrap();
    let mut buf = [0; 4];
    assert_eq!(m.read(h, 2, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"2345");
    m.release(h).unwrap();
}

#[test]
fn free_space_comes_from_the_provider_and_is_cached() {
    let f = fixture();
    let (id, mem) = memory_source(&f);
    let m = mount_fs(&f, &id, "", false);
    let space = keel_vfs::Space {
        free: 3 << 30,
        total: 8 << 30,
    };
    *mem.space.lock() = Some(space);
    assert_eq!(m.space(), Some(space));
    *mem.space.lock() = None;
    assert_eq!(
        m.space(),
        Some(space),
        "asked again only after a few seconds"
    );
    // Unknown: None (the backends show 0 / 0, or a placeholder on Windows).
    let other = mount_fs(&f, &id, "docs", false);
    assert_eq!(other.space(), None);
    // A local source: the volume of its folder.
    let id = indexed(&f);
    let local = mount_fs(&f, &id, "", false).space().unwrap();
    assert!(local.total > 0 && local.free <= local.total, "{local:?}");
}

fn partials(dir: &Path) -> Vec<String> {
    on_disk(dir)
        .into_iter()
        .filter(|n| keel_vfs::ops::is_partial(n) && n != "b.bin.keel-partial-9-9")
        .collect()
}

#[test]
fn a_file_being_created_can_be_renamed_and_lands_under_the_new_name() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let docs = f.files.path().join("docs");
    let (h, _) = m.create(&mp("docs/draft.txt")).unwrap();
    m.write(h, 0, b"first ").unwrap();
    m.rename(&mp("docs/draft.txt"), &mp("docs/final.txt"), false)
        .unwrap();
    assert_eq!(m.handle_path(h).unwrap(), mp("docs/final.txt"));
    assert_eq!(m.attr(&mp("docs/final.txt")).unwrap().1.size, 6);
    assert!(
        m.attr(&mp("docs/draft.txt")).is_err(),
        "the old name is gone"
    );
    m.write(h, 6, b"second").unwrap();
    assert_eq!(m.pending_writes(), 1);
    assert_eq!(partials(&docs).len(), 1, "the spool is beside the new name");
    m.release(h).unwrap();
    assert_eq!(fs::read(docs.join("final.txt")).unwrap(), b"first second");
    assert!(!docs.join("draft.txt").exists());
    assert!(partials(&docs).is_empty());
    assert_eq!(m.pending_writes(), 0);
    // The renamed write left the table: the name can be written again.
    let (h, _) = m.open(&mp("docs/final.txt"), true, true).unwrap();
    m.write(h, 0, b"again").unwrap();
    m.release(h).unwrap();
    assert_eq!(fs::read(docs.join("final.txt")).unwrap(), b"again");
}

#[test]
fn an_edit_renamed_across_folders_publishes_at_the_new_place() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    m.mkdir(&mp("other")).unwrap();
    let (w, _) = m.open(&mp("docs/a.txt"), true, false).unwrap();
    m.write(w, 0, b"AB").unwrap();
    m.rename(&mp("docs/a.txt"), &mp("other/moved.txt"), false)
        .unwrap();
    let other = f.files.path().join("other");
    // The published file moves at once; the write lands on it when closed.
    assert_eq!(fs::read(other.join("moved.txt")).unwrap(), b"0123456789");
    assert!(!f.files.path().join("docs/a.txt").exists());
    assert_eq!(m.stat(&mp("other/moved.txt")).unwrap().size, 10);
    assert_eq!(
        m.open(&mp("other/moved.txt"), false, false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ResourceBusy,
        "still being written"
    );
    m.release(w).unwrap();
    assert_eq!(fs::read(other.join("moved.txt")).unwrap(), b"AB23456789");
    assert!(partials(&other).is_empty());
    assert!(partials(&f.files.path().join("docs")).is_empty());
}

#[test]
fn a_folder_holding_a_write_can_be_renamed() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let (h, _) = m.create(&mp("docs/new.txt")).unwrap();
    m.write(h, 0, b"inside").unwrap();
    match m.rename(&mp("docs"), &mp("papers"), false) {
        Ok(()) => {
            assert_eq!(m.handle_path(h).unwrap(), mp("papers/new.txt"));
            m.write(h, 6, b"!").unwrap();
            m.release(h).unwrap();
            let papers = f.files.path().join("papers");
            assert_eq!(fs::read(papers.join("new.txt")).unwrap(), b"inside!");
            assert_eq!(fs::read(papers.join("a.txt")).unwrap(), b"0123456789");
            assert!(partials(&papers).is_empty());
        }
        // Windows refuses to rename a folder holding an open file (the staging file).
        Err(e) => {
            if !cfg!(windows) {
                panic!("{e}");
            }
            m.release(h).unwrap();
            assert_eq!(
                fs::read(f.files.path().join("docs/new.txt")).unwrap(),
                b"inside"
            );
        }
    }
}

#[test]
fn renames_never_replace_a_write_in_progress() {
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let docs = f.files.path().join("docs");
    let (w, _) = m.open(&mp("docs/b.bin"), true, false).unwrap();
    m.write(w, 0, b"B").unwrap();
    assert_eq!(
        m.rename(&mp("docs/a.txt"), &mp("docs/b.bin"), true)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ResourceBusy
    );
    m.release(w).unwrap();
    // A file being created over an existing name: refused unless replacing, and then the
    // existing file is replaced when the new one is published.
    let (c, _) = m.create(&mp("docs/c.txt")).unwrap();
    m.write(c, 0, b"new").unwrap();
    assert_eq!(
        m.rename(&mp("docs/c.txt"), &mp("docs/a.txt"), false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    m.rename(&mp("docs/c.txt"), &mp("docs/a.txt"), true)
        .unwrap();
    assert_eq!(fs::read(docs.join("a.txt")).unwrap(), b"0123456789");
    m.release(c).unwrap();
    assert_eq!(fs::read(docs.join("a.txt")).unwrap(), b"new");
    assert!(!docs.join("c.txt").exists());
    assert!(partials(&docs).is_empty());
}

#[test]
fn a_remote_write_renamed_while_written_uploads_under_the_new_name() {
    let f = fixture();
    let (id, mem) = memory_source(&f);
    let m = mount_fs(&f, &id, "", false);
    let (h, _) = m.create(&mp("docs/up.txt")).unwrap();
    m.write(h, 0, b"remote").unwrap();
    m.rename(&mp("docs/up.txt"), &mp("up2.txt"), false).unwrap();
    m.release(h).unwrap();
    let got = |p: &str| {
        let mut out = Vec::new();
        use keel_vfs::Provider;
        mem.read(&VPath::parse(p).unwrap())
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    };
    assert_eq!(got("sftp://box/srv/up2.txt"), b"remote");
    assert!(m.stat(&mp("docs/up.txt")).is_err());
    assert_eq!(fs::read_dir(f.spool.path()).unwrap().count(), 0);
}

#[test]
fn a_flush_before_any_write_publishes_nothing() {
    // `dd of=`, a shell's `>`: open, dup2, close the first descriptor (a flush), then write.
    let f = fixture();
    let id = indexed(&f);
    let m = mount_fs(&f, &id, "", false);
    let docs = f.files.path().join("docs");
    let (h, _) = m.create(&mp("docs/out.txt")).unwrap();
    m.flush(h).unwrap();
    assert!(!docs.join("out.txt").exists(), "no empty file published");
    m.write(h, 0, b"data").unwrap();
    assert!(!docs.join("out.txt").exists());
    m.flush(h).unwrap();
    assert_eq!(fs::read(docs.join("out.txt")).unwrap(), b"data");
    m.release(h).unwrap();
    // A truncating open keeps the old content until something is written or it closes.
    let (t, _) = m.open(&mp("docs/a.txt"), true, true).unwrap();
    m.flush(t).unwrap();
    assert_eq!(fs::read(docs.join("a.txt")).unwrap(), b"0123456789");
    m.write(t, 0, b"new").unwrap();
    m.flush(t).unwrap();
    m.release(t).unwrap();
    assert_eq!(fs::read(docs.join("a.txt")).unwrap(), b"new");
    // Created and closed without a write (`touch`): published on release.
    let (e, _) = m.create(&mp("docs/empty.txt")).unwrap();
    m.flush(e).unwrap();
    m.release(e).unwrap();
    assert_eq!(fs::read(docs.join("empty.txt")).unwrap(), b"");
    assert!(partials(&docs).is_empty());
}
