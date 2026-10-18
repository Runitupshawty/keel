//! Run history (Everything's Run count and Date run): every file or folder Keel opens with
//! an app (the default app or **Open with**) is counted, with the time of its last open, in
//! a small SQLite file next to the index (`<index dir>/runs.db`). Settings → Search index →
//! Run history turns it off or clears it.
//!
//! The search language reads it as `run-count:` and `date-run:` (through
//! `keel_query::WithRuns`), the list view as the Run count and Date run columns. Lookups
//! come from a copy in memory, reloaded when another process (the CLI) wrote the file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{const_mutex, Mutex, RwLock};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// The run history's file name in the index folder.
pub const FILE_NAME: &str = "runs.db";
/// How often lookups check whether another process wrote the file.
const RELOAD_EVERY: Duration = Duration::from_secs(2);

/// Settings → Search index → Run history (`[search_index.run_history]`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct RunHistorySettings {
    /// Count opens. Off: nothing is recorded and run counts read as unknown.
    pub enabled: bool,
}

impl Default for RunHistorySettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// How often and when a path was last opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunInfo {
    pub count: u64,
    /// Milliseconds since the epoch.
    pub last_run: i64,
}

/// Paths compare without case on Windows (ASCII letters, as SQLite's NOCASE does).
const COLLATE: &str = if cfg!(windows) { "NOCASE" } else { "BINARY" };

/// How a path is stored: the journal's text form without a trailing separator (a drive
/// root keeps its own).
fn stored(path: &Path) -> String {
    let mut text = crate::journal::path_text(path);
    while text.len() > 1 && text.ends_with(['\\', '/']) && !text.ends_with(":\\") {
        text.pop();
    }
    text
}

/// The in-memory key of a stored path (the same folding as [`COLLATE`]).
fn key(stored: &str) -> String {
    if cfg!(windows) {
        stored.to_ascii_lowercase()
    } else {
        stored.to_owned()
    }
}

struct Cache {
    map: HashMap<String, RunInfo>,
    /// SQLite's `data_version` when `map` was read.
    version: i64,
    checked: Instant,
}

/// A run history file.
pub struct RunStore {
    conn: Mutex<Connection>,
    path: PathBuf,
    cache: RwLock<Cache>,
}

impl RunStore {
    /// Opens (or creates) the run history at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS runs(
                 path TEXT PRIMARY KEY COLLATE {COLLATE},
                 count INTEGER NOT NULL,
                 last INTEGER NOT NULL);"
        ))?;
        let store = Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            cache: RwLock::new(Cache {
                map: HashMap::new(),
                version: -1,
                checked: Instant::now(),
            }),
        };
        store.reload()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn reload(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        let version: i64 = conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        let mut map = HashMap::new();
        let mut stmt = conn.prepare_cached("SELECT path, count, last FROM runs")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                RunInfo {
                    count: r.get::<_, i64>(1)?.max(0) as u64,
                    last_run: r.get(2)?,
                },
            ))
        })?;
        for row in rows {
            let (path, info) = row?;
            map.insert(key(&path), info);
        }
        *self.cache.write() = Cache {
            map,
            version,
            checked: Instant::now(),
        };
        Ok(())
    }

    /// Reloads the copy in memory when another connection wrote the file since.
    fn fresh(&self) {
        if self.cache.read().checked.elapsed() < RELOAD_EVERY {
            return;
        }
        let version = self
            .conn
            .lock()
            .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .ok();
        let mut cache = self.cache.write();
        cache.checked = Instant::now();
        if version.is_some_and(|v| v != cache.version) {
            drop(cache);
            let _ = self.reload();
        }
    }

    /// One more open of `path` at `at` (milliseconds since the epoch).
    pub fn record(&self, path: &Path, at: i64) -> anyhow::Result<RunInfo> {
        let text = stored(path);
        let info = {
            let mut conn = self.conn.lock();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO runs(path, count, last) VALUES (?1, 1, ?2)
                 ON CONFLICT(path) DO UPDATE SET count = count + 1, last = max(last, ?2)",
                params![text, at],
            )?;
            let info = tx.query_row(
                "SELECT count, last FROM runs WHERE path = ?1",
                [&text],
                |r| {
                    Ok(RunInfo {
                        count: r.get::<_, i64>(0)?.max(0) as u64,
                        last_run: r.get(1)?,
                    })
                },
            )?;
            tx.commit()?;
            info
        };
        self.cache.write().map.insert(key(&text), info);
        Ok(info)
    }

    /// Sets the count and date of `path` (`es`-style `-set-run-count`); a count of 0
    /// forgets it.
    pub fn set(&self, path: &Path, info: RunInfo) -> anyhow::Result<()> {
        let text = stored(path);
        let conn = self.conn.lock();
        if info.count == 0 {
            conn.execute("DELETE FROM runs WHERE path = ?1", [&text])?;
            self.cache.write().map.remove(&key(&text));
        } else {
            conn.execute(
                "INSERT INTO runs(path, count, last) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET count = ?2, last = ?3",
                params![text, info.count.min(i64::MAX as u64) as i64, info.last_run],
            )?;
            self.cache.write().map.insert(key(&text), info);
        }
        Ok(())
    }

    /// The count and last open of `path`; None when Keel never opened it.
    pub fn get(&self, path: &Path) -> Option<RunInfo> {
        self.fresh();
        self.cache.read().map.get(&key(&stored(path))).copied()
    }

    /// Every path with its count and last open, the most recent first.
    pub fn entries(&self) -> Vec<(String, RunInfo)> {
        self.fresh();
        let conn = self.conn.lock();
        let Ok(mut stmt) =
            conn.prepare("SELECT path, count, last FROM runs ORDER BY last DESC, path")
        else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                RunInfo {
                    count: r.get::<_, i64>(1)?.max(0) as u64,
                    last_run: r.get(2)?,
                },
            ))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.fresh();
        self.cache.read().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets every open.
    pub fn clear(&self) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM runs", [])?;
        self.cache.write().map.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------------
