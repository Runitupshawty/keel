use super::*;
use crate::index::tests::{eventually, walk, write};
use crate::library::tests::{folder, remote};
use crate::{JobStatus, LibraryQuery};
use std::time::Duration;

/// A text reader for tests: `.txt` and `.md` are their text (cut at the caps), `.doc` is
/// damaged, `.slow` sleeps past its time, `.lag` is text read in 5 ms, `.bin` is no text.
pub(crate) fn extractor() -> TextExtractor {
    TextExtractor {
        extensions: &["txt", "md", "doc", "slow", "lag", "bin"],
        extract: Arc::new(|path, ext, caps| {
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if size > caps.max_bytes {
                return TextOutcome::SkippedSize;
            }
            match ext {
                "doc" => TextOutcome::Failed("not a valid Word document".into()),
                "bin" => TextOutcome::SkippedKind,
                "slow" => {
                    std::thread::sleep(caps.time + ABANDON_AFTER + Duration::from_millis(300));
                    TextOutcome::Done {
                        text: "late".into(),
                        truncated: false,
                    }
                }
                _ => {
                    if ext == "lag" {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let text = std::fs::read_to_string(path).unwrap_or_default();
                    let truncated = text.chars().count() > caps.max_chars;
                    TextOutcome::Done {
                        text: text.chars().take(caps.max_chars).collect(),
                        truncated,
                    }
                }
            }
        }),
    }
}

fn library(data: &Path, root: &Path) -> (Library, SourceId) {
    let lib = Library::open(data, "t").unwrap();
    lib.set_pause_on_battery(false);
    lib.set_hash_after_walk(false);
    lib.set_text_extractor(extractor());
    let id = lib.add_source(folder("docs", root)).unwrap();
    walk(&lib.source(&id).unwrap(), &lib.router()).unwrap();
    (lib, id)
}

/// Runs the source's text job to its end.
fn run(lib: &Library, id: &SourceId) -> crate::JobInfo {
    let job = lib.index_text(id).unwrap().expect("documents to read");
    let info = lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    info
}

fn rows(lib: &Library, id: &SourceId) -> i64 {
    lib.source(id)
        .unwrap()
        .store
        .get()
        .unwrap()
        .query_row("SELECT count(*) FROM record_text", [], |r| r.get(0))
        .unwrap()
}

pub(crate) fn names(lib: &Library, q: &str) -> Vec<String> {
    let mut v: Vec<String> = lib
        .search(&LibraryQuery::parse(q, 0).unwrap())
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect();
    v.sort();
    v
}

#[test]
fn a_source_of_mixed_files_is_read_and_a_changed_file_read_again() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let f = files.path();
    for i in 0..20 {
        write(
            &f.join(format!("notes/n{i:02}.txt")),
            &format!("entry {i} zebra"),
        );
    }
    for i in 0..4 {
        write(&f.join(format!("m{i}.md")), "# Keel\nquarterly figures");
    }
    for i in 0..3 {
        write(&f.join(format!("old{i}.doc")), "damaged");
    }
    write(&f.join("blob.bin"), "\0\0");
    write(&f.join("empty.txt"), "");
    write(&f.join("photo.png"), "not read: no reader");
    let (lib, id) = library(data.path(), f);
    let info = run(&lib, &id);
    assert!(
        info.log
            .contains("29 documents read: 25 indexed (366 characters), 1 skipped, 3 failed"),
        "{}",
        info.log
    );
    assert!(info.log.contains("old0.doc: not a valid Word document"));
    assert_eq!(rows(&lib, &id), 29, "the png has no row");
    let stats = lib.stats();
    assert!(stats.index_contents);
    assert_eq!(
        stats.text,
        TextCounts {
            documents: 25,
            skipped: 1,
            failed: 3,
            chars: 366,
        }
    );
    assert!(stats.per_source[0].contents);
    assert_eq!(names(&lib, "zebra").len(), 20);
    assert_eq!(
        names(&lib, "quarterly"),
        ["m0.md", "m1.md", "m2.md", "m3.md"]
    );
    // Nothing left to read: no job.
    assert_eq!(lib.index_text(&id).unwrap(), None);

    // A changed file is read again (its size changed); the old words leave the index.
    std::thread::sleep(Duration::from_millis(20));
    write(&f.join("notes/n00.txt"), "entry 0 giraffe and more");
    let src = lib.source(&id).unwrap();
    walk(&src, &lib.router()).unwrap();
    let info = run(&lib, &id);
    assert!(
        info.log.contains("1 documents read: 1 indexed"),
        "{}",
        info.log
    );
    assert_eq!(names(&lib, "giraffe"), ["n00.txt"]);
    assert_eq!(names(&lib, "zebra").len(), 19);

    // A deleted file takes its text along (the record's delete trigger).
    std::fs::remove_file(f.join("m0.md")).unwrap();
    walk(&src, &lib.router()).unwrap();
    assert_eq!(rows(&lib, &id), 28);
    let left: i64 = src
        .store
        .get()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM record_content WHERE record_content MATCH 'quarterly'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 3);
}

