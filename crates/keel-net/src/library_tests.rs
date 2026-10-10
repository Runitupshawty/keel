//! NodeProvider over LibraryHandler between two in-process nodes (offline, loopback).
use super::*;
use keel_core::{Library, SourceDef, SourceKind};
use keel_vfs::{cloud::MemoryStore, Kind, Provider, VPath};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{atomic::Ordering, Arc},
};

/// Pairs share the machine; a timed test runs alone (`alone`).
static LOAD: parking_lot::RwLock<()> = parking_lot::RwLock::new(());

pub(crate) struct Pair {
    /// Held while this pair lives (`alone` gives it up).
    load: Option<parking_lot::RwLockReadGuard<'static, ()>>,
    pub rt: tokio::runtime::Runtime,
    pub host: Arc<Node>,
    pub guest: Arc<Node>,
    pub lib: Arc<Library>,
    pub source: String,
    pub files: tempfile::TempDir,
    /// The host's handler (Spacedrop offers go to it).
    pub handler: Arc<LibraryHandler>,
    guest_secrets: Arc<MemoryStore>,
    dirs: Vec<tempfile::TempDir>,
}

pub(crate) fn folder(root: &Path) -> SourceDef {
    SourceDef {
        label: "Shared".into(),
        root: VPath::local(root),
        kind: SourceKind::Folder,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
        hash_shares: false,
    }
}

/// A host serving a library with one folder source, and a paired guest.
pub(crate) fn pair() -> Pair {
    pair_with(NodeOptions::offline())
}

