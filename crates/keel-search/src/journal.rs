//! The index journal (Everything 1.5's Index Journal): every create, delete, rename,
//! modify and attribute change Keel's own index applies, with the old and new path and
//! the time it was applied, in a bounded SQLite file (`<index dir>/journal.db`, 64 MB by
//! default; when it is full the oldest entries go first).
//!
//! The walk index (the home index and every folder and volume of Settings → Search)
//! feeds it from its change batches, its fast rescans and the difference a full rescan
//! found; any other indexer (the NTFS USN reader) calls [`record`] / [`record_all`].
//! The journal answers "what changed" for the Journal tab, `keel journal`, the
//! Inspector and `rc:`-style filters ([`recently_changed`], [`Journal::recent`]).
//!
//! A rename is reported by the watcher as a pair (ReadDirectoryChangesW and inotify do)
//! or, when it is not, recognized in a batch as a delete and a create of an entry with
//! the same size, modified and created times; the entries inside a renamed folder are
//! not listed one by one. A folder's own modified time moving (an entry added in it) is
//! not a change of the folder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use regex::Regex;
use rusqlite::{params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub use crate::index_settings::JournalSettings;

/// The journal's file name in the index folder.
pub const FILE_NAME: &str = "journal.db";
/// The smallest size cap accepted (a few SQLite pages of schema and some entries).
pub const MIN_BYTES: u64 = 64 * 1024;
/// When the file passes its cap, the oldest entries are dropped down to this share of it.
const TRIM_TO: f64 = 0.75;

/// What happened to a file or folder.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Delete,
    /// Renamed or moved (within one index).
    Rename,
    /// Its size or modified time changed.
    Modify,
    /// Only its attributes changed.
    Attributes,
}

impl Action {
    pub const ALL: [Action; 5] = [
        Action::Create,
        Action::Delete,
        Action::Rename,
        Action::Modify,
        Action::Attributes,
    ];

    /// The name used on the command line, in CSV and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Delete => "delete",
            Action::Rename => "rename",
            Action::Modify => "modify",
            Action::Attributes => "attributes",
        }
    }

    /// The word shown in the Journal tab.
    pub fn label(self) -> &'static str {
        match self {
            Action::Create => "Created",
            Action::Delete => "Deleted",
            Action::Rename => "Renamed",
            Action::Modify => "Modified",
            Action::Attributes => "Attributes",
        }
    }

    /// `create`, `created`, `delete`, `rename`, `move`, `modify`, `attributes`,
    /// `attr`, ... (any case).
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "create" | "created" | "new" | "add" | "added" => Action::Create,
            "delete" | "deleted" | "remove" | "removed" => Action::Delete,
            "rename" | "renamed" | "move" | "moved" => Action::Rename,
            "modify" | "modified" | "change" | "changed" | "write" => Action::Modify,
            "attributes" | "attribute" | "attrs" | "attr" => Action::Attributes,
            _ => return None,
        })
    }

    fn code(self) -> i64 {
        match self {
            Action::Create => 1,
            Action::Delete => 2,
            Action::Rename => 3,
            Action::Modify => 4,
            Action::Attributes => 5,
        }
    }

    fn from_code(code: i64) -> Option<Self> {
        Action::ALL.into_iter().find(|a| a.code() == code)
    }
}

/// One journal entry.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The entry's number, rising with every entry; 0 for one not stored yet.
    pub id: i64,
    /// When the index applied it (the USN record's time for the NTFS index),
    /// milliseconds since the epoch.
    pub time: i64,
    pub action: Action,
    /// The path; for a rename, the new one.
    pub path: String,
    /// A rename's path before it.
    pub old_path: Option<String>,
    pub is_dir: bool,
    /// The size after the change (0 for a folder or a delete).
    pub size: u64,
}

impl Event {
    /// An event for `path` now.
    pub fn new(action: Action, path: &Path) -> Self {
        Self {
            id: 0,
            time: now_ms(),
            action,
            path: path_text(path),
            old_path: None,
            is_dir: false,
            size: 0,
        }
    }

    /// A rename of `old` to `new`, now.
    pub fn renamed(old: &Path, new: &Path) -> Self {
        Self {
            old_path: Some(path_text(old)),
            ..Self::new(Action::Rename, new)
        }
    }

