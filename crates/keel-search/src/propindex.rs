//! The property index (Everything 1.5's Properties tab): the file properties chosen in
//! Settings → Search index → Properties (`artist`, `width`, `sha256`, any canonical
//! Windows property) are read by [`keel_props`] into an on-disk store next to the index
//! (`<index dir>/props.db`), so a search or a sort by them is a read instead of opening
//! every file.
//!
//! An idle background job fills the store: it walks the chosen folders (by default the
//! home folder and the Folders and Volumes of Settings → Search index), reads the files the
//! include and exclude rules admit, and skips those whose size and modified time did not
//! move since they were read. It pauses for a moment after user input
//! ([`note_activity`]) and on battery, like the library's sidecar job, and resumes where it
//! stopped. Between passes it follows the index's changes ([`note_changes`], fed by the
//! walk index for every change it applies): created and modified files are read again,
//! deleted ones dropped, renamed ones moved. A full pass runs again every day.
//!
//! Cloud placeholders are never downloaded: **Exclude recall on data access** leaves them
//! out (on by default), and even when it is off their content is not read
//! ([`keel_props::Options::allow_recall`] stays false).
//!
//! **Fast sort** keeps a sort order per chosen property (a partial SQLite index), so
//! [`PropStore::sorted`] reads results in order.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use keel_props::{FileProps, HashAlgo, Prop};
use parking_lot::{const_mutex, Condvar, Mutex};
use rusqlite::{params, Connection, OptionalExtension};

use crate::exclude::ExcludeRules;
use crate::index_settings::{ExcludeSettings, PropertySettings};
use crate::journal::{self, Action, Event};

/// The property store's file name in the index folder.
pub const FILE_NAME: &str = "props.db";
/// The job pauses this long after user input.
pub const ACTIVITY_PAUSE: Duration = Duration::from_secs(1);
/// How often a paused job looks again.
const PAUSE_POLL: Duration = Duration::from_millis(250);
/// A full pass runs again after this long (changes are followed in between).
const PASS_EVERY: Duration = Duration::from_secs(24 * 3600);
/// The longest an idle job sleeps between checks.
const IDLE_WAIT: Duration = Duration::from_secs(60);
/// Changes queued for the job at most; past this the next pass picks them up.
const MAX_PENDING: usize = if cfg!(test) { 100 } else { 50_000 };
/// Folders deeper than this are not walked.
const MAX_DEPTH: usize = 512;
/// Battery readings are kept this long.
const BATTERY_TTL: Duration = Duration::from_secs(30);

/// Windows attribute bits the rules look at.
mod attr {
    pub const OFFLINE: u32 = 0x1000;
    pub const NOT_CONTENT_INDEXED: u32 = 0x2000;
    pub const RECALL_ON_OPEN: u32 = 0x4_0000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
}

/// Paths compare without case on Windows (ASCII letters, as SQLite's NOCASE does).
const COLLATE: &str = if cfg!(windows) { "NOCASE" } else { "BINARY" };

// ---------------------------------------------------------------------------------
// The store

/// What the store knows of one property of one file.
#[derive(Clone, Debug, PartialEq)]
pub enum Lookup {
    /// The file is not in the store (not read yet, left out, or the property is not
    /// indexed): read it from the file.
    NotIndexed,
    /// Read: the file has no such property.
    Unknown,
    Value(Prop),
}

const K_STR: i64 = 1;
const K_INT: i64 = 2;
const K_FLOAT: i64 = 3;
const K_BOOL: i64 = 4;
const K_DATE: i64 = 5;
const K_DURATION: i64 = 6;
const K_LIST: i64 = 7;
/// Joins the elements of a list value.
const LIST_SEP: char = '\u{1f}';

fn ms_of(t: SystemTime) -> i64 {
    journal::ms_of(t)
}

