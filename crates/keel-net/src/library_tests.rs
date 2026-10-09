//! NodeProvider over LibraryHandler between two in-process nodes (offline, loopback).
use super::*;
use keel_core::{Library, SourceDef, SourceKind};
use keel_vfs::{cloud::MemoryStore, Kind, Provider, VPath};
use std::{
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

pub(crate) struct Pair {
    pub rt: tokio::runtime::Runtime,
    pub host: Arc<Node>,
    pub guest: Arc<Node>,
    pub lib: Arc<Library>,
    pub source: String,
    pub files: tempfile::TempDir,
    _dirs: Vec<tempfile::TempDir>,
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let files = tempfile::tempdir().unwrap();
    let lib = Arc::new(Library::open(dirs[0].path(), "test").unwrap());
    let source = lib.add_source(folder(files.path())).unwrap().0;
    let (host, guest) = rt.block_on(async {
        let host = Node::open_with_options(
            Arc::new(MemoryStore::default()),
            dirs[1].path(),
            Arc::new(LibraryHandler::new(lib.clone())),
            NodeOptions::offline(),
        )
        .await
        .unwrap();
        let guest = Node::open_with_options(
            Arc::new(MemoryStore::default()),
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
        rt,
        host,
        guest,
        lib,
        source,
        files,
        _dirs: dirs,
    }
}

impl Pair {
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

    let log = pair.lib.op_log(10_000).unwrap();
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
    let refused = pair.lib.op_log(100).unwrap();
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
