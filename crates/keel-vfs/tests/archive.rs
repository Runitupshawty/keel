#![cfg(all(feature = "zip", feature = "sevenz", feature = "tar"))]
use keel_vfs::{archive::cache::MaterialiseCache, ops::extract, Conflict, Kind, Router, VPath};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};

/// A router whose materialise cache lives in the test's temp dir, not the user's cache.
fn router(tmp: &tempfile::TempDir) -> Router {
    Router::with_archive_cache(Arc::new(MaterialiseCache::new(
        tmp.path().join("cache"),
        2 << 30,
    )))
}

fn extract_all(archive: &VPath, dst: &Path, router: &Router) -> anyhow::Result<()> {
    extract(
        archive,
        &[],
        dst,
        Conflict::Overwrite,
        &|_| {},
        &AtomicBool::new(false),
        router,
    )
}

fn zip_file(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn zip_magic_lists_metadata_and_streams_one_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("misnamed.bin");
    zip_file(&file, &[("dir/a.txt", b"hello"), ("b.txt", b"world")]);
    let mut reader = keel_vfs::archive::open_archive(&file).unwrap();
    let entries = reader.entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].inner, "dir/a.txt");
    assert_eq!(entries[0].size, 5);
    assert!(!entries[0].encrypted);
    let mut body = String::new();
    reader
        .read("dir/a.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "hello");
    assert!(reader.read("missing").is_err());
}

#[test]
fn zip_list_does_not_decode_corrupt_bodies() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("bad.zip");
    zip_file(&file, &[("a.txt", b"body")]);
    let mut bytes = fs::read(&file).unwrap();
    let offset = 30 + "a.txt".len();
    bytes[offset] ^= 0xff;
    fs::write(&file, bytes).unwrap();
    assert_eq!(
        keel_vfs::archive::open_archive(&file)
            .unwrap()
            .entries()
            .unwrap()
            .len(),
        1
    );
}

fn tar_bytes(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(123);
    header.set_cksum();
    archive.append_data(&mut header, name, bytes).unwrap();
    archive.into_inner().unwrap()
}

#[test]
fn tar_family_dispatches_by_magic_and_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = tar_bytes("sub/a.txt", b"tar hello");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), Default::default());
    gzip.write_all(&bytes).unwrap();
    let mut bz = bzip2::write::BzEncoder::new(Vec::new(), Default::default());
    bz.write_all(&bytes).unwrap();
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut &bytes[..], &mut xz).unwrap();
    for data in [
        bytes.clone(),
        gzip.finish().unwrap(),
        bz.finish().unwrap(),
        xz,
        ruzstd::encoding::compress_to_vec(&bytes[..], ruzstd::encoding::CompressionLevel::Fastest),
    ] {
        let path = tmp.path().join("unknown.bin");
        fs::write(&path, data).unwrap();
        let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
        let entries = archive.entries().unwrap();
        assert_eq!(entries[0].inner, "sub/a.txt");
        assert_eq!(entries[0].size, 9);
        let mut body = String::new();
        archive
            .read("sub/a.txt")
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "tar hello");
    }
}

#[test]
fn sevenz_metadata_and_single_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("a.txt"), b"seven hello").unwrap();
    let path = tmp.path().join("seven.bin");
    sevenz_rust::compress_to_path(&src, &path).unwrap();
    let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
    let entries = archive.entries().unwrap();
    let entry = entries.iter().find(|e| e.inner == "a.txt").unwrap();
    assert_eq!(entry.size, 11);
    let mut body = String::new();
    archive
        .read("a.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "seven hello");
}

#[cfg(feature = "rar")]
#[test]
fn rar_fixture_lists_reads_and_extracts() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.rar");
    assert!(fs::metadata(&path).unwrap().len() < 4096);
    let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
    assert_eq!(
        archive
            .entries()
            .unwrap()
            .iter()
            .filter(|e| !e.is_dir)
            .count(),
        2
    );
    let mut body = String::new();
    archive
        .read("a.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "rar hello\n");
    let names: Vec<_> = archive
        .entries()
        .unwrap()
        .into_iter()
        .map(|e| e.inner)
        .collect();
    assert_eq!(names, ["a.txt", "b.txt"]);
    // Header times come through (2001-09-09 onwards rules out a misread layout).
    assert!(archive
        .entries()
        .unwrap()
        .iter()
        .all(|e| e.modified.is_some_and(
            |t| t > std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000)
        )));
    let tmp = tempfile::tempdir().unwrap();
    extract_all(&VPath::local(&path), tmp.path(), &router(&tmp)).unwrap();
    assert_eq!(fs::read(tmp.path().join("a.txt")).unwrap(), b"rar hello\n");
    assert_eq!(fs::read(tmp.path().join("b.txt")).unwrap().len(), 11);
}