fn time_of(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

/// A value as (kind, number, text); `None` for unknown.
fn encode(p: &Prop) -> Option<(i64, Option<f64>, Option<String>)> {
    if p.is_unknown() {
        return None;
    }
    Some(match p {
        Prop::Unknown => return None,
        Prop::Str(s) => (K_STR, None, Some(s.clone())),
        Prop::Int(n) => (K_INT, Some(*n as f64), Some(n.to_string())),
        Prop::Float(f) => (K_FLOAT, Some(*f), None),
        Prop::Bool(b) => (K_BOOL, Some(f64::from(u8::from(*b))), None),
        Prop::Date(t) => (K_DATE, Some(ms_of(*t) as f64), None),
        Prop::Duration(d) => (K_DURATION, Some(d.as_secs_f64()), None),
        Prop::List(items) => {
            let text: Vec<String> = items.iter().map(|i| i.to_string()).collect();
            (K_LIST, None, Some(text.join(&LIST_SEP.to_string())))
        }
    })
}

fn decode(kind: i64, num: Option<f64>, text: Option<String>) -> Prop {
    match kind {
        K_STR => text.map_or(Prop::Unknown, Prop::Str),
        K_INT => text
            .and_then(|t| t.parse().ok())
            .or(num.map(|n| n as i64))
            .map_or(Prop::Unknown, Prop::Int),
        K_FLOAT => num.map_or(Prop::Unknown, Prop::Float),
        K_BOOL => num.map_or(Prop::Unknown, |n| Prop::Bool(n != 0.0)),
        K_DATE => num.map_or(Prop::Unknown, |n| Prop::Date(time_of(n as i64))),
        K_DURATION => num
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map_or(Prop::Unknown, |n| {
                Prop::Duration(Duration::from_secs_f64(n))
            }),
        K_LIST => text.map_or(Prop::Unknown, |t| {
            Prop::List(t.split(LIST_SEP).map(|s| Prop::Str(s.to_owned())).collect())
        }),
        _ => Prop::Unknown,
    }
}

/// A path as stored: the journal's text form.
fn stored(path: &Path) -> String {
    journal::path_text(path)
}

/// A property name as an SQL string literal.
fn literal(name: &str) -> String {
    format!("'{}'", name.replace('\'', "''"))
}

/// The fast-sort index of a property: `fs_` and the name's bytes in hex.
fn sort_index_name(name: &str) -> String {
    let hex: String = name
        .to_ascii_lowercase()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("fs_{hex}")
}

/// The property store file.
pub struct PropStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

/// What the store holds of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileState {
    pub size: u64,
    /// Milliseconds since the epoch.
    pub modified: i64,
    /// The property list the file was read with ([`props_key`]).
    pub props: String,
}

/// The text a list of property names is stored under.
pub fn props_key(names: &[String]) -> String {
    let mut v: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
    v.sort();
    v.join("\n")
}