    pub fn dir(mut self, is_dir: bool) -> Self {
        self.is_dir = is_dir;
        self
    }

    pub fn size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    /// At this time (milliseconds since the epoch).
    pub fn at(mut self, time: i64) -> Self {
        self.time = time;
        self
    }

    /// `time` as a `SystemTime`.
    pub fn when(&self) -> SystemTime {
        time_of(self.time)
    }
}

/// Which entries a query returns. The default: all of them, newest first.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Only these actions (empty: all).
    pub actions: Vec<Action>,
    /// From this time on (ms since the epoch, inclusive).
    pub since: Option<i64>,
    /// Before this time (ms since the epoch, exclusive).
    pub until: Option<i64>,
    /// The path or the old path matches this pattern.
    pub path_regex: Option<Regex>,
    /// The path or the old path contains this text (any case).
    pub text: String,
    /// Only the entries of this path: where it is the path or a rename's old path.
    pub path: Option<String>,
    /// Only entries after this one (`Event::id`), for following the journal.
    pub after_id: Option<i64>,
    /// At most this many (0: no limit).
    pub limit: usize,
    /// Oldest first (default: newest first).
    pub oldest_first: bool,
}

impl Filter {
    /// Whether `e` passes the filters SQL does not apply (text and pattern) and those it
    /// does (for events not read from the db).
    pub fn matches(&self, e: &Event) -> bool {
        let paths = || std::iter::once(e.path.as_str()).chain(e.old_path.as_deref());
        (self.actions.is_empty() || self.actions.contains(&e.action))
            && self.since.is_none_or(|t| e.time >= t)
            && self.until.is_none_or(|t| e.time < t)
            && self.after_id.is_none_or(|id| e.id > id)
            && self
                .path
                .as_deref()
                .is_none_or(|p| paths().any(|q| same_path(p, q)))
            && (self.text.is_empty() || {
                let needle = self.text.to_lowercase();
                paths().any(|p| p.to_lowercase().contains(&needle))
            })
            && self
                .path_regex
                .as_ref()
                .is_none_or(|re| paths().any(|p| re.is_match(p)))
    }
}

fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Milliseconds since the epoch, now.
pub fn now_ms() -> i64 {
    ms_of(SystemTime::now())
}

/// A time as milliseconds since the epoch.
pub fn ms_of(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        Err(e) => -(e.duration().as_millis().min(i64::MAX as u128) as i64),
    }
}

fn time_of(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

/// How a path is stored: as text, with `\` separators on Windows.
pub fn path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('/', "\\")
    } else {
        text.into_owned()
    }
}

// ---------------------------------------------------------------------------------
// The journal file

/// A journal file. Every method takes a short lock; reads and writes from other
/// processes (the app, `keel journal`) wait for each other through SQLite.
pub struct Journal {
    conn: Mutex<Connection>,
    path: PathBuf,
    max_bytes: AtomicU64,
}

/// Paths compare without case on Windows (ASCII letters, as SQLite's NOCASE does).
const COLLATE: &str = if cfg!(windows) { "NOCASE" } else { "BINARY" };

fn schema() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS events(
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             time INTEGER NOT NULL,
             action INTEGER NOT NULL,
             path TEXT NOT NULL COLLATE {COLLATE},
             old_path TEXT COLLATE {COLLATE},
             is_dir INTEGER NOT NULL,
             size INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS events_path ON events(path, time);
         CREATE INDEX IF NOT EXISTS events_old_path ON events(old_path)
             WHERE old_path IS NOT NULL;
         CREATE INDEX IF NOT EXISTS events_time ON events(time);"
    )
}

