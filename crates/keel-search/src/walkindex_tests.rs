use super::*;
use crate::folder_index;
use crate::index_settings::{ExcludeSettings, Schedule};
use crate::schedule::fake::FakeClock;
use std::sync::atomic::AtomicU32;

struct Dirs {
    root: PathBuf,
    idx: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "keel-walkidx-{}-{}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        let (root, idx) = (base.join("root"), base.join("idx"));
        std::fs::create_dir_all(&root).unwrap();
        // macOS temp folders are behind a symlink (/var -> /private/var) the index resolves.
        #[cfg(target_os = "macos")]
        let root = root.canonicalize().unwrap();
        Self { root, idx }
    }

    fn file(&self, rel: &str, body: &[u8]) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn open(&self) -> WalkIndexSearcher {
        let s = WalkIndexSearcher::open(self.idx.clone(), vec![self.root.clone()]).unwrap();
        assert!(s.wait_ready(Duration::from_secs(60)));
        s
    }

    fn open_with(&self, opts: WalkOptions) -> WalkIndexSearcher {
        let s =
            WalkIndexSearcher::open_with(self.idx.clone(), vec![self.root.clone()], opts).unwrap();
        assert!(s.wait_ready(Duration::from_secs(60)));
        s
    }

    fn sample() -> Self {
        let d = Self::new();
        d.file("docs/Report.PDF", b"12345");
        d.file("docs/report notes.txt", b"x");
        d.file("docs/oldies/report.pdf", b"x");
        d.file("src/main.rs", b"fn main(){}");
        d.file(".hidden/report.secret", b"x");
        d
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
    }
}

fn q(text: &str) -> Query {
    Query {
        text: text.into(),
        ..Query::default()
    }
}

fn names(s: &WalkIndexSearcher, query: &Query) -> Vec<String> {
    s.query(query)
        .unwrap()
        .iter()
        .map(|h| h.path.name().to_owned())
        .collect()
}

