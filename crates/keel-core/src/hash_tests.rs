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
    assert!(su.is_some());
    assert_eq!(cu, None, "a unique large file needs no content id");
    // Small files: the content id is BLAKE3 of the bytes, like a confirmed large one.
    assert_eq!(
        hashes(sa, "small.txt").1.as_deref(),
        Some(&blake3::hash(b"hello").as_bytes()[..])
    );

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
    assert_eq!(copies.copies, 2);
    assert_eq!(
        copies
            .locations
            .iter()
            .map(|v| v.source_label.as_str())
            .collect::<Vec<_>>(),
        ["s0", "s1"]
    );
    // Both folders are on one disk: one failure domain.
    assert_eq!(copies.failure_domains, 1);
    assert_eq!(lib.redundancy(&rec(sa, "other.txt")).unwrap().copies, 1);
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
            "UPDATE record SET sampled_hash = x'aa', cas_id = NULL WHERE name LIKE '%.bin'",
        )
        .unwrap();
    assert!(lib.duplicates(0).unwrap().is_empty());
    assert!(lib.last_copy(&rec(&s[0], "x.bin")).unwrap());
    let warnings = |name: &str| {
        validate_preview_execute(
            &lib,
            Op::Delete {
                paths: vec![VPath::local(a.join(name))],
            },
        )
        .unwrap()
        .warnings
    };
    // Shared but unconfirmed: whether y.bin is a copy is not known yet.
    assert!(matches!(
        warnings("x.bin")[..],
        [Warning::ContentUnverified { files: 1, .. }]
    ));
    // The re-confirm pass of the next hash job settles it: different bytes.
    let info = hash_all(&lib);
    let result: HashResult = serde_json::from_value(info.result.unwrap()).unwrap();
    assert_eq!(result.reconfirmed, 2, "{result:?}");
    assert!(lib.duplicates(0).unwrap().is_empty());
    assert!(matches!(
        warnings("x.bin")[..],
        [Warning::LastCopy { files: 1, .. }]
    ));
    // A unique sampled hash without a content id is a last copy too.
    s[0].store
        .get()
        .unwrap()
        .execute_batch("UPDATE record SET sampled_hash = x'bb', cas_id = NULL WHERE name = 'x.bin'")
        .unwrap();
    assert!(matches!(
        warnings("x.bin")[..],
        [Warning::LastCopy { files: 1, .. }]
    ));
}

#[test]
fn hashing_resumes_from_its_checkpoint() {
    // Many files past the first checkpoint: the job must still be running when the test
    // reads the checkpoint (a fast runner hashes hundreds of tiny files in milliseconds).
    const FILES: usize = super::CHECKPOINT_EVERY * 30;
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
        state(&lib)["done"].as_u64() >= Some(super::CHECKPOINT_EVERY as u64)
    });
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
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
    assert_eq!(lib.jobs().resume_all().unwrap(), [id]);
    let info = lib.jobs().wait(id).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(info.log.contains("resumed"));
    let src = lib.sources()[0].id.clone();
    assert_eq!(hashed(&lib.source(&src).unwrap()), FILES as i64);
    assert_eq!(
        state(&lib),
        serde_json::Value::Null,
        "an ended job's state expires"
    );
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
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
    let id = lib.hash().unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(hashed(&s[0]), 0, "paused");
    assert_eq!(lib.jobs().info(id).unwrap().status, JobStatus::Running);
    lib.shared.busy_until.store(0, Ordering::SeqCst);
    assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    assert_eq!(hashed(&s[0]), 10);

    // A cancel reaches a paused job.
    write(&files.path().join("late.txt"), "y");
    walk(&s[0], &lib.router()).unwrap();
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
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