#[test]
fn caps_skip_big_files_cut_long_ones_and_abandon_stuck_readers() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let f = files.path();
    write(&f.join("big.txt"), &"x".repeat(2_000));
    write(&f.join("long.md"), &"word ".repeat(100));
    write(&f.join("stuck.slow"), "zzz");
    let (lib, id) = library(data.path(), f);
    lib.set_text_caps(TextCaps {
        max_bytes: 1_000,
        max_pdf_bytes: 1_000,
        max_chars: 50,
        time: Duration::from_millis(100),
    });
    let info = run(&lib, &id);
    assert!(info.log.contains("stuck.slow: timed out"), "{}", info.log);
    let src = lib.source(&id).unwrap();
    let status = |name: &str| -> (i64, i64, bool) {
        src.store
            .get()
            .unwrap()
            .query_row(
                "SELECT t.status, t.chars, t.truncated FROM record_text t
                 JOIN record r ON r.id = t.record WHERE r.name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    };
    assert_eq!(status("big.txt"), (SKIPPED_SIZE, 0, false));
    assert_eq!(status("long.md"), (DONE, 50, true));
    assert_eq!(status("stuck.slow"), (FAILED, 0, false));
    // The abandoned reader ends on its own and is no longer counted.
    eventually("the stuck reader ends", || {
        lib.shared.text_abandoned.load(Ordering::SeqCst) == 0
    });
}

#[test]
fn a_stopped_job_resumes_from_its_checkpoint() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    // Plenty past the first checkpoint, so the job is still running when the test sees it.
    let n = CHECKPOINT_EVERY * 30;
    for i in 0..n {
        write(&files.path().join(format!("{i:03}.lag")), "resume words");
    }
    let (lib, id) = library(data.path(), files.path());
    let job = lib.index_text(&id).unwrap().unwrap();
    eventually("first checkpoint", || {
        rows(&lib, &id) >= CHECKPOINT_EVERY as i64
    });
    // Pause (user activity), then close: the job stops at its pause.
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
    drop(lib);

    let lib = Library::open(data.path(), "t").unwrap();
    lib.set_pause_on_battery(false);
    lib.set_text_extractor(extractor());
    assert_eq!(lib.jobs().info(job).unwrap().status, JobStatus::Running);
    assert_eq!(lib.jobs().resume_all().unwrap(), [job]);
    let info = lib.jobs().wait(job).unwrap();
    assert_eq!(info.status, JobStatus::Done, "{}", info.log);
    assert!(info.log.contains("resumed"));
    // Counted once each: continued after the checkpoint, not from the start.
    assert!(
        info.log.contains(&format!("{n} documents read")),
        "{}",
        info.log
    );
    assert_eq!(rows(&lib, &id), n as i64);
}

#[test]
fn activity_pauses_the_job() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    for i in 0..5 {
        write(&files.path().join(format!("{i}.txt")), "paused");
    }
    let (lib, id) = library(data.path(), files.path());
    lib.shared.busy_until.store(u64::MAX, Ordering::SeqCst);
    let job = lib.index_text(&id).unwrap().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(rows(&lib, &id), 0, "paused while the user is busy");
    // A walk meanwhile asks for another round instead of a second job.
    assert_eq!(lib.index_text(&id).unwrap(), None);
    lib.shared.busy_until.store(0, Ordering::SeqCst);
    assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
    assert_eq!(rows(&lib, &id), 5);
    assert!(lib.shared.text_jobs.lock().is_empty());
}