#[test]
fn router_browses_and_materialises_zip_inside_tar_inside_zip() {
    let tmp = tempfile::tempdir().unwrap();
    let inner = tmp.path().join("inner.zip");
    zip_file(&inner, &[("dir/a.txt", b"nested hello")]);
    let tar = tar_bytes("inner.zip", &fs::read(inner).unwrap());
    let outer = tmp.path().join("outer.zip");
    zip_file(&outer, &[("middle.tar", &tar)]);
    let router = router(&tmp);
    let root = VPath::join_archive(&VPath::local(&outer), "middle.tar!/inner.zip!/");
    let provider = router.provider_for(&root).unwrap();
    assert_eq!(provider.scheme(), "archive");
    let list = provider.list(&root).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "dir");
    assert_eq!(list[0].kind, keel_vfs::Kind::Dir);
    let path = root.join("dir/a.txt");
    assert_eq!(provider.stat(&path).unwrap().size, 12);
    let mut body = String::new();
    provider
        .read(&path)
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "nested hello");
    let local = provider.local_copy(&path).unwrap();
    assert_eq!(fs::read(&local).unwrap(), b"nested hello");
    assert_eq!(provider.local_copy(&path).unwrap(), local);
    assert!(!provider.caps().write);
    assert!(provider.write(&path).is_err());
    assert!(provider.remove(&path).is_err());
}

#[test]
fn cache_evicts_by_bytes_and_recency_and_cleans_failures() {
    use keel_vfs::archive::cache::{CacheKey, MaterialiseCache};
    let tmp = tempfile::tempdir().unwrap();
    let cache = MaterialiseCache::new(tmp.path().into(), 8);
    let key = |inner: &str| CacheKey {
        outer: VPath::local("outer.zip"),
        modified: None,
        size: 100,
        inner: inner.into(),
    };
    // Pins are dropped at once, so everything here is evictable.
    let first = cache
        .get_or_extract(&key("a"), |p| {
            fs::write(p, b"1234")?;
            Ok(())
        })
        .unwrap()
        .path()
        .to_path_buf();
    let second = cache
        .get_or_extract(&key("b"), |p| {
            fs::write(p, b"5678")?;
            Ok(())
        })
        .unwrap()
        .path()
        .to_path_buf();
    assert_eq!(
        cache
            .get_or_extract(&key("a"), |_| panic!("cache miss"))
            .unwrap()
            .path(),
        first
    );
    cache
        .get_or_extract(&key("c"), |p| {
            fs::write(p, b"9012")?;
            Ok(())
        })
        .unwrap();
    assert!(first.exists());
    assert!(!second.exists());
    assert!(cache
        .get_or_extract(&key("big"), |p| {
            fs::write(p, b"123456789")?;
            Ok(())
        })
        .is_err());
    assert!(cache
        .get_or_extract(&key("bad"), |p| {
            fs::write(p, b"x")?;
            anyhow::bail!("failed")
        })
        .is_err());
    cache.evict_to_budget();
    assert!(
        fs::read_dir(tmp.path())
            .unwrap()
            .map(|p| p.unwrap().metadata().unwrap().len())
            .sum::<u64>()
            <= 8
    );
}

#[test]
fn too_large_preview_is_refused_before_reading_body() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("huge.zip");
    zip_file(&file, &[("big", b"x")]);
    let mut bytes = fs::read(&file).unwrap();
    let pos = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    bytes[pos + 24..pos + 28].copy_from_slice(&((1u32 << 30) + 1).to_le_bytes());
    fs::write(&file, bytes).unwrap();
    let path = VPath::join_archive(&VPath::local(file), "big");
    let router = router(&tmp);
    assert!(router
        .provider_for(&path)
        .unwrap()
        .local_copy(&path)
        .unwrap_err()
        .to_string()
        .contains("TooLarge"));
}