// The process's run history

struct Global {
    settings: RunHistorySettings,
    path: Option<PathBuf>,
    open: Option<Arc<RunStore>>,
    failed: bool,
}

static GLOBAL: Mutex<Global> = const_mutex(Global {
    settings: RunHistorySettings { enabled: true },
    path: None,
    open: None,
    failed: false,
});

/// Where the run history lives: `runs.db` in the index folder (`KEEL_INDEX_DIR`,
/// `$KEEL_CONFIG_DIR/index`, else the OS cache folder).
pub fn default_path() -> Option<PathBuf> {
    crate::walkindex::index_dir().map(|d| d.join(FILE_NAME))
}

/// Test binaries never touch a shared run history unless a test names one.
fn in_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.file_name().map(|d| d == "deps"))
        .unwrap_or(false)
}

/// Settings → Search index → Run history.
pub fn configure(settings: &RunHistorySettings) {
    let mut g = GLOBAL.lock();
    if g.settings != *settings {
        g.settings = settings.clone();
        g.failed = false;
    }
}

/// Keeps the process's run history at `path` (tests, tools); `None`: the default place.
pub fn use_path(path: Option<PathBuf>) {
    let mut g = GLOBAL.lock();
    g.path = path;
    g.open = None;
    g.failed = false;
}

/// Whether opens are counted (Settings → Search index → Run history).
pub fn enabled() -> bool {
    GLOBAL.lock().settings.enabled
}

/// The process's run history, opened on first use; None when it is off or cannot open.
pub fn global() -> Option<Arc<RunStore>> {
    let mut g = GLOBAL.lock();
    if !g.settings.enabled || g.failed {
        return None;
    }
    if let Some(s) = &g.open {
        return Some(s.clone());
    }
    let path = match &g.path {
        Some(p) => p.clone(),
        None if in_test_binary() => return None,
        None => default_path()?,
    };
    match RunStore::open(&path) {
        Ok(s) => {
            let s = Arc::new(s);
            g.open = Some(s.clone());
            Some(s)
        }
        // A history that cannot open shows as no run counts.
        Err(_) => {
            g.failed = true;
            None
        }
    }
}

/// Counts one open of `path` now (nothing when the run history is off).
pub fn note_open(path: &Path) {
    if let Some(s) = global() {
        // Best effort: a full disk must not stop an open.
        let _ = s.record(path, crate::journal::now_ms());
    }
}

/// The count and last open of `path` by the process's run history; None when it is off
/// or Keel never opened it.
pub fn info(path: &Path) -> Option<RunInfo> {
    global()?.get(path)
}

/// Whether `item` matches `query`, with the run history's count and last open over what
/// the item says when the query reads them (`run-count:`, `date-run:`). `runs` is
/// [`keel_query::Query::uses_runs`], worked out once per search, so other queries never
/// look anything up.
pub(crate) fn matches_in(
    query: &keel_query::Query,
    runs: bool,
    item: &dyn keel_query::Props,
    tree: Option<&dyn keel_query::Tree>,
) -> bool {
    if !runs {
        return query.matches_in(item, tree);
    }
    let found = info(Path::new(item.path()));
    let with = keel_query::WithRuns {
        item,
        count: found.map(|r| r.count),
        last_run: found.map(|r| r.last_run),
    };
    query.matches_in(&with, tree)
}

