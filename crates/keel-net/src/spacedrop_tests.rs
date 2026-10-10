//! Spacedrop and device sources between two in-process nodes (offline, loopback).
use crate::library_tests::pair;
use crate::*;
use keel_core::{Indexer, JobStatus, Library, SourceDef, SourceId, SourceKind};
use keel_vfs::{Caps, Entry, Kind, Progress, Provider, RemoveKind, Router, VPath};
use parking_lot::{Condvar, Mutex};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const BIG: usize = 3 * spacedrop::CHUNK as usize + 12_345;
const BLOCK_AT: usize = 2 * spacedrop::CHUNK as usize;

/// `gate://x/big.bin`: its first reader stops at `BLOCK_AT` until the gate opens.
struct Gate {
    data: Arc<Vec<u8>>,
    first: AtomicBool,
    reached: Arc<AtomicBool>,
    open: Arc<(Mutex<bool>, Condvar)>,
}
struct GateReader {
    data: Arc<Vec<u8>>,
    pos: usize,
    block: Option<usize>,
    reached: Arc<AtomicBool>,
    open: Arc<(Mutex<bool>, Condvar)>,
}
impl Read for GateReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.block == Some(self.pos) {
            self.reached.store(true, Ordering::SeqCst);
            let (lock, cv) = &*self.open;
            let mut open = lock.lock();
            while !*open {
                cv.wait(&mut open);
            }
            self.block = None;
        }
        let end = (self.pos + buf.len())
            .min(self.data.len())
            .min(self.block.unwrap_or(usize::MAX));
        let n = end - self.pos;
        buf[..n].copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(n)
    }
}
impl Gate {
    fn new() -> Arc<Gate> {
        Arc::new(Gate {
            data: Arc::new((0..BIG).map(|i| (i * 7 % 251) as u8).collect()),
            first: AtomicBool::new(true),
            reached: Arc::default(),
            open: Arc::default(),
        })
    }
    fn path() -> VPath {
        VPath::parse("gate://x/big.bin").unwrap()
    }
    fn release(&self) {
        *self.open.0.lock() = true;
        self.open.1.notify_all();
    }
}
impl Provider for Gate {
    fn scheme(&self) -> &'static str {
        "gate"
    }
    fn caps(&self) -> Caps {
        Caps::default()
    }
    fn list(&self, _: &VPath) -> anyhow::Result<Vec<Entry>> {
        anyhow::bail!("not a folder")
    }
    fn list_complete(&self, d: &VPath) -> anyhow::Result<Vec<Entry>> {
        self.list(d)
    }
    fn stat(&self, p: &VPath) -> anyhow::Result<Entry> {
        Ok(Entry {
            path: p.clone(),
            name: "big.bin".into(),
            kind: Kind::File,
            size: BIG as u64,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: "bin".into(),
        })
    }
    fn read(&self, _: &VPath) -> anyhow::Result<Box<dyn Read + Send>> {
        let first = self.first.swap(false, Ordering::SeqCst);
        Ok(Box::new(GateReader {
            data: self.data.clone(),
            pos: 0,
            block: first.then_some(BLOCK_AT),
            reached: self.reached.clone(),
            open: self.open.clone(),
        }))
    }
    fn write(&self, _: &VPath) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
        anyhow::bail!("read-only")
    }
    fn mkdir(&self, _: &VPath) -> anyhow::Result<()> {
        anyhow::bail!("read-only")
    }
    fn rename(&self, _: &VPath, _: &VPath) -> anyhow::Result<()> {
        anyhow::bail!("read-only")
    }
    fn remove(&self, _: &VPath) -> anyhow::Result<()> {
        anyhow::bail!("read-only")
    }
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }
    fn local_copy(&self, _: &VPath) -> anyhow::Result<PathBuf> {
        anyhow::bail!("no")
    }
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        _: &dyn Fn(Progress),
        _: &AtomicBool,
    ) -> anyhow::Result<PathBuf> {
        self.local_copy(p)
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn staged(inbox: &Path) -> Option<PathBuf> {
    std::fs::read_dir(inbox)
        .ok()?
        .flatten()
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".keel-partial-")
        })
        .map(|e| e.path())
}

/// Watches the inbox until stopped: anything visible outside the staging folder must
/// be complete (its expected size).
fn watch_inbox(
    inbox: PathBuf,
    sizes: Vec<(&'static str, u64)>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut bad = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            for e in std::fs::read_dir(&inbox).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(".keel-partial-") {
                    continue;
                }
                let size = e.metadata().map_or(u64::MAX, |m| m.len());
                match sizes.iter().find(|(n, _)| *n == name) {
                    Some((_, want)) if *want == size => {}
                    _ => bad.push(format!("{name}: {size} bytes")),
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        bad
    })
}