#[test]
fn extract_rejects_slip_before_writing_and_handles_conflicts_and_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("source.zip");
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    for bad in [
        "../evil.txt",
        "../escape",
        "/abs/evil.txt",
        "/absolute",
        "D:/drive",
        "dir/../../escape",
        "..\\escape",
        "dir/file:stream",
        "dir/con .txt",
        "COM\u{b9}.log",
        "tab\there",
        "what?",
    ] {
        zip_file(&file, &[("good", b"good"), (bad, b"bad")]);
        assert!(extract(
            &VPath::local(&file),
            &[],
            &dst,
            Conflict::Overwrite,
            &|_| {},
            &AtomicBool::new(false),
            &router(&tmp)
        )
        .is_err());
        assert!(!dst.join("good").exists());
        assert!(!tmp.path().join("escape").exists());
    }
    zip_file(&file, &[("dir/a.txt", b"hello"), ("b.txt", b"world")]);
    fs::create_dir(dst.join("dir")).unwrap();
    fs::write(dst.join("dir/a.txt"), b"original").unwrap();
    let path = VPath::local(&file);
    let router = router(&tmp);
    let cancel = AtomicBool::new(false);
    extract(
        &path,
        &["dir".into()],
        &dst,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(fs::read(dst.join("dir/a.txt")).unwrap(), b"original");
    assert!(!dst.join("b.txt").exists());
    extract(
        &path,
        &[],
        &dst,
        Conflict::RenameNew,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(fs::read(dst.join("dir/a (2).txt")).unwrap(), b"hello");
    let progress = std::cell::RefCell::new(Vec::new());
    extract(
        &path,
        &[],
        &dst,
        Conflict::Overwrite,
        &|p| progress.borrow_mut().push(p),
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(fs::read(dst.join("dir/a.txt")).unwrap(), b"hello");
    let progress = progress.borrow();
    let last = progress.last().unwrap();
    assert_eq!(last.done_bytes, 10);
    assert_eq!(last.done_items, last.total_items);
}

#[test]
fn extract_cancel_preserves_existing_file() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("source.zip");
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    zip_file(&file, &[("a", &vec![42; 2 << 20])]);
    fs::write(dst.join("a"), b"keep").unwrap();
    let cancel = AtomicBool::new(false);
    assert!(extract(
        &VPath::local(file),
        &[],
        &dst,
        Conflict::Overwrite,
        &|p| {
            if p.done_bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
        &router(&tmp)
    )
    .is_err());
    assert_eq!(fs::read(dst.join("a")).unwrap(), b"keep");
    assert_eq!(fs::read_dir(dst).unwrap().count(), 1);
}

#[test]
fn add_to_zip_replaces_entries_preserves_others_and_is_atomic_on_cancel() {
    use keel_vfs::ops::add_to_zip;
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("output.zip");
    let src = tmp.path().join("a.txt");
    fs::write(&src, b"first").unwrap();
    let cancel = AtomicBool::new(false);
    add_to_zip(&file, std::slice::from_ref(&src), "dir", &|_| {}, &cancel).unwrap();
    let b = tmp.path().join("b.txt");
    fs::write(&b, b"second").unwrap();
    add_to_zip(&file, &[b], "", &|_| {}, &cancel).unwrap();
    fs::write(&src, b"updated").unwrap();
    add_to_zip(&file, std::slice::from_ref(&src), "dir", &|_| {}, &cancel).unwrap();
    let mut reader = keel_vfs::archive::open_archive(&file).unwrap();
    assert_eq!(reader.entries().unwrap().len(), 2);
    let mut body = String::new();
    reader
        .read("dir/a.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "updated");
    drop(reader);
    let original = fs::read(&file).unwrap();
    fs::write(&src, vec![42; 2 << 20]).unwrap();
    assert!(add_to_zip(
        &file,
        &[src],
        "dir",
        &|p| {
            if p.done_bytes > 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel
    )
    .is_err());
    assert_eq!(fs::read(&file).unwrap(), original);
}

#[test]
#[ignore = "10,000-entry ZIP listing timing; run in release mode"]
fn perf_zip_list_10k_under_200ms() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("many.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&file).unwrap());
    for i in 0..10000 {
        zip.start_file(
            format!("file-{i}.txt"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    }
    zip.finish().unwrap();
    let root = VPath::join_archive(&VPath::local(file), "");
    let router = router(&tmp);
    let provider = router.provider_for(&root).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(provider.list(&root).unwrap().len(), 10000);
    eprintln!("10,000-entry zip listed in {:?}", start.elapsed());
    assert!(
        start.elapsed() < std::time::Duration::from_millis(200),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn archive_paths_split_at_last_boundary_and_keep_provider() {
    let outer = VPath::parse("sftp://host/backups/x.zip").unwrap();
    let nested = VPath::join_archive(&outer, "dir/y.tar!/a.txt");
    let (container, inner) = nested.split_archive().unwrap();
    assert_eq!(container.path, "/backups/x.zip!/dir/y.tar");
    assert_eq!(container.authority, "host");
    assert_eq!(inner, "a.txt");
    assert_eq!(VPath::join_archive(&outer, "").parent(), outer.parent());
    assert!(nested.to_local_path().is_none());
    let win = VPath::parse("file:///D:/x.zip!/dir/a.txt").unwrap();
    assert_eq!(win.split_archive().unwrap().1, "dir/a.txt");
    assert_eq!(win.parent().unwrap().name(), "dir");
    assert!(win.to_local_path().is_none());
    for name in [
        "a.ZIP",
        "a.jar",
        "a.7z",
        "a.tar",
        "a.tgz",
        "a.tar.gz",
        "a.tar.bz2",
        "a.tar.xz",
        "a.tar.zst",
        "a.rar",
    ] {
        assert!(VPath::is_archive_name(name), "{name}");
    }
    assert!(!VPath::is_archive_name("a.txt"));
}

#[test]
fn bang_folder_is_not_an_archive_and_archive_root_names_itself() {
    assert!(VPath::parse("sftp://h/Yahoo!/a.txt")
        .unwrap()
        .split_archive()
        .is_none());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Yahoo!");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"x").unwrap();
    let local = VPath::local(dir.join("a.txt"));
    assert!(local.split_archive().is_none());
    let router = router(&tmp);
    let provider = router.provider_for(&local).unwrap();
    assert_eq!(provider.scheme(), "file");
    assert_eq!(provider.stat(&local).unwrap().size, 1);
    let root = VPath::join_archive(&VPath::local(tmp.path().join("x.zip")), "");
    assert_eq!(root.name(), "x.zip");
    assert_eq!(VPath::local(root.display()), root);
    let inner = root.join("dir/a.txt");
    assert_eq!(VPath::local(inner.display()), inner);
    assert_eq!(inner.parent().unwrap().parent().unwrap(), root);
}

fn tar_entry(builder: &mut tar::Builder<Vec<u8>>, path: &str, kind: tar::EntryType, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(bytes.len() as u64);
    header.set_mode(0o755);
    if kind == tar::EntryType::Symlink {
        header.set_link_name("../../outside").unwrap();
    }
    header.set_path(path).unwrap();
    header.set_cksum();
    builder.append(&header, bytes).unwrap();
}

#[test]
fn dot_slash_tarball_lists_and_extracts_in_one_pass_skipping_links() {
    use tar::EntryType::{Directory, Regular, Symlink};
    let tmp = tempfile::tempdir().unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    tar_entry(&mut builder, "./", Directory, b"");
    tar_entry(&mut builder, "./pkg/", Directory, b"");
    tar_entry(&mut builder, "./pkg/a.txt", Regular, b"alpha");
    tar_entry(&mut builder, "./pkg/link", Symlink, b"");
    tar_entry(&mut builder, "./pkg/sub/b.txt", Regular, b"beta");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), Default::default());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    let file = tmp.path().join("pkg.tar.gz");
    fs::write(&file, gz.finish().unwrap()).unwrap();
    let router = router(&tmp);
    let root = VPath::join_archive(&VPath::local(&file), "");
    let provider = router.provider_for(&root).unwrap();
    let top = provider.list(&root).unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!((top[0].name.as_str(), &top[0].kind), ("pkg", &Kind::Dir));
    let names: Vec<_> = provider
        .list(&root.join("pkg"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a.txt", "sub"]);
    assert_eq!(
        provider.stat(&root.join("pkg/sub")).unwrap().kind,
        Kind::Dir
    );
    let mut body = String::new();
    provider
        .read(&root.join("pkg/sub/b.txt"))
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "beta");
    let dst = tmp.path().join("out");
    fs::create_dir(&dst).unwrap();
    extract_all(&root, &dst, &router).unwrap();
    assert_eq!(fs::read(dst.join("pkg/a.txt")).unwrap(), b"alpha");
    assert_eq!(fs::read(dst.join("pkg/sub/b.txt")).unwrap(), b"beta");
    assert!(fs::symlink_metadata(dst.join("pkg/link")).is_err());
}

#[test]
fn corrupt_archives_are_errors_not_panics() {
    let tmp = tempfile::tempdir().unwrap();
    let router = router(&tmp);
    let zip = tmp.path().join("good.zip");
    zip_file(&zip, &[("a.txt", b"hello hello hello")]);
    let zip = fs::read(zip).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), Default::default());
    gz.write_all(&tar_bytes("a.txt", &[7; 4096])).unwrap();
    let gz = gz.finish().unwrap();
    let src = tmp.path().join("seven");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("a.txt"), [9; 4096]).unwrap();
    let seven = tmp.path().join("good.7z");
    sevenz_rust::compress_to_path(&src, &seven).unwrap();
    let seven = fs::read(seven).unwrap();
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty.zip", Vec::new()),
        ("garbage.zip", b"PK\x03\x04 not really a zip".to_vec()),
        ("cut.zip", zip[..zip.len() - 10].to_vec()),
        ("garbage.7z", b"7z\xbc\xaf\x27\x1c garbage".to_vec()),
        ("cut.7z", seven[..seven.len() / 2].to_vec()),
        ("cut.tar.gz", gz[..gz.len() / 2].to_vec()),
        ("garbage.tar", vec![b'x'; 600]),
        ("garbage.bin", vec![0; 600]),
    ];
    if cfg!(feature = "rar") {
        cases.push(("garbage.rar", b"Rar!\x1a\x07\x01\x00 garbage".to_vec()));
    }
    for (name, bytes) in cases {
        let file = tmp.path().join(name);
        fs::write(&file, bytes).unwrap();
        let dst = tmp.path().join(format!("out-{name}"));
        fs::create_dir(&dst).unwrap();
        let root = VPath::join_archive(&VPath::local(&file), "");
        let listed = router.provider_for(&root).unwrap().list(&root);
        // libunrar reports a garbage body after a valid signature as an empty archive.
        if name.ends_with(".rar") {
            assert!(listed.is_err() || listed.unwrap().is_empty(), "{name}");
        } else {
            assert!(listed.is_err(), "{name} listed {listed:?}");
        }
        let extracted = extract_all(&root, &dst, &router);
        assert!(extracted.is_err() || name.ends_with(".rar"), "{name}");
        assert_eq!(fs::read_dir(&dst).unwrap().count(), 0, "{name}");
    }
}

#[test]
fn encrypted_zip_entries_are_flagged_and_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("locked.zip");
    zip_file(&file, &[("secret.txt", b"hidden"), ("plain.txt", b"open")]);
    let mut bytes = fs::read(&file).unwrap();
    bytes[6] |= 1; // first local header: general purpose bit 0 = encrypted
    let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    bytes[central + 8] |= 1;
    fs::write(&file, bytes).unwrap();
    let entries = keel_vfs::archive::open_archive(&file)
        .unwrap()
        .entries()
        .unwrap();
    assert!(entries[0].encrypted && !entries[1].encrypted);
    let router = router(&tmp);
    let root = VPath::join_archive(&VPath::local(&file), "");
    let provider = router.provider_for(&root).unwrap();
    assert_eq!(provider.list(&root).unwrap().len(), 2);
    assert!(provider.read(&root.join("secret.txt")).is_err());
    assert!(provider.local_copy(&root.join("secret.txt")).is_err());
    let mut body = String::new();
    provider
        .read(&root.join("plain.txt"))
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "open");
    let dst = tmp.path().join("out");
    fs::create_dir(&dst).unwrap();
    assert!(extract_all(&root, &dst, &router).is_err());
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 0);
    extract(
        &root,
        &["plain.txt".into()],
        &dst,
        Conflict::Skip,
        &|_| {},
        &AtomicBool::new(false),
        &router,
    )
    .unwrap();
    assert_eq!(fs::read(dst.join("plain.txt")).unwrap(), b"open");
}