/// Polls until the query's names equal `want` (the watcher batches for 500 ms).
fn settle(s: &WalkIndexSearcher, query: &Query, want: &[&str]) {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let got = s.query(query).map(|_| names(s, query)).unwrap_or_default();
        if got == want && s.available() {
            return;
        }
        assert!(Instant::now() < end, "wanted {want:?}, still {got:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until the index has finished `n` scans (rebuilds and rescans).
fn scans(s: &WalkIndexSearcher, n: usize) {
    let end = Instant::now() + Duration::from_secs(60);
    while s.shared.scans.load(Relaxed) < n {
        assert!(
            Instant::now() < end,
            "still {} scans",
            s.shared.scans.load(Relaxed)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Configured-root options: exclude rules, no watcher, a fake clock.
fn configured(f: impl FnOnce(&mut WalkOptions, &mut ExcludeSettings)) -> WalkOptions {
    let mut exclude = ExcludeSettings::default();
    let mut opts = WalkOptions {
        rules: None,
        max_age_secs: None,
        ..WalkOptions::default()
    };
    opts.scan.monitor = false;
    f(&mut opts, &mut exclude);
    opts.rules = Some(ExcludeRules::new(&exclude));
    opts
}

/// Renames, retrying while a closing handle still holds the folder (Windows).
fn rename(from: &Path, to: &Path) {
    let end = Instant::now() + Duration::from_secs(10);
    while let Err(e) = std::fs::rename(from, to) {
        assert!(Instant::now() < end, "rename {from:?}: {e}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn substring_glob_regex_and_filters() {
    let d = Dirs::sample();
    let s = d.open();
    // Exact name first, then prefix, then the rest; hidden entries are skipped.
    assert_eq!(
        names(&s, &q("report")),
        ["report.pdf", "report notes.txt", "Report.PDF"]
    );
    assert_eq!(names(&s, &q("*.pdf")), ["report.pdf", "Report.PDF"]);
    assert_eq!(names(&s, &q("report* pdf")), ["report.pdf", "Report.PDF"]);
    let re = Query {
        regex: true,
        ..q(r"^report\s")
    };
    assert_eq!(names(&s, &re), ["report notes.txt"]);
    let bad = Query {
        regex: true,
        ..q("(")
    };
    assert!(s.query(&bad).is_err());
    let case = Query {
        match_case: true,
        ..q("Report")
    };
    assert_eq!(names(&s, &case), ["Report.PDF"]);
    assert_eq!(names(&s, &q(r"regex:main\.rs$")), ["main.rs"]);
    assert_eq!(names(&s, &q("folder:o")), ["oldies", "docs"]);
    let within = format!(
        "report in:\"{}\"",
        d.root.join("docs").join("oldies").display()
    );
    assert_eq!(names(&s, &q(&within)), ["report.pdf"]);
    // A pane's folder scopes the same way, by whole folder names, in regex mode too.
    let scoped = |dir: PathBuf, text: &str, regex| Query {
        within: Some(dir),
        regex,
        ..q(text)
    };
    let oldies = d.root.join("docs").join("oldies");
    assert_eq!(
        names(&s, &scoped(oldies.clone(), "report", false)),
        ["report.pdf"]
    );
    assert_eq!(names(&s, &scoped(oldies, "^rep", true)), ["report.pdf"]);
    let partial = d.root.join("docs").join("old");
    assert!(names(&s, &scoped(partial, "report", false)).is_empty());
    let capped = Query {
        max: 2,
        ..q("report")
    };
    assert_eq!(names(&s, &capped).len(), 2);
    // Size and date come from the index, no file system call.
    let hit = &s.query(&case).unwrap()[0];
    assert_eq!((hit.size, hit.is_dir), (5, false));
    assert!(hit.modified.is_some());
}

/// The search toggles: Match Whole Words, Match Path, Match Diacritics and whole filename
/// wildcards.
#[test]
fn search_toggles() {
    let d = Dirs::sample();
    d.file("docs/café menu.txt", b"x");
    let s = d.open();
    let with = |text: &str, set: fn(&mut Query)| {
        let mut query = q(text);
        set(&mut query);
        names(&s, &query)
    };
    assert_eq!(names(&s, &q("note")), ["report notes.txt"]);
    assert!(with("note", |q| q.whole_word = true).is_empty());
    assert_eq!(with("notes", |q| q.whole_word = true), ["report notes.txt"]);
    assert_eq!(names(&s, &q("oldies")), ["oldies"]);
    assert_eq!(
        with("oldies", |q| q.match_path = true),
        ["oldies", "report.pdf"]
    );
    assert_eq!(
        with("oldies pdf", |q| q.match_path = true),
        ["report.pdf"],
        "every term, anywhere in the path"
    );
    assert_eq!(names(&s, &q("cafe")), ["café menu.txt"]);
    assert_eq!(names(&s, &q("CAFÉ")), ["café menu.txt"]);
    assert!(with("cafe", |q| q.match_diacritics = true).is_empty());
    assert_eq!(
        with("café", |q| q.match_diacritics = true),
        ["café menu.txt"]
    );
    assert!(names(&s, &q("*.pd")).is_empty());
    assert_eq!(
        with("*.pd", |q| q.whole_filename_wildcards = false),
        ["report.pdf", "Report.PDF"]
    );
}

#[test]
fn entries_store_created_time_and_attributes() {
    let d = Dirs::sample();
    let ro = d.root.join("src").join("main.rs");
    let mut perm = std::fs::metadata(&ro).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&ro, perm.clone()).unwrap();
    let s = d.open();
    let info = s.info(&ro).unwrap();
    assert!(!info.is_dir);
    assert_eq!(info.size, 11);
    assert_ne!(info.attributes & attr::READONLY, 0);
    assert_eq!(info.attributes & attr::DIRECTORY, 0);
    if cfg!(any(windows, target_os = "macos")) {
        assert!(info.created.is_some());
    }
    let dir = s.info(&d.root.join("docs")).unwrap();
    assert!(dir.is_dir);
    assert_ne!(dir.attributes & attr::DIRECTORY, 0);
    assert!(s.info(&d.root.join("nope")).is_none());
    // Columns survive the db round trip.
    drop(s);
    let s = d.open();
    assert_eq!(s.info(&ro), Some(info));
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    std::fs::set_permissions(&ro, perm).unwrap();
}

#[test]
fn rename_delete_and_create_show_up_after_the_watcher_batch() {
    let d = Dirs::sample();
    let s = d.open();
    assert!(!s.shared.unwatched.load(Relaxed), "watcher failed to start");
    let docs = d.root.join("docs");
    std::fs::rename(docs.join("report notes.txt"), docs.join("minutes.txt")).unwrap();
    std::fs::remove_file(docs.join("oldies/report.pdf")).unwrap();
    d.file("fresh/deep/report-new.md", b"x");
    settle(&s, &q("report"), &["Report.PDF", "report-new.md"]);
    settle(&s, &q("minutes"), &["minutes.txt"]);
    // A renamed folder takes its children along.
    std::fs::rename(d.root.join("fresh"), d.root.join("renamed")).unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let hits = s.query(&q("report-new")).unwrap();
        if hits.len() == 1 && hits[0].path.display().contains("renamed") {
            break;
        }
        assert!(Instant::now() < end, "still {:?}", hits.len());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::remove_dir_all(d.root.join("renamed")).unwrap();
    settle(&s, &q("report-new"), &[]);
    // Hidden files created later stay out.
    d.file(".hidden/more.txt", b"x");
    d.file("late.txt", b"x");
    settle(&s, &q("txt"), &["minutes.txt", "late.txt"]);
}

#[test]
fn restart_loads_the_db_and_rebuild_picks_up_offline_changes() {
    let d = Dirs::sample();
    {
        let s = d.open();
        d.file("watched.txt", b"x");
        settle(&s, &q("watched"), &["watched.txt"]);
        // Let the batch reach the db before closing.
        std::thread::sleep(Duration::from_millis(500));
    }
    std::fs::remove_file(d.root.join("src/main.rs")).unwrap();
    d.file("added-offline.txt", b"x");
    let s = d.open();
    // A fresh db is served as saved; the watcher only sees later changes.
    assert_eq!(names(&s, &q("watched")), ["watched.txt"]);
    assert_eq!(names(&s, &q("main.rs")), ["main.rs"]);
    s.rebuild();
    settle(&s, &q("main.rs"), &[]);
    settle(&s, &q("added-offline"), &["added-offline.txt"]);
    assert!(s.take_ready());
}

#[test]
fn a_db_older_than_a_week_rebuilds_on_open() {
    let d = Dirs::sample();
    drop(d.open());
    let db = db_path(&d.idx, &normalize_roots(vec![d.root.clone()]));
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE meta SET value = '1' WHERE key = 'built'", [])
        .unwrap();
    d.file("after.txt", b"x");
    let s = WalkIndexSearcher::open(d.idx.clone(), vec![d.root.clone()]).unwrap();
    settle(&s, &q("after.txt"), &["after.txt"]);
}

#[test]
fn a_db_of_the_old_layout_is_walked_again() {
    let d = Dirs::sample();
    drop(d.open());
    let db = db_path(&d.idx, &normalize_roots(vec![d.root.clone()]));
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM meta WHERE key = 'version'", [])
        .unwrap();
    assert!(load(&db, &normalize_roots(vec![d.root.clone()])).is_none());
    let s = d.open();
    assert_eq!(names(&s, &q("main.rs")), ["main.rs"]);
}

#[test]
fn folder_index_lists_folders_only() {
    let d = Dirs::sample();
    let s = d.open();
    let mut rel: Vec<String> = folder_index(&s)
        .unwrap()
        .iter()
        .map(|f| {
            f.replace('\\', "/")
                .rsplit("/root/")
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    rel.sort();
    // No files, no hidden folder, not the root itself.
    assert_eq!(rel, ["docs", "docs/oldies", "src"]);
}

#[test]
fn status_says_indexing_while_building() {
    let d = Dirs::sample();
    let s = d.open();
    assert_eq!(s.state(), SearchState::Ready);
    assert_eq!(s.status(), None);
    s.shared.building.store(true, Relaxed);
    s.shared.progress.store(1234, Relaxed);
    assert_eq!(s.status().as_deref(), Some("indexing 1234 files\u{2026}"));
    s.shared.ready.store(false, Relaxed);
    assert!(s.query(&q("a")).is_err());
    assert_eq!(s.state(), SearchState::Indexing { done: 1234 });
    s.shared.ready.store(true, Relaxed);
    s.shared.building.store(false, Relaxed);
}

#[test]
fn excludes_apply_at_index_time_and_a_rule_change_rebuilds() {
    let d = Dirs::sample();
    d.file("node_modules/pkg/index.js", b"x");
    d.file("docs/cache.tmp", b"x");
    d.file("photos/a.jpg", b"x");
    d.file("photos/b.txt", b"x");
    // No rules: hidden and git-ignored entries are kept for configured roots.
    let s = d.open_with(configured(|_, _| {}));
    assert_eq!(names(&s, &q("report.secret")), ["report.secret"]);
    assert_eq!(names(&s, &q("index.js")), ["index.js"]);
    drop(s);
    let s = d.open_with(configured(|_, x| {
        x.folders = vec!["node_modules".into()];
        x.files = vec!["*.tmp".into()];
        x.hidden = true;
    }));
    // The saved index was walked with other rules: walked again.
    settle(&s, &q("index.js"), &[]);
    assert!(names(&s, &q("cache")).is_empty());
    assert!(
        names(&s, &q("node_modules")).is_empty(),
        "the folder itself"
    );
    if cfg!(not(windows)) {
        assert!(
            names(&s, &q("report.secret")).is_empty(),
            "dot names are hidden"
        );
    }
    assert_eq!(names(&s, &q("a.jpg")), ["a.jpg"]);
    drop(s);
    let s = d.open_with(configured(|_, x| {
        let root = normalize_roots(vec![d.root.clone()]).remove(0);
        x.include_only_folders = vec![root.join("photos").display().to_string()];
        x.include_only_files = vec!["*.JPG".into()];
    }));
    settle(&s, &q("report"), &[]);
    assert_eq!(names(&s, &q("a")), ["a.jpg"]);
    assert_eq!(names(&s, &q("folder:")), ["photos"]);
    assert!(names(&s, &q("b.txt")).is_empty());
}

#[test]
fn excluded_files_created_later_stay_out() {
    let d = Dirs::sample();
    let s = d.open_with(configured(|o, x| {
        o.scan.monitor = true;
        x.files = vec!["*.tmp".into()];
        x.folders = vec!["build".into()];
    }));
    assert!(!s.shared.unwatched.load(Relaxed), "watcher failed to start");
    d.file("later.tmp", b"x");
    d.file("build/out.bin", b"x");
    d.file("later.txt", b"x");
    settle(&s, &q("later"), &["later.txt"]);
    assert!(names(&s, &q("out.bin")).is_empty());
}

#[test]
fn scheduled_rescans_follow_the_fake_clock() {
    let d = Dirs::sample();
    let clock = Arc::new(FakeClock::new(1_791_763_200, 0));
    let s = d.open_with(configured(|o, _| {
        o.scan.schedule = Schedule::EveryHours(1);
        o.clock = clock.clone();
    }));
    scans(&s, 1);
    d.file("new.txt", b"x");
    clock.advance(59 * 60);
    s.tick();
    assert_eq!(s.shared.scans.load(Relaxed), 1, "not due yet");
    assert!(names(&s, &q("new.txt")).is_empty(), "not watched");
    clock.advance(60);
    s.tick();
    assert_eq!(s.shared.scans.load(Relaxed), 2, "due after an hour");
    assert_eq!(names(&s, &q("new.txt")), ["new.txt"]);
    // The next one counts from that rescan.
    clock.advance(30 * 60);
    s.tick();
    assert_eq!(s.shared.scans.load(Relaxed), 2);
    // The time of the last scan is saved: a restart past it rescans at once.
    drop(s);
    clock.advance(2 * 3600);
    d.file("while-closed.txt", b"x");
    let s = d.open_with(configured(|o, _| {
        o.scan.schedule = Schedule::EveryHours(1);
        o.clock = clock.clone();
    }));
    s.tick();
    assert_eq!(names(&s, &q("while-closed")), ["while-closed.txt"]);
}

#[test]
fn fast_rescan_lists_only_folders_whose_time_moved() {
    let d = Dirs::sample();
    let opts = || {
        configured(|o, _| {
            o.scan.fast_rescan = true;
        })
    };
    let s = d.open_with(opts());
    scans(&s, 1);
    let old = d.root.join("docs").join("oldies").join("report.pdf");
    assert_eq!(s.info(&old).unwrap().size, 1);
    // Folder times have millisecond precision in the index.
    std::thread::sleep(Duration::from_millis(50));
    d.file("docs/new.txt", b"x");
    std::fs::remove_file(d.root.join("src").join("main.rs")).unwrap();
    d.file("fresh/deep/x.md", b"x");
    // Grows in place: its folder's time does not move.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&old).unwrap();
        f.write_all(b"grown").unwrap();
    }
    s.rescan();
    scans(&s, 2);
    assert_eq!(names(&s, &q("new.txt")), ["new.txt"]);
    assert!(names(&s, &q("main.rs")).is_empty());
    assert_eq!(names(&s, &q("x.md")), ["x.md"]);
    assert_eq!(names(&s, &q("folder:deep")), ["deep"]);
    assert_eq!(
        s.info(&old).unwrap().size,
        1,
        "an unchanged folder is not listed"
    );
    // A full rebuild sees it; so does the next start's catch-up rescan (saved rows).
    s.rebuild();
    scans(&s, 3);
    assert_eq!(s.info(&old).unwrap().size, 6);
    drop(s);
    std::thread::sleep(Duration::from_millis(50));
    d.file("src/back.rs", b"x");
    let s = d.open_with(opts());
    scans(&s, 1);
    assert_eq!(names(&s, &q("back.rs")), ["back.rs"]);
    assert_eq!(names(&s, &q("new.txt")), ["new.txt"]);
}

#[test]
fn an_offline_root_stays_searchable_and_is_rescanned_when_back() {
    let d = Dirs::sample();
    let opts = || configured(|o, _| o.scan.monitor = true);
    drop(d.open_with(opts()));
    let away = d.root.with_extension("away");
    rename(&d.root, &away);
    // Offline at start: the last scan is served, nothing is walked or deleted.
    let s = d.open_with(opts());
    assert_eq!(
        s.offline_roots(),
        vec![normalize_roots(vec![d.root.clone()])[0].clone()]
    );
    assert!(s.status().unwrap().contains("offline"), "{:?}", s.status());
    assert_eq!(s.state(), SearchState::Ready);
    assert_eq!(names(&s, &q("main.rs")), ["main.rs"]);
    s.rebuild();
    s.rescan();
    s.tick();
    assert_eq!(names(&s, &q("main.rs")), ["main.rs"], "kept while offline");
    assert_eq!(s.shared.scans.load(Relaxed), 0);
    // Back, with a change made meanwhile: rescanned on the next check.
    std::fs::write(away.join("while-away.txt"), b"x").unwrap();
    rename(&away, &d.root);
    s.tick();
    assert!(s.offline_roots().is_empty());
    scans(&s, 1);
    assert_eq!(names(&s, &q("while-away")), ["while-away.txt"]);
    drop(s);
    // Offline with no saved index: not ready, and says why.
    let other = Dirs::new();
    std::fs::remove_dir_all(&other.root).unwrap();
    let s =
        WalkIndexSearcher::open_with(other.idx.clone(), vec![other.root.clone()], opts()).unwrap();
    s.tick();
    assert!(!s.available());
    assert_eq!(s.state(), SearchState::Unavailable);
    assert!(s.status().unwrap().contains("offline"));
}

#[test]
fn an_overflow_rescans_or_is_noted() {
    let d = Dirs::sample();
    let s = d.open_with(configured(|_, _| {}));
    scans(&s, 1);
    d.file("lost.txt", b"x");
    let _ = s.tx.send(Cmd::Overflow);
    scans(&s, 2);
    assert_eq!(names(&s, &q("lost")), ["lost.txt"]);
    drop(s);
    let s = d.open_with(configured(|o, _| o.scan.rescan_on_overflow = false));
    let _ = s.tx.send(Cmd::Overflow);
    s.tick();
    assert!(s.status().unwrap().contains("missed"), "{:?}", s.status());
    s.rescan();
    s.tick();
    assert_eq!(s.status(), None);
}

/// A journal of the test's own, next to its index.
fn journal_of(d: &Dirs) -> Arc<Journal> {
    Arc::new(Journal::open(&d.idx.join(journal::FILE_NAME), 1 << 20).unwrap())
}

/// (action, path below the root with `/`, old path below the root) of every entry,
/// oldest first.
fn entries(j: &Journal, d: &Dirs) -> Vec<(journal::Action, String, Option<String>)> {
    // The index keeps roots resolved (macOS: `/private/var` for `/var`).
    let roots = normalize_roots(vec![d.root.clone()]);
    let roots = [journal::path_text(&d.root), journal::path_text(&roots[0])];
    let rel = |p: &str| {
        roots
            .iter()
            .find_map(|r| p.strip_prefix(r.as_str()))
            .unwrap_or(p)
            .trim_start_matches(['/', '\\'])
            .replace('\\', "/")
    };
    j.query(&journal::Filter {
        oldest_first: true,
        ..journal::Filter::default()
    })
    .unwrap()
    .iter()
    .map(|e| (e.action, rel(&e.path), e.old_path.as_deref().map(rel)))
    .collect()
}

/// Waits until the journal holds this entry.
fn journaled(j: &Journal, d: &Dirs, action: journal::Action, path: &str, old: Option<&str>) {
    let want = (action, path.to_owned(), old.map(str::to_owned));
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let got = entries(j, d);
        if got.contains(&want) {
            return;
        }
        assert!(Instant::now() < end, "wanted {want:?}, journal has {got:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn set_readonly(path: &Path, on: bool) {
    let mut perm = std::fs::metadata(path).unwrap().permissions();
    perm.set_readonly(on);
    std::fs::set_permissions(path, perm).unwrap();
}

/// Each action the watcher applies is journaled once, a renamed folder as one entry;
/// the first walk is not journaled; the journal outlives the index.
fn journal_follows_the_watcher(opts: WalkOptions) {
    use journal::Action::*;
    let d = Dirs::sample();
    let j = journal_of(&d);
    let s = d.open_with(WalkOptions {
        journal: Some(j.clone()),
        ..opts
    });
    assert!(!s.shared.unwatched.load(Relaxed), "watcher failed to start");
    assert!(j.is_empty().unwrap(), "the first walk is not a change");
    d.file("new.txt", b"x");
    journaled(&j, &d, Create, "new.txt", None);
    std::fs::write(d.root.join("docs/Report.PDF"), b"1234567890").unwrap();
    journaled(&j, &d, Modify, "docs/Report.PDF", None);
    let main = d.root.join("src").join("main.rs");
    set_readonly(&main, true);
    journaled(&j, &d, Attributes, "src/main.rs", None);
    set_readonly(&main, false);
    rename(
        &d.root.join("docs").join("report notes.txt"),
        &d.root.join("docs").join("minutes.txt"),
    );
    journaled(
        &j,
        &d,
        Rename,
        "docs/minutes.txt",
        Some("docs/report notes.txt"),
    );
    std::fs::remove_file(d.root.join("docs/oldies/report.pdf")).unwrap();
    journaled(&j, &d, Delete, "docs/oldies/report.pdf", None);
    let before = entries(&j, &d).len();
    rename(&d.root.join("src"), &d.root.join("source"));
    journaled(&j, &d, Rename, "source", Some("src"));
    std::thread::sleep(BATCH * 2);
    let after = entries(&j, &d);
    assert!(
        after[before..]
            .iter()
            .all(|(a, p, _)| !p.contains("main.rs") || *a == Attributes || *a == Modify),
        "the files of a renamed folder are not listed: {after:?}"
    );
    assert!(s.info(&d.root.join("source").join("main.rs")).is_some());
    let rc = |rel: &str| j.recently_changed(&s.roots()[0].join(rel)).unwrap();
    assert!(rc("new.txt").is_some());
    assert!(rc("docs/oldies").is_none());
    drop(s);
    drop(j);
    // A restart keeps the journal.
    let j = journal_of(&d);
    assert_eq!(entries(&j, &d), after);
}

#[test]
fn the_journal_follows_the_home_watcher() {
    journal_follows_the_watcher(WalkOptions {
        max_age_secs: None,
        ..WalkOptions::default()
    });
}

#[test]
fn the_journal_follows_a_configured_folder_watcher() {
    // ReadDirectoryChangesW with its rename pairs on Windows.
    journal_follows_the_watcher(configured(|o, _| o.scan.monitor = true));
}

#[test]
fn rescans_journal_what_changed_while_unwatched() {
    use journal::Action::*;
    for fast in [false, true] {
        let d = Dirs::sample();
        let j = journal_of(&d);
        let s = d.open_with(WalkOptions {
            journal: Some(j.clone()),
            ..configured(|o, _| o.scan.fast_rescan = fast)
        });
        scans(&s, 1);
        assert!(j.is_empty().unwrap());
        // Folder times have millisecond precision in the index.
        std::thread::sleep(Duration::from_millis(50));
        d.file("docs/new.txt", b"x");
        std::fs::remove_file(d.root.join("src").join("main.rs")).unwrap();
        rename(
            &d.root.join("docs").join("report notes.txt"),
            &d.root.join("report notes.txt"),
        );
        if !fast {
            // In place: only a full walk sees it.
            std::fs::write(d.root.join("docs/Report.PDF"), b"1234567").unwrap();
        }
        s.rescan();
        scans(&s, 2);
        let got = entries(&j, &d);
        let has = |a, p: &str, o: Option<&str>| {
            assert!(
                got.contains(&(a, p.to_owned(), o.map(str::to_owned))),
                "fast {fast}: {a:?} {p} missing from {got:?}"
            );
        };
        has(Create, "docs/new.txt", None);
        has(Delete, "src/main.rs", None);
        if cfg!(any(windows, target_os = "macos")) {
            // Same size, modified and created time: the same file moved.
            has(Rename, "report notes.txt", Some("docs/report notes.txt"));
        }
        if !fast {
            has(Modify, "docs/Report.PDF", None);
        }
        // A rules change walks again without journaling the difference.
        drop(s);
        let n = got.len();
        let s = d.open_with(WalkOptions {
            journal: Some(j.clone()),
            ..configured(|o, ex| {
                o.scan.fast_rescan = fast;
                ex.files = vec!["*.txt".into()];
            })
        });
        scans(&s, 1);
        assert_eq!(entries(&j, &d).len(), n, "fast {fast}");
    }
}

#[test]
fn merged_hits_are_ranked_once_each() {
    let hit = |p: &str| Hit {
        path: keel_vfs::VPath::local(p),
        is_dir: false,
        size: 0,
        modified: None,
        snippet: None,
    };
    let query = Query {
        max: 3,
        ..q("report")
    };
    let merged = merge_hits(
        &query,
        vec![
            hit("/a/my report.txt"),
            hit("/b/report"),
            hit("/a/reports.pdf"),
            hit("/b/report"),
            hit("/c/x-report"),
        ],
    );
    let got: Vec<&str> = merged.iter().map(|h| h.path.name()).collect();
    assert_eq!(got, ["report", "reports.pdf", "my report.txt"]);
    // A bad pattern still merges (unranked).
    let bad = Query {
        regex: true,
        ..q("(")
    };
    assert_eq!(merge_hits(&bad, vec![hit("/x"), hit("/x")]).len(), 1);
}

#[test]
fn table_remove_reuses_slots_and_round_trips_rows() {
    let mut t = Table::new(&[PathBuf::from("/r")]);
    let file = Meta {
        is_dir: false,
        size: 3,
        mtime: 7,
        created: 5,
        attrs: attr::READONLY,
    };
    let root = t.roots[0].1;
    let a = t.add(root, "a", DIR_META);
    let f = t.add(a, "f.txt", file);
    assert_eq!(
        t.path_of(f).unwrap(),
        PathBuf::from("/r").join("a").join("f.txt")
    );
    t.remove(a);
    assert_eq!((t.live, t.lookup(Path::new("/r/a/f.txt"))), (1, None));
    let b = t.add(root, "b\nb", file);
    assert!(b == a || b == f, "slot reused");
    assert_eq!(t.lookup(Path::new("/r/b b")), Some(b));
    t.compact();
    assert_eq!(t.search(&compile(&q("b")).unwrap(), 10).len(), 1);
    let rows: Vec<_> = (0..t.entries.len() as u32)
        .filter_map(|s| Some((s, t.row(s)?)))
        .collect();
    let back = Table::from_rows(&[PathBuf::from("/r")], rows).unwrap();
    assert_eq!(back.live, t.live);
    assert_eq!(back.lookup(Path::new("/r/b b")), Some(b));
    assert_eq!(back.entries[b as usize].meta, file);
    // A known folder keeps its listing time when a change notice refreshes it.
    let mut dir = DIR_META;
    dir.mtime = 10;
    let c = t.add(root, "c", dir);
    dir.mtime = 20;
    t.refresh(root, "c", dir);
    assert_eq!(t.entries[c as usize].meta.mtime, 10);
}

/// `cargo test -p keel-search --release bench -- --ignored --nocapture`
/// (`KEEL_BENCH_FILES`, default 100000).
#[test]
#[ignore]
fn bench_synthetic_tree() {
    let n: usize = std::env::var("KEEL_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let d = Dirs::new();
    let start = Instant::now();
    for i in 0..n {
        let dir = d.root.join(format!("d{}/s{}", i / 1000, (i / 50) % 20));
        if i % 50 == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(dir.join(format!("file{i}-{}.dat", i % 97)), b"x").unwrap();
    }
    println!("created {n} files in {:?}", start.elapsed());
    let start = Instant::now();
    let s = d.open();
    println!(
        "indexed in {:?} ({} entries)",
        start.elapsed(),
        s.shared.table.read().live
    );
    for text in [
        "file4242",
        "*-33.dat",
        "regex:^file9\\d+-1\\.dat$",
        "zzzzz",
        "d7 file1",
        "a",
    ] {
        let mut best = Duration::MAX;
        let mut count = 0;
        for _ in 0..20 {
            let t = Instant::now();
            count = s.query(&q(text)).unwrap().len();
            best = best.min(t.elapsed());
        }
        println!("query {text:?}: {count} hits, best of 20 {best:?}");
    }
    let t = Instant::now();
    let folders = folder_index(&s).unwrap().len();
    println!("folder_index: {folders} folders in {:?}", t.elapsed());
    let start = Instant::now();
    s.rescan();
    println!("rescan queued in {:?}", start.elapsed());
}
