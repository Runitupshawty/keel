use super::*;
use crate::library::tests::folder;
use crate::{Library, SourceDef, SourceKind};
use keel_vfs::{Caps, Entry, Kind};
use parking_lot::Mutex;
use std::{path::Path, sync::atomic::AtomicUsize};

type Listing = dyn Fn(&str) -> Result<Vec<(String, bool, u64)>> + Send + Sync;

/// A provider serving `fake://<any>/...` from a closure: `(name, is_dir, size)` per folder.
/// `remove` only records the path.
pub(crate) struct Fake(pub Box<Listing>, pub Mutex<Vec<String>>);

pub(crate) fn fake(
    list: impl Fn(&str) -> Result<Vec<(String, bool, u64)>> + Send + Sync + 'static,
) -> Fake {
    Fake(Box::new(list), Mutex::new(Vec::new()))
}

impl Provider for Fake {
    fn scheme(&self) -> &'static str {
        "fake"
    }
    fn caps(&self) -> Caps {
        Caps::default()
    }
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        Ok((self.0)(&dir.path)?
            .into_iter()
            .map(|(name, is_dir, size)| Entry {
                path: dir.join(&name),
                kind: if is_dir { Kind::Dir } else { Kind::File },
                size,
                modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                hidden: false,
                is_link: false,
                encrypted: false,
                ext: String::new(),
                name,
            })
            .collect())
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        if (self.0)(&p.path).is_ok() {
            return Ok(Entry {
                path: p.clone(),
                name: p.name().into(),
                kind: Kind::Dir,
                size: 0,
                modified: None,
                hidden: false,
                is_link: false,
                encrypted: false,
                ext: String::new(),
            });
        }
        let parent = p.parent().context("no parent")?;
        self.list(&parent)?
            .into_iter()
            .find(|e| e.name == p.name())
            .with_context(|| format!("no such entry {}", p.display()))
    }
    fn read(&self, _: &VPath) -> Result<Box<dyn std::io::Read + Send>> {
        anyhow::bail!("fake")
    }
    fn write(&self, _: &VPath) -> Result<Box<dyn std::io::Write + Send>> {
        anyhow::bail!("fake")
    }
    fn mkdir(&self, _: &VPath) -> Result<()> {
        anyhow::bail!("fake")
    }
    fn rename(&self, _: &VPath, _: &VPath) -> Result<()> {
        anyhow::bail!("fake")
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.1.lock().push(p.path.clone());
        Ok(())
    }
    fn local_copy(&self, _: &VPath) -> Result<std::path::PathBuf> {
        anyhow::bail!("fake")
    }
}

pub(crate) fn library_with(def: SourceDef) -> (tempfile::TempDir, Library, Arc<Source>) {
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "t").unwrap();
    let id = lib.add_source(def).unwrap();
    let src = lib.source(&id).unwrap();
    (data, lib, src)
}

pub(crate) fn walk(src: &Source, router: &Router) -> Result<()> {
    Indexer::full_walk(src, router, &|_| {}, &AtomicBool::new(false))
}

/// `(id, path, flags)` by path.
pub(crate) fn records(src: &Source) -> Vec<(i64, String, i64)> {
    let c = src.store.get().unwrap();
    let mut stmt = c
        .prepare("SELECT id, path, flags FROM record ORDER BY path")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn paths(src: &Source) -> Vec<String> {
    records(src).into_iter().map(|r| r.1).collect()
}

fn ids(src: &Source) -> Vec<i64> {
    records(src).into_iter().map(|r| r.0).collect()
}

pub(crate) fn fts(src: &Source, q: &str) -> i64 {
    src.store
        .get()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM record_fts WHERE record_fts MATCH ?1",
            [q],
            |r| r.get(0),
        )
        .unwrap()
}

pub(crate) fn write(path: &Path, data: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

fn hidden_file(path: &Path) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::attributes(&mut options, 0x2);
    options.open(path).unwrap();
}