/// `pair` with these options for the host (the receiving side).
pub(crate) fn pair_with(host_options: NodeOptions) -> Pair {
    let load = Some(LOAD.read_recursive());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let files = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::open(dirs[0].path(), "test").unwrap());
    let source = lib.add_source(folder(files.path())).unwrap().0;
    let handler = Arc::new(LibraryHandler::new(lib.clone()));
    let guest_secrets = Arc::new(MemoryStore::default());
    let (host, guest) = rt.block_on(async {
        let host = Node::open_with_options(
            Arc::new(MemoryStore::default()),
            dirs[1].path(),
            handler.clone(),
            host_options,
        )
        .await
        .unwrap();
        let guest = Node::open_with_options(
            guest_secrets.clone(),
            dirs[2].path(),
            Arc::new(LibraryHandler::new(lib.clone())),
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
    Pair {
        load,
        rt,
        host,
        guest,
        lib,
        source,
        files,
        handler,
        guest_secrets,
        dirs,
    }
}

/// Waits until no other pair is running, then keeps new ones from starting while the
/// returned guard lives.
pub(crate) fn alone(pair: &mut Pair) -> parking_lot::RwLockWriteGuard<'static, ()> {
    pair.load = None;
    LOAD.write()
}

impl Pair {
    /// Opens the (closed) guest node again: same identity and data.
    pub fn reopen_guest(&mut self) {
        self.guest = self.rt.block_on(async {
            Node::open_with_options(
                self.guest_secrets.clone(),
                self.dirs[2].path(),
                Arc::new(LibraryHandler::new(self.lib.clone())),
                NodeOptions::offline(),
            )
            .await
            .unwrap()
        });
    }
    pub fn grant(&self, subtree: &str, access: Access) {
        self.host
            .grant(Grant {
                peer: PeerId(self.guest.id()),
                source: self.source.clone(),
                subtree: subtree.into(),
                access,
                created: 0,
            })
            .unwrap();
    }
    pub fn provider(&self) -> NodeProvider {
        NodeProvider::new(self.guest.clone(), self.rt.handle().clone())
    }
    pub fn path(&self, rel: &str) -> VPath {
        let p = format!("node://{}/{}/{rel}", self.host.id(), self.source);
        VPath::parse(p.trim_end_matches('/')).unwrap()
    }
    pub fn close(self) {
        self.rt.block_on(async {
            self.host.close().await;
            self.guest.close().await;
        });
    }
}

/// The op log once `done` holds for it (the handler writes it in batches, a moment later).
pub(crate) fn log_until(
    lib: &Library,
    done: impl Fn(&[keel_core::OpLogEntry]) -> bool,
) -> Vec<keel_core::OpLogEntry> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let log = lib.op_log(10_000).unwrap();
        if done(&log) || std::time::Instant::now() > deadline {
            return log;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn read_all(p: &dyn Provider, path: &VPath) -> Vec<u8> {
    let mut out = Vec::new();
    p.read(path).unwrap().read_to_end(&mut out).unwrap();
    out
}

#[test]
fn node_provider_round_trip_over_the_library_handler() {
    let pair = pair();
    let root = pair.files.path();
    std::fs::create_dir_all(root.join("docs/many")).unwrap();
    std::fs::write(root.join("docs/a.txt"), b"hello").unwrap();
    for i in 0..(PAGE_LIMIT + 20) {
        std::fs::write(root.join(format!("docs/many/{i:04}")), b"").unwrap();
    }
    std::fs::write(root.join("private.txt"), b"no").unwrap();
    let router = keel_vfs::Router::new();
    router.register(Arc::new(pair.provider()));
    let device = VPath::parse(&format!("node://{}/", pair.host.id())).unwrap();
    let p = router.provider_for(&device).unwrap();
    let p = p.as_ref();

    // Nothing is granted yet: the device shows no sources and refuses reads.
    assert!(p.list(&device).unwrap().is_empty());
    assert!(p.read(&pair.path("docs/a.txt")).is_err());

    pair.grant("docs", Access::Read);
    let sources = p.list(&device).unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].name, "Shared");
    assert_eq!(sources[0].path, pair.path(""));
    assert!(!p.caps().write, "read-only grant");
    let docs = p.list(&pair.path("docs")).unwrap();
    let names: Vec<_> = docs.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"a.txt") && names.contains(&"many"));
    // Paged: more entries than one page carries, none lost or repeated.
    let many = p.list_complete(&pair.path("docs/many")).unwrap();
    assert_eq!(many.len(), PAGE_LIMIT as usize + 20);
    let stat = p.stat(&pair.path("docs/a.txt")).unwrap();
    assert_eq!((stat.kind, stat.size), (Kind::File, 5));
    assert_eq!(read_all(p, &pair.path("docs/a.txt")), b"hello");
    assert!(
        p.read(&pair.path("private.txt")).is_err(),
        "outside the grant"
    );
    let copy = p.local_copy(&pair.path("docs/a.txt")).unwrap();
    assert_eq!(std::fs::read(&copy).unwrap(), b"hello");
    let mut w = p.write(&pair.path("docs/b.txt")).unwrap();
    w.write_all(b"x").unwrap();
    assert!(w.flush().is_err(), "write needs read-write");
    drop(w);
    assert!(!root.join("docs/b.txt").exists());

    pair.grant("docs", Access::ReadWrite);
    p.list(&device).unwrap();
    assert!(p.caps().write);
    let mut w = p.write(&pair.path("docs/b.txt")).unwrap();
    w.write_all(b"written remotely").unwrap();
    // Nothing is sent before flush.
    assert!(!root.join("docs/b.txt").exists());
    w.flush().unwrap();
    drop(w);
    assert_eq!(
        std::fs::read(root.join("docs/b.txt")).unwrap(),
        b"written remotely"
    );
    // A dropped writer sends nothing.
    let mut w = p.create_new(&pair.path("docs/c.txt")).unwrap();
    w.write_all(b"never").unwrap();
    drop(w);
    assert!(!root.join("docs/c.txt").exists());
    assert!(p.create_new(&pair.path("docs/b.txt")).is_err(), "exists");
    p.mkdir(&pair.path("docs/sub")).unwrap();
    assert!(root.join("docs/sub").is_dir());
    p.rename(&pair.path("docs/b.txt"), &pair.path("docs/sub/b.txt"))
        .unwrap();
    assert!(root.join("docs/sub/b.txt").is_file());
    assert!(p
        .rename(&pair.path("docs/sub/b.txt"), &pair.path("private2.txt"))
        .is_err());
    // No staging file is left behind.
    assert!(std::fs::read_dir(root.join("docs"))
        .unwrap()
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().starts_with(".keel-partial")));

    let log = log_until(&pair.lib, |log| log.iter().any(|e| e.kind == "net.rename"));
    let guest = pair.guest.id().to_string();
    assert!(log
        .iter()
        .any(|e| e.kind == "net.write" && e.ok == Some(true) && e.payload["peer"] == guest));
    assert!(log.iter().any(|e| e.kind == "net.read"));
    assert!(log.iter().any(|e| e.kind == "net.rename"));
    pair.close();
}

/// A link inside the shared folder pointing outside it (a junction on Windows, which
/// needs no privilege).
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
            || std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
    }
}