/// Forgets every open (Settings → Search index → Run history → **Clear**). Works with the
/// history off too, when its file exists.
pub fn clear() -> anyhow::Result<()> {
    // Opened earlier (the history may be off now): its copy in memory is cleared too.
    let open = GLOBAL.lock().open.clone();
    if let Some(s) = open.or_else(global) {
        return s.clear();
    }
    let path = {
        let g = GLOBAL.lock();
        match &g.path {
            Some(p) => Some(p.clone()),
            None if in_test_binary() => None,
            None => default_path(),
        }
    };
    match path.filter(|p| p.exists()) {
        Some(p) => RunStore::open(&p)?.clear(),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_opens_and_survives_a_reopen() {
        let dir = tempfile_dir();
        let db = dir.join(FILE_NAME);
        let a = dir.join("report.pdf");
        let b = dir.join("tool.exe");
        {
            let s = RunStore::open(&db).unwrap();
            assert_eq!(s.get(&a), None);
            assert_eq!(
                s.record(&a, 1_000).unwrap(),
                RunInfo {
                    count: 1,
                    last_run: 1_000
                }
            );
            s.record(&a, 5_000).unwrap();
            // An older time (a clock step back) keeps the newest.
            let info = s.record(&a, 3_000).unwrap();
            assert_eq!(
                info,
                RunInfo {
                    count: 3,
                    last_run: 5_000
                }
            );
            s.record(&b, 2_000).unwrap();
            assert_eq!(s.len(), 2);
            let names: Vec<String> = s.entries().into_iter().map(|(p, _)| p).collect();
            assert_eq!(names, [stored(&a), stored(&b)], "most recent first");
        }
        let s = RunStore::open(&db).unwrap();
        assert_eq!(s.get(&a).map(|i| i.count), Some(3));
        assert_eq!(s.get(&b).map(|i| i.last_run), Some(2_000));
        s.set(
            &b,
            RunInfo {
                count: 12,
                last_run: 9_000,
            },
        )
        .unwrap();
        assert_eq!(s.get(&b).map(|i| i.count), Some(12));
        s.set(
            &b,
            RunInfo {
                count: 0,
                last_run: 0,
            },
        )
        .unwrap();
        assert_eq!(s.get(&b), None);
        s.clear().unwrap();
        assert!(s.is_empty());
        assert_eq!(RunStore::open(&db).unwrap().get(&a), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_connection_sees_new_opens() {
        let dir = tempfile_dir();
        let db = dir.join(FILE_NAME);
        let p = dir.join("a.txt");
        let reader = RunStore::open(&db).unwrap();
        let writer = RunStore::open(&db).unwrap();
        writer.record(&p, 10).unwrap();
        reader.cache.write().checked = Instant::now() - RELOAD_EVERY;
        assert_eq!(reader.get(&p).map(|i| i.count), Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_fold_like_the_platform() {
        let dir = tempfile_dir();
        let s = RunStore::open(&dir.join(FILE_NAME)).unwrap();
        let p = dir.join("Folder").join("Notes.TXT");
        s.record(&p, 1).unwrap();
        let upper = PathBuf::from(p.to_string_lossy().to_uppercase());
        assert_eq!(s.get(&upper).is_some(), cfg!(windows));
        // A trailing separator names the same folder.
        let folder = dir.join("Folder");
        s.record(&folder, 2).unwrap();
        let with_sep = PathBuf::from(format!("{}{}", folder.display(), std::path::MAIN_SEPARATOR));
        assert_eq!(s.get(&with_sep).map(|i| i.count), Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_global_history_follows_its_settings() {
        let dir = tempfile_dir();
        let p = dir.join("x.bin");
        use_path(Some(dir.join(FILE_NAME)));
        configure(&RunHistorySettings { enabled: true });
        note_open(&p);
        note_open(&p);
        assert_eq!(info(&p).map(|i| i.count), Some(2));
        // The search language reads it where the query asks for it.
        let item = Named(p.to_string_lossy().into_owned());
        let opts = keel_query::Options::default();
        let twice = keel_query::parse("run-count:2", &opts).unwrap();
        assert!(twice.uses_runs());
        assert!(matches_in(&twice, true, &item, None));
        assert!(!matches_in(&twice, false, &item, None), "not looked up");
        let today = keel_query::parse("dr:today", &opts).unwrap();
        assert!(matches_in(&today, today.uses_runs(), &item, None));
        configure(&RunHistorySettings { enabled: false });
        note_open(&p);
        assert_eq!(info(&p), None, "off: unknown");
        // Clear works while off.
        clear().unwrap();
        configure(&RunHistorySettings { enabled: true });
        assert_eq!(info(&p), None);
        use_path(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An item known by its path only.
    struct Named(String);

    impl keel_query::Props for Named {
        fn name(&self) -> &str {
            keel_query::name_of(&self.0)
        }
        fn path(&self) -> &str {
            &self.0
        }
        fn is_dir(&self) -> bool {
            false
        }
        fn size(&self) -> Option<u64> {
            None
        }
        fn allocated(&self) -> Option<u64> {
            None
        }
        fn date(&self, _: keel_query::DateProp) -> Option<keel_query::Timestamp> {
            None
        }
        fn attributes(&self) -> Option<u32> {
            None
        }
    }

    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "keel-runs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