impl Journal {
    /// Opens (or creates) the journal at `path`, kept under `max_bytes` (at least
    /// [`MIN_BYTES`]).
    pub fn open(path: &Path, max_bytes: u64) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // Must come before the first table: a trimmed journal gives its pages back.
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
        conn.execute_batch(&schema())?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            max_bytes: AtomicU64::new(max_bytes.max(MIN_BYTES)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Relaxed)
    }

    /// A new size cap; applied at the next write.
    pub fn set_max_bytes(&self, max: u64) {
        self.max_bytes.store(max.max(MIN_BYTES), Relaxed);
    }

    /// Appends `events` (their `id` is ignored) in one transaction, then drops the
    /// oldest entries while the file is over its cap.
    pub fn record_all(&self, events: &[Event]) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO events(time, action, path, old_path, is_dir, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for e in events {
                insert.execute(rusqlite::params![
                    e.time,
                    e.action.code(),
                    e.path,
                    e.old_path,
                    e.is_dir,
                    e.size.min(i64::MAX as u64) as i64,
                ])?;
            }
        }
        tx.commit()?;
        trim(&conn, self.max_bytes())
    }

    pub fn record(&self, event: Event) -> anyhow::Result<()> {
        self.record_all(std::slice::from_ref(&event))
    }

    /// The entries `filter` asks for.
    pub fn query(&self, filter: &Filter) -> anyhow::Result<Vec<Event>> {
        let mut sql = String::from(
            "SELECT id, time, action, path, old_path, is_dir, size FROM events WHERE 1",
        );
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !filter.actions.is_empty() {
            let codes: Vec<String> = filter
                .actions
                .iter()
                .map(|a| a.code().to_string())
                .collect();
            sql.push_str(&format!(" AND action IN ({})", codes.join(",")));
        }
        for (cond, value) in [
            ("time >=", filter.since),
            ("time <", filter.until),
            ("id >", filter.after_id),
        ] {
            if let Some(v) = value {
                args.push(v.into());
                sql.push_str(&format!(" AND {cond} ?{}", args.len()));
            }
        }
        if let Some(p) = &filter.path {
            args.push(p.clone().into());
            let n = args.len();
            sql.push_str(&format!(" AND (path = ?{n} OR old_path = ?{n})"));
        }
        sql.push_str(if filter.oldest_first {
            " ORDER BY id ASC"
        } else {
            " ORDER BY id DESC"
        });
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let Some(action) = Action::from_code(row.get(2)?) else {
                continue;
            };
            let e = Event {
                id: row.get(0)?,
                time: row.get(1)?,
                action,
                path: row.get(3)?,
                old_path: row.get(4)?,
                is_dir: row.get(5)?,
                size: row.get::<_, i64>(6)?.max(0) as u64,
            };
            if filter.matches(&e) {
                out.push(e);
                if filter.limit != 0 && out.len() >= filter.limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// When `path` last changed by the journal (its newest entry), None when it holds
    /// none: Everything's "date recently changed" (`rc:`).
    pub fn recently_changed(&self, path: &Path) -> anyhow::Result<Option<SystemTime>> {
        let conn = self.conn.lock();
        let t: Option<i64> = conn
            .query_row(
                "SELECT max(time) FROM events WHERE path = ?1",
                [path_text(path)],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(t.map(time_of))
    }

    /// Every path changed since `since` (ms), with its newest time: for filtering many
    /// hits by `rc:` at once.
    pub fn recent(&self, since: i64) -> anyhow::Result<Recent> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT path, max(time) FROM events WHERE time >= ?1 GROUP BY path")?;
        let rows = stmt.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, time): (String, i64) = row?;
            let slot = map.entry(Recent::key(&path)).or_insert(time);
            *slot = (*slot).max(time);
        }
        Ok(Recent(map))
    }

    /// The newest entry's id (0 when empty): `keel journal --watch` follows from it.
    pub fn last_id(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT coalesce(max(id), 0) FROM events", [], |r| r.get(0))?)
    }

    /// How many entries it holds.
    pub fn len(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))? as u64)
    }

    pub fn is_empty(&self) -> anyhow::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// The file's size in bytes (pages in use and free).
    pub fn file_bytes(&self) -> anyhow::Result<u64> {
        let conn = self.conn.lock();
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((pages * size).max(0) as u64)
    }

    /// Deletes every entry.
    pub fn clear(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM events", [])?;
        vacuum(&conn)?;
        Ok(())
    }
}

