#![cfg(feature = "zip")]
//! Editing entries inside zips: delete, rename, move, copy, new folders and pasted files,
//! each one rewrite of the archive with the entries that stay copied byte for byte.
use keel_vfs::{
    archive::{cache::MaterialiseCache, zipedit},
    ops::{extract, transfer},
    Conflict, Router, VPath,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};
use zip::write::SimpleFileOptions;

fn router(tmp: &Path) -> Router {
    Router::with_archive_cache(Arc::new(MaterialiseCache::new(tmp.join("cache"), 1 << 30)))
}

fn zip_file(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        if let Some(dir) = name.strip_suffix('/') {
            zip.add_directory(dir, SimpleFileOptions::default())
                .unwrap();
            continue;
        }
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.set_comment("kept comment");
    zip.finish().unwrap();
}

/// Every file in the archive as extracted (folders as `name/` with no body).
fn tree(zip: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(fs::File::open(zip).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let mut body = Vec::new();
        file.read_to_end(&mut body).unwrap();
        assert!(
            out.insert(file.name().to_owned(), body).is_none(),
            "duplicate entry {}",
            file.name()
        );
    }
    out
}

fn expect(entries: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    entries
        .iter()
        .map(|(n, b)| (n.to_string(), b.to_vec()))
        .collect()
}

