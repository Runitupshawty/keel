//! Release-only measurements on realistic data (`docs/performance.md`, `scripts/perf.sh`).
//! Each is `#[ignore]`d and asserts a generous budget, so a nightly job can run them:
//! `cargo test -p keel-core --release perf_ -- --ignored --nocapture --test-threads 1`.
//! Fixtures that are slow to make (files on disk) are kept under `target/perf-*` and reused.
use crate::index::tests::{fake, library_with, peak_rss, walk};
use crate::library::tests::folder;
use crate::{Library, LibraryQuery, RecordRef, SourceDef, SourceKind};
use keel_vfs::{Router, VPath};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A fixture folder under the workspace `target/`, made by `make` once (a `<name>.complete`
/// file next to it says it was finished).
fn fixture(name: &str, make: impl FnOnce(&Path)) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(name);
    let done = dir.with_extension("complete");
    if !done.exists() {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let start = Instant::now();
        make(&dir);
        std::fs::write(&done, b"").unwrap();
        eprintln!("fixture {name}: made in {:?}", start.elapsed());
    }
    dir
}

/// Bytes of `name` plus its WAL in `dir`.
fn db_size(dir: &Path, name: &str) -> u64 {
    [name.to_owned(), format!("{name}-wal")]
        .iter()
        .filter_map(|n| std::fs::metadata(dir.join(n)).ok())
        .map(|m| m.len())
        .sum()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// (p50, p95) of `runs`.
fn percentiles(mut runs: Vec<Duration>) -> (Duration, Duration) {
    runs.sort();
    let at = |p: usize| runs[(runs.len() * p / 100).min(runs.len() - 1)];
    (at(50), at(95))
}

fn count(lib: &Library, sql: &str) -> i64 {
    let src = lib.source(&lib.sources()[0].id).unwrap();
    let c = src.store.get().unwrap();
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Indexing a folder source of 200,000 files in 20,200 folders on disk (200 folders of 100
/// folders of 10 files): first walk, unchanged re-walk, records/s, store sizes.
#[test]
#[ignore = "release measurement: 200,000 files on disk under target/"]
fn perf_index_200k_files() {
    let root = fixture("perf-index-200k", |dir| {
        for a in 0..200 {
            for b in 0..100 {
                let leaf = dir.join(format!("area {a:03}/folder {b:03}"));
                std::fs::create_dir_all(&leaf).unwrap();
                for f in 0..10 {
                    std::fs::write(leaf.join(format!("report {a}-{b}-{f}.txt")), b"x").unwrap();
                }
            }
        }
    });
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "perf").unwrap();
    let id = lib.add_source(folder("perf", &root)).unwrap();
    let src = lib.source(&id).unwrap();
    let router = lib.router();
    let start = Instant::now();
    walk(&src, &router).unwrap();
    let first = start.elapsed();
    let start = Instant::now();
    walk(&src, &router).unwrap();
    let rewalk = start.elapsed();
    let records = count(&lib, "SELECT count(*) FROM record");
    assert_eq!(records, 1 + 200 + 20_000 + 200_000);
    let store = db_size(src.store_dir(), "source.db");
    let library = db_size(lib.dir(), "library.db");
    eprintln!(
        "PERF index_200k: first walk {:.1} s ({:.0} records/s), unchanged re-walk {:.1} s, \
         source.db {:.1} MB, library.db {:.2} MB, peak RSS {} MB",
        first.as_secs_f64(),
        records as f64 / first.as_secs_f64(),
        rewalk.as_secs_f64(),
        store as f64 / 1e6,
        library as f64 / 1e6,
        peak_rss().map_or(-1, |b| (b / 1_048_576) as i64)
    );
    assert!(first < Duration::from_secs(60), "first walk {first:?}");
    assert!(rewalk < Duration::from_secs(60), "re-walk {rewalk:?}");
    assert!(store < 200_000_000, "source.db {store} bytes");
}

/// A watched source takes in a burst of 10,000 new files (10 folders of 1,000): from the
/// first write to every file indexed.
#[test]
#[ignore = "release measurement: a 10,000-file watcher burst"]
fn perf_watcher_burst_10k() {
    let files = tempfile::tempdir().unwrap();
    for d in 0..10 {
        std::fs::create_dir(files.path().join(format!("d{d}"))).unwrap();
    }
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "watch").unwrap();
    lib.set_hash_after_walk(false);
    let id = lib.add_source(folder("w", files.path())).unwrap();
    lib.watch(&id).unwrap();
    let src = lib.source(&id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while src.generation.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "first walk");
        std::thread::sleep(Duration::from_millis(20));
    }
    let start = Instant::now();
    for i in 0..10_000 {
        let p = files.path().join(format!("d{}/new {i:05}.txt", i % 10));
        std::fs::write(p, b"x").unwrap();
    }
    let written = start.elapsed();
    let want = 1 + 10 + 10_000;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut seen = 0;
    while seen < want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        seen = count(&lib, "SELECT count(*) FROM record");
    }
    let took = start.elapsed();
    eprintln!(
        "PERF watcher_burst_10k: written in {written:?}, all indexed after {took:?} \
         ({seen} of {want} records)"
    );
    assert_eq!(seen, want, "events lost");
    assert!(took < Duration::from_secs(30), "{took:?}");
    lib.unwatch(&id);
}

