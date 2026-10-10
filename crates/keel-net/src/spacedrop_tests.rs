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

// ---- offers, decisions and receiving (Task 36 review) ----

fn offer(pair: &crate::library_tests::Pair, id: &str, files: &[(&str, u64)]) -> Response {
    let files = files.iter().map(|(p, n)| (p.to_string(), *n)).collect();
    let host = PeerId(pair.host.id());
    let req = Request::DropOffer {
        id: id.into(),
        files,
    };
    pair.rt.block_on(pair.guest.request(&host, req)).unwrap()
}

fn raw(pair: &crate::library_tests::Pair, req: Request) -> Response {
    let host = PeerId(pair.host.id());
    pair.rt.block_on(pair.guest.request(&host, req)).unwrap()
}

/// One whole file as the final piece of drop `id`.
fn put(pair: &crate::library_tests::Pair, id: &str, name: &str, bytes: &[u8]) -> Response {
    let host = PeerId(pair.host.id());
    pair.rt
        .block_on(pair.guest.write_stream(
            &host,
            &format!("drop:{id}"),
            name,
            Box::new(std::io::Cursor::new(bytes.to_vec())),
            WriteAt {
                offset: 0,
                size: bytes.len() as u64,
                final_: true,
                expect: Some(*blake3::hash(bytes).as_bytes()),
            },
        ))
        .unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut n: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    n.sort();
    n
}

/// Every offer is held for the test to answer.
fn hold(pair: &crate::library_tests::Pair, inbox: &Path) -> Arc<Mutex<Vec<IncomingDrop>>> {
    let held: Arc<Mutex<Vec<IncomingDrop>>> = Arc::default();
    let h = held.clone();
    pair.handler
        .on_drop(inbox.to_owned(), move |d: IncomingDrop| h.lock().push(d));
    held
}

#[test]
fn an_answer_holds_for_one_device_id_and_file_list_only() {
    let pair = pair();
    let inbox = tempfile::tempdir().unwrap();
    let held = hold(&pair, inbox.path());
    let id = "0123456789abcdef0123456789abcdef";
    assert_eq!(offer(&pair, id, &[("a.txt", 1)]), Response::Pending);
    held.lock().pop().unwrap().reply.answer(true);
    assert_eq!(offer(&pair, id, &[("a.txt", 1)]), Response::Ok, "resumes");
    assert!(held.lock().is_empty(), "no second prompt for the same list");
    // Another list under the accepted id is a new offer: asked again, nothing goes in.
    assert_eq!(
        offer(&pair, id, &[("a.txt", 1), ("payload.exe", 7)]),
        Response::Pending
    );
    assert_eq!(held.lock().len(), 1);
    assert!(matches!(
        put(&pair, id, "payload.exe", b"payload"),
        Response::Denied(_)
    ));
    held.lock().pop().unwrap().reply.answer(false);
    assert!(matches!(
        raw(&pair, Request::DropStatus { id: id.into() }),
        Response::Denied(_)
    ));
    // A declined offer forgets its answer: the same id again asks again.
    assert_eq!(offer(&pair, id, &[("a.txt", 1)]), Response::Pending);
    held.lock().pop().unwrap().reply.answer(true);
    assert_eq!(
        raw(&pair, Request::DropStatus { id: id.into() }),
        Response::Ok
    );
    assert_eq!(put(&pair, id, "a.txt", b"a"), Response::Ok);
    // A completed drop answers Ok from its record (a sender that lost the last reply):
    // no prompt, nothing to send again.
    assert_eq!(offer(&pair, id, &[("a.txt", 1)]), Response::Ok);
    assert!(held.lock().is_empty(), "not asked again");
    assert_eq!(
        raw(&pair, Request::DropStatus { id: id.into() }),
        Response::Ok
    );
    let partial = Request::StatPartial {
        source: format!("drop:{id}"),
        path: "a.txt".into(),
    };
    assert!(
        matches!(
            raw(&pair, partial),
            Response::Partial { complete: true, .. }
        ),
        "complete"
    );
    assert!(matches!(put(&pair, id, "a.txt", b"a"), Response::Denied(_)));
    assert_eq!(names(inbox.path()), ["a.txt"], "no a (1).txt, no staging");
    assert_eq!(offer(&pair, id, &[("payload.exe", 7)]), Response::Pending);
    assert!(matches!(
        put(&pair, id, "payload.exe", b"payload"),
        Response::Denied(_)
    ));
    assert_eq!(names(inbox.path()), ["a.txt"]);
    pair.close();
}