#[test]
fn walk_filters_and_advances_generations() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a.txt"), "a");
    write(&root.join("docs/b.md"), "bb");
    write(&root.join("docs/deep/c.txt"), "ccc");
    write(&root.join("build/out.o"), "o");
    write(&root.join("x.log"), "log");
    hidden_file(&root.join(".hidden"));
    let mut def = folder("files", root);
    def.ignore = vec!["*.log".into(), "build/".into()];
    let (_data, _lib, src) = library_with(def);
    let router = Router::new();
    let seen = AtomicUsize::new(0);
    Indexer::full_walk(
        &src,
        &router,
        &|p| {
            seen.fetch_add(1, Ordering::SeqCst);
            assert!(p.done > 0);
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(seen.load(Ordering::SeqCst) > 0);
    let expected = [
        "",
        "a.txt",
        "docs",
        "docs/b.md",
        "docs/deep",
        "docs/deep/c.txt",
    ];
    assert_eq!(paths(&src), expected);
    assert_eq!(src.generation.load(Ordering::SeqCst), 1);
    assert!(matches!(
        *src.status.read(),
        SourceStatus::Online {
            indexed_at: Some(_)
        }
    ));
    let size: i64 = src
        .store
        .get()
        .unwrap()
        .query_row(
            "SELECT size FROM record WHERE path = 'docs/deep/c.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(size, 3);
    assert_eq!(fts(&src, "deep"), 2);

    let before = records(&src);
    std::fs::remove_file(root.join("docs/deep/c.txt")).unwrap();
    walk(&src, &router).unwrap();
    assert_eq!(src.generation.load(Ordering::SeqCst), 2);
    assert_eq!(records(&src), before[..5], "same ids, removed file gone");
    assert_eq!(fts(&src, "deep"), 1);

    // Hidden entries are indexed when asked for.
    let mut def = folder("all", root);
    def.include_hidden = true;
    let (_data, _lib, all) = library_with(def);
    walk(&all, &router).unwrap();
    let hidden: Vec<_> = records(&all)
        .into_iter()
        .filter(|r| r.1 == ".hidden")
        .collect();
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0].2 & HIDDEN, HIDDEN);
}

