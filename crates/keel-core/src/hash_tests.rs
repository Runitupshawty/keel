use super::*;
use crate::index::tests::{eventually, id_of, walk, write};
use crate::library::tests::folder;
use crate::{validate_preview_execute, JobStatus, Op, Warning};
use keel_vfs::VPath;
use std::path::Path;

/// A library over `roots` (one folder source each), walked, hashing on battery too.
fn library(data: &Path, roots: &[&Path]) -> (Library, Vec<Arc<Source>>) {
    let lib = Library::open(data, "h").unwrap();
    lib.set_pause_on_battery(false);
    let sources = roots
        .iter()
        .enumerate()
        .map(|(i, root)| {
            let id = lib.add_source(folder(&format!("s{i}"), root)).unwrap();
            let src = lib.source(&id).unwrap();
            walk(&src, &lib.router()).unwrap();
            src
        })
        .collect();
    (lib, sources)
}

fn hash_all(lib: &Library) -> crate::JobInfo {
    let id = lib.hash().unwrap();
    let info = lib.jobs().wait(id).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    info
}

fn rec(src: &Source, rel: &str) -> RecordRef {
    RecordRef {
        source: src.id.clone(),
        id: id_of(src, rel).unwrap(),
    }
}

/// `(sampled_hash, cas_id)`.
fn hashes(src: &Source, rel: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    src.store
        .get()
        .unwrap()
        .query_row(
            "SELECT sampled_hash, cas_id FROM record WHERE id = ?1",
            [id_of(src, rel).unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn big(seed: u8) -> Vec<u8> {
    (0..400 * 1024).map(|i| (i % 251) as u8 ^ seed).collect()
}

fn hashed(src: &Source) -> i64 {
    src.store
        .get()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM record WHERE sampled_hash IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn collisions_are_hashed_whole_and_only_true_copies_are_duplicates() {
    let files = tempfile::tempdir().unwrap();
    let (a, b) = (files.path().join("a"), files.path().join("b"));
    let p = big(0);
    // Same size, first/middle/last 64 KiB as `p`: one byte differs outside the samples.
    let mut lookalike = p.clone();
    lookalike[100 * 1024] ^= 0xff;
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("big1.bin"), &p).unwrap();
    std::fs::write(a.join("big2.bin"), &lookalike).unwrap();
    std::fs::write(b.join("big3.bin"), &p).unwrap();
    std::fs::write(a.join("unique.bin"), big(7)).unwrap();
    write(&a.join("small.txt"), "hello");
    write(&b.join("small copy.txt"), "hello");
    write(&a.join("other.txt"), "world");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[&a, &b]);
    let (sa, sb) = (&s[0], &s[1]);
    let unhashed = rec(sa, "unique.bin");
    assert!(lib.last_copy(&unhashed).unwrap(), "no known copy yet");

    let info = hash_all(&lib);
    assert!(info.log.contains("hashed 7 files"), "{}", info.log);
    assert_eq!(info.progress, 1.0);

    // Same sampled hash, different bytes: hashed whole, different content ids.
    let (s1, c1) = hashes(sa, "big1.bin");
    let (s2, c2) = hashes(sa, "big2.bin");
    let (s3, c3) = hashes(sb, "big3.bin");
    assert_eq!(s1, s2);
    assert_eq!(s1, s3);
    assert_ne!(c1, s1, "confirmed after the collision");
    assert_ne!(c1, c2);
    assert_eq!(c1, c3);
    assert_eq!(c1.as_deref(), Some(&blake3::hash(&p).as_bytes()[..]));
    let (su, cu) = hashes(sa, "unique.bin");
    assert_eq!(su, cu, "a unique large file keeps its sampled hash");

    let dups = lib.duplicates(1).unwrap();
    assert_eq!(
        dups,
        [
            DupGroup {
                cas_id: c1.clone().unwrap(),
                size: p.len() as u64,
                records: vec![rec(sa, "big1.bin"), rec(sb, "big3.bin")],
            },
            DupGroup {
                cas_id: hashes(sa, "small.txt").1.unwrap(),
                size: 5,
                records: vec![rec(sa, "small.txt"), rec(sb, "small copy.txt")],
            },
        ]
    );
    assert_eq!(lib.duplicates(10).unwrap().len(), 1);

    assert!(lib.last_copy(&rec(sa, "big2.bin")).unwrap());
    assert!(!lib.last_copy(&rec(sa, "big1.bin")).unwrap());
    assert!(lib.last_copy(&rec(sa, "unique.bin")).unwrap());
    let copies = lib.redundancy(&rec(sb, "big3.bin")).unwrap();
    assert_eq!(copies.count, 2);
    assert_eq!(
        copies
            .volumes
            .iter()
            .map(|v| v.label.as_str())
            .collect::<Vec<_>>(),
        ["s0", "s1"]
    );
    assert_eq!(lib.redundancy(&rec(sa, "other.txt")).unwrap().count, 1);
    assert_eq!(lib.stats().unique_content, 5);

    // LastCopy warnings follow the content ids.
    let warns = |p: &Path| {
        validate_preview_execute(
            &lib,
            Op::Delete {
                paths: vec![VPath::local(p)],
            },
        )
        .unwrap()
        .warnings
    };
    assert!(warns(&a.join("big1.bin")).is_empty());
    assert_eq!(
        warns(&a.join("big2.bin")),
        [Warning::LastCopy {
            path: VPath::local(a.join("big2.bin")),
            files: 1
        }]
    );
}

#[test]
fn a_shared_unconfirmed_sampled_hash_is_never_a_duplicate() {
    let files = tempfile::tempdir().unwrap();
    let a = files.path();
    std::fs::write(a.join("x.bin"), big(1)).unwrap();
    std::fs::write(a.join("y.bin"), big(2)).unwrap();
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[a]);
    // As if hashing stopped between the two halves of a collision.
    s[0].store
        .get()
        .unwrap()
        .execute_batch(
            "UPDATE record SET sampled_hash = x'aa', cas_id = x'aa' WHERE name LIKE '%.bin'",
        )
        .unwrap();
    assert!(lib.duplicates(0).unwrap().is_empty());
    assert!(lib.last_copy(&rec(&s[0], "x.bin")).unwrap());
    let plan = validate_preview_execute(
        &lib,
        Op::Delete {
            paths: vec![VPath::local(a.join("x.bin"))],
        },
    )
    .unwrap();
    assert!(matches!(plan.warnings[..], [Warning::LastCopy { .. }]));
}

#[test]
fn hashing_resumes_from_its_checkpoint() {
    const FILES: usize = 3_000;
    let files = tempfile::tempdir().unwrap();
    for i in 0..FILES {
        std::fs::write(files.path().join(format!("{i}.txt")), i.to_string()).unwrap();
    }
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[files.path()]);
    let id = lib.hash().unwrap();
    let state = |lib: &Library| -> serde_json::Value {
        let state: String = lib
            .shared
            .db
            .get()
            .unwrap()
            .query_row("SELECT state FROM job WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        serde_json::from_str(&state).unwrap()
    };
    eventually("a checkpoint", || {
        state(&lib)["done"].as_u64() >= Some(1_000)
    });
    lib.activity().store(true, Ordering::SeqCst);
    let src_dir = s[0].store_dir().to_owned();
    drop(s);
    drop(lib); // the "kill": the paused job stops at once and stays running
    let at_kill = state_of(&data, id);
    assert!(
        at_kill["done"].as_u64().unwrap() < FILES as u64,
        "{at_kill}"
    );
    assert!(src_dir.exists());

    let lib = Library::open(data.path(), "h").unwrap();
    lib.set_pause_on_battery(false);
    // Resumed on open (a built-in kind).
    let info = lib.jobs().wait(id).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(info.log.contains("resumed"));
    let src = lib.sources()[0].id.clone();
    assert_eq!(hashed(&lib.source(&src).unwrap()), FILES as i64);
    assert_eq!(state(&lib)["done"].as_u64(), Some(FILES as u64));
}

fn state_of(data: &tempfile::TempDir, id: JobId) -> serde_json::Value {
    let c = Connection::open(data.path().join("library/h/library.db")).unwrap();
    let state: String = c
        .query_row("SELECT state FROM job WHERE id = ?1", [id], |r| r.get(0))
        .unwrap();
    serde_json::from_str(&state).unwrap()
}

#[test]
fn hashing_pauses_while_the_app_is_busy() {
    let files = tempfile::tempdir().unwrap();
    for i in 0..10 {
        write(&files.path().join(format!("{i}.txt")), "x");
    }
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[files.path()]);
    lib.activity().store(true, Ordering::SeqCst);
    let id = lib.hash().unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(hashed(&s[0]), 0, "paused");
    assert_eq!(lib.jobs().info(id).unwrap().status, JobStatus::Running);
    lib.activity().store(false, Ordering::SeqCst);
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    assert_eq!(hashed(&s[0]), 10);

    // A cancel reaches a paused job.
    write(&files.path().join("late.txt"), "y");
    walk(&s[0], &lib.router()).unwrap();
    lib.activity().store(true, Ordering::SeqCst);
    let id = lib.hash().unwrap();
    lib.jobs().cancel(id).unwrap();
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Cancelled);
}