#[test]
fn unanswered_offers_answer_at_once_and_a_cancel_withdraws_the_prompt() {
    let pair = pair();
    pair.grant("", Access::Read);
    let host = PeerId(pair.host.id());
    let inbox = tempfile::tempdir().unwrap();
    let held = hold(&pair, inbox.path());
    let id = "fedcba9876543210fedcba9876543210";
    let answers: Vec<Response> = pair.rt.block_on(async {
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let g = pair.guest.clone();
            let req = Request::DropOffer {
                id: id.into(),
                files: vec![("a.txt".into(), 1)],
            };
            tasks.push(tokio::spawn(async move { g.request(&host, req).await }));
        }
        let mut out = Vec::new();
        for t in tasks {
            out.push(t.await.unwrap().unwrap());
        }
        out
    });
    assert!(
        answers.iter().all(|r| *r == Response::Pending),
        "{answers:?}"
    );
    assert_eq!(held.lock().len(), 1, "one prompt");
    assert!(matches!(raw(&pair, Request::Ping), Response::Pong { .. }));
    let list = Request::List {
        source: pair.source.clone(),
        path: String::new(),
        after: None,
        limit: 10,
    };
    assert!(matches!(raw(&pair, list), Response::Entries { .. }));
    let status = || raw(&pair, Request::DropStatus { id: id.into() });
    assert_eq!(status(), Response::Pending);
    // The sender gives up: the prompt is withdrawn; a late Accept stages nothing.
    let prompt = held.lock().pop().unwrap();
    assert!(!prompt.reply.withdrawn());
    assert_eq!(
        raw(&pair, Request::DropCancel { id: id.into() }),
        Response::Ok
    );
    assert!(prompt.reply.withdrawn());
    prompt.reply.answer(true);
    assert!(matches!(status(), Response::Error(_)));
    assert!(names(inbox.path()).is_empty());
    // Staging an earlier session left behind is swept; nothing else is.
    std::fs::create_dir_all(inbox.path().join(".keel-partial-old")).unwrap();
    std::fs::write(inbox.path().join(".keel-partial-old/0"), b"x").unwrap();
    std::fs::create_dir_all(inbox.path().join("kept")).unwrap();
    spacedrop::sweep(inbox.path(), spacedrop::STALE);
    assert_eq!(names(inbox.path()), [".keel-partial-old", "kept"], "fresh");
    spacedrop::sweep(inbox.path(), Duration::ZERO);
    assert_eq!(names(inbox.path()), ["kept"]);
    pair.close();
}

#[test]
fn drops_are_logged_per_file_on_both_sides() {
    let pair = pair();
    spacedrop::register(&pair.lib);
    let host = PeerId(pair.host.id());
    let dir = pair.files.path().join("pack");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("one.txt"), b"one").unwrap();
    std::fs::write(dir.join("two.txt"), b"second").unwrap();
    let inbox = tempfile::tempdir().unwrap();
    pair.handler
        .on_drop(inbox.path().to_owned(), |d: IncomingDrop| {
            d.reply.answer(true)
        });
    let job = spacedrop::send(&pair.guest, &pair.lib, host, vec![VPath::local(&dir)]).unwrap();
    let info = pair.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    // Both nodes serve one library here, so both sides' entries are in its op log (the
    // receiver's are written in batches, a moment later).
    let count = |kind: &str| {
        let log = pair.lib.op_log(100).unwrap();
        log.iter().filter(|e| e.kind == kind).count()
    };
    wait_for("the receiver's entries", || count("net.drop-received") == 2);
    let log = pair.lib.op_log(100).unwrap();
    let of = |kind: &str| -> Vec<&keel_core::OpLogEntry> {
        log.iter().filter(|e| e.kind == kind).collect()
    };
    for kind in ["net.drop-sent", "net.drop-received"] {
        let entries = of(kind);
        assert_eq!(entries.len(), 2, "{kind}: {log:?}");
        let two = entries
            .iter()
            .find(|e| e.payload["path"] == "pack/two.txt")
            .unwrap();
        assert_eq!(two.payload["size"], 6);
        assert_eq!(
            two.payload["blake3"],
            blake3::hash(b"second").to_hex().as_str()
        );
        assert_eq!(two.ok, Some(true));
    }
    assert_eq!(of("net.drop-offer").len(), 1);
    assert_eq!(
        of("net.drop-received")[0].payload["peer"],
        pair.guest.id().to_string()
    );
    pair.close();
}