#[test]
fn renaming_a_10k_directory_reinserts_nothing() {
    let files = tempfile::tempdir().unwrap();
    let big = files.path().join("big");
    std::fs::create_dir_all(&big).unwrap();
    for i in 0..10_000 {
        std::fs::write(big.join(format!("f{i:05}.txt")), b"").unwrap();
    }
    std::fs::create_dir(files.path().join("moved")).unwrap();
    let (_data, _lib, src) = library_with(folder("files", files.path()));
    let router = Router::new();
    walk(&src, &router).unwrap();
    let totals = |src: &Source| -> (i64, i64, i64) {
        src.store
            .get()
            .unwrap()
            .query_row("SELECT count(*), max(id), sum(id) FROM record", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
    };
    let before = totals(&src);
    assert_eq!(before.0, 10_003);
    std::fs::rename(&big, files.path().join("moved").join("inner")).unwrap();
    walk(&src, &router).unwrap();
    assert_eq!(totals(&src), before, "same records, no inserts");
    let path: String = src
        .store
        .get()
        .unwrap()
        .query_row(
            "SELECT path FROM record WHERE name = 'f04242.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(path, "moved/inner/f04242.txt");
    assert_eq!(fts(&src, "inner"), 10_001, "dir + files re-indexed in FTS");
    assert_eq!(fts(&src, "big"), 0);
}

pub(crate) struct State {
    pub fail: Option<&'static str>,
    pub lists_left: Option<usize>,
}

pub(crate) fn fake_tree(state: Arc<Mutex<State>>) -> Fake {
    fake(move |path: &str| {
        let mut s = state.lock();
        if let Some(left) = s.lists_left.as_mut() {
            anyhow::ensure!(*left > 0, "host unreachable");
            *left -= 1;
        }
        anyhow::ensure!(s.fail != Some(path), "permission denied");
        let d = |n: &str| (n.to_owned(), true, 0);
        let f = |n: &str| (n.to_owned(), false, 7);
        Ok(match path {
            "/" => vec![d("a"), d("b"), f("f.txt")],
            "/a" => vec![f("x")],
            "/b" => vec![f("y")],
            _ => anyhow::bail!("no such folder {path}"),
        })
    })
}

pub(crate) fn fake_source(
    router: &Router,
    state: &Arc<Mutex<State>>,
    kind: SourceKind,
) -> SourceDef {
    router.register(Arc::new(fake_tree(state.clone())));
    SourceDef {
        label: "box".into(),
        root: VPath::parse("fake://box/").unwrap(),
        kind,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
    }
}

/// Tags `rel`'s record with tag 7; returns its id.
fn tagged(src: &Source, rel: &str) -> i64 {
    let id = id_of(src, rel).unwrap();
    src.store
        .get()
        .unwrap()
        .execute("INSERT INTO record_tag(record, tag) VALUES (?1, 7)", [id])
        .unwrap();
    id
}

fn tag_holder(src: &Source) -> Vec<String> {
    let c = src.store.get().unwrap();
    let mut stmt = c
        .prepare("SELECT r.path FROM record_tag t JOIN record r ON r.id = t.record")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn a_new_file_never_takes_over_a_renamed_files_record() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path();
    write(&root.join("a.txt"), "a");
    write(&root.join("s.txt"), "s");
    let (_data, lib, src) = library_with(folder("f", root));
    walk(&src, &lib.router()).unwrap();
    let a = tagged(&src, "a.txt");
    let s = tagged(&src, "s.txt");

    // Rename a -> b, then a new a.txt appears: seen by a full walk.
    std::fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
    write(&root.join("a.txt"), "new");
    walk(&src, &lib.router()).unwrap();
    assert_eq!(
        id_of(&src, "b.txt"),
        Some(a),
        "the renamed file keeps its record"
    );
    assert_ne!(id_of(&src, "a.txt"), Some(a));
    // Rename b -> c, then a new b.txt, applied in the unlucky order (new name first).
    std::fs::rename(root.join("b.txt"), root.join("c.txt")).unwrap();
    write(&root.join("b.txt"), "newer");
    for p in ["b.txt", "c.txt"] {
        Indexer::apply_change(&src, ChangeEvent::Changed(VPath::local(root.join(p)))).unwrap();
    }
    assert_eq!(id_of(&src, "c.txt"), Some(a));
    assert_ne!(id_of(&src, "b.txt"), Some(a));
    // A swap through a temporary name, applied without the temporary.
    std::fs::rename(root.join("c.txt"), root.join("tmp")).unwrap();
    std::fs::rename(root.join("s.txt"), root.join("c.txt")).unwrap();
    std::fs::rename(root.join("tmp"), root.join("s.txt")).unwrap();
    for p in ["c.txt", "s.txt"] {
        Indexer::apply_change(&src, ChangeEvent::Changed(VPath::local(root.join(p)))).unwrap();
    }
    assert_eq!(
        (id_of(&src, "s.txt"), id_of(&src, "c.txt")),
        (Some(a), Some(s))
    );
    let mut holders = tag_holder(&src);
    holders.sort();
    assert_eq!(holders, ["c.txt", "s.txt"], "tags followed both files");
    // A file replaced on save (new id, same name, old one gone) keeps its record.
    std::fs::remove_file(root.join("s.txt")).unwrap();
    write(&root.join("s.txt"), "saved");
    Indexer::apply_change(&src, ChangeEvent::Changed(VPath::local(root.join("s.txt")))).unwrap();
    assert_eq!(id_of(&src, "s.txt"), Some(a));
    walk(&src, &lib.router()).unwrap();
    assert_eq!(id_of(&src, "s.txt"), Some(a));
    assert_eq!(records(&src).len(), 5);
}

#[test]
fn a_different_folder_at_the_root_reads_as_offline() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("drive");
    write(&root.join("a.txt"), "a");
    write(&root.join("sub/b.txt"), "b");
    let (_data, lib, src) = library_with(folder("d", &root));
    walk(&src, &lib.router()).unwrap();
    let before = records(&src);
    assert_eq!(before.len(), 4);
    // Unplugged and something else mounted at the same place.
    std::fs::rename(&root, files.path().join("unplugged")).unwrap();
    write(&root.join("other.txt"), "o");
    let err = walk(&src, &lib.router()).unwrap_err();
    assert!(err.is::<Offline>(), "{err:#}");
    assert!(matches!(*src.status.read(), SourceStatus::Offline { .. }));
    assert_eq!(
        paths(&src),
        ["", "a.txt", "sub", "sub/b.txt"],
        "snapshot kept"
    );
    // Taken as the new root on request.
    Indexer::adopt_root(&src).unwrap();
    walk(&src, &lib.router()).unwrap();
    assert_eq!(paths(&src), ["", "other.txt"]);
    walk(&src, &lib.router()).unwrap();
}

/// Serves `fake://cap/`: a root with folder `big` whose complete listing fails (as a cloud
/// folder past the listing cap does).
struct Capped;
impl Provider for Capped {
    fn scheme(&self) -> &'static str {
        "fake"
    }
    fn caps(&self) -> Caps {
        Caps::default()
    }
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        let entry = |name: &str, kind| Entry {
            path: dir.join(name),
            name: name.into(),
            kind,
            size: 1,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: String::new(),
        };
        Ok(match dir.path.as_str() {
            "/" => vec![entry("big", Kind::Dir), entry("f.txt", Kind::File)],
            _ => (0..3)
                .map(|i| entry(&format!("{i}.txt"), Kind::File))
                .collect(),
        })
    }
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        anyhow::ensure!(
            dir.path != "/big" || !CAPPED.load(Ordering::SeqCst),
            "listing incomplete"
        );
        self.list(dir)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        Ok(Entry {
            path: p.clone(),
            name: p.name().into(),
            kind: Kind::Dir,
            size: 0,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: String::new(),
        })
    }
    fn read(&self, _: &VPath) -> Result<Box<dyn std::io::Read + Send>> {
        anyhow::bail!("fake")
    }
    fn write(&self, _: &VPath) -> Result<Box<dyn std::io::Write + Send>> {
        anyhow::bail!("fake")
    }
    fn mkdir(&self, _: &VPath) -> Result<()> {
        anyhow::bail!("fake")
    }
    fn rename(&self, _: &VPath, _: &VPath) -> Result<()> {
        anyhow::bail!("fake")
    }
    fn remove(&self, _: &VPath) -> Result<()> {
        anyhow::bail!("fake")
    }
    fn local_copy(&self, _: &VPath) -> Result<std::path::PathBuf> {
        anyhow::bail!("fake")
    }
}
static CAPPED: AtomicBool = AtomicBool::new(false);