#[test]
fn extracts_7z_folders_and_archives_nested_in_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let router = router(&tmp);
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), b"seven a").unwrap();
    fs::write(src.join("sub/b.txt"), b"seven b").unwrap();
    fs::write(src.join("empty.txt"), b"").unwrap();
    let seven = tmp.path().join("s.7z");
    sevenz_rust::compress_to_path(&src, &seven).unwrap();
    let dst = tmp.path().join("out7");
    fs::create_dir(&dst).unwrap();
    extract_all(&VPath::local(&seven), &dst, &router).unwrap();
    assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"seven a");
    assert_eq!(fs::read(dst.join("sub/b.txt")).unwrap(), b"seven b");
    assert_eq!(fs::read(dst.join("empty.txt")).unwrap(), b"");

    let inner = tmp.path().join("inner.zip");
    zip_file(&inner, &[("dir/a.txt", b"nested hello")]);
    let outer = tmp.path().join("outer.zip");
    zip_file(&outer, &[("x/inner.zip", &fs::read(inner).unwrap())]);
    let nested = VPath::join_archive(&VPath::local(&outer), "x/inner.zip");
    let dst = tmp.path().join("out-nested");
    fs::create_dir(&dst).unwrap();
    extract_all(&nested, &dst, &router).unwrap();
    assert_eq!(fs::read(dst.join("dir/a.txt")).unwrap(), b"nested hello");
}