#[test]
fn the_switches_turn_reading_and_searching_off_and_on() {
    let data = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    write(&files.path().join("a.txt"), "walrus");
    let (lib, id) = library(data.path(), files.path());
    run(&lib, &id);
    assert_eq!(names(&lib, "walrus"), ["a.txt"]);

    // Off for the source: its text is deleted and no longer searched.
    lib.set_source_contents(&id, Some(false)).unwrap();
    assert_eq!(lib.source_contents(&id).unwrap(), (Some(false), false));
    assert_eq!(rows(&lib, &id), 0);
    assert!(names(&lib, "walrus").is_empty());
    assert_eq!(lib.index_text(&id).unwrap(), None, "nothing read while off");
    // Back to its default (on for a local folder): read again.
    lib.set_source_contents(&id, None).unwrap();
    assert_eq!(lib.source_contents(&id).unwrap(), (None, true));
    eventually("read again", || rows(&lib, &id) == 1);
    eventually("job ended", || lib.shared.text_jobs.lock().is_empty());
    assert_eq!(names(&lib, "walrus"), ["a.txt"]);

    // The library's setting off: every source's text goes; it stays off after a reopen.
    lib.set_index_contents(false).unwrap();
    assert_eq!(rows(&lib, &id), 0);
    assert!(!lib.stats().index_contents);
    drop(lib);
    let lib = Library::open(data.path(), "t").unwrap();
    lib.set_text_extractor(extractor());
    assert!(!lib.index_contents());
    assert!(lib.start_text_jobs().is_empty());
    lib.set_index_contents(true).unwrap();
    eventually("read again", || rows(&lib, &id) == 1);
}