#[test]
fn a_cut_off_listing_keeps_the_folder_as_unreadable() {
    let router = Router::new();
    router.register(Arc::new(Capped));
    let (_data, _lib, src) = library_with(SourceDef {
        label: "cap".into(),
        root: VPath::parse("fake://cap/").unwrap(),
        kind: SourceKind::Cloud,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
    });
    walk(&src, &router).unwrap();
    assert_eq!(paths(&src).len(), 6);
    CAPPED.store(true, Ordering::SeqCst);
    walk(&src, &router).unwrap();
    let all = records(&src);
    assert_eq!(all.len(), 6, "nothing under the cut-off folder was dropped");
    let big = all.iter().find(|r| r.1 == "big").unwrap();
    assert_eq!(big.2 & UNREADABLE, UNREADABLE);
    CAPPED.store(false, Ordering::SeqCst);
}

#[test]
fn folders_are_never_listed_while_holding_the_write_lock() {
    let db: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
    let locked: Arc<Mutex<Vec<String>>> = Arc::default();
    let router = Router::new();
    let (db2, locked2) = (db.clone(), locked.clone());
    router.register(Arc::new(fake(move |path: &str| {
        if let Some(db) = db2.lock().as_ref() {
            let c = rusqlite::Connection::open(db).unwrap();
            c.busy_timeout(Duration::ZERO).unwrap();
            if c.execute_batch("BEGIN IMMEDIATE; ROLLBACK").is_err() {
                locked2.lock().push(path.to_owned());
            }
        }
        Ok(match path {
            "/" => (0..3).map(|d| (format!("d{d}"), true, 0)).collect(),
            _ => (0..50).map(|i| (format!("f{i}"), false, 1)).collect(),
        })
    })));
    let (_data, _lib, src) = library_with(SourceDef {
        label: "slow".into(),
        root: VPath::parse("fake://slow/").unwrap(),
        kind: SourceKind::Share,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
    });
    *db.lock() = Some(src.store_dir().join("source.db"));
    walk(&src, &router).unwrap();
    assert_eq!(paths(&src).len(), 1 + 3 + 150);
    assert!(
        locked.lock().is_empty(),
        "listed under the lock: {:?}",
        locked.lock()
    );
}

