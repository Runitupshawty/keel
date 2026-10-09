use super::*;
use crate::index::tests::{id_of, walk, write};
use crate::library::tests::folder;
use crate::SourceDef;
use keel_search::Searcher;
use std::time::{Duration, Instant};

/// 2026-10-09 12:00 UTC.
const NOW: i64 = 1_791_547_200;

#[test]
fn parses_an_everything_like_query() {
    let q = LibraryQuery::parse_at(
        r#"ext:pdf;.DOCX size:>1mb dm:2026-10 tag:work "exact phrase" in:"My Drive" taxes 2025"#,
        NOW,
        0,
    )
    .unwrap();
    assert_eq!(q.terms, ["taxes", "2025"]);
    assert_eq!(q.phrases, ["exact phrase"]);
    assert_eq!(q.ext, ["pdf", "docx"]);
    assert_eq!((q.min_size, q.max_size), (Some((1 << 20) + 1), None));
    assert_eq!(
        (q.modified_from, q.modified_before),
        (
            Some(days_from_civil(2026, 10, 1) * 86_400),
            Some(days_from_civil(2026, 11, 1) * 86_400)
        )
    );
    assert_eq!(q.tags, ["work"]);
    assert_eq!(q.sources, ["My Drive"]);
    assert_eq!(q.max, DEFAULT_MAX);

    let q = LibraryQuery::parse_at("size:1kb..2kb dm:today folder: kind:image", NOW, 0).unwrap();
    assert_eq!((q.min_size, q.max_size), (Some(1024), Some(2048)));
    let today = NOW - NOW % 86_400;
    assert_eq!(
        (q.modified_from, q.modified_before),
        (Some(today), Some(today + 86_400))
    );
    assert_eq!(q.kind, Some(KindFilter::Image));
    let q = LibraryQuery::parse_at("size:<=10kb dm:<2026 C:\\x -", NOW, 0).unwrap();
    assert_eq!((q.min_size, q.max_size), (None, Some(10 * 1024)));
    assert_eq!(
        q.modified_before,
        Some(days_from_civil(2026, 1, 1) * 86_400)
    );
    assert_eq!(
        q.terms,
        ["C:\\x"],
        "unknown keys are words; symbols alone are dropped"
    );
    assert!(LibraryQuery::parse("size:lots", 0).is_err());
    assert!(LibraryQuery::parse("dm:2026-13", 0).is_err());
    assert!(LibraryQuery::parse("kind:smell", 0).is_err());
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
}

struct Fixture {
    _files: tempfile::TempDir,
    _data: tempfile::TempDir,
    lib: Library,
    a: Arc<Source>,
    b: Arc<Source>,
}

fn fixture() -> Fixture {
    let files = tempfile::tempdir().unwrap();
    let (ra, rb) = (files.path().join("a"), files.path().join("b"));
    write(&ra.join("Reports/Q3 report.pdf"), "pdf");
    write(&ra.join("Reports/old report.pdf"), "an old pdf");
    write(&ra.join("notes.txt"), "n");
    write(&rb.join("report draft.docx"), "docx bytes");
    write(&rb.join("photos/beach.jpg"), "jpg");
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "s").unwrap();
    let a = lib
        .source(&lib.add_source(folder("A", &ra)).unwrap())
        .unwrap();
    let b = lib
        .source(&lib.add_source(folder("My Drive", &rb)).unwrap())
        .unwrap();
    walk(&a, &lib.router()).unwrap();
    walk(&b, &lib.router()).unwrap();
    let set_mtime = |src: &Source, rel: &str, t: i64| {
        src.store
            .get()
            .unwrap()
            .execute(
                "UPDATE record SET mtime = ?2 WHERE id = ?1",
                rusqlite::params![id_of(src, rel).unwrap(), t * 1_000_000_000],
            )
            .unwrap();
    };
    set_mtime(
        &a,
        "Reports/Q3 report.pdf",
        days_from_civil(2026, 9, 15) * 86_400,
    );
    set_mtime(
        &a,
        "Reports/old report.pdf",
        days_from_civil(2024, 1, 1) * 86_400,
    );
    set_mtime(
        &b,
        "report draft.docx",
        days_from_civil(2026, 10, 5) * 86_400,
    );
    Fixture {
        _files: files,
        _data: data,
        lib,
        a,
        b,
    }
}

fn names(lib: &Library, q: &str) -> Vec<String> {
    lib.search(&LibraryQuery::parse(q, 0).unwrap())
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect()
}

