//! Persistent media sidecars (spec 2.10 "Media"): content-addressed folders under
//! `<data dir>/sidecars/<key>/` holding `thumb-256.webp`, `thumb-1024.webp`, `strip.webp`
//! and `meta.json`, so a moved or copied file keeps them. A byte budget (default 10 GiB) is
//! kept by LRU eviction that never deletes a pinned key. `index.db` in the same folder tracks
//! each key's bytes and last use.

use crate::media::{self, MediaMeta, MediaType, TimedOutAt};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

/// Default sidecar budget: 10 GiB.
pub const DEFAULT_BUDGET: u64 = 10 << 30;
/// Writes between budget checks made by `ensure` itself.
const EVICT_EVERY: u64 = 64;
/// A video strip that timed out is tried again after this long (or when the file changes).
pub const STRIP_RETRY: Duration = Duration::from_secs(7 * 24 * 3600);

/// Whether a strip that timed out (`t`) is tried again now (unix seconds) for a file of
/// this `mtime` and `size`: a week later, once the file changed, or when the clock went back.
pub fn strip_retry_due(t: &TimedOutAt, now: i64, mtime: i64, size: u64) -> bool {
    let age = now - t.at;
    !(0..STRIP_RETRY.as_secs() as i64).contains(&age) || t.mtime != mtime || t.size != size
}

/// A strip that timed out recently: not tried again yet (`Sidecars::ensure`).
#[derive(Debug)]
pub struct StripWaits(pub TimedOutAt);
impl std::fmt::Display for StripWaits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the strip timed out at {} (unix): tried again a week later, when the file changes, or with Retry strip",
            self.0.at
        )
    }
}
impl std::error::Error for StripWaits {}

/// Identifies one file's content: its content id when known, else its path, mtime and size.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct SidecarKey {
    pub cas_id: Option<[u8; 32]>,
    pub path_hash: [u8; 32],
    pub mtime: i64,
    pub size: u64,
}

impl SidecarKey {
    /// The key of a local file: `path_hash` is BLAKE3 of the path as given (build it the same
    /// way everywhere: the source root joined with the record path).
    pub fn local(path: &Path, mtime: i64, size: u64, cas_id: Option<[u8; 32]>) -> SidecarKey {
        SidecarKey {
            cas_id,
            path_hash: *blake3::hash(path.to_string_lossy().as_bytes()).as_bytes(),
            mtime,
            size,
        }
    }

    /// The same file keyed without its content id.
    pub fn by_path(&self) -> SidecarKey {
        SidecarKey {
            cas_id: None,
            ..self.clone()
        }
    }

