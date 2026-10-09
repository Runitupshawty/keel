//! Libraries and their sources: `library.db` (sources, jobs, op log) and one portable
//! `source.db` per source under `<library>/sources/<source id>/`.

use crate::db::{Pool, Store};
use crate::jobs::{IndexJob, Job, JobId, Jobs};
use anyhow::{Context, Result};
use keel_vfs::{Router, VPath};
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LibraryId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub String);

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One record of one source (record ids are per source store).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordRef {
    pub source: SourceId,
    pub id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Folder,
    Drive,
    Share,
    Cloud,
    Device,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDef {
    pub label: String,
    /// `file://`, `sftp://`, `cloud://`; never inside an archive.
    pub root: VPath,
    pub kind: SourceKind,
    pub include_hidden: bool,
    /// gitignore-style patterns, matched against paths relative to `root`.
    pub ignore: Vec<String>,
    /// Remote and cloud sources: seconds between polls by `Indexer::watch` (None: every
    /// `POLL_INTERVAL`).
    #[serde(default)]
    pub poll_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceStatus {
    /// `indexed_at` (unix seconds) is None until the first full walk completes.
    Online {
        indexed_at: Option<i64>,
    },
    /// `total` is the previous walk's record count (0 on the first walk).
    Indexing {
        done: u64,
        total: u64,
    },
    Offline {
        last_seen: Option<i64>,
    },
    Error(String),
}

/// One indexed location. Its store keeps the last generation while the source is offline.
pub struct Source {
    pub id: SourceId,
    pub def: SourceDef,
    pub(crate) store: Pool,
    /// The last completed full walk (records of older generations were removed by it).
    pub generation: AtomicU64,
    pub status: RwLock<SourceStatus>,
    /// Generation a running full walk writes (0 = no walk running).
    pub(crate) pending_gen: AtomicU64,
    /// Serializes generation changes against watcher/executor writes.
    pub(crate) write: Mutex<()>,
    /// Set by `remove_source`: walks, hashing and watchers of this source stop.
    pub(crate) removed: AtomicBool,
    dir: PathBuf,
}

impl Source {
    fn open(dir: PathBuf, id: SourceId, def: SourceDef) -> Result<Source> {
        let store = Pool::open(&dir.join("source.db"), Store::Source)
            .with_context(|| format!("source store {}", def.label))?;
        let generation = store
            .meta("generation")?
            .and_then(|g| g.parse().ok())
            .unwrap_or(0);
        let indexed_at = store.meta("last_full_walk")?.and_then(|t| t.parse().ok());
        Ok(Source {
            id,
            def,
            store,
            generation: AtomicU64::new(generation),
            status: RwLock::new(SourceStatus::Online { indexed_at }),
            pending_gen: AtomicU64::new(0),
            removed: AtomicBool::new(false),
            write: Mutex::new(()),
            dir,
        })
    }

    /// The path of `p` relative to this source's root ("" for the root), or None when `p`
    /// is not inside it.
    pub fn relative(&self, p: &VPath) -> Option<String> {
        relative(&self.def.root, p)
    }

    /// Local Windows names compare case-insensitively.
    pub(crate) fn nocase(&self) -> bool {
        cfg!(windows) && self.def.root.scheme == "file"
    }

    /// The absolute path of a record path relative to the root.
    pub fn absolute(&self, rel: &str) -> VPath {
        if rel.is_empty() {
            self.def.root.clone()
        } else {
            self.def.root.join(rel)
        }
    }

    pub fn summary(&self) -> SourceSummary {
        SourceSummary {
            id: self.id.clone(),
            label: self.def.label.clone(),
            root: self.def.root.clone(),
            kind: self.def.kind,
            status: self.status.read().clone(),
            generation: self.generation.load(Ordering::SeqCst),
        }
    }

    /// Where `source.db` lives (moves with the data).
    pub fn store_dir(&self) -> &Path {
        &self.dir
    }
}

/// Distinct content ids across every store (unioned in a scratch database), saved in
/// `library.db` meta for `stats`.
pub(crate) fn count_unique(shared: &Shared) -> Result<u64> {
    let sources: Vec<Arc<Source>> = shared.sources.read().clone();
    let conn = Connection::open("")?;
    conn.execute_batch("CREATE TABLE c(id BLOB PRIMARY KEY) WITHOUT ROWID")?;
    for s in &sources {
        let path = s.dir.join("source.db");
        conn.execute("ATTACH DATABASE ?1 AS s", [path.to_string_lossy()])?;
        let copied = conn.execute(
            "INSERT OR IGNORE INTO c SELECT cas_id FROM s.record WHERE cas_id IS NOT NULL",
            [],
        );
        conn.execute("DETACH DATABASE s", [])?;
        copied?;
    }
    let n = conn.query_row("SELECT count(*) FROM c", [], |r| r.get::<_, i64>(0))? as u64;
    shared.db.set_meta("unique_content", &n.to_string())?;
    Ok(n)
}

/// How long `remove_source` waits for jobs and watchers to close a store it deletes.
const REMOVE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// `p` relative to `root` ("" when equal), None when `p` is not inside `root`.
pub(crate) fn relative(root: &VPath, p: &VPath) -> Option<String> {
    if p.scheme != root.scheme || p.authority != root.authority || p.split_archive().is_some() {
        return None;
    }
    let base = root.path.trim_end_matches('/');
    let path = p.path.trim_end_matches('/');
    // Local Windows paths are case-insensitive: compare whole components (lowercasing can
    // change a string's length, so no byte offsets carry over).
    if cfg!(windows) && root.scheme == "file" {
        let mut rest = Some(path);
        for comp in base.split('/') {
            let (head, tail) = match rest?.split_once('/') {
                Some((h, t)) => (h, Some(t)),
                None => (rest?, None),
            };
            if head.to_lowercase() != comp.to_lowercase() {
                return None;
            }
            rest = tail;
        }
        return Some(rest.unwrap_or("").to_owned());
    }
    let rest = path.strip_prefix(base)?;
    if rest.is_empty() {
        Some(String::new())
    } else {
        rest.strip_prefix('/').map(str::to_owned)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSummary {
    pub id: SourceId,
    pub label: String,
    pub root: VPath,
    pub kind: SourceKind,
    pub status: SourceStatus,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibrarySummary {
    pub id: LibraryId,
    pub name: String,
    pub path: PathBuf,
    pub sources: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryStats {
    pub sources: usize,
    pub offline_sources: usize,
    /// Files and folders across every source's last known generation.
    pub records: u64,
    pub files: u64,
    pub bytes: u64,
    /// Distinct content ids across sources, as of the last hashing run (or
    /// `count_unique_content`).
    pub unique_content: u64,
    pub running_jobs: usize,
}

/// State shared with jobs and watchers.
pub(crate) struct Shared {
    pub(crate) db: Pool,
    pub(crate) sources: RwLock<Vec<Arc<Source>>>,
    pub(crate) router: RwLock<Arc<Router>>,
    /// Set by the app while the user is busy: background hashing pauses.
    pub(crate) activity: AtomicBool,
    pub(crate) pause_on_battery: AtomicBool,
    /// `Jobs::subscribe` receivers.
    pub(crate) job_events: Mutex<Vec<crossbeam_channel::Sender<crate::JobEvent>>>,
}

impl Shared {
    /// Tells every subscriber; a full or closed subscriber misses it (or is dropped).
    pub(crate) fn emit(&self, ev: crate::JobEvent) {
        self.job_events.lock().retain(|tx| {
            !matches!(
                tx.try_send(ev.clone()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
    }
}

impl Shared {
    pub(crate) fn source_for(&self, p: &VPath) -> Option<(Arc<Source>, String)> {
        self.sources
            .read()
            .iter()
            .find_map(|s| s.relative(p).map(|rel| (s.clone(), rel)))
    }
}

pub struct Library {
    pub id: LibraryId,
    pub name: String,
    root: PathBuf,
    pub(crate) shared: Arc<Shared>,
    jobs: Jobs,
    /// Held while open: one process owns a library (and resumes its jobs). Dropped last.
    _lock: std::fs::File,
}

fn check_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains(['/', '\\', ':', '\0'])
            && name.trim() == name,
        "invalid library name {name:?}"
    );
    Ok(())
}

impl Library {
    /// Opens (creating if needed) the library `name` under `<root>/library/<name>/`; `root` is
    /// normally [`crate::data_dir`]. Jobs left by an earlier session wait for
    /// `jobs().resume_all()` (call it after `set_router` and registering app job kinds).
    pub fn open(root: &Path, name: &str) -> Result<Library> {
        check_name(name)?;
        let dir = root.join("library").join(name);
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::File::create(dir.join("library.lock"))?;
        if let Err(e) = lock.try_lock() {
            anyhow::bail!("library {name} is open in another process ({e})");
        }
        let db = Pool::open(&dir.join("library.db"), Store::Library)?;
        let id = match db.meta("id")? {
            Some(id) => id,
            None => {
                let id = crate::random_id()?;
                db.set_meta("id", &id)?;
                id
            }
        };
        db.set_meta("name", name)?;
        let rows: Vec<(String, String)> = {
            let conn = db.get()?;
            let mut stmt = conn.prepare("SELECT id, def FROM source ORDER BY rowid")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut sources = Vec::new();
        for (id, def) in rows {
            let def: SourceDef = serde_json::from_str(&def)?;
            sources.push(Arc::new(Source::open(
                dir.join("sources").join(&id),
                SourceId(id),
                def,
            )?));
        }
        let shared = Arc::new(Shared {
            db,
            sources: RwLock::new(sources),
            router: RwLock::new(Arc::new(Router::new())),
            activity: AtomicBool::new(false),
            pause_on_battery: AtomicBool::new(true),
            job_events: Mutex::new(Vec::new()),
        });
        let lib = Library {
            id: LibraryId(id),
            name: name.to_owned(),
            root: dir,
            jobs: Jobs::new(shared.clone()),
            shared,
            _lock: lock,
        };
        crate::jobs::prune(&lib.shared.db)?;
        // Resumed by `jobs().resume_all()` once the app has set the router.
        lib.jobs.register("index", IndexJob::restore);
        lib.jobs.register("op", crate::plan::ExecJob::restore);
        lib.jobs.register("hash", crate::HashJob::restore);
        Ok(lib)
    }

    /// Libraries under [`crate::data_dir`].
    pub fn list() -> Vec<LibrarySummary> {
        crate::data_dir().map_or_else(Vec::new, |root| Self::list_in(&root))
    }

    /// Libraries under `<root>/library/`, by name.
    pub fn list_in(root: &Path) -> Vec<LibrarySummary> {
        let Ok(dirs) = std::fs::read_dir(root.join("library")) else {
            return Vec::new();
        };
        let mut out: Vec<_> = dirs
            .flatten()
            .filter_map(|d| {
                let path = d.path();
                let conn = Connection::open_with_flags(
                    path.join("library.db"),
                    OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .ok()?;
                let meta = |key: &str| -> Option<String> {
                    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                        .ok()
                };
                let sources: i64 = conn
                    .query_row("SELECT count(*) FROM source", [], |r| r.get(0))
                    .ok()?;
                Some(LibrarySummary {
                    id: LibraryId(meta("id")?),
                    name: meta("name")?,
                    path,
                    sources: sources as usize,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The library's folder (`library.db`, `sources/`).
    pub fn dir(&self) -> &Path {
        &self.root
    }

    /// The router used to reach sources (local and archives by default); the app passes
    /// its own so remote and cloud sources resolve.
    pub fn router(&self) -> Arc<Router> {
        self.shared.router.read().clone()
    }

    pub fn set_router(&self, router: Arc<Router>) {
        *self.shared.router.write() = router;
    }

    /// Registers a source; it is indexed by `Indexer::full_walk` (or an index job).
    /// Sources never overlap: a root inside (or around) another source's root is refused.
    pub fn add_source(&self, def: SourceDef) -> Result<SourceId> {
        anyhow::ensure!(
            def.root.split_archive().is_none(),
            "a source cannot be inside an archive: {}",
            def.root.display()
        );
        let mut sources = self.shared.sources.write();
        for s in sources.iter() {
            anyhow::ensure!(
                s.relative(&def.root).is_none() && relative(&def.root, &s.def.root).is_none(),
                "{} overlaps source {}",
                def.root.display(),
                s.def.label
            );
        }
        let id = SourceId(crate::random_id()?);
        let source = Source::open(self.root.join("sources").join(&id.0), id.clone(), def)?;
        self.shared.db.get()?.execute(
            "INSERT INTO source(id, def, created) VALUES (?1, ?2, ?3)",
            rusqlite::params![id.0, serde_json::to_string(&source.def)?, crate::now()],
        )?;
        sources.push(Arc::new(source));
        Ok(id)
    }

    /// Forgets a source; its running walk, hashing and watcher stop. `delete_store` also
    /// deletes its `source.db`, waiting up to 10 s for them to let go of it (the source is
    /// forgotten either way).
    pub fn remove_source(&self, id: &SourceId, delete_store: bool) -> Result<()> {
        let source = {
            let mut sources = self.shared.sources.write();
            let i = sources
                .iter()
                .position(|s| &s.id == id)
                .with_context(|| format!("no source {id}"))?;
            self.shared
                .db
                .get()?
                .execute("DELETE FROM source WHERE id = ?1", [&id.0])?;
            sources.remove(i)
        };
        // Its walk, hashing and watcher stop at their next step and let go of the store.
        source.removed.store(true, Ordering::SeqCst);
        if delete_store {
            let dir = source.dir.clone();
            let deadline = std::time::Instant::now() + REMOVE_WAIT;
            loop {
                source.store.close_idle();
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => break,
                    Err(_) if dir.exists() && std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) if dir.exists() => {
                        return Err(e).with_context(|| format!("delete store {}", dir.display()))
                    }
                    Err(_) => break,
                }
            }
        }
        Ok(())
    }

    pub fn jobs(&self) -> &Jobs {
        &self.jobs
    }

    /// Closes the library: stops its jobs at their next checkpoint (they resume on the next
    /// open) and waits up to `timeout`. False when a job was still in an uninterruptible
    /// step (a slow listing or transfer); it finishes that step on its own. Dropping the
    /// library does the same with a 30 s timeout.
    pub fn close(self, timeout: std::time::Duration) -> bool {
        self.jobs.shutdown(timeout)
    }

    /// Indexes a source as a durable job (resumed after a restart).
    pub fn index(&self, id: &SourceId) -> Result<JobId> {
        anyhow::ensure!(self.source(id).is_some(), "no source {id}");
        self.jobs.spawn(Box::new(IndexJob { source: id.clone() }))
    }

    /// The newest `limit` operation log entries (redacted when written), newest first.
    pub fn op_log(&self, limit: usize) -> Result<Vec<crate::OpLogEntry>> {
        crate::oplog::entries(&self.shared, limit)
    }

    pub fn sources(&self) -> Vec<SourceSummary> {
        self.shared
            .sources
            .read()
            .iter()
            .map(|s| s.summary())
            .collect()
    }

    pub fn source(&self, id: &SourceId) -> Option<Arc<Source>> {
        self.shared
            .sources
            .read()
            .iter()
            .find(|s| &s.id == id)
            .cloned()
    }

    /// The source containing `p` and `p`'s path relative to its root.
    pub fn source_for(&self, p: &VPath) -> Option<(Arc<Source>, String)> {
        self.shared.source_for(p)
    }

    /// Counts over every source store; a store that cannot be read is skipped (logged).
    pub fn stats(&self) -> LibraryStats {
        let sources = self.shared.sources.read().clone();
        let mut stats = LibraryStats {
            sources: sources.len(),
            offline_sources: sources
                .iter()
                .filter(|s| matches!(*s.status.read(), SourceStatus::Offline { .. }))
                .count(),
            running_jobs: self.jobs.running(),
            ..LibraryStats::default()
        };
        let counts = |s: &Source| -> Result<(u64, u64, u64)> {
            Ok(s.store
                .get()?
                .query_row("SELECT records, files, bytes FROM counts", [], |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                    ))
                })?)
        };
        for s in &sources {
            match counts(s) {
                Ok((records, files, bytes)) => {
                    stats.records += records;
                    stats.files += files;
                    stats.bytes += bytes;
                }
                Err(e) => tracing::warn!("stats for source {}: {e:#}", s.def.label),
            }
        }
        stats.unique_content = self
            .shared
            .db
            .meta("unique_content")
            .ok()
            .flatten()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        stats
    }

    /// Counts the distinct content ids across every source (a scan of all stores) and keeps
    /// the number for `stats`. The hashing job does this when it ends.
    pub fn count_unique_content(&self) -> Result<u64> {
        count_unique(&self.shared)
    }

    /// The indexed children of a folder (`rel` relative to the source root, "" for the root),
    /// folders first, by name; from the store, so it works offline.
    pub fn list_children(&self, source: &SourceId, rel: &str) -> Result<Vec<crate::LibraryHit>> {
        let src = self
            .source(source)
            .with_context(|| format!("no source {source}"))?;
        let c = src.store.get()?;
        let Some((id, _)) = crate::index::resolve(&c, rel, src.nocase())? else {
            anyhow::bail!("{rel} is not in the index of {}", src.def.label);
        };
        let mut stmt = c.prepare(&format!(
            "SELECT {} FROM record r WHERE r.parent = ?1
             ORDER BY r.kind = 1 DESC, r.name COLLATE NOCASE",
            crate::search::HIT_COLUMNS
        ))?;
        let rows = stmt.query_map([id], |r| crate::search::hit_of(&src, r, 0.0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Checks in the background whether each source's root can be reached and marks it
    /// online or offline (sources being indexed are left alone). Returns at once.
    pub fn refresh_status(&self) -> std::thread::JoinHandle<()> {
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let router = shared.router.read().clone();
            let sources: Vec<Arc<Source>> = shared.sources.read().clone();
            for s in sources {
                let reachable = match s.def.root.to_local_path() {
                    Some(p) => p.is_dir(),
                    None => router
                        .provider_for(&s.def.root)
                        .is_some_and(|p| p.stat(&s.def.root).is_ok()),
                };
                let indexed_at = s.store.meta("last_full_walk").ok().flatten();
                let indexed_at = indexed_at.and_then(|t| t.parse().ok());
                let mut status = s.status.write();
                if matches!(*status, SourceStatus::Indexing { .. }) {
                    continue;
                }
                *status = if reachable {
                    SourceStatus::Online { indexed_at }
                } else {
                    SourceStatus::Offline {
                        last_seen: indexed_at,
                    }
                };
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn folder(label: &str, root: &Path) -> SourceDef {
        SourceDef {
            label: label.into(),
            root: VPath::local(root),
            kind: SourceKind::Folder,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
        }
    }

    #[test]
    fn open_add_reopen_list_remove() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let (a, b) = (files.path().join("a"), files.path().join("b"));
        let lib = Library::open(data.path(), "james").unwrap();
        let id = lib.add_source(folder("A", &a)).unwrap();
        lib.add_source(folder("B", &b)).unwrap();
        let lib_id = lib.id.clone();
        drop(lib);

        let lib = Library::open(data.path(), "james").unwrap();
        assert_eq!(lib.id, lib_id, "id is stable across opens");
        let labels: Vec<_> = lib.sources().into_iter().map(|s| s.label).collect();
        assert_eq!(labels, ["A", "B"]);
        assert_eq!(
            lib.sources()[0].status,
            SourceStatus::Online { indexed_at: None }
        );
        let listed = Library::list_in(data.path());
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].name.as_str(), listed[0].sources), ("james", 2));

        let store = lib.source(&id).unwrap().store_dir().to_owned();
        assert!(store.join("source.db").is_file());
        lib.remove_source(&id, true).unwrap();
        assert!(!store.exists());
        assert_eq!(lib.sources().len(), 1);
        assert!(lib.remove_source(&id, false).is_err());
        drop(lib);
        assert_eq!(
            Library::open(data.path(), "james").unwrap().sources().len(),
            1
        );
    }

    #[test]
    fn source_definitions_without_a_poll_interval_still_load() {
        let old = r#"{"label":"x","root":{"scheme":"file","authority":"","path":"/x"},
                      "kind":"Folder","include_hidden":false,"ignore":[]}"#;
        let def: SourceDef = serde_json::from_str(old).unwrap();
        assert_eq!(def.poll_secs, None);
    }

    #[test]
    fn a_library_is_open_in_one_place_at_a_time() {
        let data = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "solo").unwrap();
        let err = Library::open(data.path(), "solo").err().unwrap();
        assert!(err.to_string().contains("open in another process"), "{err}");
        assert!(Library::open(data.path(), "other").is_ok());
        drop(lib);
        Library::open(data.path(), "solo").unwrap();
    }

    #[test]
    fn overlapping_archive_and_bad_names_are_refused() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "x").unwrap();
        lib.add_source(folder("root", &files.path().join("a")))
            .unwrap();
        assert!(lib
            .add_source(folder("inner", &files.path().join("a").join("b")))
            .is_err());
        assert!(lib.add_source(folder("outer", files.path())).is_err());
        lib.add_source(folder("sibling", &files.path().join("ab")))
            .unwrap();
        let mut zip = folder("zip", files.path());
        zip.root = VPath::join_archive(&VPath::local(files.path().join("z.zip")), "");
        assert!(lib.add_source(zip).is_err());
        for bad in ["", "..", "a/b", r"a\b", " a"] {
            assert!(Library::open(data.path(), bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn relative_paths() {
        let root = VPath::parse("sftp://box/home/me").unwrap();
        let rel = |p: &str| relative(&root, &VPath::parse(p).unwrap());
        assert_eq!(rel("sftp://box/home/me").as_deref(), Some(""));
        assert_eq!(rel("sftp://box/home/me/a/b").as_deref(), Some("a/b"));
        assert_eq!(rel("sftp://box/home/meow"), None);
        assert_eq!(rel("sftp://other/home/me/a"), None);
        #[cfg(windows)]
        assert_eq!(
            relative(&VPath::local(r"D:\Data"), &VPath::local(r"d:\data\X y")).as_deref(),
            Some("X y")
        );
        #[cfg(windows)]
        assert_eq!(
            relative(&VPath::local(r"D:\"), &VPath::local(r"D:\x")).as_deref(),
            Some("x")
        );
        // Lowercasing İ changes its length.
        #[cfg(windows)]
        assert_eq!(
            relative(&VPath::local(r"D:\Dir"), &VPath::local(r"d:\dir\İx\y")).as_deref(),
            Some("İx/y")
        );
        #[cfg(windows)]
        assert_eq!(
            relative(&VPath::local(r"D:\Dir"), &VPath::local(r"D:\Dirt\a")),
            None
        );
    }

    #[test]
    fn stats_count_records_and_unique_content() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "s").unwrap();
        for (i, name) in ["a", "b"].iter().enumerate() {
            let id = lib
                .add_source(folder(name, &files.path().join(name)))
                .unwrap();
            let src = lib.source(&id).unwrap();
            let c = src.store.get().unwrap();
            c.execute_batch(&format!(
                "INSERT INTO record(name, path, kind, size, fs_id, gen, cas_id) VALUES
                    ('{name}', '', 1, 0, 'r', 1, NULL),
                    ('x', 'x', 0, 10, 'x', 1, x'01'),
                    ('y', 'y', 0, 5, 'y', 1, x'0{}');",
                i + 2
            ))
            .unwrap();
        }
        let stats = lib.stats();
        assert_eq!(
            (
                stats.sources,
                stats.records,
                stats.files,
                stats.bytes,
                stats.unique_content
            ),
            (2, 6, 4, 30, 0)
        );
        assert_eq!(lib.count_unique_content().unwrap(), 3);
        assert_eq!(lib.stats().unique_content, 3);
        // Counters follow changes.
        let src = lib.source(&lib.sources()[0].id).unwrap();
        src.store
            .get()
            .unwrap()
            .execute_batch("UPDATE record SET size = 100 WHERE name = 'x'; DELETE FROM record WHERE name = 'y';")
            .unwrap();
        let stats = lib.stats();
        assert_eq!((stats.records, stats.files, stats.bytes), (5, 3, 115));
    }

    #[test]
    fn children_and_status_come_from_the_store() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("drive");
        std::fs::create_dir_all(root.join("b dir")).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("b dir/c.txt"), "c").unwrap();
        let lib = Library::open(data.path(), "c").unwrap();
        let id = lib.add_source(folder("d", &root)).unwrap();
        crate::index::tests::walk(&lib.source(&id).unwrap(), &lib.router()).unwrap();
        let names = |rel: &str| -> Vec<String> {
            lib.list_children(&id, rel)
                .unwrap()
                .into_iter()
                .map(|h| h.name)
                .collect()
        };
        assert_eq!(names(""), ["b dir", "a.txt"]);
        assert_eq!(names("b dir"), ["c.txt"]);
        assert!(lib.list_children(&id, "nope").is_err());
        // Unplugged: listed from the store, and the probe marks it offline.
        std::fs::rename(&root, files.path().join("away")).unwrap();
        lib.refresh_status().join().unwrap();
        assert!(matches!(
            lib.sources()[0].status,
            SourceStatus::Offline { last_seen: Some(_) }
        ));
        assert_eq!(names(""), ["b dir", "a.txt"]);
        std::fs::rename(files.path().join("away"), &root).unwrap();
        lib.refresh_status().join().unwrap();
        assert!(matches!(
            lib.sources()[0].status,
            SourceStatus::Online { .. }
        ));
    }
}