#[test]
fn a_big_new_folder_is_applied_in_batches() {
    let files = tempfile::tempdir().unwrap();
    let (_data, lib, src) = library_with(folder("w", files.path()));
    walk(&src, &lib.router()).unwrap();
    let big = files.path().join("big");
    std::fs::create_dir_all(big.join("sub")).unwrap();
    for i in 0..BATCH + 500 {
        std::fs::write(big.join(format!("{i}.txt")), "").unwrap();
    }
    Indexer::apply_change(&src, ChangeEvent::Changed(VPath::local(&big))).unwrap();
    assert_eq!(paths(&src).len() as u64, 1 + 2 + BATCH + 500);
}

#[test]
fn a_walk_leaves_no_trace_on_the_source_however_it_ends() {
    let panic = Arc::new(AtomicBool::new(true));
    let router = Router::new();
    let flag = panic.clone();
    router.register(Arc::new(fake(move |path: &str| {
        assert!(!flag.load(Ordering::SeqCst), "provider bug");
        Ok(match path {
            "/" => vec![("a".into(), false, 1)],
            _ => anyhow::bail!("no such folder {path}"),
        })
    })));
    let (_data, _lib, src) = library_with(SourceDef {
        label: "p".into(),
        root: VPath::parse("fake://p/").unwrap(),
        kind: SourceKind::Share,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
    });
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| walk(&src, &router)));
    assert!(unwound.is_err());
    panic.store(false, Ordering::SeqCst);
    walk(&src, &router).unwrap(); // not "already being indexed"
    let cache: i64 = src
        .store
        .get()
        .unwrap()
        .query_row("PRAGMA cache_size", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        cache, -16384,
        "the pooled connection's cache is back to normal"
    );
}

#[test]
fn an_emptied_root_reads_as_offline() {
    let empty = Arc::new(AtomicBool::new(false));
    let router = Router::new();
    let flag = empty.clone();
    router.register(Arc::new(fake(move |path: &str| {
        Ok(match path {
            "/" if flag.load(Ordering::SeqCst) => Vec::new(),
            "/" => vec![("a".into(), true, 0), ("f.txt".into(), false, 3)],
            "/a" => vec![("x".into(), false, 1)],
            _ => anyhow::bail!("no such folder {path}"),
        })
    })));
    let (_data, _lib, src) = library_with(SourceDef {
        label: "nas".into(),
        root: VPath::parse("fake://nas/").unwrap(),
        kind: SourceKind::Share,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
    });
    walk(&src, &router).unwrap();
    assert_eq!(paths(&src).len(), 4);
    empty.store(true, Ordering::SeqCst);
    let err = walk(&src, &router).unwrap_err();
    assert!(err.is::<Offline>(), "{err:#}");
    assert_eq!(paths(&src).len(), 4, "snapshot kept");
    Indexer::adopt_root(&src).unwrap();
    walk(&src, &router).unwrap();
    assert_eq!(paths(&src), [""]);
}