impl PropStore {
    /// Opens (or creates) the store at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(&format!(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS files(
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE COLLATE {COLLATE},
                 size INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 props TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS vals(
                 file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 prop TEXT NOT NULL COLLATE NOCASE,
                 kind INTEGER NOT NULL,
                 num REAL,
                 text TEXT,
                 PRIMARY KEY(file, prop)) WITHOUT ROWID;"
        ))?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the store holds of `path`, if anything.
    pub fn file(&self, path: &Path) -> Option<FileState> {
        self.conn
            .lock()
            .query_row(
                "SELECT size, mtime, props FROM files WHERE path = ?1",
                [stored(path)],
                |r| {
                    Ok(FileState {
                        size: r.get::<_, i64>(0)?.max(0) as u64,
                        modified: r.get(1)?,
                        props: r.get(2)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Stores what was read of `path` (its earlier values are replaced); unknown values
    /// are not stored.
    pub fn put(
        &self,
        path: &Path,
        state: &FileState,
        values: &[(String, Prop)],
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let text = stored(path);
        tx.execute(
            "INSERT INTO files(path, size, mtime, props) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET size = ?2, mtime = ?3, props = ?4",
            params![
                text,
                state.size.min(i64::MAX as u64) as i64,
                state.modified,
                state.props
            ],
        )?;
        let id: i64 = tx.query_row("SELECT id FROM files WHERE path = ?1", [&text], |r| {
            r.get(0)
        })?;
        tx.execute("DELETE FROM vals WHERE file = ?1", [id])?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO vals(file, prop, kind, num, text)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (name, value) in values {
                if let Some((kind, num, text)) = encode(value) {
                    insert.execute(params![id, name, kind, num, text])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The stored value of property `name` of `path`.
    pub fn get(&self, path: &Path, name: &str) -> Lookup {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT f.props, v.kind, v.num, v.text FROM files f
                 LEFT JOIN vals v ON v.file = f.id AND v.prop = ?2
                 WHERE f.path = ?1",
                params![stored(path), name],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten();
        match row {
            None => Lookup::NotIndexed,
            Some((props, None, _, _)) => {
                let lower = name.to_ascii_lowercase();
                if props.split('\n').any(|p| p == lower) {
                    Lookup::Unknown
                } else {
                    Lookup::NotIndexed
                }
            }
            Some((_, Some(kind), num, text)) => Lookup::Value(decode(kind, num, text)),
        }
    }

    /// Every stored value of `path`.
    pub fn values(&self, path: &Path) -> Vec<(String, Prop)> {
        let conn = self.conn.lock();
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT v.prop, v.kind, v.num, v.text FROM vals v JOIN files f ON f.id = v.file
             WHERE f.path = ?1 ORDER BY v.prop",
        ) else {
            return Vec::new();
        };
        stmt.query_map([stored(path)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                decode(r.get(1)?, r.get(2)?, r.get(3)?),
            ))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Drops `path` and, for a folder, everything below it.
    pub fn remove(&self, path: &Path) -> anyhow::Result<usize> {
        let text = stored(path);
        let below = below_prefix(&text);
        Ok(self.conn.lock().execute(
            &format!(
                "DELETE FROM files WHERE path = ?1
                 OR substr(path, 1, ?3) = ?2 COLLATE {COLLATE}"
            ),
            params![text, below, below.chars().count() as i64],
        )?)
    }

    /// Moves what is stored of `old` (and below it) to `new`.
    pub fn rename(&self, old: &Path, new: &Path) -> anyhow::Result<usize> {
        let (old, new) = (stored(old), stored(new));
        let below = below_prefix(&old);
        let new_below = below_prefix(&new);
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // What is stored at the new path (and below it) is replaced.
        tx.execute(
            &format!(
                "DELETE FROM files
                 WHERE (path = ?1 OR substr(path, 1, ?2) = ?3 COLLATE {COLLATE})
                 AND NOT (path = ?4 OR substr(path, 1, ?5) = ?6 COLLATE {COLLATE})"
            ),
            params![
                new,
                new_below.chars().count() as i64,
                new_below,
                old,
                below.chars().count() as i64,
                below
            ],
        )?;
        let n = tx.execute(
            &format!(
                "UPDATE files SET path = ?2 || substr(path, ?3 + 1)
                 WHERE path = ?1 OR substr(path, 1, ?4) = ?5 COLLATE {COLLATE}"
            ),
            params![
                old,
                new,
                old.chars().count() as i64,
                below.chars().count() as i64,
                below
            ],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Paths with a known value of `name`, in its order (numbers, then text), at most
    /// `limit`. With **Fast sort** on for `name` this reads an index instead of sorting.
    pub fn sorted(&self, name: &str, descending: bool, limit: usize) -> Vec<String> {
        let dir = if descending { "DESC" } else { "ASC" };
        // The name is a literal so the planner can use the property's partial index.
        let sql = format!(
            "SELECT f.path FROM vals v JOIN files f ON f.id = v.file
             WHERE v.prop = {} ORDER BY v.num {dir}, v.text {dir} LIMIT ?1",
            literal(name)
        );
        let conn = self.conn.lock();
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Vec::new();
        };
        stmt.query_map([limit.min(i64::MAX as usize) as i64], |r| r.get(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Keeps a sort order for exactly the properties in `names` (Fast sort).
    pub fn set_fast_sort(&self, names: &[String]) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        let want: HashSet<String> = names.iter().map(|n| sort_index_name(n)).collect();
        let have: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'fs\\_%' ESCAPE '\\'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };
        for name in &have {
            if !want.contains(name) {
                conn.execute_batch(&format!("DROP INDEX IF EXISTS \"{name}\""))?;
            }
        }
        for n in names {
            conn.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS \"{}\" ON vals(num, text) WHERE prop = {}",
                sort_index_name(n),
                literal(n)
            ))?;
        }
        Ok(())
    }

    /// The properties with a fast-sort order now (their index names).
    pub fn fast_sort_indexes(&self) -> Vec<String> {
        let conn = self.conn.lock();
        let Ok(mut stmt) = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'fs\\_%' ESCAPE '\\' ORDER BY name",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| r.get(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Drops the values of properties no longer in `names`.
    pub fn keep_only(&self, names: &[String]) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        if names.is_empty() {
            conn.execute("DELETE FROM files", [])?;
            return Ok(());
        }
        let list: Vec<String> = names.iter().map(|n| literal(n)).collect();
        conn.execute(
            &format!("DELETE FROM vals WHERE prop NOT IN ({})", list.join(", ")),
            [],
        )?;
        Ok(())
    }

    /// How many files the store holds.
    pub fn len(&self) -> u64 {
        self.conn
            .lock()
            .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .map_or(0, |n| n.max(0) as u64)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        self.conn.lock().execute("DELETE FROM files", [])?;
        Ok(())
    }
}

/// `path` with a separator after it: what paths below it start with.
fn below_prefix(text: &str) -> String {
    let sep = if cfg!(windows) { '\\' } else { '/' };
    if text.ends_with(['\\', '/']) {
        text.to_owned()
    } else {
        format!("{text}{sep}")
    }
}

// ---------------------------------------------------------------------------------
// What the job reads

/// Which files the job reads, from Settings → Search index → Properties.
#[derive(Clone, Debug)]
pub struct Rules {
    names: Vec<String>,
    /// Hash functions among `names`, read in one pass.
    hashes: Vec<(String, HashAlgo)>,
    roots: Vec<PathBuf>,
    exclude: ExcludeRules,
    not_content_indexed: bool,
    placeholders: bool,
    max_bytes: u64,
    fast_sort: Vec<String>,
}

impl Rules {
    /// `index_roots`: the folders the index holds, walked when the settings name no
    /// include-only folders.
    pub fn new(s: &PropertySettings, index_roots: &[PathBuf]) -> Self {
        let names = s.names();
        let hashes = names
            .iter()
            .filter_map(|n| match keel_props::lookup(n)?.reader {
                keel_props::functions::Reader::Hash(a) => Some((n.clone(), a)),
                _ => None,
            })
            .collect();
        let only: Vec<PathBuf> = (s.include_only_folders.iter())
            .map(|f| f.trim())
            .filter(|f| !f.is_empty())
            .map(PathBuf::from)
            .collect();
        let roots = if only.is_empty() {
            index_roots.to_vec()
        } else {
            only
        };
        let exclude = ExcludeRules::new(&ExcludeSettings {
            hidden: false,
            system: false,
            folders: s.exclude_folders.clone(),
            include_only_folders: s.include_only_folders.clone(),
            include_only_files: s.include_only_files.clone(),
            files: s.exclude_files.clone(),
        });
        let fast_sort = (s.properties.iter())
            .filter(|p| p.fast_sort && !p.name.trim().is_empty())
            .map(|p| p.name.trim().to_owned())
            .collect();
        Self {
            names,
            hashes,
            roots: crate::walkindex::normalize_roots(roots),
            exclude,
            not_content_indexed: s.exclude_not_content_indexed,
            placeholders: s.exclude_recall_on_data_access,
            max_bytes: s.max_file_bytes(),
            fast_sort,
        }
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether a file with these size and attributes ([`attr`] bits; `placeholder`: a
    /// cloud file whose data is elsewhere) is read. The path's folders are checked by
    /// [`Rules::admit_path`].
    pub fn admit_file(&self, path: &Path, size: u64, attrs: u32, placeholder: bool) -> bool {
        if size > self.max_bytes {
            return false;
        }
        if self.not_content_indexed && attrs & attr::NOT_CONTENT_INDEXED != 0 {
            return false;
        }
        const CLOUD: u32 = attr::OFFLINE | attr::RECALL_ON_OPEN | attr::RECALL_ON_DATA_ACCESS;
        if self.placeholders && (placeholder || attrs & CLOUD != 0) {
            return false;
        }
        self.exclude.admit(path, false, attrs)
    }

    /// Whether a folder is walked.
    fn admit_dir(&self, path: &Path) -> bool {
        self.exclude.admit(path, true, 0)
    }

    /// Whether `path` lies under a root, in folders the rules walk.
    pub fn admit_path(&self, path: &Path) -> bool {
        let Some(root) = self.roots.iter().find(|r| path.starts_with(r)) else {
            return false;
        };
        let mut dirs: Vec<&Path> = path
            .ancestors()
            .skip(1)
            .take_while(|a| a.starts_with(root))
            .collect();
        dirs.pop(); // the root itself is walked whatever its name
        dirs.iter().all(|d| self.admit_dir(d))
    }
}

/// A file's size, modified time (ms), attributes and whether it is a cloud placeholder.
fn file_meta(meta: &std::fs::Metadata) -> (u64, i64, u32, bool) {
    let modified = meta.modified().map_or(i64::MIN, ms_of);
    #[cfg(windows)]
    let attrs = std::os::windows::fs::MetadataExt::file_attributes(meta);
    #[cfg(not(windows))]
    let attrs = 0;
    (
        meta.len(),
        modified,
        attrs,
        keel_props::is_placeholder(meta),
    )
}

// ---------------------------------------------------------------------------------
// The job

/// The clock, the battery and sleeping, for the job ([`SystemEnv`]; a fake in tests).
pub trait Env: Send + Sync {
    /// Milliseconds since the epoch.
    fn now_ms(&self) -> i64;
    /// Some battery is discharging.
    fn on_battery(&self) -> bool;
    fn sleep(&self, d: Duration);
}

/// The real clock; the battery as the app reports it ([`set_battery_probe`]).
pub struct SystemEnv;

static BATTERY_PROBE: Mutex<Option<fn() -> bool>> = const_mutex(None);
static BATTERY: Mutex<Option<(std::time::Instant, bool)>> = const_mutex(None);

/// How the job tells whether the machine runs on battery (the app passes keel-core's
/// reading); without one, never.
pub fn set_battery_probe(probe: Option<fn() -> bool>) {
    *BATTERY_PROBE.lock() = probe;
    *BATTERY.lock() = None;
}

impl Env for SystemEnv {
    fn now_ms(&self) -> i64 {
        journal::now_ms()
    }

    fn on_battery(&self) -> bool {
        let Some(probe) = *BATTERY_PROBE.lock() else {
            return false;
        };
        let mut cached = BATTERY.lock();
        match *cached {
            Some((at, v)) if at.elapsed() < BATTERY_TTL => v,
            _ => {
                let v = probe();
                *cached = Some((std::time::Instant::now(), v));
                v
            }
        }
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// When the user was last active: the job waits [`ACTIVITY_PAUSE`] after it.
#[derive(Debug, Default)]
pub struct Activity(AtomicI64);

impl Activity {
    /// User input at `now_ms`.
    pub fn note(&self, now_ms: i64) {
        self.0.fetch_max(now_ms, Relaxed);
    }

    pub fn busy(&self, now_ms: i64) -> bool {
        let last = self.0.load(Relaxed);
        last != 0 && now_ms < last.saturating_add(ACTIVITY_PAUSE.as_millis() as i64)
    }
}

/// One index change for the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Read the file again (created, modified, attributes changed).
    Read(PathBuf),
    /// Dropped with everything below it.
    Remove(PathBuf),
    Rename(PathBuf, PathBuf),
}

#[derive(Default)]
struct Queue {
    changes: VecDeque<Change>,
    /// Changes were dropped: the next pass is due.
    overflow: bool,
    /// [`Pending::wake`] was called since the last wait.
    woken: bool,
}

/// Changes queued for a job.
#[derive(Default)]
pub struct Pending {
    queue: Mutex<Queue>,
    wake: Condvar,
}

impl Pending {
    /// Queues `changes`; past [`MAX_PENDING`] they are dropped and the next pass is due.
    pub fn push(&self, changes: impl IntoIterator<Item = Change>) {
        let mut q = self.queue.lock();
        for c in changes {
            if q.changes.len() >= MAX_PENDING {
                q.overflow = true;
                break;
            }
            q.changes.push_back(c);
        }
        drop(q);
        self.wake.notify_all();
    }

    fn pop(&self) -> Option<Change> {
        self.queue.lock().changes.pop_front()
    }

    /// True once after changes were dropped.
    fn take_overflow(&self) -> bool {
        std::mem::take(&mut self.queue.lock().overflow)
    }

    pub fn len(&self) -> usize {
        self.queue.lock().changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Waits for a change or a wake-up, at most `timeout`.
    fn wait(&self, timeout: Duration) {
        let mut q = self.queue.lock();
        if q.changes.is_empty() && !q.overflow && !q.woken {
            self.wake.wait_for(&mut q, timeout);
        }
        q.woken = false;
    }

    /// Ends a [`Pending::wait`] now (or the next one, when none is waiting).
    pub fn wake(&self) {
        self.queue.lock().woken = true;
        self.wake.notify_all();
    }
}

/// The job's changes from index events.
pub fn changes_of(events: &[Event]) -> Vec<Change> {
    let mut out = Vec::new();
    for e in events {
        let path = PathBuf::from(&e.path);
        match e.action {
            Action::Delete => out.push(Change::Remove(path)),
            Action::Rename => match &e.old_path {
                Some(old) => out.push(Change::Rename(PathBuf::from(old), path)),
                None => out.push(Change::Read(path)),
            },
            Action::Create | Action::Modify | Action::Attributes if !e.is_dir => {
                out.push(Change::Read(path))
            }
            _ => {}
        }
    }
    out
}

/// What one [`Indexer::step`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Asked to stop.
    Stopped,
    /// The user is active or the machine runs on battery: nothing was done.
    Paused,
    /// Read a file's properties.
    Read(PathBuf),
    /// Looked at an entry and left it (a folder, unchanged, or not admitted).
    Skipped,
    /// Dropped or moved stored files.
    Updated,
    /// A full pass finished.
    PassDone,
    /// Nothing to do until a change comes or the next pass is due.
    Idle,
}

/// The job's counters, for Settings.
#[derive(Debug, Default)]
pub struct Progress {
    /// Files read in this process.
    pub read: AtomicU64,
    /// Entries looked at in the current pass.
    pub seen: AtomicU64,
    /// Paused right now.
    pub paused: AtomicBool,
    /// A pass is running.
    pub walking: AtomicBool,
    /// When the last full pass finished (ms), 0: never in this process.
    pub last_pass: AtomicI64,
}

/// A resumable walk: the folders still to list and the listing in progress.
struct Walk {
    dirs: Vec<(PathBuf, usize)>,
    current: Option<(std::fs::ReadDir, usize)>,
}

impl Walk {
    fn new(roots: &[PathBuf]) -> Self {
        Self {
            dirs: roots.iter().rev().map(|r| (r.clone(), 0)).collect(),
            current: None,
        }
    }

    /// The next entry: (path, metadata, depth); None at the end.
    fn next(&mut self) -> Option<(PathBuf, std::fs::Metadata, usize)> {
        loop {
            if let Some((rd, depth)) = &mut self.current {
                match rd.next() {
                    Some(Ok(entry)) => {
                        let Ok(meta) = entry.metadata() else {
                            continue;
                        };
                        return Some((entry.path(), meta, *depth + 1));
                    }
                    Some(Err(_)) => continue,
                    None => self.current = None,
                }
            }
            let (dir, depth) = self.dirs.pop()?;
            if let Ok(rd) = std::fs::read_dir(&dir) {
                self.current = Some((rd, depth));
            }
        }
    }

    fn push_dir(&mut self, dir: PathBuf, depth: usize) {
        self.dirs.push((dir, depth));
    }
}

/// The property job: [`Indexer::step`] does one small piece of work; [`Indexer::run`]
/// loops until stopped.
pub struct Indexer {
    rules: Rules,
    key: String,
    store: Arc<PropStore>,
    env: Arc<dyn Env>,
    activity: Arc<Activity>,
    pause_on_battery: bool,
    pending: Arc<Pending>,
    cancel: keel_props::Cancel,
    walk: Option<Walk>,
    /// When the next full pass is due (ms); `i64::MIN`: now.
    next_pass: i64,
    pub progress: Arc<Progress>,
}

impl Indexer {
    pub fn new(
        rules: Rules,
        store: Arc<PropStore>,
        env: Arc<dyn Env>,
        activity: Arc<Activity>,
        pending: Arc<Pending>,
    ) -> anyhow::Result<Self> {
        store.keep_only(rules.names())?;
        store.set_fast_sort(&rules.fast_sort)?;
        Ok(Self {
            key: props_key(rules.names()),
            rules,
            store,
            env,
            activity,
            pause_on_battery: true,
            pending,
            cancel: keel_props::Cancel::default(),
            walk: None,
            next_pass: i64::MIN,
            progress: Arc::default(),
        })
    }

    /// Whether the job pauses on battery (default true).
    pub fn pause_on_battery(mut self, on: bool) -> Self {
        self.pause_on_battery = on;
        self
    }

    /// The flag that stops [`Indexer::run`] (and a hash in progress).
    pub fn cancel(&self) -> keel_props::Cancel {
        self.cancel.clone()
    }

    fn busy(&self) -> bool {
        self.activity.busy(self.env.now_ms()) || (self.pause_on_battery && self.env.on_battery())
    }

    /// One piece of work: a queued change first, else the next entry of the pass.
    pub fn step(&mut self) -> Step {
        if self.cancel.is_cancelled() {
            return Step::Stopped;
        }
        if self.busy() {
            self.progress.paused.store(true, Relaxed);
            return Step::Paused;
        }
        self.progress.paused.store(false, Relaxed);
        if self.pending.take_overflow() {
            self.next_pass = i64::MIN;
        }
        if let Some(change) = self.pending.pop() {
            return self.apply(change);
        }
        if self.walk.is_none() {
            if self.env.now_ms() < self.next_pass || self.rules.names.is_empty() {
                return Step::Idle;
            }
            self.walk = Some(Walk::new(&self.rules.roots));
            self.progress.seen.store(0, Relaxed);
            self.progress.walking.store(true, Relaxed);
        }
        let next = self.walk.as_mut().and_then(Walk::next);
        let Some((path, meta, depth)) = next else {
            self.walk = None;
            let now = self.env.now_ms();
            self.next_pass = now.saturating_add(PASS_EVERY.as_millis() as i64);
            self.progress.last_pass.store(now, Relaxed);
            self.progress.walking.store(false, Relaxed);
            return Step::PassDone;
        };
        self.progress.seen.fetch_add(1, Relaxed);
        if meta.is_dir() {
            if depth < MAX_DEPTH && self.rules.admit_dir(&path) {
                if let Some(w) = &mut self.walk {
                    w.push_dir(path, depth);
                }
            }
            return Step::Skipped;
        }
        if !meta.is_file() {
            return Step::Skipped;
        }
        self.consider(path, &meta)
    }

    /// Reads `path` when it is admitted and changed since it was read; drops it when it
    /// is no longer admitted.
    fn consider(&mut self, path: PathBuf, meta: &std::fs::Metadata) -> Step {
        let (size, modified, attrs, placeholder) = file_meta(meta);
        if !self.rules.admit_file(&path, size, attrs, placeholder) {
            if self.store.file(&path).is_some() {
                let _ = self.store.remove(&path);
                return Step::Updated;
            }
            return Step::Skipped;
        }
        let state = FileState {
            size,
            modified,
            props: self.key.clone(),
        };
        if self.store.file(&path).as_ref() == Some(&state) {
            return Step::Skipped;
        }
        match self.read(&path) {
            Ok(values) => {
                let _ = self.store.put(&path, &state, &values);
                self.progress.read.fetch_add(1, Relaxed);
                Step::Read(path)
            }
            Err(keel_props::Error::Cancelled) => Step::Stopped,
            // Gone or unreadable now: the next change or pass looks again.
            Err(_) => {
                let _ = self.store.remove(&path);
                Step::Skipped
            }
        }
    }

    /// Every chosen property of `path` (hashes in one pass).
    fn read(&self, path: &Path) -> Result<Vec<(String, Prop)>, keel_props::Error> {
        let opts = keel_props::Options {
            cancel: self.cancel.clone(),
            max_hash_bytes: self.rules.max_bytes,
            allow_recall: false,
            ..keel_props::Options::default()
        };
        let mut fp = FileProps::new(path, &opts);
        let mut out = Vec::with_capacity(self.rules.names.len());
        if !self.rules.hashes.is_empty() {
            let algos: Vec<HashAlgo> = self.rules.hashes.iter().map(|(_, a)| *a).collect();
            let values = fp.hashes(&algos)?;
            for ((name, _), v) in self.rules.hashes.iter().zip(values) {
                out.push((name.clone(), v));
            }
        }
        for name in &self.rules.names {
            if out.iter().any(|(n, _)| n == name) {
                continue;
            }
            match fp.get(name) {
                Ok(v) => out.push((name.clone(), v)),
                // A name this platform cannot read is stored as unknown.
                Err(
                    keel_props::Error::UnknownFunction(_) | keel_props::Error::UnknownProperty(_),
                )
                | Err(keel_props::Error::Pattern(_)) => out.push((name.clone(), Prop::Unknown)),
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn apply(&mut self, change: Change) -> Step {
        match change {
            Change::Remove(p) => {
                let _ = self.store.remove(&p);
                Step::Updated
            }
            Change::Rename(old, new) => {
                let _ = self.store.rename(&old, &new);
                if !self.rules.admit_path(&new) {
                    let _ = self.store.remove(&new);
                    return Step::Updated;
                }
                match std::fs::metadata(&new) {
                    Ok(m) if m.is_file() => self.consider(new, &m),
                    _ => Step::Updated,
                }
            }
            Change::Read(p) => {
                if !self.rules.admit_path(&p) {
                    return Step::Skipped;
                }
                match std::fs::metadata(&p) {
                    Ok(m) if m.is_file() => self.consider(p, &m),
                    Ok(_) => Step::Skipped,
                    Err(_) => {
                        let _ = self.store.remove(&p);
                        Step::Updated
                    }
                }
            }
        }
    }

    /// Steps until stopped: sleeps while paused, waits for changes while idle.
    pub fn run(mut self) {
        lower_priority();
        loop {
            match self.step() {
                Step::Stopped => return,
                Step::Paused => self.env.sleep(PAUSE_POLL),
                Step::Idle => {
                    let left = self.next_pass.saturating_sub(self.env.now_ms());
                    let wait = match u64::try_from(left) {
                        Ok(ms) if ms > 0 => Duration::from_millis(ms).min(IDLE_WAIT),
                        _ => IDLE_WAIT,
                    };
                    self.pending.wait(wait);
                }
                _ => {}
            }
        }
    }
}

/// The job's thread runs at idle priority (Windows; elsewhere at its default).
fn lower_priority() {
    #[cfg(all(windows, not(test)))]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_IDLE,
        };
        // SAFETY: the pseudo handle of the calling thread.
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_IDLE) };
    }
}

// ---------------------------------------------------------------------------------
// The process's job and store

struct Running {
    rules_key: String,
    cancel: keel_props::Cancel,
    pending: Arc<Pending>,
    progress: Arc<Progress>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct Global {
    running: Option<Running>,
    path: Option<PathBuf>,
    store: Option<Arc<PropStore>>,
}

static GLOBAL: Mutex<Global> = const_mutex(Global {
    running: None,
    path: None,
    store: None,
});

fn activity() -> &'static Arc<Activity> {
    static ACTIVITY: std::sync::OnceLock<Arc<Activity>> = std::sync::OnceLock::new();
    ACTIVITY.get_or_init(Arc::default)
}

/// User input: the property job pauses for [`ACTIVITY_PAUSE`].
pub fn note_activity() {
    activity().note(journal::now_ms());
}

/// Where the store lives: `props.db` in the index folder.
pub fn default_path() -> Option<PathBuf> {
    crate::walkindex::index_dir().map(|d| d.join(FILE_NAME))
}

/// Keeps the process's store at `path` (tests, tools); `None`: the default place.
pub fn use_path(path: Option<PathBuf>) {
    let mut g = GLOBAL.lock();
    g.path = path;
    g.store = None;
}

fn in_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.file_name().map(|d| d == "deps"))
        .unwrap_or(false)
}

fn store_locked(g: &mut Global) -> Option<Arc<PropStore>> {
    if let Some(s) = &g.store {
        return Some(s.clone());
    }
    let path = match &g.path {
        Some(p) => p.clone(),
        None if in_test_binary() => return None,
        None => default_path()?,
    };
    let s = Arc::new(PropStore::open(&path).ok()?);
    g.store = Some(s.clone());
    Some(s)
}

/// The process's property store, opened on first use.
pub fn store() -> Option<Arc<PropStore>> {
    store_locked(&mut GLOBAL.lock())
}

/// The stored value of property `name` of `path` ([`Lookup::NotIndexed`] without a
/// store).
pub fn lookup(path: &Path, name: &str) -> Lookup {
    store().map_or(Lookup::NotIndexed, |s| s.get(path, name))
}

/// Settings → Search index → Properties: starts, restarts or stops the job.
/// `index_roots`: what the index holds (walked when no include-only folder is set).
pub fn configure(settings: &PropertySettings, index_roots: &[PathBuf], pause_on_battery: bool) {
    let rules = Rules::new(settings, index_roots);
    let rules_key = format!("{rules:?}{pause_on_battery}");
    let mut g = GLOBAL.lock();
    if g.running.as_ref().is_some_and(|r| r.rules_key == rules_key) {
        return;
    }
    if let Some(mut old) = g.running.take() {
        old.cancel.cancel();
        old.pending.wake();
        if let Some(t) = old.thread.take() {
            drop(g);
            let _ = t.join();
            g = GLOBAL.lock();
        }
    }
    let Some(store) = store_locked(&mut g) else {
        return;
    };
    if rules.names.is_empty() {
        let _ = store.keep_only(&[]);
        let _ = store.set_fast_sort(&[]);
        return;
    }
    let pending = Arc::new(Pending::default());
    let Ok(job) = Indexer::new(
        rules,
        store,
        Arc::new(SystemEnv),
        activity().clone(),
        pending.clone(),
    ) else {
        return;
    };
    let job = job.pause_on_battery(pause_on_battery);
    let (cancel, progress) = (job.cancel(), job.progress.clone());
    let thread = std::thread::Builder::new()
        .name("keel-props-index".into())
        .spawn(move || job.run())
        .ok();
    g.running = Some(Running {
        rules_key,
        cancel,
        pending,
        progress,
        thread,
    });
}

/// Stops the job (at exit), waiting for it.
pub fn shutdown() {
    let running = GLOBAL.lock().running.take();
    if let Some(mut r) = running {
        r.cancel.cancel();
        r.pending.wake();
        if let Some(t) = r.thread.take() {
            let _ = t.join();
        }
    }
}

/// Hands the index's changes to the job (nothing when it does not run).
pub fn note_changes(events: &[Event]) {
    let pending = GLOBAL.lock().running.as_ref().map(|r| r.pending.clone());
    if let Some(p) = pending {
        p.push(changes_of(events));
    }
}

/// What the job is doing, for Settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub running: bool,
    pub paused: bool,
    pub walking: bool,
    /// Files read since Keel started.
    pub read: u64,
    /// Files in the store.
    pub stored: u64,
    /// Changes waiting.
    pub pending: usize,
    /// When the last full pass ended (ms), if one did in this process.
    pub last_pass: Option<i64>,
}

/// The job's status now (reads the store's file count).
pub fn status() -> Status {
    let (progress, pending) = {
        let g = GLOBAL.lock();
        match &g.running {
            Some(r) => (Some(r.progress.clone()), r.pending.len()),
            None => (None, 0),
        }
    };
    let stored = store().map_or(0, |s| s.len());
    match progress {
        None => Status {
            stored,
            ..Status::default()
        },
        Some(p) => Status {
            running: true,
            paused: p.paused.load(Relaxed),
            walking: p.walking.load(Relaxed),
            read: p.read.load(Relaxed),
            stored,
            pending,
            last_pass: Some(p.last_pass.load(Relaxed)).filter(|t| *t != 0),
        },
    }
}

#[cfg(test)]
#[path = "propindex_tests.rs"]
mod tests;