    /// The key's folder: hex of the content id, else `p<path hash>-<mtime>-<size>`.
    pub fn dir_name(&self) -> String {
        match &self.cas_id {
            Some(cas) => hex(cas),
            None => format!("p{}-{:x}-{:x}", hex(&self.path_hash), self.mtime, self.size),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SidecarKind {
    Thumb256,
    Thumb1024,
    /// 20 frames x 160 px for video.
    Strip,
    Meta,
}

impl SidecarKind {
    pub fn file_name(self) -> &'static str {
        match self {
            SidecarKind::Thumb256 => "thumb-256.webp",
            SidecarKind::Thumb1024 => "thumb-1024.webp",
            SidecarKind::Strip => "strip.webp",
            SidecarKind::Meta => "meta.json",
        }
    }
}

impl MediaMeta {
    /// Why `kind` cannot be made for this file, when recorded: the metadata's own error
    /// for `Meta`, else that image kind's.
    pub fn failure(&self, kind: SidecarKind) -> Option<&str> {
        match kind {
            SidecarKind::Meta => self.error.as_deref(),
            k => self.failed.get(k.file_name()).map(String::as_str),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SidecarStats {
    /// Key folders tracked.
    pub keys: u64,
    pub bytes: u64,
    pub budget: u64,
}

/// A key that eviction leaves alone while this lives (hold one for every sidecar on screen).
pub struct Pinned {
    pins: Arc<Mutex<HashMap<String, usize>>>,
    dir: String,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let mut pins = self.pins.lock();
        if let Some(n) = pins.get_mut(&self.dir) {
            *n -= 1;
            if *n == 0 {
                pins.remove(&self.dir);
            }
        }
    }
}

pub struct Sidecars {
    root: PathBuf,
    budget: AtomicU64,
    db: Mutex<Connection>,
    /// Pinned key folders and their pin counts; also held while a folder is deleted or moved.
    pins: Arc<Mutex<HashMap<String, usize>>>,
    writes: AtomicU64,
}

impl Sidecars {
    /// Opens (creating) the store at `root` with a byte budget.
    pub fn open(root: &Path, budget: u64) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        let conn = Connection::open(root.join("index.db"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch(
            "PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS entry(
                 dir TEXT PRIMARY KEY,
                 cas BLOB,
                 path_hash BLOB NOT NULL,
                 mtime INTEGER NOT NULL,
                 size INTEGER NOT NULL,
                 bytes INTEGER NOT NULL,
                 used INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS entry_used ON entry(used);",
        )?;
        Ok(Sidecars {
            root: root.to_owned(),
            budget: AtomicU64::new(budget),
            db: Mutex::new(conn),
            pins: Arc::default(),
            writes: AtomicU64::new(0),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_budget(&self, budget: u64) {
        self.budget.store(budget, Ordering::SeqCst);
    }

    fn path(&self, key: &SidecarKey, kind: SidecarKind) -> PathBuf {
        self.root.join(key.dir_name()).join(kind.file_name())
    }

    /// The sidecar when it exists (and marks the key used). Pin the key while showing it.
    pub fn get(&self, key: &SidecarKey, kind: SidecarKind) -> Option<PathBuf> {
        let path = self.path(key, kind);
        if !path.is_file() {
            return None;
        }
        if let Err(e) = self.touch(key) {
            tracing::debug!("sidecar index: {e:#}");
        }
        Some(path)
    }

    /// Keeps `key` from eviction until the guard drops.
    pub fn pin(&self, key: &SidecarKey) -> Pinned {
        let dir = key.dir_name();
        *self.pins.lock().entry(dir.clone()).or_default() += 1;
        Pinned {
            pins: self.pins.clone(),
            dir,
        }
    }

    /// The parsed `meta.json` of `key`, if any.
    pub fn meta(&self, key: &SidecarKey) -> Option<MediaMeta> {
        load_meta(&self.path(key, SidecarKind::Meta))
    }

    /// The sidecar, generated from `source` (the local file) when missing. Blocking (decodes
    /// the file, may run ffmpeg for up to 10 s): call it from a job or worker thread.
    /// A kind that cannot be made (a corrupt or oversized file, a video ffmpeg cannot read)
    /// gets its error recorded in `meta.json` once, per kind, and is not tried again for
    /// this key: `Meta` then still succeeds (with `error` set), an image kind fails at once
    /// (`MediaMeta::failure`). Other kinds are unaffected; IO errors are not recorded. A
    /// strip whose `ffmpeg` timed out is remembered (`MediaMeta::timed_out`) and fails at
    /// once with [`StripWaits`] until `strip_retry_due` (or `retry`).
    pub fn ensure(&self, key: &SidecarKey, kind: SidecarKind, source: &Path) -> Result<PathBuf> {
        if let Some(path) = self.get(key, kind) {
            return Ok(path);
        }
        let _pin = self.pin(key);
        let video = media::media_type(source) == Some(MediaType::Video);
        if kind != SidecarKind::Meta {
            if let Some(error) = self
                .meta(key)
                .and_then(|m| m.failure(kind).map(str::to_owned))
            {
                return Err(media::corrupt(error));
            }
        }
        if kind == SidecarKind::Strip {
            let waits = self
                .meta(key)
                .and_then(|m| m.timed_out.get(kind.file_name()).copied())
                // A content id names the bytes: copies of them at other mtimes share this
                // record, so only the size is compared (else each copy would undo the other).
                .filter(|t| {
                    let mtime = if key.cas_id.is_some() {
                        t.mtime
                    } else {
                        key.mtime
                    };
                    !strip_retry_due(t, crate::now(), mtime, key.size)
                });
            if let Some(t) = waits {
                return Err(StripWaits(t).into());
            }
        }
        let made = match kind {
            SidecarKind::Meta => {
                media::read_meta(source).and_then(|m| Ok(serde_json::to_vec_pretty(&m)?))
            }
            SidecarKind::Thumb256 => media::thumbnail(source, 256),
            SidecarKind::Thumb1024 => media::thumbnail(source, 1024),
            SidecarKind::Strip if video => self
                .ensure(key, SidecarKind::Meta, source)
                .map(|p| load_meta(&p).unwrap_or_default())
                .and_then(|m| match m.error {
                    Some(e) => Err(media::corrupt(e)),
                    None => media::strip(source, m.duration_ms),
                }),
            SidecarKind::Strip => anyhow::bail!("only videos have a strip"),
        };
        let bytes = match made {
            Ok(bytes) => bytes,
            Err(e) if media::is_corrupt(&e) => {
                self.record_error(key, kind, source, &e.to_string())?;
                if kind == SidecarKind::Meta {
                    return Ok(self.path(key, kind));
                }
                return Err(e);
            }
            Err(e) if kind == SidecarKind::Strip && e.is::<media::TimedOut>() => {
                self.record_timeout(key, crate::now())?;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        self.write(key, kind, &bytes)
    }

    /// Remembers that `key`'s strip timed out at `at` (its `meta.json` is there: the strip
    /// is made after it).
    pub(crate) fn record_timeout(&self, key: &SidecarKey, at: i64) -> Result<()> {
        let mut meta = self.meta(key).unwrap_or_default();
        let t = TimedOutAt {
            at,
            mtime: key.mtime,
            size: key.size,
        };
        meta.timed_out
            .insert(SidecarKind::Strip.file_name().to_owned(), t);
        self.write(key, SidecarKind::Meta, &serde_json::to_vec_pretty(&meta)?)?;
        Ok(())
    }

    /// Forgets that `kind` failed or timed out for `key`, so the next `ensure` tries it
    /// again (the viewer's Retry strip).
    pub fn retry(&self, key: &SidecarKey, kind: SidecarKind) -> Result<()> {
        let Some(mut meta) = self.meta(key) else {
            return Ok(());
        };
        let name = kind.file_name();
        if meta.failed.remove(name).is_some() | meta.timed_out.remove(name).is_some() {
            self.write(key, SidecarKind::Meta, &serde_json::to_vec_pretty(&meta)?)?;
        }
        Ok(())
    }

    /// Writes one sidecar atomically and accounts for it.
    fn write(&self, key: &SidecarKey, kind: SidecarKind, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.path(key, kind);
        let dir = path.parent().context("sidecar folder")?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{}.{}.tmp", kind.file_name(), crate::random_id()?));
        fs::write(&tmp, bytes)?;
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("write {}", path.display()));
        }
        self.record(key)?;
        if self.writes.fetch_add(1, Ordering::Relaxed) % EVICT_EVERY == EVICT_EVERY - 1 {
            self.evict_to_budget(&|_| false);
        }
        Ok(path)
    }

    /// Records why `kind` cannot be made, in `meta.json`. An image kind's failure goes on
    /// the file's real metadata (made first when missing), never on an empty stand-in.
    fn record_error(
        &self,
        key: &SidecarKey,
        kind: SidecarKind,
        source: &Path,
        error: &str,
    ) -> Result<()> {
        let mut meta = match kind {
            SidecarKind::Meta => MediaMeta::default(),
            _ => load_meta(&self.ensure(key, SidecarKind::Meta, source)?).unwrap_or_default(),
        };
        match kind {
            SidecarKind::Meta => meta.error = Some(error.to_owned()),
            k => {
                meta.failed
                    .insert(k.file_name().to_owned(), error.to_owned());
            }
        }
        self.write(key, SidecarKind::Meta, &serde_json::to_vec_pretty(&meta)?)?;
        Ok(())
    }

    /// Re-measures a key folder and marks it used.
    fn record(&self, key: &SidecarKey) -> Result<()> {
        let dir = key.dir_name();
        let bytes = folder_bytes(&self.root.join(&dir));
        self.db.lock().execute(
            "INSERT INTO entry(dir, cas, path_hash, mtime, size, bytes, used)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, (SELECT coalesce(max(used), 0) + 1 FROM entry))
             ON CONFLICT(dir) DO UPDATE SET bytes = excluded.bytes, used = excluded.used",
            params![
                dir,
                key.cas_id.as_ref().map(|c| &c[..]),
                &key.path_hash[..],
                key.mtime,
                key.size as i64,
                bytes as i64
            ],
        )?;
        Ok(())
    }

    fn touch(&self, key: &SidecarKey) -> Result<()> {
        let n = self.db.lock().execute(
            "UPDATE entry SET used = (SELECT max(used) + 1 FROM entry) WHERE dir = ?1",
            [key.dir_name()],
        )?;
        // A folder the index does not know (index lost): adopt it.
        if n == 0 {
            self.record(key)?;
        }
        Ok(())
    }

    /// Deletes least-recently-used key folders until the store is within budget, skipping
    /// pinned keys (`pin` guards and `pinned`) and folders that cannot be deleted (open
    /// elsewhere); those can keep it over budget.
    pub fn evict_to_budget(&self, pinned: &dyn Fn(&SidecarKey) -> bool) {
        if let Err(e) = self.evict(pinned) {
            tracing::warn!("sidecar eviction: {e:#}");
        }
    }

    fn evict(&self, pinned: &dyn Fn(&SidecarKey) -> bool) -> Result<()> {
        let budget = self.budget.load(Ordering::SeqCst);
        let (mut total, candidates) = {
            let db = self.db.lock();
            let total: i64 =
                db.query_row("SELECT coalesce(sum(bytes), 0) FROM entry", [], |r| {
                    r.get(0)
                })?;
            if total as u64 <= budget {
                return Ok(());
            }
            let mut stmt = db.prepare(
                "SELECT dir, cas, path_hash, mtime, size, bytes FROM entry ORDER BY used",
            )?;
            let rows = stmt.query_map([], |r| {
                let cas: Option<Vec<u8>> = r.get(1)?;
                let path_hash: Vec<u8> = r.get(2)?;
                Ok((
                    r.get::<_, String>(0)?,
                    SidecarKey {
                        cas_id: cas.and_then(|c| c.try_into().ok()),
                        path_hash: path_hash.try_into().unwrap_or([0; 32]),
                        mtime: r.get(3)?,
                        size: r.get::<_, i64>(4)? as u64,
                    },
                    r.get::<_, i64>(5)? as u64,
                ))
            })?;
            let all: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
            (total as u64, all)
        };
        for (dir, key, bytes) in candidates {
            if total <= budget {
                break;
            }
            if pinned(&key) {
                continue;
            }
            // Held across the delete: a key cannot be pinned halfway through it.
            let pins = self.pins.lock();
            if pins.contains_key(&dir) {
                continue;
            }
            let path = self.root.join(&dir);
            if fs::remove_dir_all(&path).is_err() && path.exists() {
                continue;
            }
            drop(pins);
            self.db
                .lock()
                .execute("DELETE FROM entry WHERE dir = ?1", [&dir])?;
            total = total.saturating_sub(bytes);
        }
        Ok(())
    }

    /// Moves `from`'s sidecars to `to` (a record's content id appeared or changed format);
    /// when `to` already has sidecars, `from`'s are dropped. False when there was nothing to
    /// move or `from` is pinned (try again later).
    pub fn relink(&self, from: &SidecarKey, to: &SidecarKey) -> Result<bool> {
        self.relink_dir(&from.dir_name(), to)
    }

    pub(crate) fn relink_dir(&self, from: &str, to: &SidecarKey) -> Result<bool> {
        let to_dir = to.dir_name();
        let src = self.root.join(from);
        if from == to_dir || !src.is_dir() {
            return Ok(false);
        }
        let pins = self.pins.lock();
        if pins.contains_key(from) {
            return Ok(false);
        }
        let dst = self.root.join(&to_dir);
        if dst.exists() {
            fs::remove_dir_all(&src)?;
        } else {
            fs::rename(&src, &dst)?;
        }
        drop(pins);
        self.db
            .lock()
            .execute("DELETE FROM entry WHERE dir = ?1", [from])?;
        self.record(to)?;
        Ok(true)
    }

    pub fn stats(&self) -> SidecarStats {
        let (keys, bytes) = self
            .db
            .lock()
            .query_row(
                "SELECT count(*), coalesce(sum(bytes), 0) FROM entry",
                [],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_default();
        SidecarStats {
            keys,
            bytes,
            budget: self.budget.load(Ordering::SeqCst),
        }
    }
}

fn load_meta(path: &Path) -> Option<MediaMeta> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn folder_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

impl crate::Library {
    /// The sidecar store under `<data dir>/sidecars`, shared by every library of the data dir
    /// (one instance per process, so pins and eviction see each other).
    pub fn sidecars(&self) -> Result<Arc<Sidecars>> {
        shared(&sidecar_root(self)?)
    }
}

/// `<data dir>/sidecars` (a library lives at `<data dir>/library/<name>`).
pub(crate) fn sidecar_root(lib: &crate::Library) -> Result<PathBuf> {
    Ok(lib
        .dir()
        .parent()
        .and_then(Path::parent)
        .context("library folder has no data dir")?
        .join("sidecars"))
}

/// One store per root per process, so pins and eviction see each other.
pub(crate) fn shared(root: &Path) -> Result<Arc<Sidecars>> {
    static OPEN: Mutex<Vec<Arc<Sidecars>>> = Mutex::new(Vec::new());
    let mut open = OPEN.lock();
    if let Some(s) = open.iter().find(|s| s.root == root) {
        return Ok(s.clone());
    }
    let s = Arc::new(Sidecars::open(root, DEFAULT_BUDGET)?);
    open.push(s.clone());
    Ok(s)
}

#[cfg(test)]
#[path = "sidecars_tests.rs"]
mod tests;