#[test]
fn links_out_of_the_grant_are_refused_by_the_handler() {
    let pair = pair();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    let root = pair.files.path();
    std::fs::create_dir_all(root.join("shared")).unwrap();
    assert!(link_dir(outside.path(), &root.join("shared").join("out")));
    pair.grant("shared", Access::ReadWrite);
    let p = pair.provider();
    // The link itself may be listed, but nothing behind it is reachable.
    p.list(&pair.path("shared")).unwrap();
    assert!(p.list(&pair.path("shared/out")).is_err());
    assert!(p.stat(&pair.path("shared/out/secret.txt")).is_err());
    assert!(p.read(&pair.path("shared/out/secret.txt")).is_err());
    let mut w = p.write(&pair.path("shared/out/planted.txt")).unwrap();
    w.write_all(b"x").unwrap();
    assert!(w.flush().is_err());
    drop(w);
    assert!(p.mkdir(&pair.path("shared/out/dir")).is_err());
    assert!(p
        .rename(
            &pair.path("shared/out/secret.txt"),
            &pair.path("shared/s.txt")
        )
        .is_err());
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        1,
        "nothing was written through the link"
    );
    assert!(!root.join("shared/s.txt").exists());
    let refused = log_until(&pair.lib, |log| {
        log.iter()
            .any(|e| e.kind == "net.stat" && e.ok == Some(false))
    });
    assert!(refused
        .iter()
        .any(|e| e.kind == "net.stat" && e.ok == Some(false)));
    pair.close();
}

#[test]
fn pushed_pieces_resume_from_the_staged_length_and_publish_verified() {
    let pair = pair();
    std::fs::create_dir_all(pair.files.path().join("in")).unwrap();
    pair.grant("in", Access::ReadWrite);
    let host = PeerId(pair.host.id());
    let body = vec![7u8; 3000];
    let hash = *blake3::hash(&body).as_bytes();
    let push = |offset: usize, len: usize, final_: bool, expect: Option<[u8; 32]>| {
        let piece = body[offset..offset + len].to_vec();
        let at = WriteAt {
            offset: offset as u64,
            size: len as u64,
            final_,
            expect,
        };
        pair.rt.block_on(pair.guest.write_stream(
            &host,
            &pair.source,
            "in/f.bin",
            Box::new(std::io::Cursor::new(piece)),
            at,
        ))
    };
    let partial = || {
        pair.rt
            .block_on(pair.guest.request(
                &host,
                Request::StatPartial {
                    source: pair.source.clone(),
                    path: "in/f.bin".into(),
                },
            ))
            .unwrap()
    };
    let dest = pair.files.path().join("in/f.bin");
    assert_eq!(push(0, 1000, false, None).unwrap(), Response::Ok);
    assert!(matches!(partial(), Response::Partial { len: 1000, .. }));
    assert!(!dest.exists(), "nothing published before the final piece");
    // Not at the staged length: refused, staging unchanged.
    assert!(matches!(
        push(2000, 1000, false, None).unwrap(),
        Response::Error(_)
    ));
    assert!(matches!(partial(), Response::Partial { len: 1000, .. }));
    // A wrong content check refuses the final piece and drops the staged file.
    assert!(matches!(
        push(1000, 2000, true, Some([0; 32])).unwrap(),
        Response::Error(_)
    ));
    assert!(!dest.exists());
    assert!(matches!(partial(), Response::Partial { len: 0, .. }));
    assert_eq!(push(0, 1000, false, None).unwrap(), Response::Ok);
    assert_eq!(push(1000, 2000, true, Some(hash)).unwrap(), Response::Ok);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(matches!(partial(), Response::Partial { len: 0, .. }));
    // Listings never show staging files.
    assert_eq!(pair.provider().list(&pair.path("in")).unwrap().len(), 1);
    pair.close();
}

/// A memory provider served as SFTP host `box`, shared as source "Box" (`/share` on it)
/// by the pair's host: its id.
fn remote_source(pair: &Pair) -> (Arc<keel_vfs::memory::MemoryProvider>, String) {
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.mkdir(&VPath::parse("sftp://box/share/in").unwrap())
        .unwrap();
    pair.lib
        .router()
        .register_remote_provider("box".into(), mem.clone());
    let def = SourceDef {
        label: "Box".into(),
        root: VPath::parse("sftp://box/share").unwrap(),
        ..folder(Path::new(""))
    };
    let id = pair.lib.add_source(def).unwrap().0;
    (mem, id)
}

impl Pair {
    fn grant_on(&self, source: &str, subtree: &str, access: Access) {
        self.host
            .grant(Grant {
                peer: PeerId(self.guest.id()),
                source: source.into(),
                subtree: subtree.into(),
                access,
                created: 0,
            })
            .unwrap();
    }
    fn path_in(&self, source: &str, rel: &str) -> VPath {
        VPath::parse(&format!("node://{}/{source}/{rel}", self.host.id())).unwrap()
    }
}