#[test]
fn searches_every_source_with_filters() {
    let f = fixture();
    let lib = &f.lib;
    let mut all = names(lib, "report");
    all.sort();
    // Prefix match on name and path words: the folder, and the files inside it by path.
    assert_eq!(
        all,
        [
            "Q3 report.pdf",
            "Reports",
            "old report.pdf",
            "report draft.docx"
        ]
    );
    assert_eq!(names(lib, "ext:pdf"), ["Q3 report.pdf", "old report.pdf"]);
    assert_eq!(names(lib, "ext:docx;jpg kind:file").len(), 2);
    assert_eq!(names(lib, "kind:image"), ["beach.jpg"]);
    assert_eq!(names(lib, "kind:image ext:pdf"), Vec::<String>::new());
    assert_eq!(names(lib, "folder: report"), ["Reports"]);
    assert_eq!(names(lib, "\"q3 report\""), ["Q3 report.pdf"]);
    assert_eq!(names(lib, "rep dra"), ["report draft.docx"]);
    assert_eq!(names(lib, "in:\"my drive\" report"), ["report draft.docx"]);
    assert_eq!(names(lib, &format!("in:{} report", f.a.id.0)).len(), 3);
    assert_eq!(names(lib, "size:>5 ext:pdf"), ["old report.pdf"]);
    assert_eq!(names(lib, "size:3 report"), ["Q3 report.pdf"]);
    assert_eq!(
        names(lib, "dm:2026-10 report ext:docx;pdf"),
        ["report draft.docx"]
    );
    assert_eq!(names(lib, "dm:2026-10-05"), ["report draft.docx"]);
    assert_eq!(names(lib, "nothing-like-this"), Vec::<String>::new());
    // Only filters: newest first.
    let mut q = LibraryQuery::parse("kind:file", 0).unwrap();
    q.max = 2;
    assert_eq!(lib.search(&q).unwrap().len(), 2);
}

#[test]
fn ranking_prefers_name_matches_then_recent_files() {
    let f = fixture();
    // Same bm25 for both pdfs; the recent one wins.
    assert_eq!(
        names(&f.lib, "ext:pdf report"),
        ["Q3 report.pdf", "old report.pdf"]
    );
    // A name match outranks a path-only match.
    write(&f._files.path().join("a/Reports/summary.txt"), "s");
    walk(&f.a, &f.lib.router()).unwrap();
    let hits = names(&f.lib, "reports");
    assert_eq!(hits[0], "Reports");
    assert!(hits.contains(&"summary.txt".to_owned()));
}

#[test]
fn hits_carry_the_source_status_and_offline_sources_still_answer() {
    let f = fixture();
    *f.b.status.write() = SourceStatus::Offline {
        last_seen: Some(42),
        reason: crate::OfflineReason::Unreachable,
    };
    let hits = f
        .lib
        .search(&LibraryQuery::parse("draft", 0).unwrap())
        .unwrap();
    assert_eq!(hits.len(), 1);
    let h = &hits[0];
    assert_eq!(
        h.status,
        SourceStatus::Offline {
            last_seen: Some(42),
            reason: crate::OfflineReason::Unreachable,
        }
    );
    assert_eq!(h.source_label, "My Drive");
    assert_eq!(h.record.source, f.b.id);
    assert_eq!(h.path, f.b.absolute("report draft.docx"));
    assert_eq!((h.is_dir, h.size), (false, 10));
    let searcher = LibrarySearcher(Arc::new(f.lib));
    assert_eq!(
        searcher.status().as_deref(),
        Some("1 offline source(s): last indexed contents")
    );
}

#[test]
fn tag_filter_uses_the_source_copy_of_the_tags() {
    let f = fixture();
    let c = f.a.store.get().unwrap();
    c.execute_batch(&format!(
        "INSERT INTO tag(id, name, color, parent) VALUES (10, 'Work', NULL, NULL), (11, 'client', NULL, 10);
         INSERT INTO record_tag(record, tag) VALUES ({}, 11), ({}, 10);",
        id_of(&f.a, "Reports/Q3 report.pdf").unwrap(),
        id_of(&f.a, "notes.txt").unwrap(),
    ))
    .unwrap();
    let mut work = names(&f.lib, "tag:work");
    work.sort();
    assert_eq!(work, ["Q3 report.pdf", "notes.txt"], "nested tags included");
    assert_eq!(names(&f.lib, "tag:client"), ["Q3 report.pdf"]);
    assert_eq!(
        names(&f.lib, "tag:client tag:work notes"),
        Vec::<String>::new()
    );
}