/// Gives free pages back to the file system. The pragma frees one page per step, so it
/// is stepped to the end (a batch would step it once).
fn vacuum(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA incremental_vacuum")?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

/// Bytes in use: pages minus free pages.
fn used_bytes(conn: &Connection) -> rusqlite::Result<u64> {
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok(((pages - free) * size).max(0) as u64)
}

/// Over the cap: drop the oldest entries down to [`TRIM_TO`] of it and give the pages
/// back to the file system.
fn trim(conn: &Connection, max: u64) -> anyhow::Result<()> {
    for _ in 0..8 {
        let used = used_bytes(conn)?;
        if used <= max {
            break;
        }
        let rows: i64 = conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
        if rows == 0 {
            break;
        }
        let keep = (max as f64 * TRIM_TO) / used as f64;
        let drop = ((rows as f64) * (1.0 - keep))
            .ceil()
            .clamp(1.0, rows as f64) as i64;
        conn.execute(
            "DELETE FROM events WHERE id IN (SELECT id FROM events ORDER BY id LIMIT ?1)",
            [drop],
        )?;
        vacuum(&conn)?;
    }
    Ok(())
}

/// Paths changed since a time, with their newest time ([`Journal::recent`]).
#[derive(Clone, Debug, Default)]
pub struct Recent(HashMap<String, i64>);

impl Recent {
    fn key(path: &str) -> String {
        if cfg!(windows) {
            path.replace('/', "\\").to_lowercase()
        } else {
            path.to_owned()
        }
    }

    /// When `path` last changed, if it did since the time asked for.
    pub fn get(&self, path: &Path) -> Option<SystemTime> {
        self.0
            .get(&Self::key(&path_text(path)))
            .map(|&t| time_of(t))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// ---------------------------------------------------------------------------------
// The process's journal

struct Global {
    settings: JournalSettings,
    /// Set by [`use_path`]; else [`default_path`].
    path: Option<PathBuf>,
    open: Option<Arc<Journal>>,
    /// Opening failed with these settings: not tried again until they change.
    failed: bool,
}

static GLOBAL: Mutex<Global> = parking_lot::const_mutex(Global {
    settings: JournalSettings {
        enabled: true,
        max_mb: crate::index_settings::DEFAULT_JOURNAL_MB,
    },
    path: None,
    open: None,
    failed: false,
});

/// Where the journal lives: `journal.db` in the index folder (`KEEL_INDEX_DIR`,
/// `$KEEL_CONFIG_DIR/index`, else the OS cache folder).
pub fn default_path() -> Option<PathBuf> {
    crate::walkindex::index_dir().map(|d| d.join(FILE_NAME))
}

/// Test binaries never touch a shared journal unless a test names one.
fn in_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.file_name().map(|d| d == "deps"))
        .unwrap_or(false)
}

/// Settings → Search → Journal: on or off, and the size cap.
pub fn configure(settings: &JournalSettings) {
    let mut g = GLOBAL.lock();
    if g.settings == *settings {
        return;
    }
    g.settings = settings.clone();
    g.failed = false;
    if !settings.enabled {
        g.open = None;
    } else if let Some(j) = &g.open {
        j.set_max_bytes(settings.max_bytes());
    }
}

/// Keeps the process's journal at `path` (tests, tools); `None`: the default place.
pub fn use_path(path: Option<PathBuf>) {
    let mut g = GLOBAL.lock();
    g.path = path;
    g.open = None;
    g.failed = false;
}

/// The process's journal, opened on first use; None when it is off or cannot open.
pub fn global() -> Option<Arc<Journal>> {
    let mut g = GLOBAL.lock();
    if !g.settings.enabled || g.failed {
        return None;
    }
    if let Some(j) = &g.open {
        return Some(j.clone());
    }
    let path = match &g.path {
        Some(p) => p.clone(),
        None if in_test_binary() => return None,
        None => default_path()?,
    };
    match Journal::open(&path, g.settings.max_bytes()) {
        Ok(j) => {
            let j = Arc::new(j);
            g.open = Some(j.clone());
            Some(j)
        }
        Err(_) => {
            g.failed = true;
            None
        }
    }
}

/// Whether the journal is on (Settings → Search → Journal).
pub fn enabled() -> bool {
    GLOBAL.lock().settings.enabled
}

/// Adds one entry to the process's journal (nothing when it is off). For indexers:
/// the NTFS USN reader calls this (or [`record_all`] per batch) for each change it
/// applies.
pub fn record(event: Event) {
    record_all(vec![event]);
}

/// Adds entries to the process's journal in one write.
pub fn record_all(events: Vec<Event>) {
    if events.is_empty() {
        return;
    }
    if let Some(j) = global() {
        // Best effort: a full disk must not stop the index.
        let _ = j.record_all(&events);
    }
}

/// When `path` last changed by the process's journal (`rc:`); None when off or unknown.
pub fn recently_changed(path: &Path) -> Option<SystemTime> {
    global()?.recently_changed(path).ok().flatten()
}

// ---------------------------------------------------------------------------------
// Turning index changes into entries

/// An indexed entry's columns, as the index saw them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<i64>,
    pub created: Option<i64>,
    pub attrs: u32,
}

