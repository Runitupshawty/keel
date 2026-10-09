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

/// `p` relative to `root` ("" when equal), None when `p` is not inside `root`.
pub(crate) fn relative(root: &VPath, p: &VPath) -> Option<String> {
    if p.scheme != root.scheme || p.authority != root.authority || p.split_archive().is_some() {
        return None;
    }
    let base = root.path.trim_end_matches('/');
    let path = p.path.trim_end_matches('/');
    // Local Windows paths are case-insensitive.
    let rest = if cfg!(windows) && root.scheme == "file" {
        let (lower, base) = (path.to_lowercase(), base.to_lowercase());
        let len = lower.strip_prefix(&base)?.len();
        path.get(path.len().checked_sub(len)?..)?
    } else {
        path.strip_prefix(base)?
    };
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
    /// Distinct content ids across sources (filled by hashing).
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
        });
        let lib = Library {
            id: LibraryId(id),
            name: name.to_owned(),
            root: dir,
            jobs: Jobs::new(shared.clone()),
            shared,
            _lock: lock,
        };
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

    /// Forgets a source. `delete_store` also deletes its `source.db` (fails while a watcher
    /// or job still holds the source on Windows; the source is forgotten either way).
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
        if delete_store {
            let dir = source.dir.clone();
            source.store.close_idle();
            drop(source);
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("delete store {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn jobs(&self) -> &Jobs {
        &self.jobs
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
            Ok(s.store.get()?.query_row(
                "SELECT count(*), coalesce(sum(kind = 0), 0),
                        coalesce(sum(CASE WHEN kind = 0 THEN size END), 0) FROM record",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                    ))
                },
            )?)
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
        // Distinct across stores: union every store's content ids in a scratch database.
        let unique = || -> Result<u64> {
            let conn = Connection::open_in_memory()?;
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
            Ok(conn.query_row("SELECT count(*) FROM c", [], |r| r.get::<_, i64>(0))? as u64)
        };
        match unique() {
            Ok(n) => stats.unique_content = n,
            Err(e) => tracing::warn!("unique content count: {e:#}"),
        }
        stats
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
            (2, 6, 4, 30, 3)
        );
    }
}