#[test]
fn library_searcher_adapter() {
    let f = fixture();
    let searcher = LibrarySearcher(Arc::new(f.lib));
    assert!(searcher.available());
    assert_eq!(searcher.name(), "Library");
    assert_eq!(searcher.status(), None);
    let hits = searcher
        .query(&keel_search::Query {
            text: "report".into(),
            folders_only: true,
            ..keel_search::Query::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].is_dir);
    assert_eq!(hits[0].path, f.a.absolute("Reports"));
    let hits = searcher
        .query(&keel_search::Query {
            text: "ext:pdf".into(),
            max: 1,
            ..keel_search::Query::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].modified.is_some());
    assert!(searcher
        .query(&keel_search::Query {
            text: "size:huge".into(),
            ..keel_search::Query::default()
        })
        .is_err());
}

/// Review focus 4: ranked library queries on 2,002,001 records answer in < 50 ms.
/// `cargo test -p keel-core --release -- --ignored two_million_row_search`
#[test]
#[ignore]
fn two_million_row_search_is_fast() {
    use crate::index::tests::{two_million, FILES};
    let (router, _data, lib, src) = two_million();
    walk(&src, &router).unwrap();
    let word = |n: u64| n.wrapping_mul(2_654_435_761) % 5_000;
    let queries = [
        "w4242".to_owned(),
        "d1234 f0999".to_owned(),
        format!("\"f0999 w{}\"", word(1234 * FILES + 999)),
        // 44,400 matches: ranked.
        "w42".to_owned(),
        // 200,000 matches: over the rank cap.
        "f09".to_owned(),
        "w4242 ext:dat size:>10kb".to_owned(),
        "f0999 kind:file in:perf".to_owned(),
        // Filters only.
        "kind:folder".to_owned(),
        "size:>99990".to_owned(),
        "size:<10 kind:file".to_owned(),
        "dm:2023".to_owned(),
        "ext:dat".to_owned(),
        "kind:file".to_owned(),
    ];
    let mut timings = Vec::new();
    for q in &queries {
        let q = LibraryQuery::parse(q, 0).unwrap();
        lib.search(&q).unwrap(); // warm the statement cache
        let start = Instant::now();
        let hits = lib.search(&q).unwrap();
        timings.push((hits.len(), start.elapsed()));
    }
    eprintln!("2M library search: {timings:?}");
    for ((hits, t), q) in timings.iter().zip(&queries) {
        assert!(*hits > 0, "{q} found nothing");
        assert!(*t < Duration::from_millis(50), "{q} took {t:?}");
    }
}

/// Review focus 4 with realistic paths: 2,000,000 files five folders deep
/// (`user3/Documents/client 7/2026-04/invoice budget 042.pdf`), common words in names and
/// paths; ranked and capped queries answer in < 50 ms.
/// `cargo test -p keel-core --release -- --ignored realistic_paths`
#[test]
#[ignore]
fn realistic_paths_search_is_fast() {
    use crate::index::tests::{fake, library_with};
    const AREAS: [&str; 4] = ["Documents", "Projects", "Pictures", "Downloads"];
    const WORDS: [&str; 20] = [
        "invoice", "budget", "report", "notes", "photo", "scan", "contract", "draft", "final",
        "summary", "meeting", "plan", "receipt", "letter", "quote", "estimate", "backup", "export",
        "minutes", "review",
    ];
    const EXT: [&str; 5] = ["pdf", "docx", "jpg", "txt", "xlsx"];
    let router = keel_vfs::Router::new();
    router.register(Arc::new(fake(|path: &str| {
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let dirs = |names: Vec<String>| Ok(names.into_iter().map(|n| (n, true, 0)).collect());
        match segs.len() {
            0 => dirs((0..10).map(|u| format!("user{u}")).collect()),
            1 => dirs(AREAS.iter().map(|a| a.to_string()).collect()),
            2 => dirs((0..10).map(|c| format!("client {c}")).collect()),
            3 => dirs((1..=5).map(|m| format!("2026-0{m}")).collect()),
            _ => {
                let seed = path
                    .bytes()
                    .fold(7u64, |h, b| h.wrapping_mul(31) ^ u64::from(b));
                Ok((0..1_000u64)
                    .map(|i| {
                        let n = (seed ^ i).wrapping_mul(2_654_435_761);
                        // 20 real words plus 180 rarer ones: about 1% of names hold a word.
                        let word = |k: u64| match k % 200 {
                            w if w < 20 => WORDS[w as usize].to_owned(),
                            w => format!("term{w}"),
                        };
                        let name = format!(
                            "{} {} {i:03}.{}",
                            word(n >> 7),
                            word(n >> 17),
                            EXT[(n >> 27) as usize % 5]
                        );
                        (name, false, n % 1_000_000)
                    })
                    .collect())
            }
        }
    })));
    let (_data, lib, src) = library_with(SourceDef {
        label: "home".into(),
        root: VPath::parse("fake://home/").unwrap(),
        kind: crate::SourceKind::Share,
        include_hidden: false,
        ignore: Vec::new(),
        poll_secs: None,
        hash_shares: false,
    });
    let start = Instant::now();
    walk(&src, &router).unwrap();
    eprintln!("realistic 2M index: {:?}", start.elapsed());
    let queries = [
        "invoice",
        "invoice budget",
        "\"invoice budget\"",
        "documents invoice",
        "client 7 report",
        "2026-03 receipt",
        "ext:pdf contract",
        "inv",
        "documents",
        "term42 kind:file size:>500kb",
        "user3 projects final size:<100kb",
        "client invoice",
    ];
    let mut timings = Vec::new();
    for q in queries {
        let q = LibraryQuery::parse(q, 0).unwrap();
        lib.search(&q).unwrap();
        let start = Instant::now();
        let hits = lib.search(&q).unwrap();
        timings.push((hits.len(), start.elapsed()));
    }
    eprintln!("realistic 2M search: {timings:?}");
    for ((hits, t), q) in timings.iter().zip(queries) {
        assert!(*hits > 0, "{q} found nothing");
        assert!(*t < Duration::from_millis(50), "{q} took {t:?}");
    }
}

