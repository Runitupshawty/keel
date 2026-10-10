//! Extraction into a folder on another provider (the in-memory one stands in for SFTP,
//! cloud and device folders).
use crate::{
    archive::cache::MaterialiseCache, memory::MemoryProvider, ops::extract_to, Conflict, Provider,
    Router, VPath,
};
use std::{
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in entries {
        if let Some(dir) = name.strip_suffix('/') {
            zip.add_directory(dir, zip::write::SimpleFileOptions::default())
                .unwrap();
        } else {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(body).unwrap();
        }
    }
    zip.finish().unwrap().into_inner()
}

fn setup(tmp: &Path) -> (Router, Arc<MemoryProvider>, VPath) {
    let router =
        Router::with_archive_cache(Arc::new(MaterialiseCache::new(tmp.join("cache"), 1 << 30)));
    let mem = Arc::new(MemoryProvider::new());
    router.register(mem.clone());
    let dest = VPath::parse("memory://box/dest").unwrap();
    mem.mkdir(&dest).unwrap();
    (router, mem, dest)
}

fn extract(
    archive: &VPath,
    dest: &VPath,
    conflict: Conflict,
    router: &Router,
) -> anyhow::Result<crate::Progress> {
    let last = std::sync::Mutex::new(crate::Progress::default());
    extract_to(
        archive,
        "",
        &[],
        dest,
        conflict,
        &|p| *last.lock().unwrap() = p,
        &AtomicBool::new(false),
        router,
    )?;
    Ok(last.into_inner().unwrap())
}

fn partials(mem: &MemoryProvider) -> Vec<String> {
    mem.files()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.contains("keel-partial"))
        .collect()
}

#[test]
fn a_nested_zip_extracts_into_another_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let inner = zip_bytes(&[
        ("a.txt", b"alpha"),
        ("dir/b.txt", b"bravo"),
        ("dir/empty/", b""),
    ]);
    let outer = tmp.path().join("outer.zip");
    std::fs::write(&outer, zip_bytes(&[("inner.zip", &inner)])).unwrap();
    let (router, mem, dest) = setup(tmp.path());
    let archive = VPath::join_archive(&VPath::local(&outer), "inner.zip");
    let done = extract(&archive, &dest, Conflict::Skip, &router).unwrap();
    assert_eq!(
        mem.files(),
        [
            ("/dest/a.txt".to_owned(), b"alpha".to_vec()),
            ("/dest/dir/b.txt".to_owned(), b"bravo".to_vec()),
        ]
    );
    assert!(mem.dirs().contains(&"/dest/dir/empty".to_owned()));
    assert_eq!((done.done_bytes, done.total_bytes), (10, 10));
    assert_eq!((done.done_items, done.total_items), (3, 3));
    // Part of an archive, relative to a folder inside it.
    let sub = VPath::parse("memory://box/sub").unwrap();
    mem.mkdir(&sub).unwrap();
    extract_to(
        &archive,
        "dir",
        &["dir/b.txt".into()],
        &sub,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
        &router,
    )
    .unwrap();
    assert!(mem.read(&sub.join("b.txt")).is_ok());
    assert!(partials(&mem).is_empty());
}

#[test]
fn clashes_are_skipped_renamed_or_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("a.zip");
    std::fs::write(
        &zip,
        zip_bytes(&[("a.txt", b"from the zip"), ("d/c.txt", b"c")]),
    )
    .unwrap();
    let (router, mem, dest) = setup(tmp.path());
    mem.put("/dest/a.txt", "mine");
    let archive = VPath::local(&zip);
    let done = extract(&archive, &dest, Conflict::Skip, &router).unwrap();
    assert_eq!(done.skipped, 1);
    let body = |p: &str| {
        mem.files()
            .into_iter()
            .find(|(n, _)| n == p)
            .map(|(_, b)| String::from_utf8(b).unwrap())
    };
    assert_eq!(body("/dest/a.txt").as_deref(), Some("mine"));
    assert_eq!(body("/dest/d/c.txt").as_deref(), Some("c"), "folders merge");
    extract(&archive, &dest, Conflict::RenameNew, &router).unwrap();
    assert_eq!(body("/dest/a (2).txt").as_deref(), Some("from the zip"));
    assert_eq!(body("/dest/d/c (2).txt").as_deref(), Some("c"));
    extract(&archive, &dest, Conflict::Overwrite, &router).unwrap();
    assert_eq!(body("/dest/a.txt").as_deref(), Some("from the zip"));
    assert!(partials(&mem).is_empty());
    // Not into a folder inside an archive, nor onto a file.
    let into_zip = VPath::join_archive(&archive, "d");
    assert!(extract(&archive, &into_zip, Conflict::Skip, &router).is_err());
    assert!(extract(&archive, &dest.join("a.txt"), Conflict::Skip, &router).is_err());
}

#[test]
fn a_cancel_mid_way_leaves_no_partial_file() {
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("big.zip");
    let big = vec![7u8; 4 << 20];
    std::fs::write(&zip, zip_bytes(&[("first.txt", b"1"), ("big.bin", &big)])).unwrap();
    let (router, mem, dest) = setup(tmp.path());
    // The partial file is there while it is written, as on a server: only its removal
    // keeps it out of the listing below.
    mem.write_through.store(true, Ordering::SeqCst);
    let partial_seen = AtomicBool::new(false);
    let cancel = AtomicBool::new(false);
    let result = extract_to(
        &VPath::local(&zip),
        "",
        &[],
        &dest,
        Conflict::Skip,
        &|p| {
            if p.done_bytes > 1 << 20 {
                if mem.paths().iter().any(|n| n.contains("big.bin")) {
                    partial_seen.store(true, Ordering::Relaxed);
                }
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
        &router,
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
    let names: Vec<String> = mem.files().into_iter().map(|(p, _)| p).collect();
    assert_eq!(names, ["/dest/first.txt"], "the finished file only");
    assert!(
        partial_seen.load(Ordering::Relaxed),
        "the partial file was there"
    );
}

/// The test provider's change log and folder times see files placed by a writer and by a
/// rename, as they see `put` and `remove`.
#[test]
fn memory_writes_and_renames_are_in_the_change_log() {
    use crate::ChangeKind::*;
    let mem = MemoryProvider::new();
    mem.folder_times.store(true, Ordering::SeqCst);
    let no = AtomicBool::new(false);
    let dir = VPath::parse("memory://box/d").unwrap();
    mem.put("/d/other.txt", "o");
    let start = mem.changes(None, &no).unwrap().cursor;
    let time = || mem.stat(&dir).unwrap().modified;
    let before = time();
    let mut w = mem.create_new(&dir.join("a.txt")).unwrap();
    w.write_all(b"a").unwrap();
    w.flush().unwrap();
    drop(w);
    let written = time();
    assert_ne!(written, before);
    mem.rename(&dir.join("a.txt"), &dir.join("b.txt")).unwrap();
    assert_ne!(time(), written);
    let page = mem.changes(Some(start), &no).unwrap();
    let seen: Vec<(String, crate::ChangeKind)> = (page.changes.iter())
        .map(|c| (c.path.path.clone(), c.kind))
        .collect();
    assert_eq!(
        seen,
        [
            ("/d/a.txt".to_owned(), Created),
            ("/d/a.txt".to_owned(), Removed),
            ("/d/b.txt".to_owned(), Created),
        ]
    );
}