#[test]
fn unreadable_offline_and_cancelled_walks_keep_the_snapshot() {
    let state = Arc::new(Mutex::new(State {
        fail: None,
        lists_left: None,
    }));
    let router = Router::new();
    let (_data, _lib, src) = library_with(fake_source(&router, &state, SourceKind::Share));
    walk(&src, &router).unwrap();
    let all = ["", "a", "a/x", "b", "b/y", "f.txt"];
    assert_eq!(paths(&src), all);
    let first = ids(&src);

    // An unreadable folder keeps its error and its last known contents.
    state.lock().fail = Some("/a");
    walk(&src, &router).unwrap();
    assert_eq!(src.generation.load(Ordering::SeqCst), 2);
    assert_eq!(ids(&src), first);
    assert_eq!(records(&src)[1].2 & UNREADABLE, UNREADABLE);
    let error: Option<String> = src
        .store
        .get()
        .unwrap()
        .query_row("SELECT error FROM record WHERE path = 'a'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(error.unwrap().contains("permission denied"));

    // The host drops mid-walk: nothing is removed, the generation stays.
    *state.lock() = State {
        fail: None,
        lists_left: Some(2),
    };
    let err = walk(&src, &router).unwrap_err();
    assert!(err.is::<Offline>(), "{err:#}");
    assert_eq!(src.generation.load(Ordering::SeqCst), 2);
    assert!(matches!(
        *src.status.read(),
        SourceStatus::Offline { last_seen: Some(_) }
    ));
    assert_eq!(paths(&src), all);

    // Cancelled before it starts: also keeps everything.
    state.lock().lists_left = None;
    let err = Indexer::full_walk(&src, &router, &|_| {}, &AtomicBool::new(true)).unwrap_err();
    assert!(err.is::<Cancelled>());
    assert!(matches!(
        *src.status.read(),
        SourceStatus::Online {
            indexed_at: Some(_)
        }
    ));

    // Back online: records the abandoned walks touched are matched again (no duplicates)
    // and the unreadable flag clears.
    walk(&src, &router).unwrap();
    assert_eq!(ids(&src), first);
    assert_eq!(records(&src)[1].2 & UNREADABLE, 0);
    assert!(src.generation.load(Ordering::SeqCst) > 2);
}

pub(crate) fn id_of(src: &Source, rel: &str) -> Option<i64> {
    resolve(&src.store.get().unwrap(), rel, src.nocase())
        .unwrap()
        .map(|r| r.0)
}

#[test]
fn apply_change_creates_moves_and_removes() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("root");
    write(&root.join("d/a.txt"), "a");
    write(&root.join("d/sub/s.txt"), "s");
    let mut def = folder("root", &root);
    def.ignore = vec!["*.tmp".into()];
    let (_data, _lib, src) = library_with(def);
    walk(&src, &Router::new()).unwrap();
    let changed = |p: &Path| {
        Indexer::apply_change(&src, ChangeEvent::Changed(VPath::local(p))).unwrap();
    };

    write(&root.join("d/new.txt"), "n");
    changed(&root.join("d/new.txt"));
    assert!(id_of(&src, "d/new.txt").is_some());

    let a = id_of(&src, "d/a.txt").unwrap();
    std::fs::rename(root.join("d/a.txt"), root.join("d/b.txt")).unwrap();
    changed(&root.join("d/b.txt"));
    changed(&root.join("d/a.txt"));
    assert_eq!(id_of(&src, "d/b.txt"), Some(a), "rename keeps identity");
    assert_eq!(id_of(&src, "d/a.txt"), None);

    // A folder arriving from outside the source is indexed with its contents.
    write(&files.path().join("outside/in/one.txt"), "1");
    write(&files.path().join("outside/in/deeper/two.txt"), "2");
    std::fs::rename(files.path().join("outside/in"), root.join("d/in")).unwrap();
    changed(&root.join("d/in"));
    assert!(id_of(&src, "d/in/deeper/two.txt").is_some());

    // Renaming a folder moves its subtree.
    let s = id_of(&src, "d/sub/s.txt").unwrap();
    std::fs::rename(root.join("d"), root.join("e")).unwrap();
    changed(&root.join("e"));
    changed(&root.join("d"));
    assert_eq!(id_of(&src, "e/sub/s.txt"), Some(s));
    assert_eq!(id_of(&src, "d"), None);
    let path: String = src
        .store
        .get()
        .unwrap()
        .query_row("SELECT path FROM record WHERE id = ?1", [s], |r| r.get(0))
        .unwrap();
    assert_eq!(path, "e/sub/s.txt");

    std::fs::remove_file(root.join("e/new.txt")).unwrap();
    Indexer::apply_change(
        &src,
        ChangeEvent::Removed(VPath::local(root.join("e/new.txt"))),
    )
    .unwrap();
    assert_eq!(id_of(&src, "e/new.txt"), None);

    write(&root.join("e/skip.tmp"), "t");
    changed(&root.join("e/skip.tmp"));
    assert_eq!(id_of(&src, "e/skip.tmp"), None, "ignored");
    assert!(Indexer::apply_change(&src, ChangeEvent::Rescan).is_err());
    let outside = ChangeEvent::Changed(VPath::local(files.path()));
    assert!(Indexer::apply_change(&src, outside).is_err());
}