/// A symlink (Unix) or junction (Windows) at `link` pointing to `target`.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    assert!(std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn extract_never_follows_a_link_inside_the_destination_but_accepts_a_linked_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let router = router(&tmp);
    let file = tmp.path().join("a.zip");
    zip_file(&file, &[("dir/x.txt", b"payload")]);
    let outside = tmp.path().join("outside");
    let dst = tmp.path().join("dst");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&dst).unwrap();
    link_dir(&outside, &dst.join("dir"));
    assert!(extract_all(&VPath::local(&file), &dst, &router).is_err());
    assert!(!outside.join("x.txt").exists());
    // The destination itself may be reached through a link (e.g. a junctioned folder).
    let linked = tmp.path().join("linked");
    link_dir(&outside, &linked);
    extract_all(&VPath::local(&file), &linked, &router).unwrap();
    assert_eq!(fs::read(outside.join("dir/x.txt")).unwrap(), b"payload");
}

#[test]
fn add_to_zip_creates_and_recurses_into_folders() {
    use keel_vfs::ops::add_to_zip;
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("folder");
    fs::create_dir_all(folder.join("deep")).unwrap();
    fs::write(folder.join("deep/x.txt"), b"x").unwrap();
    let file = tmp.path().join("new.zip");
    add_to_zip(&file, &[folder], "", &|_| {}, &AtomicBool::new(false)).unwrap();
    let mut names: Vec<_> = keel_vfs::archive::open_archive(&file)
        .unwrap()
        .entries()
        .unwrap()
        .into_iter()
        .map(|e| (e.inner, e.modified.is_some()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("folder/".to_owned(), true),
            ("folder/deep/".to_owned(), true),
            ("folder/deep/x.txt".to_owned(), true)
        ]
    );
}

