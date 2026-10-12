use super::*;
use std::sync::atomic::AtomicU32;

/// A journal file in a fresh temp folder, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "keel-journal-{}-{}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join(FILE_NAME)
    }

    fn open(&self, max: u64) -> Journal {
        Journal::open(&self.db(), max).unwrap()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn file(size: u64, mtime: i64, created: i64) -> Stat {
    Stat {
        is_dir: false,
        size,
        mtime: Some(mtime),
        created: Some(created),
        attrs: 0,
    }
}

fn dir(created: Option<i64>) -> Stat {
    Stat {
        is_dir: true,
        size: 0,
        mtime: Some(1),
        created,
        attrs: 0x10,
    }
}

fn summary(events: &[Event]) -> Vec<(Action, String, Option<String>)> {
    events
        .iter()
        .map(|e| {
            (
                e.action,
                e.path.replace('\\', "/"),
                e.old_path.clone().map(|o| o.replace('\\', "/")),
            )
        })
        .collect()
}

fn act(a: Action, path: &str, old: Option<&str>) -> (Action, String, Option<String>) {
    (a, path.into(), old.map(str::to_owned))
}

#[test]
fn changes_become_create_delete_modify_and_attribute_entries() {
    let a = file(5, 100, 50);
    let grown = file(9, 200, 50);
    let hidden = Stat { attrs: 0x2, ..a };
    let events = events_from(
        vec![
            Change::Created(p("/r/new.txt"), a),
            Change::Deleted(p("/r/gone.txt"), file(1, 1, 1)),
            Change::Changed(p("/r/grew.txt"), a, grown),
            Change::Changed(p("/r/hid.txt"), a, hidden),
            // A folder's time moving because of its entries is not a change of it.
            Change::Changed(
                p("/r/sub"),
                dir(Some(3)),
                Stat {
                    mtime: Some(9),
                    ..dir(Some(3))
                },
            ),
            Change::Changed(
                p("/r/sub2"),
                dir(Some(3)),
                Stat {
                    attrs: 0x12,
                    ..dir(Some(3))
                },
            ),
            // created-only changes are not reported
            Change::Changed(
                p("/r/same.txt"),
                a,
                Stat {
                    created: Some(7),
                    ..a
                },
            ),
        ],
        &[],
        1234,
    );
    assert_eq!(
        summary(&events),
        [
            act(Action::Create, "/r/new.txt", None),
            act(Action::Delete, "/r/gone.txt", None),
            act(Action::Modify, "/r/grew.txt", None),
            act(Action::Attributes, "/r/hid.txt", None),
            act(Action::Attributes, "/r/sub2", None),
        ]
    );
    assert!(events.iter().all(|e| e.time == 1234));
    assert_eq!((events[0].size, events[0].is_dir), (5, false));
    assert_eq!(events[2].size, 9);
    assert!(events[4].is_dir);
}

#[test]
fn renames_are_paired_by_the_watcher_or_by_identity() {
    let a = file(5, 100, 50);
    let b = file(7, 100, 50);
    // The watcher's pair wins even when the identity differs (b's size).
    let events = events_from(
        vec![
            Change::Deleted(p("/r/old.txt"), a),
            Change::Created(p("/r/new.txt"), b),
        ],
        &[(p("/r/old.txt"), p("/r/new.txt"))],
        1,
    );
    assert_eq!(
        summary(&events),
        [act(Action::Rename, "/r/new.txt", Some("/r/old.txt"))]
    );
    // No pair from the watcher: same size and times is the same file.
    let events = events_from(
        vec![
            Change::Deleted(p("/r/a.txt"), a),
            Change::Deleted(p("/r/other.txt"), b),
            Change::Created(p("/r/sub/a.txt"), a),
        ],
        &[],
        1,
    );
    assert_eq!(
        summary(&events),
        [
            act(Action::Delete, "/r/other.txt", None),
            act(Action::Rename, "/r/sub/a.txt", Some("/r/a.txt")),
        ]
    );
    // A file with no times cannot be told apart: delete and create.
    let blind = Stat {
        mtime: None,
        created: None,
        ..a
    };
    let events = events_from(
        vec![
            Change::Deleted(p("/r/x"), blind),
            Change::Created(p("/r/y"), blind),
        ],
        &[],
        1,
    );
    assert_eq!(
        summary(&events),
        [
            act(Action::Delete, "/r/x", None),
            act(Action::Create, "/r/y", None)
        ]
    );
}

#[test]
fn a_renamed_folder_is_one_entry_not_one_per_file() {
    let d = dir(Some(42));
    let f = file(3, 10, 11);
    let changes = vec![
        Change::Deleted(p("/r/old"), d),
        Change::Deleted(p("/r/old/f.txt"), f),
        Change::Deleted(p("/r/old/in/g.txt"), file(4, 10, 12)),
        Change::Created(
            p("/r/new"),
            Stat {
                mtime: Some(99),
                ..d
            },
        ),
        Change::Created(p("/r/new/f.txt"), f),
        Change::Created(p("/r/new/in/g.txt"), file(4, 10, 12)),
    ];
    let want = [act(Action::Rename, "/r/new", Some("/r/old"))];
    assert_eq!(summary(&events_from(changes.clone(), &[], 1)), want);
    let hinted = events_from(changes, &[(p("/r/old"), p("/r/new"))], 1);
    assert_eq!(summary(&hinted), want);
    assert!(hinted[0].is_dir);
    // A folder without a created time (some file systems) is a delete and a create.
    let blind = dir(None);
    let events = events_from(
        vec![
            Change::Deleted(p("/r/old"), blind),
            Change::Created(p("/r/new"), blind),
        ],
        &[],
        1,
    );
    assert_eq!(events.len(), 2);
}

fn ev(action: Action, path: &str, time: i64) -> Event {
    Event::new(action, Path::new(path)).at(time)
}

#[test]
fn queries_filter_by_action_time_path_pattern_and_text() {
    let t = Temp::new();
    let j = t.open(1 << 20);
    assert!(j.is_empty().unwrap());
    let root = if cfg!(windows) { "C:\\data" } else { "/data" };
    let at = |rel: &str| format!("{root}{}{rel}", std::path::MAIN_SEPARATOR);
    j.record_all(&[
        ev(Action::Create, &at("a.txt"), 1_000),
        ev(Action::Modify, &at("a.txt"), 2_000),
        ev(Action::Create, &at("Report.PDF"), 3_000).size(10),
        Event::renamed(Path::new(&at("a.txt")), Path::new(&at("b.txt"))).at(4_000),
        ev(Action::Delete, &at("Report.PDF"), 5_000),
        ev(Action::Attributes, &at("dir"), 6_000).dir(true),
    ])
    .unwrap();
    let paths = |f: Filter| -> Vec<(Action, String)> {
        j.query(&f)
            .unwrap()
            .into_iter()
            .map(|e| {
                (
                    e.action,
                    e.path.rsplit(['/', '\\']).next().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let all = j.query(&Filter::default()).unwrap();
    assert_eq!(all.len(), 6);
    assert!(all.windows(2).all(|w| w[0].id > w[1].id), "newest first");
    let creates = Filter {
        actions: vec![Action::Create],
        oldest_first: true,
        ..Filter::default()
    };
    assert_eq!(
        paths(creates),
        [
            (Action::Create, "a.txt".into()),
            (Action::Create, "Report.PDF".into())
        ]
    );
    let range = Filter {
        since: Some(2_000),
        until: Some(4_000),
        ..Filter::default()
    };
    assert_eq!(
        paths(range),
        [
            (Action::Create, "Report.PDF".into()),
            (Action::Modify, "a.txt".into())
        ]
    );
    let re = Filter {
        path_regex: Some(Regex::new(r"(?i)\.pdf$").unwrap()),
        ..Filter::default()
    };
    assert_eq!(paths(re).len(), 2);
    // Text: any case, the old path of a rename counts.
    let text = Filter {
        text: "A.TXT".into(),
        ..Filter::default()
    };
    assert_eq!(paths(text).len(), 3);
    // One file's entries: as the path or as the old path.
    let of_a = Filter {
        path: Some(at("a.txt")),
        ..Filter::default()
    };
    assert_eq!(
        paths(of_a),
        [
            (Action::Rename, "b.txt".into()),
            (Action::Modify, "a.txt".into()),
            (Action::Create, "a.txt".into())
        ]
    );
    let limited = Filter {
        limit: 2,
        ..Filter::default()
    };
    assert_eq!(paths(limited).len(), 2);
    let after = Filter {
        after_id: Some(all[1].id),
        ..Filter::default()
    };
    assert_eq!(j.query(&after).unwrap(), all[..1]);
    assert_eq!(j.last_id().unwrap(), all[0].id);
    // rc: the newest change of a path; a whole set at once.
    let rc = j.recently_changed(Path::new(&at("Report.PDF"))).unwrap();
    assert_eq!(rc, Some(UNIX_EPOCH + Duration::from_millis(5_000)));
    assert_eq!(j.recently_changed(Path::new(&at("nope"))).unwrap(), None);
    let recent = j.recent(3_000).unwrap();
    assert_eq!(recent.len(), 3);
    assert!(recent.get(Path::new(&at("b.txt"))).is_some());
    assert!(recent.get(Path::new(&at("a.txt"))).is_none(), "before 3000");
    if cfg!(windows) {
        // Paths compare without case on Windows.
        assert!(j
            .recently_changed(Path::new(&at("report.pdf")))
            .unwrap()
            .is_some());
        assert!(recent.get(Path::new(&at("REPORT.pdf"))).is_some());
    }
}

#[test]
fn the_journal_survives_a_restart_and_numbers_keep_rising() {
    let t = Temp::new();
    {
        let j = t.open(1 << 20);
        j.record(ev(Action::Create, "/x/one", 1)).unwrap();
    }
    let j = t.open(1 << 20);
    let first = j.query(&Filter::default()).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].path, path_text(Path::new("/x/one")));
    j.clear().unwrap();
    j.record(ev(Action::Delete, "/x/one", 2)).unwrap();
    let again = j.query(&Filter::default()).unwrap();
    assert!(
        again[0].id > first[0].id,
        "a watcher never sees an old number again"
    );
}

#[test]
fn the_oldest_entries_go_when_the_file_reaches_its_cap() {
    let t = Temp::new();
    let cap = 128 * 1024;
    let j = t.open(cap);
    let long = "x".repeat(200);
    for batch in 0..40 {
        let events: Vec<Event> = (0..50)
            .map(|i| {
                ev(
                    Action::Modify,
                    &format!("/r/{long}/{batch}-{i}"),
                    batch * 100 + i,
                )
            })
            .collect();
        j.record_all(&events).unwrap();
        assert!(
            j.file_bytes().unwrap() <= cap + 8 * 4096,
            "batch {batch}: {} bytes",
            j.file_bytes().unwrap()
        );
    }
    let kept = j.len().unwrap();
    assert!(kept > 50 && kept < 2000, "kept {kept}");
    let newest = j
        .query(&Filter {
            limit: 1,
            ..Filter::default()
        })
        .unwrap();
    assert!(newest[0].path.ends_with("39-49"));
    let oldest = j
        .query(&Filter {
            limit: 1,
            oldest_first: true,
            ..Filter::default()
        })
        .unwrap();
    assert!(oldest[0].time > 1_000, "the first entries are gone");
    // A lower cap applies at the next write.
    j.set_max_bytes(MIN_BYTES);
    j.record(ev(Action::Create, "/r/last", 9_999_999)).unwrap();
    assert!(j.file_bytes().unwrap() <= MIN_BYTES + 8 * 4096);
    assert!(j.len().unwrap() < kept);
}

#[test]
fn the_process_journal_follows_its_settings() {
    let t = Temp::new();
    use_path(Some(t.db()));
    let mine = |j: &Journal| {
        j.query(&Filter {
            text: "keel-global-journal-test".into(),
            ..Filter::default()
        })
        .unwrap()
        .len()
    };
    record(ev(Action::Create, "/keel-global-journal-test/a", 1));
    let j = global().expect("on by default");
    assert_eq!(mine(&j), 1);
    let path = Path::new("/keel-global-journal-test/a");
    assert!(recently_changed(path).is_some());
    configure(&JournalSettings {
        enabled: false,
        max_mb: 8,
    });
    assert!(global().is_none() && !enabled());
    record(ev(Action::Delete, "/keel-global-journal-test/a", 2));
    assert_eq!(mine(&j), 1, "off: nothing recorded");
    configure(&JournalSettings::default());
    record(ev(Action::Delete, "/keel-global-journal-test/a", 3));
    assert_eq!(mine(&global().unwrap()), 2);
    use_path(None);
    assert!(global().is_none(), "a test binary has no default journal");
}

#[test]
fn actions_times_and_csv_as_typed() {
    for a in Action::ALL {
        assert_eq!(Action::parse(a.as_str()), Some(a));
        assert_eq!(Action::parse(&a.label().to_uppercase()), Some(a));
    }
    assert_eq!(Action::parse("moved"), Some(Action::Rename));
    assert_eq!(Action::parse("nope"), None);

    let now = 10_000_000_000;
    assert_eq!(parse_when("2h", now), Some((now - 7_200_000, false)));
    assert_eq!(parse_when("30m", now), Some((now - 1_800_000, false)));
    assert_eq!(parse_when("1w", now), Some((now - 604_800_000, false)));
    assert_eq!(parse_when("now", now), Some((now, false)));
    assert_eq!(parse_when("", now), None);
    assert_eq!(parse_when("5x", now), None);
    assert_eq!(parse_when("2026-13-01", now), None);
    let (day, whole) = parse_when("2026-10-12", now).unwrap();
    assert!(whole);
    let (at, whole) = parse_when("2026-10-12 14:30", now).unwrap();
    assert!(!whole);
    assert_eq!(at - day, (14 * 60 + 30) * 60_000);
    assert_eq!(
        parse_when("2026-10-12T14:30:05", now).unwrap().0 - at,
        5_000
    );
    assert_eq!(local_text(at), "2026-10-12 14:30:00");
    let today = start_of_today();
    assert_eq!(parse_since("today"), Some(today));
    assert_eq!(parse_until("today"), Some(today + 86_400_000));
    assert!(parse_since("yesterday").unwrap() < today);
    assert_eq!(parse_until("2026-10-12"), Some(day + 86_400_000));

    let e = Event::renamed(Path::new("/a/old, \"q\".txt"), Path::new("/a/new.txt"))
        .at(at)
        .size(3);
    let text = csv(&[e]);
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("time,action,path,old_path,folder,size"));
    let new = path_text(Path::new("/a/new.txt"));
    let old = path_text(Path::new("/a/old, \"q\".txt")).replace('"', "\"\"");
    assert_eq!(
        lines.next(),
        Some(format!("2026-10-12 14:30:00,rename,{new},\"{old}\",false,3").as_str())
    );
}