pub(crate) fn eventually(what: &str, check: impl Fn() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn watch_applies_local_changes() {
    let files = tempfile::tempdir().unwrap();
    write(&files.path().join("old.txt"), "o");
    let (_data, _lib, src) = library_with(folder("w", files.path()));
    let router = Arc::new(Router::new());
    walk(&src, &router).unwrap();
    let old = id_of(&src, "old.txt").unwrap();
    let handle = Indexer::watch(&src, &router).unwrap();
    write(&files.path().join("dir/new.txt"), "n");
    std::fs::rename(
        files.path().join("old.txt"),
        files.path().join("renamed.txt"),
    )
    .unwrap();
    eventually("new file indexed", || id_of(&src, "dir/new.txt").is_some());
    eventually("rename applied", || id_of(&src, "renamed.txt") == Some(old));
    drop(handle);
}

#[test]
fn watch_reconciles_on_start_and_periodically() {
    let files = tempfile::tempdir().unwrap();
    let (_data, _lib, src) = library_with(folder("w", files.path()));
    let router = Arc::new(Router::new());
    walk(&src, &router).unwrap();
    // Changed while nobody watched.
    write(&files.path().join("missed.txt"), "m");
    let cfg = WatchConfig {
        reconcile: Duration::from_millis(300),
        ..WatchConfig::default()
    };
    let handle = Indexer::watch_with(&src, &router, cfg).unwrap();
    eventually("start-up walk", || id_of(&src, "missed.txt").is_some());
    let gen = src.generation.load(Ordering::SeqCst);
    eventually("periodic walks", || {
        src.generation.load(Ordering::SeqCst) >= gen + 2
    });
    drop(handle);
}

#[test]
fn lost_events_trigger_a_full_walk() {
    let files = tempfile::tempdir().unwrap();
    let (_data, _lib, src) = library_with(folder("w", files.path()));
    let router = Router::new();
    let (tx, rx) = crossbeam_channel::unbounded();
    let walks = AtomicUsize::new(0);
    let quit = AtomicBool::new(false);
    let rescan = |s: &Source| {
        walk(s, &router).unwrap();
        walks.fetch_add(1, Ordering::SeqCst);
    };
    /// Stops the loop even when an assertion below fails (else the scope never ends).
    struct Quit<'a>(&'a AtomicBool);
    impl Drop for Quit<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    std::thread::scope(|scope| {
        scope.spawn(|| watch_loop(&src, &rx, Duration::from_secs(3600), &quit, &rescan));
        let _quit = Quit(&quit);
        eventually("start-up walk", || walks.load(Ordering::SeqCst) == 1);
        write(&files.path().join("lost.txt"), "l");
        tx.send(Ok(
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ))
        .unwrap();
        eventually("rescan", || walks.load(Ordering::SeqCst) == 2);
        assert!(id_of(&src, "lost.txt").is_some());
        tx.send(Err(notify::Error::generic("queue overflow")))
            .unwrap();
        eventually("rescan after an error", || {
            walks.load(Ordering::SeqCst) == 3
        });
        // Plain events are applied without a walk.
        write(&files.path().join("seen.txt"), "s");
        tx.send(Ok(
            notify::Event::new(notify::EventKind::Any).add_path(files.path().join("seen.txt"))
        ))
        .unwrap();
        eventually("applied", || id_of(&src, "seen.txt").is_some());
        assert_eq!(walks.load(Ordering::SeqCst), 3);
        quit.store(true, Ordering::SeqCst);
    });
}

#[test]
fn watch_polls_at_the_sources_own_interval() {
    let state = Arc::new(Mutex::new(State {
        fail: None,
        lists_left: None,
    }));
    let router = Arc::new(Router::new());
    let mut def = fake_source(&router, &state, SourceKind::Cloud);
    def.poll_secs = Some(1);
    let (_data, _lib, src) = library_with(def);
    let start = Instant::now();
    let handle = Indexer::watch(&src, &router).unwrap();
    eventually("two polls", || src.generation.load(Ordering::SeqCst) >= 2);
    assert!(start.elapsed() >= Duration::from_secs(1));
    drop(handle);
}

#[test]
fn watch_polls_remote_sources() {
    let state = Arc::new(Mutex::new(State {
        fail: None,
        lists_left: None,
    }));
    let router = Arc::new(Router::new());
    let (_data, _lib, src) = library_with(fake_source(&router, &state, SourceKind::Cloud));
    let cfg = WatchConfig {
        poll: Duration::from_millis(20),
        ..WatchConfig::default()
    };
    let handle = Indexer::watch_with(&src, &router, cfg).unwrap();
    eventually("two polls", || src.generation.load(Ordering::SeqCst) >= 2);
    drop(handle);
    assert_eq!(paths(&src).len(), 6);
}

/// Peak resident memory of this process, where the platform reports it.
fn peak_rss() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: `counters` is a PROCESS_MEMORY_COUNTERS of the size passed.
        unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) }.ok()?;
        Some(counters.PeakWorkingSetSize as u64)
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    None
}

