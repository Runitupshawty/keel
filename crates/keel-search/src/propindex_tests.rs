use super::*;
use crate::index_settings::IndexedProperty;
use std::sync::atomic::AtomicBool;

/// A clock that moves only when the job sleeps or the test says so, and a battery the
/// test switches.
#[derive(Default)]
struct FakeEnv {
    now: AtomicI64,
    battery: AtomicBool,
}

impl FakeEnv {
    fn advance(&self, d: Duration) {
        self.now.fetch_add(d.as_millis() as i64, Relaxed);
    }
}

impl Env for FakeEnv {
    fn now_ms(&self) -> i64 {
        self.now.load(Relaxed)
    }
    fn on_battery(&self) -> bool {
        self.battery.load(Relaxed)
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    store: Arc<PropStore>,
    env: Arc<FakeEnv>,
    activity: Arc<Activity>,
    pending: Arc<Pending>,
}

fn temp_dir() -> PathBuf {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "keel-props-index-{}-{}",
        std::process::id(),
        N.fetch_add(1, Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    // The walk reports real paths (macOS: /private/var).
    std::fs::canonicalize(&d).unwrap()
}

impl Fixture {
    fn new() -> Self {
        let dir = temp_dir();
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("skip")).unwrap();
        std::fs::write(root.join("a.txt"), "one two\nthree\n").unwrap();
        std::fs::write(root.join("sub").join("b.txt"), "four\n").unwrap();
        std::fs::write(root.join("skip").join("c.txt"), "five\n").unwrap();
        let store = Arc::new(PropStore::open(&dir.join(FILE_NAME)).unwrap());
        Self {
            dir,
            root,
            store,
            env: Arc::default(),
            activity: Arc::default(),
            pending: Arc::default(),
        }
    }

    fn settings(&self) -> PropertySettings {
        PropertySettings {
            properties: vec![
                IndexedProperty {
                    name: "line-count".into(),
                    fast_sort: true,
                },
                IndexedProperty {
                    name: "crc32".into(),
                    fast_sort: false,
                },
            ],
            exclude_folders: vec!["skip".into()],
            ..PropertySettings::default()
        }
    }

    fn job(&self, s: &PropertySettings) -> Indexer {
        Indexer::new(
            Rules::new(s, std::slice::from_ref(&self.root)),
            self.store.clone(),
            self.env.clone(),
            self.activity.clone(),
            self.pending.clone(),
        )
        .unwrap()
    }

    /// Steps until `stop` holds of a step (at most 1000 steps); returns the files read.
    fn until(&self, job: &mut Indexer, stop: impl Fn(&Step) -> bool) -> Vec<PathBuf> {
        let mut read = Vec::new();
        for _ in 0..1000 {
            let step = job.step();
            if let Step::Read(p) = &step {
                read.push(p.clone());
            }
            if stop(&step) {
                return read;
            }
        }
        panic!("the job never got there");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn values_round_trip_through_the_store() {
    let f = Fixture::new();
    let p = f.root.join("a.txt");
    let state = FileState {
        size: 3,
        modified: 42,
        props: props_key(&["artist".into(), "year".into(), "length".into()]),
    };
    let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
    let values = vec![
        ("artist".to_owned(), Prop::Str("Ann".into())),
        ("year".to_owned(), Prop::Int(1999)),
        (
            "length".to_owned(),
            Prop::Duration(Duration::from_millis(1500)),
        ),
        ("date-taken".to_owned(), Prop::Date(t)),
        ("compilation".to_owned(), Prop::Bool(true)),
        ("fps".to_owned(), Prop::Float(29.97)),
        (
            "people".to_owned(),
            Prop::List(vec![Prop::Str("a".into()), Prop::Str("b".into())]),
        ),
        ("title".to_owned(), Prop::Unknown),
    ];
    f.store.put(&p, &state, &values).unwrap();
    assert_eq!(f.store.file(&p), Some(state));
    for (name, v) in &values[..7] {
        assert_eq!(f.store.get(&p, name), Lookup::Value(v.clone()), "{name}");
    }
    // Read with the file's property list: known to be unknown.
    let mut with_title = values[0..1].to_vec();
    with_title.push(("title".into(), Prop::Unknown));
    let state = FileState {
        size: 3,
        modified: 42,
        props: props_key(&["artist".into(), "title".into()]),
    };
    f.store.put(&p, &state, &with_title).unwrap();
    assert_eq!(f.store.get(&p, "title"), Lookup::Unknown);
    assert_eq!(f.store.get(&p, "year"), Lookup::NotIndexed, "replaced");
    assert_eq!(
        f.store.get(&f.root.join("none"), "artist"),
        Lookup::NotIndexed
    );
    assert_eq!(f.store.values(&p).len(), 1);
}

#[test]
fn renames_and_removes_move_whole_folders() {
    let f = Fixture::new();
    let s = |n: &str| f.root.join(n);
    let state = FileState {
        size: 1,
        modified: 1,
        props: "line-count".into(),
    };
    let v = [("line-count".to_owned(), Prop::Int(1))];
    for p in ["sub/x.txt", "sub/deep/y.txt", "subway.txt", "other.txt"] {
        f.store.put(&s(p), &state, &v).unwrap();
    }
    f.store.rename(&s("sub"), &s("moved")).unwrap();
    assert!(f.store.file(&s("moved/x.txt")).is_some());
    assert!(f.store.file(&s("moved/deep/y.txt")).is_some());
    assert!(f.store.file(&s("sub/x.txt")).is_none());
    assert!(
        f.store.file(&s("subway.txt")).is_some(),
        "a sibling with the same start"
    );
    // Onto an existing file: replaced.
    f.store.rename(&s("subway.txt"), &s("other.txt")).unwrap();
    assert_eq!(f.store.len(), 3);
    assert_eq!(f.store.remove(&s("moved")).unwrap(), 2);
    assert_eq!(f.store.len(), 1);
}

#[test]
fn fast_sort_keeps_an_order_per_property() {
    let f = Fixture::new();
    let state = FileState {
        size: 1,
        modified: 1,
        props: "line-count".into(),
    };
    for (name, n) in [("a", 3), ("b", 1), ("c", 2)] {
        let v = [("line-count".to_owned(), Prop::Int(n))];
        f.store.put(&f.root.join(name), &state, &v).unwrap();
    }
    f.store.set_fast_sort(&["line-count".into()]).unwrap();
    assert_eq!(f.store.fast_sort_indexes().len(), 1);
    let names = |v: Vec<String>| -> Vec<String> {
        v.iter()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    };
    assert_eq!(
        names(f.store.sorted("line-count", false, 10)),
        ["b", "c", "a"]
    );
    assert_eq!(names(f.store.sorted("line-count", true, 2)), ["a", "c"]);
    // The planner reads the partial index.
    let plan: Vec<String> = {
        let conn = f.store.conn.lock();
        let mut stmt = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT f.path FROM vals v JOIN files f ON f.id = v.file
                 WHERE v.prop = 'line-count' ORDER BY v.num, v.text",
            )
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .flatten()
            .collect()
    };
    assert!(
        plan.iter()
            .any(|l| l.contains(&sort_index_name("line-count"))),
        "{plan:?}"
    );
    f.store.set_fast_sort(&[]).unwrap();
    assert!(f.store.fast_sort_indexes().is_empty());
    assert_eq!(
        names(f.store.sorted("line-count", false, 10)),
        ["b", "c", "a"]
    );
}

#[test]
fn the_job_reads_admitted_files_and_skips_unchanged_ones() {
    let f = Fixture::new();
    let mut job = f.job(&f.settings());
    let mut read = f.until(&mut job, |s| *s == Step::PassDone);
    read.sort();
    assert_eq!(
        read,
        [f.root.join("a.txt"), f.root.join("sub").join("b.txt")]
    );
    let a = f.root.join("a.txt");
    assert_eq!(f.store.get(&a, "line-count"), Lookup::Value(Prop::Int(2)));
    assert!(matches!(
        f.store.get(&a, "crc32"),
        Lookup::Value(Prop::Str(_))
    ));
    assert_eq!(
        f.store
            .get(&f.root.join("skip").join("c.txt"), "line-count"),
        Lookup::NotIndexed,
        "an excluded folder"
    );
    assert_eq!(job.step(), Step::Idle, "the next pass is a day away");
    // A day later the pass reads nothing again.
    f.env.advance(PASS_EVERY);
    let read = f.until(&mut job, |s| *s == Step::PassDone);
    assert!(read.is_empty(), "{read:?}");
    // A new property list reads everything again, and drops the old values.
    let mut s = f.settings();
    s.properties.remove(1);
    let mut job = f.job(&s);
    assert_eq!(f.until(&mut job, |s| *s == Step::PassDone).len(), 2);
    assert_eq!(f.store.get(&a, "crc32"), Lookup::NotIndexed);
}

#[test]
fn the_job_pauses_after_input_and_on_battery_and_resumes_where_it_was() {
    let f = Fixture::new();
    for i in 0..4 {
        std::fs::write(f.root.join(format!("n{i}.txt")), "x\n").unwrap();
    }
    let mut job = f.job(&f.settings());
    f.env.advance(Duration::from_secs(10));
    f.activity.note(f.env.now_ms());
    assert_eq!(job.step(), Step::Paused);
    assert!(job.progress.paused.load(Relaxed));
    f.env.advance(ACTIVITY_PAUSE / 2);
    assert_eq!(job.step(), Step::Paused, "still within the pause");
    f.env.advance(ACTIVITY_PAUSE);
    let first = f.until(&mut job, |s| matches!(s, Step::Read(_)));
    assert_eq!(first.len(), 1);
    assert!(!job.progress.paused.load(Relaxed));

    f.env.battery.store(true, Relaxed);
    for _ in 0..3 {
        assert_eq!(job.step(), Step::Paused);
    }
    f.env.battery.store(false, Relaxed);
    let mut rest = f.until(&mut job, |s| *s == Step::PassDone);
    rest.extend(first.clone());
    rest.sort();
    rest.dedup();
    assert_eq!(rest.len(), 6, "every file once: {rest:?}");

    // Run: a paused job sleeps (the fake clock moves) and stops when asked.
    f.activity.note(f.env.now_ms());
    let cancel = job.cancel();
    let env = f.env.clone();
    let t = std::thread::spawn(move || job.run());
    while env.now_ms() < f.activity.0.load(Relaxed) + ACTIVITY_PAUSE.as_millis() as i64 {
        std::thread::yield_now();
    }
    cancel.cancel();
    f.pending.wake();
    t.join().unwrap();
}

#[test]
fn battery_is_ignored_when_the_job_does_not_pause_on_it() {
    let f = Fixture::new();
    let mut job = f.job(&f.settings()).pause_on_battery(false);
    f.env.battery.store(true, Relaxed);
    assert_eq!(f.until(&mut job, |s| *s == Step::PassDone).len(), 2);
}

#[test]
fn changes_keep_the_store_current() {
    let f = Fixture::new();
    let mut job = f.job(&f.settings());
    f.until(&mut job, |s| *s == Step::PassDone);
    let a = f.root.join("a.txt");
    let b = f.root.join("sub").join("b.txt");

    std::fs::write(&a, "1\n2\n3\n4\n").unwrap();
    let new = f.root.join("new.txt");
    std::fs::write(&new, "x\n").unwrap();
    let skipped = f.root.join("skip").join("d.txt");
    std::fs::write(&skipped, "x\n").unwrap();
    std::fs::remove_file(&b).unwrap();
    let mut events = vec![
        Event::new(Action::Modify, &a),
        Event::new(Action::Create, &new),
        Event::new(Action::Create, &skipped),
        Event::new(Action::Delete, &b),
    ];
    let moved = f.root.join("moved");
    std::fs::rename(f.root.join("sub"), &moved).unwrap();
    events.push(Event::renamed(&f.root.join("sub"), &moved).dir(true));
    f.pending.push(changes_of(&events));
    let mut read = Vec::new();
    while !f.pending.is_empty() {
        if let Step::Read(p) = job.step() {
            read.push(p);
        }
    }
    read.sort();
    assert_eq!(read, [a.clone(), new.clone()]);
    assert_eq!(f.store.get(&a, "line-count"), Lookup::Value(Prop::Int(4)));
    assert_eq!(f.store.get(&b, "line-count"), Lookup::NotIndexed);
    assert_eq!(f.store.get(&skipped, "line-count"), Lookup::NotIndexed);
    assert_eq!(f.store.len(), 2);

    // Too many changes: the next pass is due at once.
    let flood = (0..MAX_PENDING + 1).map(|i| Change::Read(f.root.join(format!("{i}"))));
    f.pending.push(flood);
    while !f.pending.is_empty() {
        job.step();
    }
    assert!(
        matches!(job.step(), Step::Skipped | Step::Read(_)),
        "a pass started"
    );
}

#[test]
fn rules_leave_out_placeholders_big_and_unindexed_files() {
    let f = Fixture::new();
    let mut s = f.settings();
    s.max_file_mb = 1;
    s.exclude_not_content_indexed = true;
    s.include_only_files = vec!["*.txt".into()];
    let r = Rules::new(&s, std::slice::from_ref(&f.root));
    let p = f.root.join("a.txt");
    assert!(r.admit_file(&p, 10, 0, false));
    assert!(!r.admit_file(&p, 2 << 20, 0, false), "over the size cap");
    assert!(!r.admit_file(&p, 10, attr::NOT_CONTENT_INDEXED, false));
    assert!(!r.admit_file(&p, 10, attr::RECALL_ON_DATA_ACCESS, false));
    assert!(!r.admit_file(&p, 10, attr::RECALL_ON_OPEN, false));
    assert!(!r.admit_file(&p, 10, attr::OFFLINE, false));
    assert!(!r.admit_file(&p, 10, 0, true), "a dataless file");
    assert!(
        !r.admit_file(&f.root.join("a.pdf"), 10, 0, false),
        "include only"
    );
    s.exclude_recall_on_data_access = false;
    let r = Rules::new(&s, std::slice::from_ref(&f.root));
    assert!(r.admit_file(&p, 10, attr::RECALL_ON_DATA_ACCESS, false));

    assert!(r.admit_path(&f.root.join("sub").join("b.txt")));
    assert!(!r.admit_path(&f.root.join("skip").join("c.txt")));
    assert!(
        !r.admit_path(&f.dir.join("elsewhere.txt")),
        "outside the roots"
    );
    // Include-only folders replace the index's roots.
    s.include_only_folders = vec![f.root.join("sub").to_string_lossy().into_owned()];
    let r = Rules::new(&s, std::slice::from_ref(&f.root));
    assert_eq!(r.roots(), [f.root.join("sub")]);
    assert!(!r.admit_path(&f.root.join("a.txt")));
}

#[test]
fn events_become_changes() {
    let p = Path::new("x");
    let events = [
        Event::new(Action::Create, p),
        Event::new(Action::Create, p).dir(true),
        Event::new(Action::Modify, p),
        Event::new(Action::Attributes, p),
        Event::new(Action::Delete, p).dir(true),
        Event::renamed(Path::new("old"), p),
    ];
    assert_eq!(
        changes_of(&events),
        [
            Change::Read(p.into()),
            Change::Read(p.into()),
            Change::Read(p.into()),
            Change::Remove(p.into()),
            Change::Rename("old".into(), p.into()),
        ]
    );
}

#[test]
fn the_global_job_follows_its_settings() {
    let f = Fixture::new();
    use_path(Some(f.dir.join("global.db")));
    configure(&f.settings(), std::slice::from_ref(&f.root), false);
    assert!(status().running);
    // The real job runs at once (no input yet in this process).
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while status().stored < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(status().stored, 2);
    assert_eq!(
        lookup(&f.root.join("a.txt"), "line-count"),
        Lookup::Value(Prop::Int(2))
    );
    // No properties: the job stops and the store empties.
    configure(&PropertySettings::default(), &[], false);
    assert!(!status().running);
    assert_eq!(status().stored, 0);
    shutdown();
    use_path(None);
}