/// Review item 7: hard links of one file are one file to duplicates and redundancy.
#[test]
fn hard_links_are_not_copies() {
    let files = tempfile::tempdir().unwrap();
    let a = files.path();
    write(&a.join("x.txt"), "hello");
    std::fs::hard_link(a.join("x.txt"), a.join("y.txt")).unwrap();
    write(&a.join("z.txt"), "hello");
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[a]);
    hash_all(&lib);
    let dups = lib.duplicates(0).unwrap();
    assert_eq!(dups.len(), 1);
    // One record for the hard-linked pair (whichever link the platform lists first) plus z.
    let group = &dups[0].records;
    assert_eq!(group.len(), 2, "{group:?}");
    assert!(group.contains(&rec(&s[0], "z.txt")), "{group:?}");
    assert!(
        group.contains(&rec(&s[0], "x.txt")) || group.contains(&rec(&s[0], "y.txt")),
        "{group:?}"
    );
    assert_eq!(lib.redundancy(&rec(&s[0], "y.txt")).unwrap().copies, 2);
    std::fs::remove_file(a.join("z.txt")).unwrap();
    walk(&s[0], &lib.router()).unwrap();
    assert!(lib.duplicates(0).unwrap().is_empty());
    assert!(lib.last_copy(&rec(&s[0], "x.txt")).unwrap());
    // Deleting one link leaves the content at the other: not a last copy.
    let plan = validate_preview_execute(
        &lib,
        Op::Delete {
            paths: vec![VPath::local(a.join("x.txt"))],
        },
    )
    .unwrap();
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
}

/// Review item 8: hashing a big file whole stops between 1 MiB reads.
#[test]
fn a_whole_file_hash_stops_when_asked() {
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("big.bin");
    let bytes = big(3).repeat(10);
    std::fs::write(&path, &bytes).unwrap();
    let size = bytes.len() as u64;
    let stop = AtomicBool::new(true);
    let err = full_hash(&path, size, &stop).unwrap_err();
    assert!(err.is::<Cancelled>(), "{err:#}");
    stop.store(false, Ordering::SeqCst);
    assert_eq!(
        full_hash(&path, size, &stop).unwrap(),
        *blake3::hash(&bytes).as_bytes()
    );
}

/// Review item 9: a network share is skipped (and said so in the job's result) unless it
/// opts in.
#[test]
fn shares_are_hashed_only_when_asked() {
    let files = tempfile::tempdir().unwrap();
    let (a, b) = (files.path().join("a"), files.path().join("b"));
    write(&a.join("x.txt"), "x");
    write(&b.join("y.txt"), "y");
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "h").unwrap();
    lib.set_pause_on_battery(false);
    let share = |label: &str, root: &Path, opt_in: bool| {
        let mut def = folder(label, root);
        def.kind = crate::SourceKind::Share;
        def.hash_shares = opt_in;
        let src = lib.source(&lib.add_source(def).unwrap()).unwrap();
        walk(&src, &lib.router()).unwrap();
        src
    };
    let (sa, sb) = (share("nas", &a, false), share("opted", &b, true));
    let info = hash_all(&lib);
    let result: HashResult = serde_json::from_value(info.result.unwrap()).unwrap();
    assert_eq!(
        result.skipped,
        [SkippedSource {
            source: sa.id.clone(),
            label: "nas".into(),
            reason: SkipReason::Share,
        }]
    );
    assert_eq!(result.hashed, 1);
    assert_eq!((hashed(&sa), hashed(&sb)), (0, 1));
}

/// Review item 11: one hash job at a time (asking again while one runs returns it), app
/// activity pauses it only for a while, and a completed index job schedules hashing.
#[test]
fn hashing_is_one_job_paused_briefly_and_scheduled_after_walks() {
    let files = tempfile::tempdir().unwrap();
    for i in 0..10 {
        write(&files.path().join(format!("{i}.txt")), "x");
    }
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[files.path()]);
    lib.note_activity();
    let first = lib.hash().unwrap();
    assert_eq!(lib.hash().unwrap(), first, "the running job");
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(hashed(&s[0]), 0, "paused by the activity");
    // Expires without an all-clear from the app.
    let info = lib.jobs().wait(first).unwrap();
    assert_eq!(info.status, JobStatus::Done);
    assert_eq!(hashed(&s[0]), 10);
    // The next ask starts a new job; an index job asks by itself.
    write(&files.path().join("late.txt"), "y");
    let index = lib.index(&s[0].id).unwrap();
    assert_eq!(lib.jobs().wait(index).unwrap().status, JobStatus::Done);
    let jobs = lib.jobs().list().unwrap();
    let scheduled = jobs.iter().find(|j| j.kind == "hash" && j.id != first);
    let scheduled = scheduled.expect("hashing scheduled after the walk").id;
    lib.jobs().wait(scheduled).unwrap();
    assert_eq!(hashed(&s[0]), 11);
    lib.set_hash_after_walk(false);
    write(&files.path().join("later.txt"), "z");
    let index = lib.index(&s[0].id).unwrap();
    lib.jobs().wait(index).unwrap();
    assert_eq!(
        lib.jobs()
            .list()
            .unwrap()
            .iter()
            .filter(|j| j.kind == "hash")
            .count(),
        2,
        "off"
    );
}