#[test]
fn two_thousand_files_arrive_in_linear_time() {
    let mut pair = pair();
    let inbox = tempfile::tempdir().unwrap();
    pair.handler
        .on_drop(inbox.path().to_owned(), |d: IncomingDrop| {
            d.reply.answer(true)
        });
    let id = "00000000000000000000000000002000";
    let files: Vec<String> = (0..2000).map(|i| format!("d/f{i:06}.bin")).collect();
    let listed: Vec<(&str, u64)> = files.iter().map(|f| (f.as_str(), 0)).collect();
    assert_eq!(offer(&pair, id, &listed), Response::Ok);
    let meta = staged(inbox.path()).unwrap().join("meta.json");
    let written = std::fs::read(&meta).unwrap();
    // Timed alone: other tests' nodes would share the machine.
    let _alone = crate::library_tests::alone(&mut pair);
    let send = |part: &[String]| {
        for f in part {
            assert_eq!(put(&pair, id, f, b""), Response::Ok);
        }
    };
    // What the same number of bare requests costs on this machine now (a loaded machine
    // stretches the 3 s budget accordingly).
    let floor = Instant::now();
    for f in &files {
        let status = Request::StatPartial {
            source: format!("drop:{id}"),
            path: f.clone(),
        };
        assert!(matches!(raw(&pair, status), Response::Partial { .. }));
    }
    // Plus one fsync per file (the `published` marker is synced as each file lands).
    let synced = Instant::now();
    let mut marker = std::fs::File::create(inbox.path().join("sync-probe")).unwrap();
    for _ in 0..200 {
        use std::io::Write;
        marker.write_all(b"1\n").unwrap();
        marker.sync_all().unwrap();
    }
    drop(marker);
    std::fs::remove_file(inbox.path().join("sync-probe")).unwrap();
    let fsyncs = synced.elapsed() * 10;
    let budget = Duration::from_secs(3).max(floor.elapsed() * 3) + fsyncs;
    let started = Instant::now();
    send(&files[..1000]);
    let first = started.elapsed();
    assert_eq!(std::fs::read(&meta).unwrap(), written, "meta written once");
    send(&files[1000..]);
    let took = started.elapsed();
    assert_eq!(
        std::fs::read_dir(inbox.path().join("d")).unwrap().count(),
        2000
    );
    assert!(staged(inbox.path()).is_none(), "staging removed when done");
    assert!(
        took < budget,
        "2,000 files took {took:?} (budget {budget:?})"
    );
    // Linear: the second thousand costs about what the first did.
    assert!(
        took - first < first * 3,
        "{first:?}, then {:?}",
        took - first
    );
    pair.close();
}

#[test]
fn publishing_never_replaces_a_file() {
    let inbox = tempfile::tempdir().unwrap();
    let stage = tempfile::tempdir().unwrap();
    std::fs::write(inbox.path().join("a.txt"), b"mine").unwrap();
    let staged = |n: usize| {
        let p = stage.path().join(n.to_string());
        std::fs::write(&p, n.to_string()).unwrap();
        p
    };
    let first = spacedrop::publish(&staged(0), inbox.path(), "a.txt").unwrap();
    assert_eq!(first, inbox.path().join("a (1).txt"));
    assert_eq!(std::fs::read(inbox.path().join("a.txt")).unwrap(), b"mine");
    // Many at once under one name: every one arrives.
    let paths: Vec<PathBuf> = (1..=40).map(staged).collect();
    std::thread::scope(|s| {
        for p in &paths {
            let inbox = inbox.path();
            s.spawn(move || spacedrop::publish(p, inbox, "a.txt").unwrap());
        }
    });
    assert_eq!(names(inbox.path()).len(), 42);
}