fn at(zip: &Path, inner: &str) -> VPath {
    VPath::join_archive(&VPath::local(zip), inner)
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("keel-partial"))
        .collect()
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn delete_rename_move_copy_and_mkdir_through_the_provider_and_transfer() {
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("a.zip");
    zip_file(
        &zip,
        &[
            ("docs/", b""),
            ("docs/a.txt", b"alpha"),
            ("docs/b.txt", b"bravo"),
            ("deep/x/c.txt", b"charlie"),
            ("top.txt", b"top"),
        ],
    );
    let router = router(tmp.path());
    let p = router.provider_for(&at(&zip, "docs")).unwrap();
    // Delete one file: its (explicit) folder stays.
    p.remove(&at(&zip, "docs/a.txt")).unwrap();
    // Delete a folder that exists only through its entries; its parent stays.
    p.remove(&at(&zip, "deep/x")).unwrap();
    assert_eq!(
        tree(&zip),
        expect(&[
            ("docs/", b""),
            ("docs/b.txt", b"bravo"),
            ("top.txt", b"top"),
            ("deep/", b""),
        ])
    );
    // Rename a file and a folder; a clash is refused and changes nothing.
    p.rename(&at(&zip, "top.txt"), &at(&zip, "renamed.txt"))
        .unwrap();
    p.rename(&at(&zip, "docs"), &at(&zip, "papers")).unwrap();
    let before = fs::read(&zip).unwrap();
    let clash = p.rename(&at(&zip, "renamed.txt"), &at(&zip, "papers"));
    assert!(format!("{:#}", clash.unwrap_err()).contains("already exists"));
    assert_eq!(fs::read(&zip).unwrap(), before);
    // New folder, then a move and a copy inside the archive (raw, one rewrite each).
    p.mkdir(&at(&zip, "deep/new")).unwrap();
    assert!(p.mkdir(&at(&zip, "deep/new")).is_err(), "exists");
    let cancel = no_cancel();
    transfer(
        &[at(&zip, "renamed.txt")],
        &at(&zip, "deep/new"),
        true,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    transfer(
        &[at(&zip, "papers")],
        &at(&zip, "deep"),
        false,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(
        tree(&zip),
        expect(&[
            ("papers/", b""),
            ("papers/b.txt", b"bravo"),
            ("deep/", b""),
            ("deep/new/", b""),
            ("deep/new/renamed.txt", b"top"),
            ("deep/papers/", b""),
            ("deep/papers/b.txt", b"bravo"),
        ])
    );
    // The provider lists the new state and the comment survived every rewrite.
    let names: Vec<String> = p
        .list(&at(&zip, "deep"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["new", "papers"]);
    let archive = zip::ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
    assert_eq!(archive.comment(), b"kept comment");
    // It re-opens and extracts to the same tree.
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    extract(
        &VPath::local(&zip),
        &[],
        &out,
        Conflict::Overwrite,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap();
    assert_eq!(fs::read(out.join("deep/papers/b.txt")).unwrap(), b"bravo");
    assert!(out.join("deep/new").is_dir());
    assert!(leftovers(tmp.path()).is_empty());
}

#[test]
fn files_and_folders_are_pasted_into_a_folder_with_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("a.zip");
    zip_file(&zip, &[("in/old.txt", b"old"), ("in/keep.txt", b"keep")]);
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("folder/sub")).unwrap();
    fs::write(src.join("old.txt"), b"new body").unwrap();
    fs::write(src.join("folder/one.txt"), b"one").unwrap();
    fs::create_dir(src.join("folder/empty")).unwrap();
    fs::write(src.join("folder/sub/two.txt"), "zwei \u{e9}").unwrap();
    fs::write(src.join("caf\u{e9}.txt"), b"unicode name").unwrap();
    let router = router(tmp.path());
    let cancel = no_cancel();
    let paste = |names: &[&str], dst: &str, mv: bool, conflict| {
        let from: Vec<VPath> = names.iter().map(|n| VPath::local(src.join(n))).collect();
        let seen = std::cell::Cell::new((0u64, 0u64, 0usize));
        let result = transfer(
            &from,
            &at(&zip, dst),
            mv,
            conflict,
            &|p| seen.set((p.done_bytes, p.total_bytes, p.skipped)),
            &cancel,
            &router,
        );
        result.map(|()| seen.get())
    };
    // Skip keeps the archive's file, the rest lands; progress counts the whole rewrite.
    let (done, total, skipped) =
        paste(&["old.txt", "folder"], "in", false, Conflict::Skip).unwrap();
    assert_eq!((done, skipped), (total, 1));
    let t = tree(&zip);
    assert_eq!(t["in/old.txt"], b"old");
    assert_eq!(t["in/folder/one.txt"], b"one");
    assert_eq!(t["in/folder/sub/two.txt"], "zwei \u{e9}".as_bytes());
    assert!(t.contains_key("in/folder/empty/"));
    // Overwrite replaces it; Keep both adds "old (2).txt".
    paste(&["old.txt"], "in", false, Conflict::Overwrite).unwrap();
    assert_eq!(tree(&zip)["in/old.txt"], b"new body");
    paste(&["old.txt"], "in", false, Conflict::RenameNew).unwrap();
    assert_eq!(tree(&zip)["in/old (2).txt"], b"new body");
    // A folder pasted again merges: Skip leaves what is there.
    fs::write(src.join("folder/three.txt"), b"three").unwrap();
    let (_, _, skipped) = paste(&["folder"], "in", false, Conflict::Skip).unwrap();
    assert_eq!(skipped, 2, "one.txt and sub/two.txt are there already");
    assert_eq!(tree(&zip)["in/folder/three.txt"], b"three");
    // A move deletes the sources once the archive is in place; non-ASCII names are UTF-8.
    paste(&["caf\u{e9}.txt"], "", true, Conflict::Skip).unwrap();
    assert!(!src.join("caf\u{e9}.txt").exists());
    assert_eq!(tree(&zip)["caf\u{e9}.txt"], b"unicode name");
    assert_eq!(tree(&zip)["in/keep.txt"], b"keep");
    // The archive itself cannot go into itself.
    let err = transfer(
        &[VPath::local(&zip)],
        &at(&zip, "in"),
        false,
        Conflict::Skip,
        &|_| {},
        &cancel,
        &router,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("itself"), "{err:#}");
    assert!(leftovers(tmp.path()).is_empty());
}

#[test]
fn stored_and_encrypted_entries_are_copied_byte_for_byte() {
    use zip::unstable::write::FileOptionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("raw.zip");
    {
        let mut w = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("stored.bin", stored).unwrap();
        w.write_all(&[7u8; 5000]).unwrap();
        let secret = SimpleFileOptions::default().with_deprecated_encryption(b"pass");
        w.start_file("dir/secret.txt", secret).unwrap();
        w.write_all(b"the secret body").unwrap();
        w.start_file("gone.txt", SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"x").unwrap();
        w.finish().unwrap();
    }
    // The raw bytes of an entry's data, as stored in the archive.
    let raw = |zip: &Path, name: &str| {
        let bytes = fs::read(zip).unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(zip).unwrap()).unwrap();
        let i = archive.index_for_name(name).unwrap();
        let file = archive.by_index_raw(i).unwrap();
        let start = file.data_start() as usize;
        bytes[start..start + file.compressed_size() as usize].to_vec()
    };
    let (stored, secret) = (raw(&zip, "stored.bin"), raw(&zip, "dir/secret.txt"));
    let router = router(tmp.path());
    let p = router.provider_for(&at(&zip, "")).unwrap();
    p.remove(&at(&zip, "gone.txt")).unwrap();
    p.rename(&at(&zip, "dir"), &at(&zip, "moved")).unwrap();
    assert_eq!(raw(&zip, "stored.bin"), stored);
    assert_eq!(raw(&zip, "moved/secret.txt"), secret, "never decrypted");
    let mut archive = zip::ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
    assert_eq!(
        archive.by_name("stored.bin").unwrap().compression(),
        zip::CompressionMethod::Stored
    );
    let i = archive.index_for_name("moved/secret.txt").unwrap();
    assert!(archive.by_index_raw(i).unwrap().encrypted());
    let mut body = String::new();
    archive
        .by_name_decrypt("moved/secret.txt", b"pass")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "the secret body");
    // The listing still marks it.
    let listed = p.list(&at(&zip, "moved")).unwrap();
    assert!(listed[0].encrypted);
}