#[test]
fn extract_under_strips_the_base_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("source.zip");
    zip_file(
        &file,
        &[
            ("top.txt", b"top"),
            ("dir/a.txt", b"a"),
            ("dir/sub/b.txt", b"b"),
        ],
    );
    let (router, cancel) = (router(&tmp), AtomicBool::new(false));
    let run = |entries: &[String], dst: &Path| {
        fs::create_dir_all(dst).unwrap();
        keel_vfs::extract_under(
            &VPath::local(&file),
            "dir",
            entries,
            dst,
            Conflict::Skip,
            &|_| {},
            &cancel,
            &router,
        )
    };
    let all = tmp.path().join("all");
    run(&[], &all).unwrap();
    assert_eq!(fs::read(all.join("a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(all.join("sub/b.txt")).unwrap(), b"b");
    assert!(!all.join("top.txt").exists() && !all.join("dir").exists());
    let picked = tmp.path().join("picked");
    run(&["dir/sub".into()], &picked).unwrap();
    assert_eq!(fs::read(picked.join("sub/b.txt")).unwrap(), b"b");
    assert!(!picked.join("a.txt").exists());
    assert!(
        run(&["top.txt".into()], &picked).is_err(),
        "outside the base"
    );
}

#[cfg(feature = "rar")]
#[test]
fn rar_file_reference_entries_are_never_resolved() {
    // reference.rar (WinRAR `a -oi:1 -ep`): `Cargo.toml` ("fixture body\n"), then `evil.txt`,
    // a file reference to `Cargo.toml`. unrar resolved such references against the process
    // CWD (here the crate folder, whose real Cargo.toml differs) and copied that file out.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reference.rar");
    assert!(fs::metadata(&path).unwrap().len() < 4096);
    let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
    let names: Vec<_> = archive
        .entries()
        .unwrap()
        .into_iter()
        .map(|e| e.inner)
        .collect();
    assert_eq!(names, ["Cargo.toml"]);
    assert!(archive.read("evil.txt").is_err());
    let mut body = String::new();
    archive
        .read("Cargo.toml")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "fixture body\n");
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    extract_all(&VPath::local(&path), &dst, &router(&tmp)).unwrap();
    assert_eq!(fs::read(dst.join("Cargo.toml")).unwrap(), b"fixture body\n");
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 1);
    let root = VPath::join_archive(&VPath::local(&path), "");
    let router = router(&tmp);
    let listed = router.provider_for(&root).unwrap().list(&root).unwrap();
    assert_eq!(listed.len(), 1);
}

/// 7-Zip's default: every file in one solid block.
fn solid_7z(tmp: &Path) -> std::path::PathBuf {
    let src = tmp.join("solid-src");
    fs::create_dir(&src).unwrap();
    for (name, n) in [("f1.txt", 1), ("f2.txt", 2), ("f3.txt", 3)] {
        fs::write(src.join(name), format!("file {n} ").repeat(1000)).unwrap();
    }
    let path = tmp.join("solid.7z");
    let mut writer = sevenz_rust::SevenZWriter::create(&path).unwrap();
    writer.push_source_path(&src, |_| true).unwrap();
    writer.finish().unwrap();
    path
}

#[test]
fn solid_7z_reads_and_extracts_any_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let path = solid_7z(tmp.path());
    let mut archive = keel_vfs::archive::open_archive(&path).unwrap();
    for (name, n) in [("f2.txt", 2), ("f3.txt", 3)] {
        let mut body = String::new();
        archive
            .read(name)
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, format!("file {n} ").repeat(1000));
    }
    let router = router(&tmp);
    let dst = tmp.path().join("only-f2");
    fs::create_dir(&dst).unwrap();
    let cancel = AtomicBool::new(false);
    let path = VPath::local(&path);
    extract(
        &path,
        &["f2.txt".into()],
        &dst,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(
        fs::read(dst.join("f2.txt")).unwrap(),
        "file 2 ".repeat(1000).as_bytes()
    );
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 1);
    // A conflict-skipped entry must still be decoded past.
    let dst = tmp.path().join("skip-f1");
    fs::create_dir(&dst).unwrap();
    fs::write(dst.join("f1.txt"), b"keep").unwrap();
    extract(&path, &[], &dst, Conflict::Skip, &|_| {}, &cancel, &router).unwrap();
    assert_eq!(fs::read(dst.join("f1.txt")).unwrap(), b"keep");
    assert_eq!(
        fs::read(dst.join("f3.txt")).unwrap(),
        "file 3 ".repeat(1000).as_bytes()
    );
}