pub(crate) const DIRS: u64 = 2_000;
pub(crate) const FILES: u64 = 1_000;

/// The 2,002,001-entry generated tree (`fake://perf/`, never on disk): DIRS folders of FILES
/// files named `f<i> w<word>.dat`, and an unwalked library source over it.
pub(crate) fn two_million() -> (Router, tempfile::TempDir, Library, Arc<Source>) {
    let router = Router::new();
    router.register(Arc::new(fake(|path: &str| {
        if path == "/" {
            return Ok((0..DIRS).map(|d| (format!("d{d:04}"), true, 0)).collect());
        }
        let d: u64 = path.trim_start_matches("/d").parse()?;
        Ok((0..FILES)
            .map(|i| {
                let n = d * FILES + i;
                let word = n.wrapping_mul(2_654_435_761) % 5_000;
                (format!("f{i:04} w{word}.dat"), false, n % 100_000)
            })
            .collect())
    })));
    let (data, lib, src) = library_with(SourceDef {
        label: "perf".into(),
        root: VPath::parse("fake://perf/").unwrap(),
        kind: SourceKind::Share,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
    });
    (router, data, lib, src)
}

/// Review focus 1 + 4: 2,000,000 generated entries (2,000 folders of 1,000 files, never on
/// disk) index in < 90 s with < 400 MB peak RSS, and FTS queries answer in < 50 ms.
/// `cargo test -p keel-core --release -- --ignored two_million`
#[test]
#[ignore]
fn two_million_entries_index_fast_in_bounded_memory() {
    let (router, _data, _lib, src) = two_million();
    let start = Instant::now();
    walk(&src, &router).unwrap();
    let took = start.elapsed();
    let start = Instant::now();
    walk(&src, &router).unwrap();
    let rewalk = start.elapsed();
    let rss = peak_rss();
    let count: i64 = src
        .store
        .get()
        .unwrap()
        .query_row("SELECT count(*) FROM record", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count as u64, 1 + DIRS + DIRS * FILES);
    let c = src.store.get().unwrap();
    let query = |q: &str| {
        let start = Instant::now();
        let mut stmt = c
            .prepare_cached(
                "SELECT r.id, r.path FROM record_fts JOIN record r ON r.id = record_fts.rowid
                 WHERE record_fts MATCH ?1 LIMIT 100",
            )
            .unwrap();
        let hits = stmt.query_map([q], |r| r.get::<_, i64>(0)).unwrap().count();
        (hits, start.elapsed())
    };
    let phrase = format!(
        "\"f0999 w{}\"",
        (1234 * FILES + 999).wrapping_mul(2_654_435_761) % 5_000
    );
    let queries = ["w4242", "d1234 AND f0999", &phrase, "w42*"];
    let timings: Vec<_> = queries.iter().map(|q| query(q)).collect();
    eprintln!(
        "2M index: {took:?}, unchanged re-walk {rewalk:?}, peak RSS {} MB, queries {timings:?}",
        rss.map_or(-1, |b| (b / 1_048_576) as i64)
    );
    assert!(took < Duration::from_secs(90), "index took {took:?}");
    assert!(rewalk < Duration::from_secs(90), "re-walk took {rewalk:?}");
    if let Some(rss) = rss {
        assert!(rss < 400 * 1_048_576, "peak RSS {} MB", rss / 1_048_576);
    }
    for ((hits, t), q) in timings.iter().zip(queries) {
        assert!(*hits > 0, "{q} found nothing");
        assert!(*t < Duration::from_millis(50), "{q} took {t:?}");
    }
}

#[test]
fn identity_lookups_use_indexes() {
    let (_data, _lib, src) = library_with(folder("q", Path::new("/nowhere")));
    let c = src.store.get().unwrap();
    let plan = |sql: &str| -> String {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
        rows.map(Result::unwrap).collect::<Vec<_>>().join("; ")
    };
    let by_id =
        plan("SELECT id FROM record WHERE fs_id = 'a' AND substr(fs_id, 1, 2) <> 'h:' AND gen < 3");
    assert!(by_id.contains("record_fs_id"), "{by_id}");
    let by_name = plan("SELECT id FROM record WHERE parent IS 1 AND name = 'x' AND gen < 3");
    assert!(by_name.contains("record_parent"), "{by_name}");
}