/// Hashing 10,000 files of 1 MiB (1,000 of them copies of others, hashed whole), local,
/// idle-only off.
#[test]
#[ignore = "release measurement: 10 GiB of files under target/"]
fn perf_hash_10k_files_of_1mib() {
    let root = fixture("perf-hash-10k", |dir| {
        let mut body = vec![0u8; 1 << 20];
        for (i, b) in body.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        for i in 0..10_000u64 {
            let sub = dir.join(format!("d{}", i / 1_000));
            std::fs::create_dir_all(&sub).unwrap();
            // The last 1,000 repeat the first 1,000.
            body[..8].copy_from_slice(&(i % 9_000).to_le_bytes());
            std::fs::write(sub.join(format!("f{i:05}.bin")), &body).unwrap();
        }
    });
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "hash").unwrap();
    lib.set_pause_on_battery(false);
    lib.set_hash_idle_only(false);
    lib.set_hash_after_walk(false);
    let id = lib.add_source(folder("h", &root)).unwrap();
    walk(&lib.source(&id).unwrap(), &lib.router()).unwrap();
    let start = Instant::now();
    let job = lib.hash().unwrap();
    let info = lib.jobs().wait(job).unwrap();
    let took = start.elapsed();
    assert_eq!(info.status, crate::JobStatus::Done, "{}", info.log);
    let hashed = count(
        &lib,
        "SELECT count(*) FROM record WHERE sampled_hash IS NOT NULL",
    );
    let whole = count(&lib, "SELECT count(*) FROM record WHERE cas_id IS NOT NULL");
    let dups = lib.duplicates(0).unwrap();
    eprintln!(
        "PERF hash_10k_1mib: {took:?} ({:.0} files/s), {hashed} sampled, {whole} with a \
         content id, {} duplicate groups",
        10_000.0 / took.as_secs_f64(),
        dups.len()
    );
    assert_eq!(hashed, 10_000);
    assert_eq!(dups.len(), 1_000);
    assert!(took < Duration::from_secs(60), "{took:?}");
}