/// `many://x/pack`: a folder of `n` files with long names (an offer too big to send).
struct Many(usize);
impl Provider for Many {
    fn scheme(&self) -> &'static str {
        "many"
    }
    fn caps(&self) -> Caps {
        Caps::default()
    }
    fn list(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
        Ok((0..self.0)
            .map(|i| {
                let name = format!("{i:06}-{}.bin", "x".repeat(150));
                Entry {
                    path: dir.join(&name),
                    name,
                    kind: Kind::File,
                    size: 1,
                    modified: None,
                    hidden: false,
                    is_link: false,
                    encrypted: false,
                    ext: "bin".into(),
                }
            })
            .collect())
    }
    fn list_complete(&self, d: &VPath) -> anyhow::Result<Vec<Entry>> {
        self.list(d)
    }
    fn stat(&self, p: &VPath) -> anyhow::Result<Entry> {
        Ok(Entry {
            path: p.clone(),
            name: "pack".into(),
            kind: Kind::Dir,
            size: 0,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: String::new(),
        })
    }
    fn read(&self, _: &VPath) -> anyhow::Result<Box<dyn Read + Send>> {
        anyhow::bail!("not here")
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

#[test]
fn an_offer_too_big_to_send_fails_at_once() {
    let pair = pair();
    spacedrop::register(&pair.lib);
    let router = Arc::new(Router::new());
    router.register(Arc::new(Many(8_000)));
    pair.lib.set_router(router);
    let started = Instant::now();
    let host = PeerId(pair.host.id());
    let pack = VPath::parse("many://x/pack").unwrap();
    let job = spacedrop::send(&pair.guest, &pair.lib, host, vec![pack]).unwrap();
    let info = pair.lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Failed);
    assert!(info.log.contains("too many files"), "{}", info.log);
    assert!(started.elapsed() < Duration::from_secs(30));
    pair.close();
}

#[test]
fn forgetting_a_device_drops_its_offers_and_staging() {
    let pair = pair();
    let inbox = tempfile::tempdir().unwrap();
    let held = hold(&pair, inbox.path());
    let (a, b) = (
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    assert_eq!(offer(&pair, a, &[("big.bin", 10)]), Response::Pending);
    held.lock().pop().unwrap().reply.answer(true);
    assert_eq!(offer(&pair, a, &[("big.bin", 10)]), Response::Ok);
    let host = PeerId(pair.host.id());
    let piece = pair.rt.block_on(pair.guest.write_stream(
        &host,
        &format!("drop:{a}"),
        "big.bin",
        Box::new(std::io::Cursor::new(b"half!".to_vec())),
        WriteAt {
            offset: 0,
            size: 5,
            final_: false,
            expect: None,
        },
    ));
    assert_eq!(piece.unwrap(), Response::Ok);
    let staging = staged(inbox.path()).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        let attributes = std::fs::metadata(&staging).unwrap().file_attributes();
        assert_ne!(attributes & HIDDEN, 0, "staging is hidden in Explorer");
    }
    assert_eq!(offer(&pair, b, &[("c.txt", 1)]), Response::Pending);
    let prompt = held.lock().pop().unwrap();
    pair.host.forget_peer(&PeerId(pair.guest.id())).unwrap();
    assert!(prompt.reply.withdrawn());
    assert!(staged(inbox.path()).is_none());
    pair.close();
}

#[test]
fn received_files_and_pairings_are_never_dropped_from_events() {
    let pair = pair();
    let events = pair.host.events();
    for _ in 0..300 {
        pair.host.emit(NetEvent::GrantChanged);
    }
    let guest = PeerId(pair.guest.id());
    pair.host.emit(NetEvent::DropReceived {
        peer: guest,
        id: "x".into(),
        path: PathBuf::from("a.txt"),
    });
    // Requests are coalesced: one event a second per device.
    pair.host.emit_request(guest, "list");
    pair.host.emit_request(guest, "list");
    let got: Vec<NetEvent> = events.try_iter().collect();
    assert_eq!(
        got.len(),
        257,
        "256 refreshable events, then only what matters"
    );
    assert!(matches!(got.last(), Some(NetEvent::DropReceived { .. })));
    let rest = pair.host.events();
    pair.host.emit_request(guest, "stat");
    assert_eq!(rest.try_iter().count(), 0, "within the same second");
    pair.close();
}