#[test]
#[ignore = "3,000-entry ZIP extraction timing; run in release mode"]
fn perf_zip_extract_3k_under_2s() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("many.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&file).unwrap());
    for i in 0..3000 {
        zip.start_file(
            format!("file-{i}.txt"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(format!("body {i}").as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    let start = std::time::Instant::now();
    extract_all(&VPath::local(&file), &dst, &router(&tmp)).unwrap();
    eprintln!("3,000-entry zip extracted in {:?}", start.elapsed());
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 3000);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

/// Rewrites the uncompressed size in both the local and the central header of the only entry.
fn patch_zip_size(file: &Path, size: u32) {
    let mut bytes = fs::read(file).unwrap();
    bytes[22..26].copy_from_slice(&size.to_le_bytes());
    let pos = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    bytes[pos + 24..pos + 28].copy_from_slice(&size.to_le_bytes());
    fs::write(file, bytes).unwrap();
}

#[test]
fn entry_longer_than_its_header_is_an_error_everywhere() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("liar.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&file).unwrap());
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("a.txt", stored).unwrap();
    zip.write_all(&[b'x'; 2000]).unwrap();
    zip.finish().unwrap();
    patch_zip_size(&file, 1000);
    let mut body = Vec::new();
    assert!(keel_vfs::archive::open_archive(&file)
        .unwrap()
        .read("a.txt")
        .unwrap()
        .read_to_end(&mut body)
        .is_err());
    let router = router(&tmp);
    let path = VPath::join_archive(&VPath::local(&file), "a.txt");
    assert!(router
        .provider_for(&path)
        .unwrap()
        .local_copy(&path)
        .is_err());
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    assert!(extract_all(&VPath::local(&file), &dst, &router).is_err());
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 0);
}

#[test]
fn extract_checks_free_space_before_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_path("huge.bin").unwrap();
    header.set_size(1 << 60);
    header.set_mode(0o644);
    header.set_cksum();
    let mut bytes = tar_bytes("small.txt", b"small");
    bytes.truncate(1024); // drop the end-of-archive blocks
    bytes.extend_from_slice(header.as_bytes());
    let file = tmp.path().join("huge.tar");
    fs::write(&file, bytes).unwrap();
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    let error = extract_all(&VPath::local(&file), &dst, &router(&tmp)).unwrap_err();
    assert!(format!("{error:#}").contains("free space"), "{error:#}");
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 0);
}

#[test]
fn truncated_tar_entry_is_an_error_and_never_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bytes = tar_bytes("big.bin", &[7; 10000]);
    bytes.truncate(512 + 5000);
    let file = tmp.path().join("cut.tar");
    fs::write(&file, bytes).unwrap();
    let mut body = Vec::new();
    assert!(keel_vfs::archive::open_archive(&file)
        .unwrap()
        .read("big.bin")
        .unwrap()
        .read_to_end(&mut body)
        .is_err());
    let router = router(&tmp);
    let path = VPath::join_archive(&VPath::local(&file), "big.bin");
    assert!(router
        .provider_for(&path)
        .unwrap()
        .local_copy(&path)
        .is_err());
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    assert!(extract_all(&VPath::local(&file), &dst, &router).is_err());
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 0);
}

#[test]
fn transfer_copies_out_of_an_archive_and_refuses_to_move_out_of_one() {
    use keel_vfs::ops::transfer;
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("source.zip");
    zip_file(&file, &[("dir/a.txt", b"hello")]);
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    let router = router(&tmp);
    let cancel = AtomicBool::new(false);
    let source = VPath::join_archive(&VPath::local(&file), "dir/a.txt");
    let error = transfer(
        std::slice::from_ref(&source),
        &VPath::local(&dst),
        true,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap_err();
    assert!(error.to_string().contains("read-only"), "{error:#}");
    assert_eq!(fs::read_dir(&dst).unwrap().count(), 0);
    for source in [source, VPath::join_archive(&VPath::local(&file), "dir")] {
        transfer(
            &[source],
            &VPath::local(&dst),
            false,
            Conflict::RenameNew,
            &|_| {},
            &cancel,
            &router,
        )
        .unwrap();
    }
    assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
    assert_eq!(fs::read(dst.join("dir/a.txt")).unwrap(), b"hello");
    assert_eq!(
        fs::read_dir(&dst).unwrap().count(),
        2,
        "no staging files left"
    );
}

/// Serves `sftp://remote/<name>` from a local folder and only through
/// `local_copy_cancellable`, recording the call.
struct Remote {
    root: std::path::PathBuf,
    downloads: std::sync::Mutex<usize>,
}
impl keel_vfs::Provider for Remote {
    fn scheme(&self) -> &'static str {
        "sftp"
    }
    fn caps(&self) -> keel_vfs::Caps {
        keel_vfs::Caps::default()
    }
    fn list(&self, _: &VPath) -> anyhow::Result<Vec<keel_vfs::Entry>> {
        unimplemented!()
    }
    fn stat(&self, p: &VPath) -> anyhow::Result<keel_vfs::Entry> {
        let local = VPath::local(self.root.join(p.name()));
        Ok(keel_vfs::Entry {
            path: p.clone(),
            ..keel_vfs::LocalProvider.stat(&local)?
        })
    }
    fn read(&self, _: &VPath) -> anyhow::Result<Box<dyn Read + Send>> {
        unimplemented!()
    }
    fn write(&self, _: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
        unimplemented!()
    }
    fn mkdir(&self, _: &VPath) -> anyhow::Result<()> {
        unimplemented!()
    }
    fn rename(&self, _: &VPath, _: &VPath) -> anyhow::Result<()> {
        unimplemented!()
    }
    fn remove(&self, _: &VPath) -> anyhow::Result<()> {
        unimplemented!()
    }
    fn local_copy(&self, _: &VPath) -> anyhow::Result<std::path::PathBuf> {
        panic!("remote archives must be fetched with local_copy_cancellable")
    }
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(keel_vfs::Progress),
        cancel: &AtomicBool,
    ) -> anyhow::Result<std::path::PathBuf> {
        anyhow::ensure!(
            !cancel.load(std::sync::atomic::Ordering::Relaxed),
            "cancelled"
        );
        *self.downloads.lock().unwrap() += 1;
        progress(keel_vfs::Progress {
            done_bytes: 1,
            total_bytes: 1,
            current: "download".into(),
            done_items: 0,
            total_items: 1,
        });
        Ok(self.root.join(p.name()))
    }
}