/// A memory provider served as SFTP host `box`, and a source `name` at `sftp://box/<dir>`,
/// walked.
fn remote_source(
    lib: &Library,
    mem: &Arc<keel_vfs::memory::MemoryProvider>,
    name: &str,
    root: &str,
) -> Arc<Source> {
    lib.router()
        .register_remote_provider("box".into(), mem.clone());
    let id = lib
        .add_source(crate::library::tests::remote(name, root))
        .unwrap();
    let src = lib.source(&id).unwrap();
    walk(&src, &lib.router()).unwrap();
    src
}

fn result(info: &crate::JobInfo) -> HashResult {
    serde_json::from_value(info.result.clone().unwrap()).unwrap()
}

#[test]
fn remote_files_get_the_content_ids_of_their_local_copies() {
    let files = tempfile::tempdir().unwrap();
    write(&files.path().join("small.txt"), "hello");
    std::fs::write(files.path().join("big.bin"), big(1)).unwrap();
    let data = tempfile::tempdir().unwrap();
    let (lib, s) = library(data.path(), &[files.path()]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.put("/srv/small.txt", "hello");
    mem.put("/srv/d/big.bin", big(1));
    mem.put("/srv/other.bin", big(2));
    let r = remote_source(&lib, &mem, "server", "sftp://box/srv");
    let info = hash_all(&lib);
    assert_eq!(result(&info).hashed, 5, "{}", info.log);
    // One pass, whole: every remote file has its content id.
    assert_eq!(hashes(&r, "small.txt"), hashes(&s[0], "small.txt"));
    assert_eq!(hashes(&r, "d/big.bin"), hashes(&s[0], "big.bin"));
    assert!(hashes(&r, "d/big.bin").1.is_some());
    assert!(hashes(&r, "other.bin").1.is_some());
    assert_eq!(
        hashes(&r, "d/big.bin").1.unwrap(),
        blake3::hash(&big(1)).as_bytes()
    );
    let dups = lib.duplicates(0).unwrap();
    assert_eq!(dups.len(), 2, "{dups:?}");
    for g in &dups {
        let sources: Vec<_> = g.records.iter().map(|r| r.source.clone()).collect();
        assert_eq!(sources, [s[0].id.clone(), r.id.clone()]);
    }
}

#[test]
fn remote_hashing_follows_its_policy_and_size_cap() {
    let data = tempfile::tempdir().unwrap();
    let (lib, _) = library(data.path(), &[]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.put("/srv/a.txt", "a");
    mem.put("/srv/big.bin", big(3));
    let r = remote_source(&lib, &mem, "server", "sftp://box/srv");
    lib.router()
        .register_cloud_provider("acct".into(), mem.clone());
    let id = lib
        .add_source(crate::library::tests::remote("drive", "cloud://acct/srv"))
        .unwrap();
    let cloud = lib.source(&id).unwrap();
    walk(&cloud, &lib.router()).unwrap();
    assert_eq!(lib.remote_hash_settings(), RemoteHashSettings::default());

    // Off: nothing is downloaded.
    lib.set_remote_hash_settings(RemoteHashSettings {
        hash_remote: false,
        ..RemoteHashSettings::default()
    })
    .unwrap();
    let info = hash_all(&lib);
    assert!(
        info.log.contains("server: remote hashing is off, skipped"),
        "{}",
        info.log
    );
    assert!(
        info.log.contains("drive: cloud hashing is off, skipped"),
        "{}",
        info.log
    );
    assert_eq!((hashed(&r), hashed(&cloud)), (0, 0));
    assert_eq!(mem.served.load(Ordering::SeqCst), 0);

    // On, with a cap below big.bin: the rest is hashed, big.bin stays unhashed.
    let capped = RemoteHashSettings {
        remote_hash_max_bytes: 1000,
        ..RemoteHashSettings::default()
    };
    lib.set_remote_hash_settings(capped).unwrap();
    let info = hash_all(&lib);
    assert!(
        info.log
            .contains("server: big.bin is over the remote size cap, skipped"),
        "{}",
        info.log
    );
    assert_eq!(
        result(&info)
            .skipped
            .iter()
            .map(|s| s.reason)
            .collect::<Vec<_>>(),
        [SkipReason::TooBig, SkipReason::Remote]
    );
    assert_eq!(hashed(&r), 1);
    assert_eq!(hashes(&r, "big.bin"), (None, None));
    assert_eq!(hashed(&cloud), 0, "cloud is off by default");

    // Cloud on (and the cap kept by the library): the cloud source is hashed too.
    lib.set_remote_hash_settings(RemoteHashSettings {
        hash_cloud: true,
        ..capped
    })
    .unwrap();
    hash_all(&lib);
    assert_eq!(hashed(&cloud), 1);
    let src_dir = data.path().to_owned();
    drop((r, cloud));
    drop(lib);
    let lib = Library::open(&src_dir, "h").unwrap();
    assert_eq!(
        lib.remote_hash_settings(),
        RemoteHashSettings {
            hash_cloud: true,
            ..capped
        },
        "saved with the library"
    );
}

#[test]
fn an_offline_remote_or_a_failed_read_leaves_files_unhashed_until_the_next_run() {
    let data = tempfile::tempdir().unwrap();
    let (lib, _) = library(data.path(), &[]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    for name in ["a", "b", "c"] {
        mem.put(&format!("/srv/{name}.txt"), name);
    }
    let r = remote_source(&lib, &mem, "server", "sftp://box/srv");

    mem.offline.store(true, Ordering::SeqCst);
    let info = hash_all(&lib);
    assert!(
        info.log.contains("server: offline, skipped"),
        "{}",
        info.log
    );
    assert_eq!(hashed(&r), 0);

    mem.offline.store(false, Ordering::SeqCst);
    mem.fail_reads.lock().insert("/srv/b.txt".into());
    let info = hash_all(&lib);
    assert!(info.log.contains("server: b.txt: read of"), "{}", info.log);
    assert_eq!(result(&info).unreadable, 1);
    assert_eq!(hashed(&r), 2);
    let flags: i64 = r
        .store
        .get()
        .unwrap()
        .query_row("SELECT max(flags) FROM record", [], |r| r.get(0))
        .unwrap();
    assert_eq!(flags & UNREADABLE, 0, "nothing marked unreadable");

    mem.fail_reads.lock().clear();
    hash_all(&lib);
    assert_eq!(hashed(&r), 2, "backed off");
    lib.shared.hash_backoff.lock().clear();
    hash_all(&lib);
    assert_eq!(hashed(&r), 3);
}

/// Review 45 M2: turning cloud hashing off stops the running job's download at once (and
/// it is not counted as unreadable).
#[test]
fn turning_cloud_hashing_off_stops_the_running_download() {
    let data = tempfile::tempdir().unwrap();
    let (lib, _) = library(data.path(), &[]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    for name in ["a", "b", "c"] {
        mem.put(&format!("/srv/{name}.bin"), big(1));
    }
    lib.router()
        .register_cloud_provider("acct".into(), mem.clone());
    lib.set_remote_hash_settings(RemoteHashSettings {
        hash_cloud: true,
        ..RemoteHashSettings::default()
    })
    .unwrap();
    let id = lib
        .add_source(crate::library::tests::remote("drive", "cloud://acct/srv"))
        .unwrap();
    let cloud = lib.source(&id).unwrap();
    walk(&cloud, &lib.router()).unwrap();
    let shared = Arc::downgrade(&lib.shared);
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counted = reads.clone();
    *mem.on_read.lock() = Some(Box::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        if let Some(shared) = shared.upgrade() {
            shared.remote_hash_settings.write().hash_cloud = false;
        }
    }));
    let info = hash_all(&lib);
    assert!(
        info.log.contains("drive: cloud hashing is off, skipped"),
        "{}",
        info.log
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(hashed(&cloud), 0);
    assert_eq!(result(&info).unreadable, 0);
}

/// Review 45 minor 1: removing a remote source while one of its files is read skips the
/// rest of that source only; the job goes on.
#[test]
fn removing_a_remote_source_mid_read_leaves_the_job_running() {
    let data = tempfile::tempdir().unwrap();
    let (lib, _) = library(data.path(), &[]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.put("/one/a.txt", "a");
    mem.put("/one/b.txt", "b");
    mem.put("/two/c.txt", "c");
    let one = remote_source(&lib, &mem, "one", "sftp://box/one");
    let two = remote_source(&lib, &mem, "two", "sftp://box/two");
    let gone = Arc::downgrade(&one);
    *mem.on_read.lock() = Some(Box::new(move |path| {
        if let Some(src) = gone.upgrade().filter(|_| path.starts_with("/one/")) {
            src.removed.store(true, Ordering::SeqCst);
        }
    }));
    hash_all(&lib);
    assert_eq!((hashed(&one), hashed(&two)), (0, 1));
}

/// Review 45 M3: a remote file over the size cap does not schedule hashing after every
/// walk, and a failed read is retried only once its backoff runs out.
#[test]
fn over_cap_and_failing_remote_files_do_not_reschedule_every_walk() {
    let data = tempfile::tempdir().unwrap();
    let (lib, _) = library(data.path(), &[]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.put("/srv/a.txt", "a");
    mem.put("/srv/big.bin", big(3));
    let r = remote_source(&lib, &mem, "server", "sftp://box/srv");
    lib.set_remote_hash_settings(RemoteHashSettings {
        remote_hash_max_bytes: 1000,
        ..RemoteHashSettings::default()
    })
    .unwrap();
    hash_all(&lib);
    let hash_jobs = || {
        let jobs = lib.jobs().list().unwrap();
        let ids: Vec<JobId> = jobs
            .iter()
            .filter(|j| j.kind == "hash")
            .map(|j| j.id)
            .collect();
        ids
    };
    let walk_again = || {
        let id = lib.index(&r.id).unwrap();
        assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Done);
    };
    walk_again();
    walk_again();
    assert_eq!(hash_jobs().len(), 1, "big.bin alone schedules nothing");

    mem.put("/srv/b.txt", "b");
    mem.fail_reads.lock().insert("/srv/b.txt".into());
    walk_again();
    let jobs = hash_jobs();
    assert_eq!(jobs.len(), 2, "b.txt is new");
    let info = lib.jobs().wait(*jobs.iter().max().unwrap()).unwrap();
    assert_eq!(result(&info).unreadable, 1, "{}", info.log);
    assert!(info.log.contains("tried again in 1 h"), "{}", info.log);
    walk_again();
    assert_eq!(hash_jobs().len(), 2, "b.txt is backed off");

    mem.fail_reads.lock().clear();
    lib.shared.hash_backoff.lock().clear();
    walk_again();
    let jobs = hash_jobs();
    assert_eq!(jobs.len(), 3);
    lib.jobs().wait(*jobs.iter().max().unwrap()).unwrap();
    assert_eq!(hashed(&r), 2);
}

#[test]
fn remote_hashing_resumes_from_its_checkpoint() {
    const FILES: usize = super::CHECKPOINT_EVERY * 5;
    let data = tempfile::tempdir().unwrap();
    let (lib, _) = library(data.path(), &[]);
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    for i in 0..FILES {
        mem.put(&format!("/srv/{i}.txt"), i.to_string());
    }
    let r = remote_source(&lib, &mem, "server", "sftp://box/srv");
    // Slow reads: the job is still running when the test reads the first checkpoint.
    mem.read_delay_ms.store(10, Ordering::SeqCst);
    let id = lib.hash().unwrap();
    eventually("a checkpoint", || {
        state_of(&data, id)["done"].as_u64() >= Some(super::CHECKPOINT_EVERY as u64)
    });
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
    drop(r);
    drop(lib);
    let at_kill = state_of(&data, id)["done"].as_u64().unwrap();
    assert!(at_kill < FILES as u64, "{at_kill}");

    mem.read_delay_ms.store(0, Ordering::SeqCst);
    let lib = Library::open(data.path(), "h").unwrap();
    lib.set_pause_on_battery(false);
    lib.router()
        .register_remote_provider("box".into(), mem.clone());
    assert_eq!(lib.jobs().resume_all().unwrap(), [id]);
    let info = lib.jobs().wait(id).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    let r = lib.source(&lib.sources()[0].id).unwrap();
    assert_eq!(hashed(&r), FILES as i64);
    // Files done before the stop were not read again.
    let served = mem.served.load(Ordering::SeqCst);
    let all: u64 = (0..FILES).map(|i| i.to_string().len() as u64).sum();
    assert!(served < all + 3 * 4, "served {served} of {all}");
}