#[test]
fn unreadable_files_are_recorded_with_their_error() {
    let files = tempfile::tempdir().unwrap();
    write(&files.path().join("gone.txt"), "x");
    write(&files.path().join("ok.txt"), "y");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[files.path()]);
    std::fs::remove_file(files.path().join("gone.txt")).unwrap();
    let info = hash_all(&lib);
    assert!(info.log.contains("1 unreadable"), "{}", info.log);
    let (flags, error): (i64, Option<String>) = s[0]
        .store
        .get()
        .unwrap()
        .query_row(
            "SELECT flags, error FROM record WHERE name = 'gone.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(flags & UNREADABLE, UNREADABLE);
    assert!(error.unwrap().starts_with("hash: "));
    assert!(hashes(&s[0], "ok.txt").1.is_some());
    // Not retried until a walk refreshes the record.
    assert!(hash_all(&lib).log.contains("hashed 0 files"));
}

#[test]
fn offline_and_remote_sources_are_skipped() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().join("drive");
    write(&root.join("a.txt"), "a");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[&root]);
    std::fs::rename(&root, files.path().join("unplugged")).unwrap();
    let info = hash_all(&lib);
    assert!(info.log.contains("s0: offline, skipped"), "{}", info.log);
    assert_eq!(hashed(&s[0]), 0);
    let flags: i64 = s[0]
        .store
        .get()
        .unwrap()
        .query_row("SELECT max(flags) FROM record", [], |r| r.get(0))
        .unwrap();
    assert_eq!(flags & UNREADABLE, 0, "nothing marked unreadable");
}

/// `cargo test -p keel-core --release -- --ignored ten_thousand`
#[test]
#[ignore]
fn ten_thousand_small_files_hash_fast() {
    let files = tempfile::tempdir().unwrap();
    for i in 0..10_000 {
        let dir = files.path().join(format!("d{}", i / 1_000));
        if i % 1_000 == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(dir.join(format!("{i}.txt")), format!("file {i}")).unwrap();
    }
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[files.path()]);
    let start = Instant::now();
    hash_all(&lib);
    let took = start.elapsed();
    eprintln!("hashed 10,000 small files in {took:?}");
    assert_eq!(hashed(&s[0]), 10_000);
    assert!(took < Duration::from_secs(10), "took {took:?}");
}