/// What an index did to one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Created(PathBuf, Stat),
    Deleted(PathBuf, Stat),
    /// Before and after.
    Changed(PathBuf, Stat, Stat),
}

/// The same file under another name: its type, size (files), modified time (files)
/// and created time. None when they cannot tell (no created time for a folder, no
/// times at all for a file).
fn identity(s: &Stat) -> Option<(bool, u64, Option<i64>, Option<i64>)> {
    if s.is_dir {
        s.created.map(|c| (true, 0, None, Some(c)))
    } else if s.mtime.is_some() || s.created.is_some() {
        Some((false, s.size, s.mtime, s.created))
    } else {
        None
    }
}

/// Below `root` (not `root` itself).
fn inside(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

/// The journal entries for one batch of index changes, all at `time`: deletes and
/// creates paired into renames (`renames` as the watcher reported them, else by
/// [`identity`]), the entries inside a renamed folder dropped, and in-place changes
/// sorted into modify (size or modified time of a file, or its type) and attributes.
pub(crate) fn events_from(
    changes: Vec<Change>,
    renames: &[(PathBuf, PathBuf)],
    time: i64,
) -> Vec<Event> {
    let mut deleted: HashMap<&Path, Stat> = HashMap::new();
    let mut created: Vec<(&Path, Stat)> = Vec::new();
    for c in &changes {
        match c {
            Change::Deleted(p, s) => {
                deleted.insert(p.as_path(), *s);
            }
            Change::Created(p, s) => created.push((p.as_path(), *s)),
            Change::Changed(..) => {}
        }
    }
    let was_dir = |p: &Path| deleted.get(p).is_some_and(|s| s.is_dir);
    let created_set: HashSet<&Path> = created.iter().map(|(p, _)| *p).collect();
    // new path -> old path
    let mut pairs: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut taken_old: HashSet<PathBuf> = HashSet::new();
    for (old, new) in renames {
        if deleted.contains_key(old.as_path())
            && created_set.contains(new.as_path())
            && !taken_old.contains(old)
            && !pairs.contains_key(new)
        {
            pairs.insert(new.clone(), old.clone());
            taken_old.insert(old.clone());
        }
    }
    // Unpaired: match by identity, shallowest first so a folder pairs before its files.
    let mut by_id: HashMap<_, Vec<&Path>> = HashMap::new();
    let mut olds: Vec<(&Path, Stat)> = deleted.iter().map(|(p, s)| (*p, *s)).collect();
    olds.sort_by_key(|(p, _)| (p.components().count(), p.to_path_buf()));
    for (p, s) in olds {
        if let Some(id) = identity(&s) {
            by_id.entry(id).or_default().push(p);
        }
    }
    created.sort_by_key(|(p, _)| p.components().count());
    for (new, s) in &created {
        if pairs.contains_key(*new) {
            continue;
        }
        // Inside a folder already paired: it moved with it.
        if pairs.keys().any(|n| inside(new, n)) {
            continue;
        }
        let Some(list) = identity(s).and_then(|id| by_id.get_mut(&id)) else {
            continue;
        };
        let found = list.iter().position(|o| {
            !taken_old.contains(*o) && !pairs.values().any(|d| was_dir(d) && inside(o, d))
        });
        if let Some(i) = found {
            let old = list.remove(i).to_path_buf();
            taken_old.insert(old.clone());
            pairs.insert(new.to_path_buf(), old);
        }
    }
    let renamed_dirs: Vec<(&PathBuf, &PathBuf)> =
        pairs.iter().filter(|(_, o)| was_dir(o)).collect();
    let mut out = Vec::new();
    for c in &changes {
        let event = match c {
            Change::Created(p, s) => if let Some(old) = pairs.get(p) {
                Event::renamed(old, p)
            } else if renamed_dirs.iter().any(|(n, _)| inside(p, n)) {
                continue;
            } else {
                Event::new(Action::Create, p)
            }
            .dir(s.is_dir)
            .size(if s.is_dir { 0 } else { s.size }),
            Change::Deleted(p, s) => {
                if taken_old.contains(p) || renamed_dirs.iter().any(|(_, o)| inside(p, o)) {
                    continue;
                }
                Event::new(Action::Delete, p).dir(s.is_dir)
            }
            Change::Changed(p, a, b) => {
                let action = if a.is_dir != b.is_dir
                    || (!b.is_dir && (a.size != b.size || a.mtime != b.mtime))
                {
                    Action::Modify
                } else if a.attrs != b.attrs {
                    Action::Attributes
                } else {
                    continue;
                };
                Event::new(action, p)
                    .dir(b.is_dir)
                    .size(if b.is_dir { 0 } else { b.size })
            }
        };
        out.push(event.at(time));
    }
    out
}

// ---------------------------------------------------------------------------------
// Times as typed and shown

/// `ms` as local time, `2026-10-12 14:03:05`.
pub fn local_text(ms: i64) -> String {
    use chrono::{Datelike, TimeZone, Timelike};
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.year(),
            t.month(),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ),
        None => ms.to_string(),
    }
}