/// Over 65,535 entries: the archive needs zip64 end records, and keeps them.
#[test]
fn zip64_archives_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("many.zip");
    let count = 65_600;
    {
        let mut w = zip::ZipWriter::new(std::io::BufWriter::new(fs::File::create(&zip).unwrap()));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for i in 0..count {
            w.start_file(format!("d/{i}.txt"), opts).unwrap();
            w.write_all(i.to_string().as_bytes()).unwrap();
        }
        // An entry with zip64 sizes although it is small.
        w.start_file("wide.txt", opts.large_file(true)).unwrap();
        w.write_all(b"wide").unwrap();
        w.finish().unwrap();
    }
    let has_locator = |zip: &Path| {
        let bytes = fs::read(zip).unwrap();
        bytes
            .windows(4)
            .rev()
            .take(200)
            .any(|w| w == [0x50, 0x4b, 0x06, 0x07])
    };
    assert!(has_locator(&zip));
    let cancel = no_cancel();
    zipedit::edit(
        &zip,
        vec![
            zipedit::Edit::Delete("d/5.txt".into()),
            zipedit::Edit::Rename {
                from: "wide.txt".into(),
                to: "narrow.txt".into(),
            },
        ],
        &|_| {},
        &cancel,
    )
    .unwrap();
    assert!(has_locator(&zip), "zip64 end records kept");
    let mut archive = zip::ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
    assert_eq!(archive.len(), count);
    let mut body = String::new();
    archive
        .by_name("narrow.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "wide");
    body.clear();
    archive
        .by_name("d/65599.txt")
        .unwrap()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "65599");
    assert!(archive.by_name("d/5.txt").is_err());
    // Its local header (copied as it was) still carries the zip64 sizes.
    let i = archive.index_for_name("narrow.txt").unwrap();
    let header = archive.by_index_raw(i).unwrap().header_start() as usize;
    let bytes = fs::read(&zip).unwrap();
    let n = u16::from_le_bytes([bytes[header + 26], bytes[header + 27]]) as usize;
    assert_eq!(&bytes[header + 30 + n..header + 32 + n], [1, 0]);
}