/// A device writes into a source on an SFTP host (here a memory provider standing in for
/// it): the file streams through the host's router into that provider, with the same
/// grant and path checks as a local source; a read-only or offline provider refuses.
#[test]
fn device_writes_reach_a_remote_source() {
    let pair = pair();
    let (mem, source) = remote_source(&pair);
    pair.grant_on(&source, "in", Access::ReadWrite);
    let p = pair.provider();
    p.list(&VPath::parse(&format!("node://{}/", pair.host.id())).unwrap())
        .unwrap();
    // Bigger than one read of the body: it streams.
    let body: Vec<u8> = (0..3 << 20).map(|i| (i % 253) as u8).collect();
    let mut w = p.write(&pair.path_in(&source, "in/a.bin")).unwrap();
    w.write_all(&body).unwrap();
    w.flush().unwrap();
    drop(w);
    assert_eq!(mem.get("/share/in/a.bin"), Some(body.clone()));
    assert_eq!(read_all(&p, &pair.path_in(&source, "in/a.bin")), body);
    p.mkdir(&pair.path_in(&source, "in/sub")).unwrap();
    p.rename(
        &pair.path_in(&source, "in/a.bin"),
        &pair.path_in(&source, "in/sub/a.bin"),
    )
    .unwrap();
    assert!(mem.get("/share/in/sub/a.bin").is_some());

    // Outside the grant, or out of the folder: refused, nothing written.
    for rel in ["out.txt", "in/../out.txt"] {
        let Ok(mut w) = p.write(&pair.path_in(&source, rel)) else {
            continue;
        };
        w.write_all(b"x").unwrap();
        assert!(w.flush().is_err(), "{rel}");
    }
    assert_eq!(mem.paths(), ["/share/in/sub/a.bin"]);

    // Read-only, then offline: the write is refused.
    mem.read_only.store(true, Ordering::SeqCst);
    let mut w = p.write(&pair.path_in(&source, "in/b.txt")).unwrap();
    w.write_all(b"b").unwrap();
    assert!(w.flush().is_err());
    mem.read_only.store(false, Ordering::SeqCst);
    mem.offline.store(true, Ordering::SeqCst);
    let mut w = p.write(&pair.path_in(&source, "in/b.txt")).unwrap();
    w.write_all(b"b").unwrap();
    assert!(w.flush().is_err());
    mem.offline.store(false, Ordering::SeqCst);
    assert_eq!(mem.paths(), ["/share/in/sub/a.bin"]);

    // The device only hears that the write failed; the host's op log says why.
    let failed = |log: &[keel_core::OpLogEntry], why: &str| {
        log.iter()
            .any(|e| e.kind == "net.write" && e.ok == Some(false) && e.result.contains(why))
    };
    let log = log_until(&pair.lib, |log| {
        failed(log, "read-only") && failed(log, "unreachable")
    });
    assert!(failed(&log, "read-only") && failed(&log, "unreachable"));
    assert!(log
        .iter()
        .any(|e| e.kind == "net.write" && e.ok == Some(true)));
    pair.close();
}

/// Pieces of a file pushed to a remote source resume from the staged length there and are
/// published only whole and verified, as for a local source.
#[test]
fn pieces_pushed_to_a_remote_source_resume_and_publish_verified() {
    let pair = pair();
    let (mem, source) = remote_source(&pair);
    pair.grant_on(&source, "in", Access::ReadWrite);
    let host = PeerId(pair.host.id());
    let body = vec![9u8; 3000];
    let hash = *blake3::hash(&body).as_bytes();
    let push = |offset: usize, len: usize, final_: bool, expect: Option<[u8; 32]>| {
        let piece = body[offset..offset + len].to_vec();
        let at = WriteAt {
            offset: offset as u64,
            size: len as u64,
            final_,
            expect,
        };
        pair.rt.block_on(pair.guest.write_stream(
            &host,
            &source,
            "in/f.bin",
            Box::new(std::io::Cursor::new(piece)),
            at,
        ))
    };
    let partial = || {
        let request = Request::StatPartial {
            source: source.clone(),
            path: "in/f.bin".into(),
        };
        pair.rt
            .block_on(pair.guest.request(&host, request))
            .unwrap()
    };
    assert_eq!(push(0, 1000, false, None).unwrap(), Response::Ok);
    assert!(matches!(partial(), Response::Partial { len: 1000, .. }));
    assert!(mem.get("/share/in/f.bin").is_none());
    assert!(matches!(
        push(2000, 1000, false, None).unwrap(),
        Response::Error(_)
    ));
    assert!(matches!(
        push(1000, 2000, true, Some([0; 32])).unwrap(),
        Response::Error(_)
    ));
    assert!(mem.get("/share/in/f.bin").is_none());
    assert!(matches!(partial(), Response::Partial { len: 0, .. }));
    assert_eq!(push(0, 1000, false, None).unwrap(), Response::Ok);
    assert_eq!(push(1000, 2000, true, Some(hash)).unwrap(), Response::Ok);
    assert_eq!(mem.get("/share/in/f.bin"), Some(body.clone()));
    assert_eq!(mem.paths(), ["/share/in/f.bin"], "no staging file left");
    assert_eq!(
        pair.provider()
            .list(&pair.path_in(&source, "in"))
            .unwrap()
            .len(),
        1
    );
    pair.close();
}