/// The start of today, local time, in ms.
pub fn start_of_today() -> i64 {
    day_start(chrono::Local::now().date_naive()).unwrap_or_else(now_ms)
}

fn day_start(day: chrono::NaiveDate) -> Option<i64> {
    use chrono::TimeZone;
    let t = chrono::Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .earliest()?;
    Some(t.timestamp_millis())
}

/// A time as typed: `today`, `yesterday`, `now`, a span back from now (`30m`, `2h`,
/// `7d`, `1w`, `90s`), a local date `2026-10-12`, or a date and time
/// `2026-10-12 14:30[:05]` (or with `T`). Returns (ms, whether it was a whole day).
fn parse_when(text: &str, now: i64) -> Option<(i64, bool)> {
    let text = text.trim().to_ascii_lowercase();
    let today = chrono::Local::now().date_naive();
    match text.as_str() {
        "" => return None,
        "now" => return Some((now, false)),
        "today" => return Some((day_start(today)?, true)),
        "yesterday" => return Some((day_start(today.pred_opt()?)?, true)),
        _ => {}
    }
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &text[digits.len()..];
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && !unit.is_empty() {
        let n: i64 = digits.parse().ok()?;
        let secs = match unit {
            "s" | "sec" | "secs" => 1,
            "m" | "min" | "mins" => 60,
            "h" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 7 * 86_400,
            _ => return None,
        };
        return Some((now - n.checked_mul(secs * 1000)?, false));
    }
    let (date, clock) = match text.split_once([' ', 't']) {
        Some((d, c)) => (d, Some(c)),
        None => (text.as_str(), None),
    };
    let mut ymd = date.split('-').map(|p| p.parse::<u32>().ok());
    let (y, m, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    if ymd.next().is_some() {
        return None;
    }
    let day = chrono::NaiveDate::from_ymd_opt(y as i32, m, d)?;
    let Some(clock) = clock else {
        return Some((day_start(day)?, true));
    };
    let mut hms = clock.split(':').map(|p| p.parse::<u32>().ok());
    let (h, mi) = (hms.next()??, hms.next()??);
    let s = hms.next().unwrap_or(Some(0))?;
    use chrono::TimeZone;
    let t = chrono::Local
        .from_local_datetime(&day.and_hms_opt(h, mi, s)?)
        .earliest()?;
    Some((t.timestamp_millis(), false))
}

/// The start of a range as typed (see [`parse_when`]): a date means its first moment.
pub fn parse_since(text: &str) -> Option<i64> {
    parse_when(text, now_ms()).map(|(t, _)| t)
}

/// The end of a range as typed (exclusive): a date means the whole of that day.
pub fn parse_until(text: &str) -> Option<i64> {
    let (t, day) = parse_when(text, now_ms())?;
    Some(if day { t + 86_400_000 } else { t })
}

/// `events` as CSV with a header: local time, action, path, old path, folder, size.
pub fn csv(events: &[Event]) -> String {
    fn field(s: &str) -> String {
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_owned()
        }
    }
    let mut out = String::from("time,action,path,old_path,folder,size\n");
    for e in events {
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            local_text(e.time),
            e.action.as_str(),
            field(&e.path),
            field(e.old_path.as_deref().unwrap_or("")),
            e.is_dir,
            e.size
        ));
    }
    out
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
