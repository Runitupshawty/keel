use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::{JobStatus, RecordRef, Source};
use std::sync::Arc;

/// A library `main` over one folder of `n` files (and a subfolder), walked.
fn library(data: &Path, files: &Path, n: usize) -> (Library, Arc<Source>) {
    for i in 0..n {
        write(&files.join(format!("f{i}.txt")), &format!("file {i}"));
    }
    write(&files.join("sub/photo.jpg"), "not really a photo");
    let lib = Library::open(data, "main").unwrap();
    lib.set_pause_on_battery(false);
    let src = lib
        .source(&lib.add_source(folder("Files", files)).unwrap())
        .unwrap();
    walk(&src, &lib.router()).unwrap();
    (lib, src)
}

fn rec(src: &Source, rel: &str) -> RecordRef {
    RecordRef {
        source: src.id.clone(),
        id: id_of(src, rel).unwrap(),
    }
}

/// `(path, sampled_hash, cas_id)`.
type Content = (String, Option<Vec<u8>>, Option<Vec<u8>>);

/// Every record's content, by path.
fn contents(src: &Source) -> Vec<Content> {
    let c = src.store.get().unwrap();
    let mut stmt = c
        .prepare("SELECT path, sampled_hash, cas_id FROM record ORDER BY path")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn names(hits: Vec<crate::LibraryHit>) -> Vec<String> {
    let mut v: Vec<String> = hits.into_iter().map(|h| h.name).collect();
    v.sort();
    v
}

fn kind(e: &anyhow::Error) -> BackupError {
    e.downcast_ref::<BackupError>()
        .unwrap_or_else(|| panic!("not a BackupError: {e:#}"))
        .clone()
}

/// No library, no scratch folder: a refused restore leaves the data folder as it was.
fn untouched(data: &Path) {
    let left: Vec<_> = std::fs::read_dir(data)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(left.iter().all(|n| n == "library"), "left behind: {left:?}");
    let libs: Vec<_> = std::fs::read_dir(data.join("library"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(libs.is_empty(), "{libs:?}");
}

#[test]
fn file_names_carry_the_utc_date() {
    assert_eq!(file_name("main", 0), "main-1970-01-01.keelbackup");
    assert_eq!(
        file_name("main", 1_760_000_000),
        "main-2025-10-09.keelbackup"
    );
    assert_eq!(file_name("x", 951_782_400), "x-2000-02-29.keelbackup");
    assert_eq!(file_name("x", -86_400), "x-1969-12-31.keelbackup");
}

/// The writer refuses exactly what the reader would: no backup is written that cannot be
/// restored.
#[test]
fn a_backup_never_holds_a_file_a_restore_refuses() {
    assert!(fits("library.db", MAX_ENTRY).is_ok());
    let e = fits("x.db", MAX_ENTRY + 1).unwrap_err();
    assert!(format!("{e:#}").contains("at most 4 GiB"), "{e:#}");
    let mut m = Manifest {
        format: FORMAT,
        keel_version: String::new(),
        library: "main".into(),
        library_id: String::new(),
        created: 0,
        sources: Vec::new(),
        files: vec![BackupFile {
            name: LIBRARY_DB.into(),
            size: MAX_ENTRY,
            sha256: "0".repeat(64),
        }],
    };
    assert!(check_manifest(&m).is_ok());
    m.files[0].size = MAX_ENTRY + 1;
    assert!(check_manifest(&m).is_err());
}

#[test]
fn a_restore_brings_back_files_tags_favorites_content_ids_and_sources() {
    let (data, files, other) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let (lib, src) = library(data.path(), files.path(), 20);
    let work = lib.create_tag("Work", Some("#e5484d"), None).unwrap();
    lib.set_tag(work, &[rec(&src, "f1.txt"), rec(&src, "sub")], true)
        .unwrap();
    lib.set_favorite(&[rec(&src, "f2.txt")], true).unwrap();
    let hash = lib.hash().unwrap();
    assert_eq!(lib.jobs().wait(hash).unwrap().status, JobStatus::Done);
    assert_eq!(lib.stats().last_backup, None);

    let file = other.path().join(file_name("main", crate::now()));
    let manifest = lib.backup(&file).unwrap();
    assert_eq!(manifest.library, "main");
    assert_eq!(manifest.sources.len(), 1);
    assert_eq!(manifest.sources[0].root, VPath::local(files.path()));
    assert_eq!(inspect(&file).unwrap(), manifest);
    assert!(lib.stats().last_backup.is_some());
    // No scratch copies are left in the library folder, no partial file next to the backup.
    let left: Vec<_> = std::fs::read_dir(lib.dir()).unwrap().flatten().collect();
    assert!(
        left.iter()
            .all(|e| !e.file_name().to_string_lossy().starts_with("backup-")),
        "{left:?}"
    );
    assert_eq!(std::fs::read_dir(other.path()).unwrap().count(), 1);

    // Into a fresh data folder, as on another machine.
    let fresh = tempfile::tempdir().unwrap();
    let restored = restore(fresh.path(), &file, None).unwrap();
    assert_eq!(restored.name, "main");
    assert_eq!(restored.sources.len(), 1);
    assert!(restored.sources[0].online, "the fixture root exists here");
    let back = Library::open(fresh.path(), "main").unwrap();
    assert_eq!(back.id, lib.id);
    assert_eq!(back.sources().len(), 1);
    let (a, b) = (&lib.sources()[0], &back.sources()[0]);
    assert_eq!((&a.id, &a.label, &a.root), (&b.id, &b.label, &b.root));
    let bsrc = back.source(&b.id).unwrap();
    assert_eq!(contents(&bsrc), contents(&src));
    assert!(contents(&bsrc).iter().any(|c| c.2.is_some()), "content ids");
    let (s1, s2) = (lib.stats(), back.stats());
    assert_eq!(
        (s1.files, s1.records, s1.bytes),
        (s2.files, s2.records, s2.bytes)
    );
    assert_eq!(lib.tags().unwrap(), back.tags().unwrap());
    assert_eq!(
        names(back.records_with_tag(work).unwrap()),
        ["f1.txt", "sub"]
    );
    assert_eq!(names(back.favorites().unwrap()), ["f2.txt"]);
    // The search index was rebuilt.
    let hits = back
        .search(&crate::LibraryQuery::parse("photo", 0).unwrap())
        .unwrap();
    assert_eq!(names(hits), ["photo.jpg"]);
    // The source is walked again when the library opens: a file added since shows up.
    write(&files.path().join("new.txt"), "new");
    let resumed = back.jobs().resume_all().unwrap();
    assert_eq!(resumed.len(), 1, "one index job, no job of the original");
    assert_eq!(
        back.jobs().wait(resumed[0]).unwrap().status,
        JobStatus::Done
    );
    assert_eq!(back.stats().files, s1.files + 1);
}

#[test]
fn a_restore_never_replaces_a_library_and_takes_another_name() {
    let (data, files, other) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let (lib, _) = library(data.path(), files.path(), 3);
    let file = other.path().join("b.keelbackup");
    lib.backup(&file).unwrap();
    let before = std::fs::read(lib.dir().join("library.db")).ok();
    let err = restore(data.path(), &file, None).unwrap_err();
    assert_eq!(kind(&err), BackupError::NameTaken("main".into()));
    assert!(
        format!("{err}").contains("a library named main exists"),
        "{err}"
    );
    assert_eq!(std::fs::read(lib.dir().join("library.db")).ok(), before);
    assert!(restore(data.path(), &file, Some("../x")).is_err());
    let copy = restore(data.path(), &file, Some("copy")).unwrap();
    assert_eq!(copy.dir, data.path().join("library").join("copy"));
    let names: Vec<String> = Library::list_in(data.path())
        .into_iter()
        .map(|l| l.name)
        .collect();
    assert_eq!(names, ["copy", "main"]);
    let copy = Library::open(data.path(), "copy").unwrap();
    assert_eq!(copy.stats().files, lib.stats().files);
    // The source of the copy overlaps nothing in the copy (sources are per library).
    assert_eq!(copy.sources().len(), 1);
}

#[test]
fn a_backup_during_an_index_job_is_a_consistent_snapshot() {
    let (data, files, other) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    for i in 0..3000 {
        write(
            &files.path().join(format!("d{}/f{i}.txt", i % 30)),
            &i.to_string(),
        );
    }
    let lib = Library::open(data.path(), "main").unwrap();
    let id = lib.add_source(folder("Files", files.path())).unwrap();
    let job = lib.index(&id).unwrap();
    let file = other.path().join("during.keelbackup");
    lib.backup(&file).unwrap();
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    let fresh = tempfile::tempdir().unwrap();
    // `restore` ran `quick_check` on every database.
    restore(fresh.path(), &file, None).unwrap();
    let back = Library::open(fresh.path(), "main").unwrap();
    let files_then = back.stats().files;
    assert!(files_then <= 3000, "{files_then}");
    let resumed = back.jobs().resume_all().unwrap();
    for id in resumed {
        assert_eq!(back.jobs().wait(id).unwrap().status, JobStatus::Done);
    }
    assert_eq!(back.stats().files, 3000);
}

/// A valid backup's entries, rewritten by `edit` into a new zip (the manifest as JSON).
fn crafted(
    file: &Path,
    out: &Path,
    edit: impl FnOnce(&mut serde_json::Value, &mut Vec<(String, Vec<u8>)>),
) {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(file).unwrap()).unwrap();
    let mut entries = Vec::new();
    let mut manifest = serde_json::Value::Null;
    for i in 0..zip.len() {
        let mut e = zip.by_index(i).unwrap();
        let mut bytes = Vec::new();
        e.read_to_end(&mut bytes).unwrap();
        if e.name() == MANIFEST {
            manifest = serde_json::from_slice(&bytes).unwrap();
        } else {
            entries.push((e.name().to_owned(), bytes));
        }
    }
    edit(&mut manifest, &mut entries);
    let mut w = zip::ZipWriter::new(std::fs::File::create(out).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &entries {
        w.start_file(name.as_str(), options).unwrap();
        w.write_all(bytes).unwrap();
    }
    if !manifest.is_null() {
        w.start_file(MANIFEST, options).unwrap();
        w.write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
    }
    w.finish().unwrap();
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Changes `library.db` with `sql` and lists its new size and hash.
fn edit_library_db(m: &mut serde_json::Value, entries: &mut [(String, Vec<u8>)], sql: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.db");
    let entry = entries.iter_mut().find(|e| e.0 == LIBRARY_DB).unwrap();
    std::fs::write(&path, &entry.1).unwrap();
    Connection::open(&path).unwrap().execute_batch(sql).unwrap();
    entry.1 = std::fs::read(&path).unwrap();
    let listed = m["files"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|f| f["name"] == LIBRARY_DB)
        .unwrap();
    listed["size"] = entry.1.len().into();
    listed["sha256"] = sha(&entry.1).into();
}

#[test]
fn crafted_backups_are_refused_without_a_trace() {
    let (data, files, other) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let (lib, _) = library(data.path(), files.path(), 3);
    let good = other.path().join("good.keelbackup");
    lib.backup(&good).unwrap();
    drop(lib);
    type Edit = Box<dyn FnOnce(&mut serde_json::Value, &mut Vec<(String, Vec<u8>)>)>;
    type Case = (&'static str, Edit, fn(&BackupError) -> bool);
    let cases: Vec<Case> = vec![
        (
            "a path entry",
            Box::new(|_, e| e.push(("../evil.db".into(), b"x".to_vec()))),
            |k| matches!(k, BackupError::Damaged(_)),
        ),
        (
            "a path in the manifest",
            Box::new(|m, e| {
                m["files"][0]["name"] = "sub/library.db".into();
                e[0].0 = "sub/library.db".into();
            }),
            |k| matches!(k, BackupError::NotABackup(_)),
        ),
        (
            "an extra entry",
            Box::new(|_, e| e.push(("notes.txt".into(), b"x".to_vec()))),
            |k| matches!(k, BackupError::Damaged(_)),
        ),
        (
            "a missing entry",
            Box::new(|_, e| {
                e.pop();
            }),
            |k| matches!(k, BackupError::Damaged(_)),
        ),
        (
            "an entry over 4 GiB",
            Box::new(|m, _| m["files"][0]["size"] = (5u64 << 30).into()),
            |k| matches!(k, BackupError::NotABackup(_)),
        ),
        (
            "an entry that expands over 100 times",
            Box::new(|m, e| {
                let zeros = vec![0u8; 4 << 20];
                m["files"][0]["size"] = zeros.len().into();
                m["files"][0]["sha256"] = sha(&zeros).into();
                e.iter_mut().find(|e| e.0 == LIBRARY_DB).unwrap().1 = zeros;
            }),
            |k| matches!(k, BackupError::Damaged(w) if w.contains("expands")),
        ),
        (
            "a wrong hash",
            Box::new(|m, _| m["files"][0]["sha256"] = "0".repeat(64).into()),
            |k| matches!(k, BackupError::Damaged(w) if w.contains("checksum")),
        ),
        (
            "a newer format",
            Box::new(|m, _| m["format"] = (FORMAT + 1).into()),
            |k| matches!(k, BackupError::Newer(_)),
        ),
        (
            "a newer database schema",
            Box::new(|m, e| edit_library_db(m, e, "PRAGMA user_version = 999;")),
            |k| matches!(k, BackupError::Newer(_)),
        ),
        (
            "a trigger of its own",
            Box::new(|m, e| {
                edit_library_db(
                    m,
                    e,
                    "CREATE TRIGGER evil AFTER INSERT ON job BEGIN DELETE FROM source; END;",
                )
            }),
            |k| matches!(k, BackupError::Damaged(w) if w.contains("triggers")),
        ),
        (
            "a source the manifest does not list",
            Box::new(|m, e| {
                edit_library_db(
                    m,
                    e,
                    "INSERT INTO source(id, def, created) VALUES ('../../outside', '{}', 0);",
                )
            }),
            |k| matches!(k, BackupError::Damaged(_)),
        ),
        (
            "a source id that is a path",
            Box::new(|m, _| m["sources"][0]["id"] = "../../outside".into()),
            |k| matches!(k, BackupError::NotABackup(_)),
        ),
        (
            "an invalid library name",
            Box::new(|m, _| m["library"] = "..".into()),
            |k| matches!(k, BackupError::NotABackup(_)),
        ),
        (
            "no manifest",
            Box::new(|m, _| *m = serde_json::Value::Null),
            |k| matches!(k, BackupError::NotABackup(_)),
        ),
        (
            "a manifest that is not Keel's",
            Box::new(|m, _| *m = serde_json::json!({"hello": "world"})),
            |k| matches!(k, BackupError::NotABackup(_)),
        ),
    ];
    for (what, edit, expected) in cases {
        let file = other.path().join("crafted.keelbackup");
        crafted(&good, &file, edit);
        let fresh = tempfile::tempdir().unwrap();
        let err = restore(fresh.path(), &file, None).unwrap_err();
        let k = kind(&err);
        assert!(expected(&k), "{what}: {k:?}");
        untouched(fresh.path());
        assert!(!other.path().join("evil.db").exists() && !fresh.path().join("evil.db").exists());
        assert!(!fresh.path().parent().unwrap().join("outside").exists());
    }
    // A newer Keel's backup says so.
    let file = other.path().join("newer.keelbackup");
    crafted(&good, &file, |m, _| m["format"] = 9.into());
    let err = restore(tempfile::tempdir().unwrap().path(), &file, None).unwrap_err();
    assert!(format!("{err}").contains("made by a newer Keel"), "{err}");
    // Truncated, and not a zip at all.
    let bytes = std::fs::read(&good).unwrap();
    for (what, data) in [
        ("truncated", bytes[..bytes.len() / 2].to_vec()),
        ("not a zip", b"hello".to_vec()),
    ] {
        let file = other.path().join("bad.keelbackup");
        std::fs::write(&file, data).unwrap();
        let fresh = tempfile::tempdir().unwrap();
        let err = restore(fresh.path(), &file, None).unwrap_err();
        assert!(
            matches!(kind(&err), BackupError::NotABackup(_)),
            "{what}: {err:#}"
        );
        assert!(format!("{err}").starts_with("not a Keel backup"), "{err}");
        untouched(fresh.path());
    }
    // A damaged body (a flipped byte inside a compressed entry) is caught by its CRC or hash.
    let mut flipped = bytes.clone();
    let mid = 200.min(flipped.len() / 4);
    flipped[mid] ^= 0x55;
    let file = other.path().join("flipped.keelbackup");
    std::fs::write(&file, flipped).unwrap();
    let fresh = tempfile::tempdir().unwrap();
    assert!(restore(fresh.path(), &file, None).is_err());
    untouched(fresh.path());
    // The good one restores.
    let fresh = tempfile::tempdir().unwrap();
    restore(fresh.path(), &good, None).unwrap();
}

/// The fuzz corpus seeds (`fuzz/corpus/backup/`): a small valid backup and crafted ones.
#[test]
#[ignore = "writes the fuzz corpus seeds"]
fn write_fuzz_seeds() {
    let data = tempfile::tempdir().unwrap();
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/backup");
    std::fs::create_dir_all(&corpus).unwrap();
    // A source at a made-up root, never walked: the corpus is public (no local paths).
    let lib = Library::open(data.path(), "main").unwrap();
    lib.add_source(crate::library::tests::remote(
        "Files",
        "sftp://example.invalid/files",
    ))
    .unwrap();
    let good = corpus.join("one-source.keelbackup");
    lib.backup(&good).unwrap();
    crafted(&good, &corpus.join("path-entry.keelbackup"), |_, e| {
        e.push(("../evil.db".into(), b"x".to_vec()))
    });
    crafted(&good, &corpus.join("newer-format.keelbackup"), |m, _| {
        m["format"] = 2.into()
    });
    crafted(&good, &corpus.join("no-sources.keelbackup"), |m, e| {
        m["sources"] = serde_json::json!([]);
        m["files"].as_array_mut().unwrap().truncate(1);
        e.truncate(1);
    });
}