#[test]
fn remote_sources_are_read_only_when_their_switch_is_on() {
    let data = tempfile::tempdir().unwrap();
    let lib = Library::open(data.path(), "t").unwrap();
    lib.set_pause_on_battery(false);
    lib.set_text_extractor(extractor());
    let mem = Arc::new(keel_vfs::memory::MemoryProvider::new());
    mem.put("/srv/far.txt", "remote okapi");
    mem.put("/srv/big.txt", vec![b'x'; 2_000]);
    lib.router()
        .register_remote_provider("box".into(), mem.clone());
    let id = lib.add_source(remote("server", "sftp://box/srv")).unwrap();
    let src = lib.source(&id).unwrap();
    walk(&src, &lib.router()).unwrap();
    // Off by default: nothing is downloaded.
    assert_eq!(lib.source_contents(&id).unwrap(), (None, false));
    assert_eq!(lib.index_text(&id).unwrap(), None);
    assert!(!lib.stats().per_source[0].contents);
    // On: read through the provider (a copy in the store's folder, removed after).
    lib.set_text_caps(TextCaps {
        max_bytes: 1_000,
        ..TEXT_CAPS
    });
    lib.set_source_contents(&id, Some(true)).unwrap();
    eventually("read", || rows(&lib, &id) == 2);
    eventually("job ended", || lib.shared.text_jobs.lock().is_empty());
    assert_eq!(names(&lib, "okapi"), ["far.txt"]);
    let left: Vec<_> = std::fs::read_dir(src.store_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("text-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(
        lib.stats().text.skipped,
        1,
        "over the cap: never downloaded"
    );
}

// --- the search (Task 62) ---

/// Four documents: `ferry.txt` has the word in its name, two others in their text.
fn ferries(files: &Path) -> (tempfile::TempDir, Library, SourceId) {
    write(
        &files.join("budget.txt"),
        "Keel quarterly budget for the ferry",
    );
    write(&files.join("ferry.txt"), "timetable");
    write(
        &files.join("notes.md"),
        "The ferry leaves at noon, NEAR the dock (or not).",
    );
    write(&files.join("other.txt"), "nothing here");
    let data = tempfile::tempdir().unwrap();
    let (lib, id) = library(data.path(), files);
    run(&lib, &id);
    (data, lib, id)
}

#[test]
fn words_match_names_and_contents_and_terms_restrict_them() {
    let files = tempfile::tempdir().unwrap();
    let (_data, lib, _) = ferries(files.path());
    assert_eq!(
        names(&lib, "ferry"),
        ["budget.txt", "ferry.txt", "notes.md"]
    );
    // The name hit ranks first; content hits carry a snippet.
    let hits = lib
        .search(&LibraryQuery::parse("ferry", 0).unwrap())
        .unwrap();
    assert_eq!(hits[0].name, "ferry.txt");
    assert!(hits[0].snippet.is_none());
    for h in &hits[1..] {
        let s = h.snippet.as_ref().expect("a content hit has a snippet");
        assert!(s.text.chars().count() <= crate::search::SNIPPET_CHARS);
        assert!(
            s.marks
                .iter()
                .any(|&(a, b)| s.text[a..b].eq_ignore_ascii_case("ferry")),
            "{s:?}"
        );
    }
    assert_eq!(names(&lib, "name:ferry"), ["ferry.txt"]);
    assert_eq!(names(&lib, "content:ferry"), ["budget.txt", "notes.md"]);
    assert_eq!(
        names(&lib, "ferry -content:noon"),
        ["budget.txt", "ferry.txt"]
    );
    assert_eq!(names(&lib, "-content:ferry ext:md"), Vec::<String>::new());
    assert_eq!(names(&lib, "ferry contents:off"), ["ferry.txt"]);
    assert_eq!(names(&lib, "\"ferry leaves\""), ["notes.md"]);
    assert_eq!(names(&lib, "content:\"leaves at noon\""), ["notes.md"]);
    // The last word is a prefix (3 characters or more); `content:` words are whole.
    assert_eq!(names(&lib, "quarte"), ["budget.txt"]);
    assert!(names(&lib, "content:quarte").is_empty());
    assert_eq!(names(&lib, "content:ferry name:notes"), ["notes.md"]);
    assert_eq!(names(&lib, "content:dock kind:document"), ["notes.md"]);
    // Snippets only for the first hits asked for.
    let mut q = LibraryQuery::parse("content:ferry", 0).unwrap();
    q.snippets = 1;
    let hits = lib.search(&q).unwrap();
    assert_eq!(hits.iter().filter(|h| h.snippet.is_some()).count(), 1);
}

#[test]
fn fts5_operators_typed_by_the_user_are_plain_words() {
    let files = tempfile::tempdir().unwrap();
    let (_data, lib, id) = ferries(files.path());
    let src = lib.source(&id).unwrap();
    for text in [
        "NEAR(",
        "NEAR(ferry dock)",
        "\"ferry",
        "ferry\"",
        "a\"b\"c",
        "*",
        "ferry*",
        "OR",
        "ferry OR budget",
        "AND NOT",
        "^ferry",
        "(ferry",
        "ferry:",
        "{text}: ferry",
        "content:*",
        "content:\"\"",
        "content:NEAR(",
        "-content:\"",
        "name:\"",
        "-content:OR content:AND",
        "\u{E000}ferry\u{E001}",
    ] {
        let q = LibraryQuery::parse(text, 0).unwrap();
        let hits = crate::search::search_source(&src, &q, crate::now(), true);
        assert!(hits.is_ok(), "{text}: {hits:?}");
    }
    // Operators are words: `OR` does not widen, `NEAR(ferry dock)` is the words.
    assert!(names(&lib, "ferry OR budget").is_empty());
    assert_eq!(names(&lib, "NEAR(the dock)"), ["notes.md"]);
}

#[test]
fn snippets_are_one_line_and_bounded() {
    let raw = format!(
        "  first\n\n line {}{}{} and {} \t end",
        crate::search::MARK_ON,
        "word",
        crate::search::MARK_OFF,
        "x".repeat(400)
    );
    let s = crate::search::Snippet::from_marked(&raw);
    assert!(s.text.starts_with("first line word and x"), "{s:?}");
    assert_eq!(s.marks, [(11, 15)]);
    assert_eq!(&s.text[11..15], "word");
    assert_eq!(s.text.chars().count(), crate::search::SNIPPET_CHARS);
    assert!(s.text.ends_with('\u{2026}'));
    // A long document: the snippet stays short.
    let files = tempfile::tempdir().unwrap();
    let body = format!(
        "{} narwhal {}",
        "filler ".repeat(3_000),
        "tail ".repeat(3_000)
    );
    write(&files.path().join("long.txt"), &body);
    let data = tempfile::tempdir().unwrap();
    let (lib, id) = library(data.path(), files.path());
    run(&lib, &id);
    let hits = lib
        .search(&LibraryQuery::parse("narwhal", 0).unwrap())
        .unwrap();
    let s = hits[0].snippet.as_ref().unwrap();
    assert!(
        s.text.chars().count() <= crate::search::SNIPPET_CHARS,
        "{s:?}"
    );
    assert!(s.text.contains("narwhal"));
}