#[test]
fn offers_per_device_are_capped() {
    let pair = pair();
    let inbox = tempfile::tempdir().unwrap();
    let held = hold(&pair, inbox.path());
    let ids: Vec<String> = (0..=spacedrop::PENDING_PER_DEVICE)
        .map(|i| format!("{i:032x}"))
        .collect();
    for id in &ids[..spacedrop::PENDING_PER_DEVICE] {
        assert_eq!(offer(&pair, id, &[("a.txt", 1)]), Response::Pending);
    }
    let last = &ids[spacedrop::PENDING_PER_DEVICE];
    assert_eq!(
        offer(&pair, last, &[("a.txt", 1)]),
        Response::Denied("busy".into())
    );
    assert_eq!(
        held.lock().len(),
        spacedrop::PENDING_PER_DEVICE,
        "no prompt"
    );
    // A re-offer of a waiting one is not a new offer.
    assert_eq!(offer(&pair, &ids[0], &[("a.txt", 1)]), Response::Pending);
    // One answered: room for another.
    held.lock().remove(0).reply.answer(false);
    assert!(matches!(
        raw(&pair, Request::DropStatus { id: ids[0].clone() }),
        Response::Denied(_)
    ));
    assert_eq!(offer(&pair, last, &[("a.txt", 1)]), Response::Pending);
    pair.close();
}

#[test]
fn an_offer_its_sender_stopped_polling_is_withdrawn() {
    let wait = Duration::from_millis(400);
    let pair = crate::library_tests::pair_with(NodeOptions {
        drop_answer_wait: wait,
        ..NodeOptions::offline()
    });
    let inbox = tempfile::tempdir().unwrap();
    let held = hold(&pair, inbox.path());
    let id = "abababababababababababababababab";
    assert_eq!(offer(&pair, id, &[("a.txt", 1)]), Response::Pending);
    // Polled: kept past the wait.
    let polled_until = Instant::now() + wait * 3;
    while Instant::now() < polled_until {
        assert_eq!(
            raw(&pair, Request::DropStatus { id: id.into() }),
            Response::Pending
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let prompt = held.lock().pop().unwrap();
    assert!(!prompt.reply.withdrawn());
    // Not polled: withdrawn, the prompt goes.
    wait_for("the withdrawal", || prompt.reply.withdrawn());
    assert!(matches!(
        raw(&pair, Request::DropStatus { id: id.into() }),
        Response::Error(_)
    ));
    pair.close();
}

#[test]
fn invalid_offers_and_staging_failures_are_denied_for_good() {
    let pair = pair();
    let inbox = tempfile::tempdir().unwrap();
    // An inbox that cannot hold a staging folder (it is a file).
    let not_a_folder = inbox.path().join("file");
    std::fs::write(&not_a_folder, b"x").unwrap();
    pair.handler
        .on_drop(not_a_folder, |d: IncomingDrop| d.reply.answer(true));
    let id = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
    let denied = |r: Response, why: &str| match r {
        Response::Denied(m) => assert!(m.contains(why), "{m}"),
        r => panic!("{r:?}"),
    };
    denied(offer(&pair, id, &[("../x", 1)]), "invalid");
    denied(offer(&pair, id, &[("a.txt", 1)]), "stage");
    if cfg!(windows) {
        // 8.3 short names (a staging folder's is `KEEL-P~1`) are refused on Windows.
        denied(offer(&pair, id, &[("KEEL-P~1/published", 1)]), "invalid");
    }
    pair.close();
}

#[test]
fn publishing_refuses_a_folder_that_leads_into_staging() {
    let inbox = tempfile::tempdir().unwrap();
    let staging = inbox.path().join(".keel-partial-0123");
    std::fs::create_dir(&staging).unwrap();
    let file = inbox.path().join("piece");
    std::fs::write(&file, b"x").unwrap();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&staging, inbox.path().join("door"));
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&staging, inbox.path().join("door"));
    if linked.is_err() {
        eprintln!("no symlinks here (Windows without developer mode); skipped");
        return;
    }
    let err = spacedrop::publish(&file, inbox.path(), "door/published").unwrap_err();
    assert!(format!("{err:#}").contains("staging"), "{err:#}");
    assert!(!staging.join("published").exists());
    assert!(spacedrop::publish(&file, inbox.path(), "ok/piece").is_ok());
}

#[test]
fn the_op_log_is_written_out_on_close() {
    let pair = pair();
    let ctx = RequestCtx {
        peer: PeerId(pair.guest.id()),
        label: "Desk".into(),
    };
    for i in 0..50 {
        pair.handler
            .log(&ctx, "probe", serde_json::json!({ "n": i }), true);
    }
    pair.rt.block_on(pair.host.close());
    let log = pair.lib.op_log(1000).unwrap();
    assert_eq!(
        log.iter().filter(|e| e.kind == "net.probe").count(),
        50,
        "all written by the time close returns"
    );
    pair.rt.block_on(pair.guest.close());
}