/// Review item 16: `dm:` days are local days for the offset passed; `size:<0` matches
/// nothing (not every size).
#[test]
fn dates_are_local_and_negative_sizes_match_nothing() {
    // UTC-5: at 2026-10-09 03:00 UTC it is still the 8th.
    let at = days_from_civil(2026, 10, 9) * 86_400 + 3 * 3_600;
    let q = LibraryQuery::parse_at("dm:today", at, -5 * 3_600).unwrap();
    let local_8th = days_from_civil(2026, 10, 8) * 86_400 + 5 * 3_600;
    assert_eq!(
        (q.modified_from, q.modified_before),
        (Some(local_8th), Some(local_8th + 86_400))
    );
    let q = LibraryQuery::parse_at("dm:2026-10-09", at, 2 * 3_600).unwrap();
    assert_eq!(
        q.modified_from,
        Some(days_from_civil(2026, 10, 9) * 86_400 - 2 * 3_600)
    );
    let f = fixture();
    assert!(names(&f.lib, "size:<0").is_empty());
    assert!(names(&f.lib, "size:<0 report").is_empty());
    write(&f._files.path().join("a/empty.txt"), "");
    walk(&f.a, &f.lib.router()).unwrap();
    assert_eq!(names(&f.lib, "size:<=0 kind:file"), ["empty.txt"]);
}

/// Review item 5: a query too broad to rank (over 50,000 word matches) still applies its
/// filters before taking candidates: a rare match is found.
#[test]
fn a_rare_filter_finds_its_match_among_too_many_word_matches() {
    let (_data, lib, src) =
        crate::index::tests::library_with(folder("big", std::path::Path::new("/nowhere")));
    let now_ns = crate::now() * 1_000_000_000;
    let old_ns = (crate::now() - 400 * 86_400) * 1_000_000_000;
    src.store
        .get()
        .unwrap()
        .execute_batch(&format!(
            "INSERT INTO record(id, parent, name, path, kind, fs_id, gen)
                 VALUES (1, NULL, 'big', '', 1, 'r', 1);
             WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i + 1 FROM n WHERE i < 51000)
             INSERT INTO record(id, parent, name, path, kind, size, mtime, fs_id, gen)
                 SELECT i, 1, 'photos ' || i || '.jpg', 'photos ' || i || '.jpg', 0, 1,
                     {old_ns}, 'h:' || i, 1 FROM n;
             INSERT INTO record(id, parent, name, path, kind, size, mtime, fs_id, gen)
                 VALUES (51001, 1, 'photos new.jpg', 'photos new.jpg', 0, 1, {now_ns}, 'h:n', 1);"
        ))
        .unwrap();
    let q = LibraryQuery::parse("dm:today photos", 0).unwrap();
    let hits: Vec<String> = lib
        .search(&q)
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect();
    assert_eq!(hits, ["photos new.jpg"]);
}