#[test]
fn a_crash_before_the_rename_or_a_cancel_leaves_the_original() {
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("a.zip");
    zip_file(&zip, &[("a.txt", b"alpha"), ("b.txt", b"bravo")]);
    let original = fs::read(&zip).unwrap();
    let cancel = no_cancel();
    let staged = zipedit::stage(
        &zip,
        vec![zipedit::Edit::Delete("a.txt".into())],
        &|_| {},
        &cancel,
    )
    .unwrap();
    let partial = staged.path().unwrap().to_path_buf();
    assert!(partial.exists());
    // The process dies here: nothing removes the staged file, nothing renames it.
    std::mem::forget(staged);
    assert_eq!(fs::read(&zip).unwrap(), original);
    assert_eq!(tree(&zip).len(), 2);
    // The next edit is not blocked by the leftover (staging names are unique).
    zipedit::edit(
        &zip,
        vec![zipedit::Edit::Delete("b.txt".into())],
        &|_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(tree(&zip), expect(&[("a.txt", b"alpha")]));
    fs::remove_file(partial).unwrap();
    // Cancelled half-way: no staging file, the archive unchanged.
    let before = fs::read(&zip).unwrap();
    let stop = AtomicBool::new(false);
    let result = zipedit::edit(
        &zip,
        vec![zipedit::Edit::Add {
            name: "big.bin".into(),
            size: 4 << 20,
            modified: None,
            open: Box::new(|| Ok(Box::new(std::io::repeat(1).take(4 << 20)))),
        }],
        &|p| {
            if p.done_bytes > 1 << 20 {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        },
        &stop,
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
    assert_eq!(fs::read(&zip).unwrap(), before);
    assert!(leftovers(tmp.path()).is_empty());
}

#[test]
fn other_formats_and_nested_zips_stay_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let router = router(tmp.path());
    let cancel = no_cancel();
    for (name, format) in [
        ("x.7z", "7z"),
        ("x.tar", "tar"),
        ("x.tar.gz", "tar"),
        ("x.rar", "RAR"),
    ] {
        let path = VPath::join_archive(&VPath::local(tmp.path().join(name)), "a.txt");
        let err = zipedit::editable(&path).unwrap_err();
        assert!(
            format!("{err:#}").starts_with(&format!("{format} archives are read-only")),
            "{err:#}"
        );
        let into = transfer(
            &[VPath::local(tmp.path())],
            &path.parent().unwrap(),
            false,
            Conflict::Skip,
            &|_| {},
            &cancel,
            &router,
        );
        assert!(format!("{:#}", into.unwrap_err()).contains("read-only"));
    }
    let outer = tmp.path().join("outer.zip");
    let inner = tmp.path().join("inner.zip");
    zip_file(&inner, &[("a.txt", b"alpha")]);
    zip_file(&outer, &[("inner.zip", &fs::read(&inner).unwrap())]);
    let nested = VPath::join_archive(&at(&outer, "inner.zip"), "a.txt");
    let p = router.provider_for(&nested).unwrap();
    let err = p.remove(&nested).unwrap_err();
    assert!(
        format!("{err:#}").contains("inside another archive"),
        "{err:#}"
    );
    assert!(p
        .mkdir(&VPath::join_archive(&at(&outer, "inner.zip"), "new"))
        .is_err());
    // A zip on another provider is refused by name, before anything is read.
    let remote = VPath::join_archive(&VPath::parse("sftp://host/x.zip").unwrap(), "a");
    assert!(
        format!("{:#}", zipedit::editable(&remote).unwrap_err()).contains("not on this computer")
    );
}

/// A job stopped after the archive's rewrite but before it recorded the batch (or, for a
/// move, before it deleted the sources) runs the batch again: what the rewrite placed was
/// recorded before it ran, so Keep both adds no second copy and a move only deletes what
/// is left of its sources. Recorded against an archive that did not change (stopped
/// before the rewrite), the batch runs as the first time.
#[test]
fn a_transfer_into_a_zip_resumed_after_its_rewrite_adds_nothing_twice() {
    use keel_vfs::ops::{transfer_resumable, Journal, Placed};
    use std::collections::HashMap;
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("a.zip");
    zip_file(&zip, &[("old.txt", b"old"), ("dir/keep.txt", b"keep")]);
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("dir/sub")).unwrap();
    fs::write(src.join("old.txt"), b"new body").unwrap();
    fs::write(src.join("dir/one.txt"), b"one").unwrap();
    fs::write(src.join("dir/sub/two.txt"), b"two").unwrap();
    let router = router(tmp.path());
    let cancel = no_cancel();
    let run = |names: &[&str], mv: bool, placed: HashMap<VPath, Placed>, fail: bool| {
        let from: Vec<VPath> = names.iter().map(|n| VPath::local(src.join(n))).collect();
        let recorded = std::cell::RefCell::new(placed.clone());
        let mut journal = Journal::new(placed, None, |fresh, _| {
            recorded.borrow_mut().extend(fresh);
            anyhow::ensure!(!fail, "stopped before the rewrite");
            Ok(())
        });
        let result = transfer_resumable(
            &from,
            &at(&zip, ""),
            mv,
            Conflict::RenameNew,
            &|_| {},
            &cancel,
            &router,
            &mut journal,
        );
        drop(journal);
        (result, recorded.into_inner())
    };
    // A copy with Keep both: "old (2).txt", and run again from its record, nothing more.
    let (result, placed) = run(&["old.txt"], false, HashMap::new(), false);
    result.unwrap();
    let once = tree(&zip);
    assert_eq!(once["old (2).txt"], b"new body");
    run(&["old.txt"], false, placed, false).0.unwrap();
    assert_eq!(tree(&zip), once, "no \"old (3).txt\"");
    // A move of a folder into "dir (2)", stopped before its sources were deleted (put back
    // here as they were): run again, it deletes them and adds nothing.
    let (result, placed) = run(&["dir"], true, HashMap::new(), false);
    result.unwrap();
    let moved = tree(&zip);
    assert_eq!(moved["dir (2)/sub/two.txt"], b"two");
    assert!(!src.join("dir").exists());
    fs::create_dir_all(src.join("dir/sub")).unwrap();
    fs::write(src.join("dir/one.txt"), b"one").unwrap();
    fs::write(src.join("dir/sub/two.txt"), b"two").unwrap();
    run(&["dir"], true, placed, false).0.unwrap();
    assert_eq!(tree(&zip), moved, "no \"dir (3)\"");
    assert!(!src.join("dir").exists(), "the sources left are deleted");
    // Stopped after recording but before the rewrite: the record does not count.
    fs::write(src.join("late.txt"), b"late").unwrap();
    let (result, placed) = run(&["late.txt"], true, HashMap::new(), true);
    assert!(result.is_err());
    assert!(!tree(&zip).contains_key("late.txt"));
    assert!(placed.values().all(|p| p.archive.is_some()));
    run(&["late.txt"], true, placed, false).0.unwrap();
    assert_eq!(tree(&zip)["late.txt"], b"late");
    assert!(!src.join("late.txt").exists());
}

/// Deleting one entry from a 1 GiB zip (1,024 stored entries of 1 MiB, kept under
/// `target/perf-zip-1g.zip`): one rewrite copying the rest byte for byte.
/// `cargo test -p keel-vfs --release --test zip_edit -- --ignored --nocapture perf_`
#[test]
#[ignore = "1 GiB ZIP rewrite timing; run in release mode"]
fn perf_zip_delete_one_entry_of_1gib() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/perf-zip-1g.zip");
    if !fixture.exists() {
        let part = fixture.with_extension("part");
        let mut zip = zip::ZipWriter::new(fs::File::create(&part).unwrap());
        let stored = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(false);
        let mut body: Vec<u8> = (0..1u32 << 20).map(|i| (i % 251) as u8).collect();
        for i in 0..1_024u32 {
            body[..4].copy_from_slice(&i.to_le_bytes());
            zip.start_file(format!("data/f{i:04}.bin"), stored).unwrap();
            zip.write_all(&body).unwrap();
        }
        zip.finish().unwrap();
        fs::rename(&part, &fixture).unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let zip = tmp.path().join("big.zip");
    fs::copy(&fixture, &zip).unwrap();
    let router = router(tmp.path());
    let p = router.provider_for(&at(&zip, "data")).unwrap();
    let start = std::time::Instant::now();
    p.remove(&at(&zip, "data/f0512.bin")).unwrap();
    let took = start.elapsed();
    let size = fs::metadata(&zip).unwrap().len();
    eprintln!(
        "PERF zip_delete_1gib: {took:?} ({:.0} MB/s rewritten)",
        size as f64 / 1e6 / took.as_secs_f64()
    );
    assert_eq!(
        zip::ZipArchive::new(fs::File::open(&zip).unwrap())
            .unwrap()
            .len(),
        1_023
    );
    assert!(leftovers(tmp.path()).is_empty());
    assert!(took < std::time::Duration::from_secs(20), "{took:?}");
}