#[test]
fn spacedrop_resumes_after_the_sender_restarts_and_never_shows_partials() {
    let mut pair = pair();
    let gate = Gate::new();
    let router = Arc::new(Router::new());
    router.register(gate.clone());
    pair.lib.set_router(router);
    spacedrop::register(&pair.lib);
    let small = pair.files.path().join("note.txt");
    std::fs::write(&small, b"small note").unwrap();
    let inbox = tempfile::tempdir().unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    pair.handler
        .on_drop(inbox.path().to_owned(), move |offer: IncomingDrop| {
            seen.lock().push((offer.label.clone(), offer.files.len()));
            offer.reply.answer(true);
        });
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = watch_inbox(
        inbox.path().to_owned(),
        vec![("big.bin", BIG as u64), ("note.txt", 10)],
        stop.clone(),
    );
    let host = PeerId(pair.host.id());
    let job = spacedrop::send(
        &pair.guest,
        &pair.lib,
        host,
        vec![Gate::path(), VPath::local(&small)],
    )
    .unwrap();

    // Two pieces arrived; the sender is reading the third when its node goes away.
    wait_for("the gate", || gate.reached.load(Ordering::SeqCst));
    let part = staged(inbox.path()).expect("staging folder").join("0");
    assert_eq!(std::fs::metadata(&part).unwrap().len(), BLOCK_AT as u64);
    pair.rt.block_on(pair.guest.close());
    gate.release();
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        pair.lib.jobs().info(job).unwrap().status,
        JobStatus::Running,
        "a dropped link is retried, not failed"
    );
    pair.reopen_guest();

    let info = pair.lib.jobs().wait(job).unwrap();
    stop.store(true, Ordering::SeqCst);
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(
        info.log.contains(&format!("resuming at {BLOCK_AT} bytes")),
        "{}",
        info.log
    );
    let got = std::fs::read(inbox.path().join("big.bin")).unwrap();
    assert_eq!(blake3::hash(&got), blake3::hash(&gate.data));
    assert_eq!(
        std::fs::read(inbox.path().join("note.txt")).unwrap(),
        b"small note"
    );
    assert!(staged(inbox.path()).is_none(), "staging removed when done");
    assert_eq!(watcher.join().unwrap(), Vec::<String>::new());
    assert_eq!(
        asked.lock().len(),
        1,
        "one prompt per drop, re-offers resume"
    );
    pair.close();
}

#[test]
fn spacedrop_declined_ungranted_and_cancelled_leave_nothing() {
    let pair = pair();
    let host = PeerId(pair.host.id());
    let file = pair.files.path().join("a.txt");
    std::fs::write(&file, b"a").unwrap();
    let inbox = tempfile::tempdir().unwrap();

    // Declined (and, before that, no drop policy at all: declined too).
    let job = spacedrop::send(&pair.guest, &pair.lib, host, vec![VPath::local(&file)]).unwrap();
    assert_eq!(pair.lib.jobs().wait(job).unwrap().status, JobStatus::Failed);
    pair.handler
        .on_drop(inbox.path().to_owned(), |d: IncomingDrop| {
            d.reply.answer(false)
        });
    let job = spacedrop::send(&pair.guest, &pair.lib, host, vec![VPath::local(&file)]).unwrap();
    let info = pair.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Failed);
    assert!(info.log.contains("declined"), "{}", info.log);
    assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 0);

    // Pieces for a drop that was never accepted are refused.
    let id = "0123456789abcdef0123456789abcdef";
    let raw = |req: Request| pair.rt.block_on(pair.guest.request(&host, req)).unwrap();
    assert!(matches!(
        raw(Request::StatPartial {
            source: format!("drop:{id}"),
            path: "a.txt".into()
        }),
        Response::Denied(_)
    ));
    let piece = pair.rt.block_on(pair.guest.write_stream(
        &host,
        &format!("drop:{id}"),
        "a.txt",
        Box::new(std::io::Cursor::new(b"a".to_vec())),
        WriteAt {
            offset: 0,
            size: 1,
            final_: true,
            expect: Some(*blake3::hash(b"a").as_bytes()),
        },
    ));
    assert!(matches!(piece.unwrap(), Response::Denied(_)));
    // Nor can drop pieces reach a library source.
    assert!(matches!(
        raw(Request::StatPartial {
            source: pair.source.clone(),
            path: "a.txt".into()
        }),
        Response::Denied(_)
    ));
    assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 0);

    // Cancelled mid-transfer: the receiver drops its staging.
    pair.handler
        .on_drop(inbox.path().to_owned(), |d: IncomingDrop| {
            d.reply.answer(true)
        });
    let gate = Gate::new();
    let router = Arc::new(Router::new());
    router.register(gate.clone());
    pair.lib.set_router(router);
    let job = spacedrop::send(&pair.guest, &pair.lib, host, vec![Gate::path()]).unwrap();
    wait_for("the gate", || gate.reached.load(Ordering::SeqCst));
    assert!(staged(inbox.path()).is_some());
    pair.lib.jobs().cancel(job).unwrap();
    gate.release();
    assert_eq!(
        pair.lib.jobs().wait(job).unwrap().status,
        JobStatus::Cancelled
    );
    wait_for("staging removed", || staged(inbox.path()).is_none());
    assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 0);
    pair.close();
}