/// 200,000 generated records (`fake://perf/`: 200 folders of 1,000 files, never on disk).
fn library_200k() -> (Router, tempfile::TempDir, Library, Arc<crate::Source>) {
    const WORDS: [&str; 10] = [
        "invoice", "budget", "report", "notes", "photo", "scan", "contract", "draft", "final",
        "summary",
    ];
    let router = Router::new();
    router.register(Arc::new(fake(|path: &str| {
        if path == "/" {
            return Ok((0..200).map(|d| (format!("d{d:03}"), true, 0)).collect());
        }
        let d: u64 = path.trim_start_matches("/d").parse()?;
        Ok((0..1_000u64)
            .map(|i| {
                let n = (d * 1_000 + i).wrapping_mul(2_654_435_761);
                let name = format!(
                    "{} {} w{} {i:04}.pdf",
                    WORDS[(n % 10) as usize],
                    WORDS[((n >> 8) % 10) as usize],
                    (n >> 16) % 5_000
                );
                (name, false, n % 1_000_000)
            })
            .collect())
    })));
    let (data, lib, src) = library_with(SourceDef {
        label: "perf".into(),
        root: VPath::parse("fake://perf/").unwrap(),
        kind: SourceKind::Share,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
        hash_shares: false,
    });
    walk(&src, &router).unwrap();
    (router, data, lib, src)
}

/// `library.search` over 200,000 records: name queries and `tag:` queries (2,000 tagged
/// records), p50/p95 of 50 runs each.
#[test]
#[ignore = "release measurement: library search over 200,000 records"]
fn perf_search_200k() {
    let (_router, _data, lib, src) = library_200k();
    let ids: Vec<i64> = {
        let c = src.store.get().unwrap();
        let mut stmt = c
            .prepare("SELECT id FROM record WHERE kind = 0 AND id % 100 = 0")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    let work = lib.create_tag("work", None, None).unwrap();
    let refs: Vec<RecordRef> = ids
        .iter()
        .map(|&id| RecordRef {
            source: src.id.clone(),
            id,
        })
        .collect();
    let start = Instant::now();
    lib.set_tag(work, &refs, true).unwrap();
    let tagging = start.elapsed();
    let queries = [
        "w4242",
        "invoice budget",
        "contract w42",
        "tag:work",
        "tag:work invoice",
        "tag:work ext:pdf",
    ];
    let mut lines = Vec::new();
    for q in queries {
        let parsed = LibraryQuery::parse(q, 0).unwrap();
        let hits = lib.search(&parsed).unwrap().len();
        assert!(hits > 0, "{q} found nothing");
        let runs = (0..50)
            .map(|_| {
                let t = Instant::now();
                lib.search(&parsed).unwrap();
                t.elapsed()
            })
            .collect();
        let (p50, p95) = percentiles(runs);
        lines.push(format!("{q:?} p50 {:.2} ms p95 {:.2} ms", ms(p50), ms(p95)));
        assert!(p95 < Duration::from_millis(50), "{q}: p95 {p95:?}");
    }
    eprintln!(
        "PERF search_200k: tagging {} records {tagging:?}; {}",
        refs.len(),
        lines.join("; ")
    );
}

/// Duplicates and the protection recount over 200,000 hashed records (50,000 contents,
/// four files each).
#[test]
#[ignore = "release measurement: duplicates and recount over 200,000 records"]
fn perf_duplicates_and_recount_200k() {
    let (_router, _data, lib, src) = library_200k();
    src.store
        .get()
        .unwrap()
        .execute_batch(
            "UPDATE record SET cas_id = CAST(printf('%032d', id % 50000) AS BLOB),
                 sampled_hash = CAST(printf('%032d', id % 50000) AS BLOB),
                 size = 1000 + id % 50000 WHERE kind = 0",
        )
        .unwrap();
    let start = Instant::now();
    let groups = lib.duplicates(0).unwrap();
    let dups = start.elapsed();
    assert_eq!(groups.len(), 50_000);
    let start = Instant::now();
    lib.recount_protection().unwrap();
    let recount = start.elapsed();
    let summary = lib.protection_summary().unwrap();
    eprintln!(
        "PERF duplicates_200k: {dups:?} ({} groups); protection recount {recount:?} \
         (single copy {})",
        groups.len(),
        summary.single_copy
    );
    assert!(dups < Duration::from_secs(5), "duplicates {dups:?}");
    assert!(recount < Duration::from_secs(5), "recount {recount:?}");
}