#[test]
fn remote_archives_are_fetched_cancellably() {
    let tmp = tempfile::tempdir().unwrap();
    zip_file(&tmp.path().join("r.zip"), &[("a.txt", b"remote")]);
    let remote = Arc::new(Remote {
        root: tmp.path().into(),
        downloads: Default::default(),
    });
    let mut router = router(&tmp);
    router.register_remote_provider("remote".into(), remote.clone());
    let archive = VPath::parse("sftp://remote/r.zip").unwrap();
    let root = VPath::join_archive(&archive, "");
    assert_eq!(
        router
            .provider_for(&root)
            .unwrap()
            .list(&root)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(*remote.downloads.lock().unwrap(), 1);
    let dst = tmp.path().join("dst");
    fs::create_dir(&dst).unwrap();
    let seen = std::cell::Cell::new(false);
    extract(
        &archive,
        &[],
        &dst,
        Conflict::Skip,
        &|p| seen.set(seen.get() || p.current == "download"),
        &AtomicBool::new(false),
        &router,
    )
    .unwrap();
    assert!(seen.get(), "extract forwards the download's progress");
    assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"remote");
}

#[test]
fn cache_keeps_one_file_per_key_under_races_and_never_evicts_pinned_files() {
    use keel_vfs::archive::cache::{CacheKey, MaterialiseCache};
    let tmp = tempfile::tempdir().unwrap();
    let cache = MaterialiseCache::new(tmp.path().into(), 8);
    let key = |inner: &str| CacheKey {
        outer: VPath::local("outer.zip"),
        modified: None,
        size: 100,
        inner: inner.into(),
    };
    let barrier = std::sync::Barrier::new(2);
    let pins: Vec<_> = std::thread::scope(|s| {
        let racers: Vec<_> = (0..2)
            .map(|_| {
                s.spawn(|| {
                    cache
                        .get_or_extract(&key("a"), |p| {
                            fs::write(p, b"1234")?;
                            barrier.wait();
                            Ok(())
                        })
                        .unwrap()
                })
            })
            .collect();
        racers.into_iter().map(|r| r.join().unwrap()).collect()
    });
    assert_eq!(pins[0].path(), pins[1].path());
    assert!(pins[0].path().exists());
    // Both callers hold pins: going over budget must not delete the file under them.
    for (name, body) in [("b", b"5678"), ("c", b"9012")] {
        cache
            .get_or_extract(&key(name), |p| {
                fs::write(p, body)?;
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(fs::read(pins[0].path()).unwrap(), b"1234");
    let a = pins[0].path().to_path_buf();
    drop(pins);
    // Released, it is the least recently used entry again.
    cache
        .get_or_extract(&key("d"), |p| {
            fs::write(p, b"3456")?;
            Ok(())
        })
        .unwrap();
    assert!(!a.exists());
}

#[test]
fn add_to_zip_keeps_the_comment_and_mode_and_writes_local_time() {
    use chrono::{Datelike, Timelike};
    use keel_vfs::ops::add_to_zip;
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("commented.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&file).unwrap());
    zip.set_comment("keep me");
    zip.start_file("old.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.finish().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let src = tmp.path().join("new.txt");
    fs::write(&src, b"new").unwrap();
    let cancel = AtomicBool::new(false);
    add_to_zip(&file, std::slice::from_ref(&src), "", &|_| {}, &cancel).unwrap();
    let mut archive = zip::ZipArchive::new(fs::File::open(&file).unwrap()).unwrap();
    assert_eq!(archive.comment(), b"keep me");
    let stamp = archive.by_name("new.txt").unwrap().last_modified().unwrap();
    let local: chrono::DateTime<chrono::Local> =
        fs::metadata(&src).unwrap().modified().unwrap().into();
    assert_eq!(
        (
            stamp.year(),
            stamp.month(),
            stamp.day(),
            stamp.hour(),
            stamp.minute()
        ),
        (
            local.year() as u16,
            local.month() as u8,
            local.day() as u8,
            local.hour() as u8,
            local.minute() as u8
        )
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }
}