#[test]
fn device_source_rewalk_picks_up_a_changed_listing_with_content_ids() {
    let pair = pair();
    let root = pair.files.path();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("sub/b.txt"), b"beta").unwrap();
    // The host indexes and hashes its source, so its listings carry content ids.
    pair.lib.set_pause_on_battery(false);
    let host_source = SourceId(pair.source.clone());
    let job = pair.lib.index(&host_source).unwrap();
    pair.lib.jobs().wait(job).unwrap();
    let job = pair.lib.hash().unwrap();
    pair.lib.jobs().wait(job).unwrap();
    pair.grant("", Access::Read);

    let data = tempfile::tempdir().unwrap();
    let guest = Library::open(data.path(), "guest").unwrap();
    let router = Arc::new(Router::new());
    router.register(Arc::new(pair.provider()));
    guest.set_router(router.clone());
    let id = guest
        .add_source(SourceDef {
            label: "Desk".into(),
            root: pair.path(""),
            kind: SourceKind::Device,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap();
    let src = guest.source(&id).unwrap();
    let walk = || Indexer::full_walk(&src, &router, &|_| {}, &AtomicBool::new(false)).unwrap();
    let names = |rel: &str| -> Vec<String> {
        let mut n: Vec<_> = guest
            .list_children(&id, rel)
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect();
        n.sort();
        n
    };
    // What the host claims is kept apart (`remote_cas`): never a confirmed content id.
    let cas = |name: &str| -> Option<[u8; 32]> {
        let db = rusqlite::Connection::open(src.store_dir().join("source.db")).unwrap();
        let (claim, confirmed): (Option<Vec<u8>>, Option<Vec<u8>>) = db
            .query_row(
                "SELECT remote_cas, cas_id FROM record WHERE path = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(confirmed, None);
        claim.map(|c| c.try_into().unwrap())
    };

    walk();
    let first = src.generation.load(Ordering::SeqCst);
    assert_eq!(names(""), ["a.txt", "sub"]);
    assert_eq!(names("sub"), ["b.txt"]);
    assert_eq!(cas("a.txt"), Some(*blake3::hash(b"alpha").as_bytes()));

    std::thread::sleep(Duration::from_millis(1100)); // a new mtime second
    std::fs::write(root.join("a.txt"), b"alpha, changed").unwrap();
    std::fs::remove_file(root.join("sub/b.txt")).unwrap();
    std::fs::write(root.join("c.txt"), b"gamma").unwrap();
    walk();
    assert!(src.generation.load(Ordering::SeqCst) > first);
    assert_eq!(names(""), ["a.txt", "c.txt", "sub"]);
    assert!(names("sub").is_empty());
    // The host's index is stale for a.txt: no id is sent and the old one is gone.
    assert_eq!(cas("a.txt"), None);
    drop(guest);
    pair.close();
}

/// Serves a real library but claims `claim` as the content id of every file it lists.
struct Liar {
    inner: LibraryHandler,
    claim: [u8; 32],
}
#[async_trait::async_trait]
impl Handler for Liar {
    async fn sources(&self, c: &RequestCtx) -> Vec<SourceInfo> {
        self.inner.sources(c).await
    }
    async fn list(&self, c: &RequestCtx, s: &str, p: &str) -> anyhow::Result<Vec<EntryInfo>> {
        let mut v = self.inner.list(c, s, p).await?;
        for e in v.iter_mut().filter(|e| !e.is_dir) {
            e.content_id = Some(self.claim);
        }
        Ok(v)
    }
    async fn stat(&self, c: &RequestCtx, s: &str, p: &str) -> anyhow::Result<EntryInfo> {
        self.inner.stat(c, s, p).await
    }
    async fn read(
        &self,
        c: &RequestCtx,
        s: &str,
        p: &str,
        r: Option<(u64, u64)>,
    ) -> anyhow::Result<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.inner.read(c, s, p, r).await
    }
    async fn write(
        &self,
        c: &RequestCtx,
        s: &str,
        p: &str,
        b: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> anyhow::Result<()> {
        self.inner.write(c, s, p, b, at).await
    }
    async fn stat_partial(&self, c: &RequestCtx, s: &str, p: &str) -> anyhow::Result<u64> {
        self.inner.stat_partial(c, s, p).await
    }
    async fn mkdir(&self, c: &RequestCtx, s: &str, p: &str) -> anyhow::Result<()> {
        self.inner.mkdir(c, s, p).await
    }
    async fn rename(&self, c: &RequestCtx, s: &str, a: &str, b: &str) -> anyhow::Result<()> {
        self.inner.rename(c, s, a, b).await
    }
    async fn remove(&self, c: &RequestCtx, s: &str, p: &str) -> anyhow::Result<()> {
        self.inner.remove(c, s, p).await
    }
    async fn storage(&self, c: &RequestCtx) -> Option<Storage> {
        self.inner.storage(c).await
    }
}

/// A paired device listing a decoy under a stolen content id: the only real copy still
/// warns LastCopy, the decoy is no duplicate, and it shows only as a claim.
#[test]
fn a_device_claiming_a_content_id_never_counts_as_a_copy() {
    use keel_core::{validate_preview_execute, Op, Warning};
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let precious = b"the only copy of this document".to_vec();
    let only = dirs[0].path().join("precious.txt");
    std::fs::write(&only, &precious).unwrap();
    let victim = Arc::new(Library::open(dirs[1].path(), "victim").unwrap());
    victim.set_pause_on_battery(false);
    let router = Arc::new(Router::new());
    victim.set_router(router.clone());
    let local = victim
        .add_source(crate::library_tests::folder(dirs[0].path()))
        .unwrap();
    let done = |job| {
        let info = victim.jobs().wait(job).unwrap();
        assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    };
    done(victim.index(&local).unwrap());
    done(victim.hash().unwrap());
    let warnings = || {
        validate_preview_execute(
            &victim,
            Op::Delete {
                paths: vec![VPath::local(&only)],
            },
        )
        .unwrap()
        .warnings
    };
    assert!(matches!(warnings()[..], [Warning::LastCopy { .. }]));

    std::fs::write(dirs[2].path().join("decoy.txt"), b"junk").unwrap();
    let attacker = Arc::new(Library::open(dirs[3].path(), "attacker").unwrap());
    let shared = attacker
        .add_source(crate::library_tests::folder(dirs[2].path()))
        .unwrap();
    let claim = *blake3::hash(&precious).as_bytes();
    let (host, guest) = rt.block_on(async {
        let host = Node::open_with_options(
            Arc::new(keel_vfs::cloud::MemoryStore::default()),
            dirs[4].path(),
            Arc::new(Liar {
                inner: LibraryHandler::new(attacker.clone()),
                claim,
            }),
            NodeOptions::offline(),
        )
        .await
        .unwrap();
        let guest = Node::open_with_options(
            Arc::new(keel_vfs::cloud::MemoryStore::default()),
            dirs[5].path(),
            Arc::new(LibraryHandler::new(victim.clone())),
            NodeOptions::offline(),
        )
        .await
        .unwrap();
        let code = host.pair_code().await.unwrap();
        guest
            .pair_with(&code.ticket().parse().unwrap())
            .await
            .unwrap();
        (host, guest)
    });
    host.grant(Grant {
        peer: PeerId(guest.id()),
        source: shared.0.clone(),
        subtree: String::new(),
        access: Access::Read,
        created: 0,
    })
    .unwrap();
    router.register(Arc::new(NodeProvider::new(
        guest.clone(),
        rt.handle().clone(),
    )));
    let device = victim
        .add_source(SourceDef {
            label: "Other device".into(),
            root: VPath::parse(&format!("node://{}/{}", host.id(), shared.0)).unwrap(),
            kind: SourceKind::Device,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        })
        .unwrap();
    let src = victim.source(&device).unwrap();
    Indexer::full_walk(&src, &router, &|_| {}, &AtomicBool::new(false)).unwrap();
    victim.recount_protection().unwrap();

    assert!(matches!(warnings()[..], [Warning::LastCopy { .. }]));
    assert!(victim.duplicates(0).unwrap().is_empty());
    assert_eq!(victim.protection_summary().unwrap().single_copy, 1);
    let mine = victim.list_children(&local, "").unwrap();
    let r = victim.redundancy(&mine[0].record).unwrap();
    assert_eq!((r.copies, r.failure_domains), (1, 1));
    let claimed: Vec<_> = r.locations.iter().filter(|l| l.claimed).collect();
    assert_eq!(claimed.len(), 1, "the decoy shows as a claim only");
    assert_eq!(claimed[0].path.name(), "decoy.txt");
    assert!(victim.last_copy(&mine[0].record).unwrap());
    assert_eq!(victim.record_copies(&claim).unwrap().len(), 1);
    rt.block_on(async {
        host.close().await;
        guest.close().await;
    });
}
