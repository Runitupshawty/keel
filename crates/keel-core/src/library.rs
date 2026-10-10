//! Libraries and their sources: `library.db` (sources, jobs, op log) and one portable
//! `source.db` per source under `<library>/sources/<source id>/`.

use crate::db::{Pool, Store};
use crate::index::{WatchConfig, WatchHandle};
use crate::jobs::{IndexJob, Job, JobId, JobState, Jobs};
use crate::Indexer;
use anyhow::{Context, Result};
use keel_vfs::{Router, VPath};
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Weak,
    },
    time::Duration,
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

/// One record of one source (record ids are per source store and never reused).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordRef {
    pub source: SourceId,
    pub id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Folder,
    Drive,
    /// A network share (UNC path or mapped drive): indexed, but only hashed when
    /// `SourceDef::hash_shares` is set (hashing reads every file over the network).
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
    /// Hash this source although it is a network share (a UNC path or `SourceKind::Share`).
    #[serde(default)]
    pub hash_shares: bool,
}

/// Why a source reads as offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OfflineReason {
    /// The root (or its host, or its provider) cannot be reached.
    Unreachable,
    /// Something else is at the root (another volume on the drive letter, an unmounted
    /// mount point): kept offline, and its changes kept out of the snapshot, until the
    /// indexed folder is back or `Indexer::adopt_root` accepts the new one.
    RootMismatch,
    /// The root is empty although the snapshot has entries (more likely unmounted than
    /// emptied); held like `RootMismatch`.
    Empty,
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
        reason: OfflineReason,
    },
    Error(String),
}

/// `store` meta key: a held `OfflineReason` (root mismatch or empty root).
const HELD: &str = "offline_reason";

impl OfflineReason {
    fn key(self) -> &'static str {
        match self {
            OfflineReason::Unreachable => "unreachable",
            OfflineReason::RootMismatch => "root_mismatch",
            OfflineReason::Empty => "empty",
        }
    }
    fn parse(s: &str) -> Option<OfflineReason> {
        Some(match s {
            "root_mismatch" => OfflineReason::RootMismatch,
            "empty" => OfflineReason::Empty,
            _ => return None,
        })
    }
}

/// One indexed location. Its store keeps the last generation while the source is offline.
pub struct Source {
    pub id: SourceId,
    pub def: SourceDef,
    pub(crate) store: Pool,
    /// The library holding it (set when it is opened or added).
    pub(crate) owner: RwLock<Weak<Shared>>,
    /// The last completed full walk (records of older generations were removed by it).
    pub generation: AtomicU64,
    pub status: RwLock<SourceStatus>,
    /// Generation a running full walk writes (0 = no walk running).
    pub(crate) pending_gen: AtomicU64,
    /// Serializes generation changes against watcher/executor writes.
    pub(crate) write: Mutex<()>,
    /// Set by `remove_source`: walks, hashing and watchers of this source stop.
    pub(crate) removed: AtomicBool,
    /// A root mismatch or empty root, persisted in the store: changes are not applied and
    /// the status stays offline until a full walk succeeds again (or adopts the new root).
    held: RwLock<Option<OfflineReason>>,
    /// The volume the root was last seen on (`library.db` `source.volume_id`).
    pub(crate) volume_id: RwLock<Option<String>>,
    /// The library's sources (this one included), for checks across sources.
    peers: Weak<RwLock<Vec<Arc<Source>>>>,
    dir: PathBuf,
}

impl Source {
    fn open(
        dir: PathBuf,
        id: SourceId,
        def: SourceDef,
        volume_id: Option<String>,
        peers: Weak<RwLock<Vec<Arc<Source>>>>,
    ) -> Result<Source> {
        let store = Pool::open(&dir.join("source.db"), Store::Source)
            .with_context(|| format!("source store {}", def.label))?;
        let generation = store
            .meta("generation")?
            .and_then(|g| g.parse().ok())
            .unwrap_or(0);
        let indexed_at = store.meta("last_full_walk")?.and_then(|t| t.parse().ok());
        let held = store.meta(HELD)?.as_deref().and_then(OfflineReason::parse);
        // Built after a first walk; a walk that never ended left them out.
        store.get()?.execute_batch(crate::index::FILTER_INDEXES)?;
        let status = match held {
            Some(reason) => SourceStatus::Offline {
                last_seen: indexed_at,
                reason,
            },
            None => SourceStatus::Online { indexed_at },
        };
        Ok(Source {
            id,
            def,
            store,
            owner: RwLock::new(Weak::new()),
            generation: AtomicU64::new(generation),
            status: RwLock::new(status),
            pending_gen: AtomicU64::new(0),
            removed: AtomicBool::new(false),
            write: Mutex::new(()),
            held: RwLock::new(held),
            volume_id: RwLock::new(volume_id),
            peers,
            dir,
        })
    }

    /// The path of `p` relative to this source's root ("" for the root), or None when `p`
    /// is not inside it.
    pub fn relative(&self, p: &VPath) -> Option<String> {
        relative(&self.def.root, p)
    }

    /// Local Windows names compare case-insensitively.
    /// A paired device's folder: its content ids are that device's claims, never copies.
    pub(crate) fn is_device(&self) -> bool {
        self.def.kind == SourceKind::Device || self.def.root.scheme == "node"
    }

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

    /// Another source of the library holding a folder record with native id `fs_id` (the
    /// same folder reached through a junction, a symlink, a subst drive or a bind mount):
    /// its label.
    pub(crate) fn folder_elsewhere(&self, fs_id: &str) -> Option<String> {
        if fs_id.starts_with("h:") {
            return None;
        }
        let peers = self.peers.upgrade()?.read().clone();
        peers.iter().filter(|s| s.id != self.id).find_map(|s| {
            let c = s.store.get().ok()?;
            let hit = c
                .prepare_cached("SELECT 1 FROM record WHERE fs_id = ?1 AND kind = 1 LIMIT 1")
                .and_then(|mut q| q.exists([fs_id]))
                .unwrap_or(false);
            hit.then(|| s.def.label.clone())
        })
    }

    /// Where `source.db` lives (moves with the data).
    pub fn store_dir(&self) -> &Path {
        &self.dir
    }

    /// The held root mismatch or empty root, if any.
    pub(crate) fn held(&self) -> Option<OfflineReason> {
        *self.held.read()
    }

    /// Forgets the hold in memory (a walk released it in the store).
    pub(crate) fn release_held(&self) {
        *self.held.write() = None;
    }

    /// Holds (`Some`) or releases (`None`) the source, persistently.
    pub(crate) fn hold(&self, reason: Option<OfflineReason>) -> Result<()> {
        match reason {
            Some(r) => self.store.set_meta(HELD, r.key())?,
            None => {
                self.store
                    .get()?
                    .execute("DELETE FROM meta WHERE key = ?1", [HELD])?;
            }
        }
        *self.held.write() = reason;
        Ok(())
    }

    /// Network shares are hashed only when asked for.
    pub(crate) fn hashable_share(&self) -> bool {
        let unc = self
            .def
            .root
            .to_local_path()
            .is_some_and(|p| p.to_string_lossy().starts_with(r"\\"));
        self.def.hash_shares || !(self.def.kind == SourceKind::Share || unc)
    }

    /// For a held local source: whether the indexed folder is back at the root (the same
    /// root identity, and entries again after an empty root).
    fn root_is_back(&self) -> bool {
        let Some(root) = self.def.root.to_local_path() else {
            return false;
        };
        let same = match self.store.meta("root_id").ok().flatten() {
            Some(was) => crate::fsid::root_ids(&root).contains(&was),
            None => true,
        };
        let filled = std::fs::read_dir(&root).is_ok_and(|mut d| d.next().is_some());
        same && (self.held() != Some(OfflineReason::Empty) || filled)
    }

    /// For a local source: whether a different folder than the indexed one is at the root.
    fn root_swapped(&self) -> bool {
        let Some(root) = self.def.root.to_local_path() else {
            return false;
        };
        if self.store.meta("adopt_root").ok().flatten().is_some() {
            return false;
        }
        match self.store.meta("root_id").ok().flatten() {
            Some(was) => {
                let now = crate::fsid::root_ids(&root);
                !now.is_empty() && !now.contains(&was)
            }
            None => false,
        }
    }
}

/// Distinct content across every store (unioned in a scratch database), saved in
/// `library.db` meta for `stats`: confirmed content ids, plus each unconfirmed sampled hash
/// (unique when it was hashed, so content no other file has).
pub(crate) fn count_unique(shared: &Shared) -> Result<u64> {
    let sources: Vec<Arc<Source>> = shared.sources.read().clone();
    let conn = Connection::open("")?;
    conn.execute_batch("CREATE TABLE c(id BLOB PRIMARY KEY) WITHOUT ROWID")?;
    for s in &sources {
        let path = s.dir.join("source.db");
        conn.execute("ATTACH DATABASE ?1 AS s", [path.to_string_lossy()])?;
        let copied = conn.execute(
            "INSERT OR IGNORE INTO c SELECT coalesce(cas_id, sampled_hash) FROM s.record
             WHERE kind = 0 AND coalesce(cas_id, sampled_hash) IS NOT NULL",
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
const REMOVE_WAIT: Duration = Duration::from_secs(10);
/// How long hashing and integrity checks stay paused after `Library::note_activity`...
pub(crate) const ACTIVITY_PAUSE: Duration = Duration::from_secs(5);
/// ...and sidecar jobs: a window asked for those, and it says the user works at most every
/// 4 s while the user does, so they run between its notes.
pub(crate) const SIDECAR_PAUSE: Duration = Duration::from_secs(1);

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

/// A source store's footprint (`Library::store_usage`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreUsage {
    pub bytes: u64,
    /// Tags applied to its records (one per record and tag), Favorites not counted.
    pub tags: u64,
    pub favorites: u64,
}

/// Bytes of the files under `dir` (unreadable entries count 0).
fn walkdir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => walkdir_size(&e.path()),
            Ok(_) => e.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryStats {
    pub sources: usize,
    pub offline_sources: usize,
    /// Files and folders across every source's last known generation.
    pub records: u64,
    pub files: u64,
    pub bytes: u64,
    /// Distinct content across sources, as of the last hashing run (or
    /// `count_unique_content`).
    pub unique_content: u64,
    pub running_jobs: usize,
    /// Each source's own counts, in the library's order.
    pub per_source: Vec<SourceStats>,
}

/// One source's counts (`LibraryStats::per_source`), from its store as last indexed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStats {
    pub id: SourceId,
    pub label: String,
    pub files: u64,
    /// Folders below the root.
    pub folders: u64,
    pub bytes: u64,
    /// Files with a content hash (a sampled hash at least).
    pub hashed_files: u64,
    /// The last completed full walk (unix seconds).
    pub last_walk: Option<i64>,
    pub offline: bool,
}

/// Unix milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// State shared with jobs and watchers. Holds the library's lock file: the library stays
/// locked until the last job thread that outlived `close` has ended.
pub(crate) struct Shared {
    pub(crate) db: Pool,
    pub(crate) sources: Arc<RwLock<Vec<Arc<Source>>>>,
    pub(crate) router: RwLock<Arc<Router>>,
    /// Unix ms until which background hashing pauses (`Library::note_activity`).
    pub(crate) busy_until: AtomicU64,
    /// Whether hashing pauses on activity too (`Library::set_hash_idle_only`); sidecar and
    /// integrity jobs always do.
    pub(crate) hash_on_activity: AtomicBool,
    pub(crate) remote_hash_settings: RwLock<crate::hash::RemoteHashSettings>,
    pub(crate) remote_hash_io: Mutex<()>,
    pub(crate) pause_on_battery: AtomicBool,
    /// Whether a completed walk schedules hashing.
    pub(crate) hash_after_walk: AtomicBool,
    /// The hash job `Library::hash` started or found (one at a time).
    pub(crate) hash_job: Mutex<Option<JobId>>,
    /// Set when hashing was asked for while a hash job ran: it goes round once more.
    pub(crate) hash_again: AtomicBool,
    /// The hash job past its last look at `hash_again` (ending): never handed new work.
    pub(crate) hash_finishing: AtomicI64,
    /// Protection recounts read and write as one (the last to start writes last).
    pub(crate) recounting: Mutex<()>,
    /// Changes waiting for a debounced recount (`protect::schedule_recount`).
    pub(crate) recount_pending: Mutex<crate::protect::PendingRecount>,
    /// Completed recounts (`Library::protection_revision`).
    pub(crate) protection_revision: AtomicU64,
    /// Seconds east of UTC for `dm:` dates (`Library::set_utc_offset`).
    pub(crate) utc_offset: AtomicI64,
    /// `Jobs::subscribe` receivers.
    pub(crate) job_events: Mutex<Vec<crossbeam_channel::Sender<crate::JobEvent>>>,
    pub(crate) jobs: JobState,
    /// Sources kept current by `Library::watch` (None: not armed, the root was away).
    watchers: Mutex<HashMap<SourceId, Option<WatchHandle>>>,
    _lock: std::fs::File,
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

    pub(crate) fn source_for(&self, p: &VPath) -> Option<(Arc<Source>, String)> {
        self.sources
            .read()
            .iter()
            .find_map(|s| s.relative(p).map(|rel| (s.clone(), rel)))
    }

    pub(crate) fn closing(&self) -> bool {
        self.jobs.closing.load(Ordering::SeqCst)
    }

    /// The app reported activity within the last `ACTIVITY_PAUSE`.
    pub(crate) fn busy(&self) -> bool {
        self.busy_within(ACTIVITY_PAUSE)
    }

    /// The app reported activity within the last `pause` (at most `ACTIVITY_PAUSE`).
    pub(crate) fn busy_within(&self, pause: Duration) -> bool {
        let early = ACTIVITY_PAUSE.saturating_sub(pause).as_millis() as u64;
        now_ms().saturating_add(early) < self.busy_until.load(Ordering::SeqCst)
    }

    /// (Re)starts the watcher of a watched source; a root that cannot be watched now leaves
    /// it unarmed (`refresh_status` arms it when the root is back).
    fn arm(self: &Arc<Self>, src: &Arc<Source>) {
        let old = match self.watchers.lock().get_mut(&src.id) {
            Some(slot) => slot.take(),
            None => return,
        };
        drop(old); // joins its thread, outside the lock
        if self.closing() || src.removed.load(Ordering::SeqCst) {
            return;
        }
        let mut cfg = WatchConfig::default();
        if let Some(secs) = src.def.poll_secs {
            cfg.poll = Duration::from_secs(secs);
        }
        let weak = Arc::downgrade(self);
        let after_walk = move |s: &Source| {
            if let Some(lib) = Weak::upgrade(&weak) {
                crate::protect::after_walk(&lib, s);
                crate::hash::after_walk(&lib, s);
            }
        };
        let router = self.router.read().clone();
        let handle = match Indexer::watch_hooked(src, &router, cfg, Box::new(after_walk)) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::debug!("watch {}: {e:#}", src.def.label);
                None
            }
        };
        let mut watchers = self.watchers.lock();
        match watchers.get_mut(&src.id) {
            Some(slot) if !self.closing() => *slot = handle,
            // Unwatched or closed meanwhile: dropped (joined) below, outside the lock.
            _ => {
                drop(watchers);
                drop(handle);
            }
        }
    }

    /// `Library::refresh_status`, on the calling thread.
    fn refresh(self: &Arc<Self>) {
        let router = self.router.read().clone();
        let sources: Vec<Arc<Source>> = self.sources.read().clone();
        for s in sources {
            if self.closing() {
                return;
            }
            let reachable = match s.def.root.to_local_path() {
                Some(p) => p.is_dir(),
                None => router
                    .provider_for(&s.def.root)
                    .is_some_and(|p| p.stat(&s.def.root).is_ok()),
            };
            // A held root stays offline until the indexed folder is back (local roots are
            // checked here; remote ones by their next walk).
            if reachable && s.held().is_some() && s.root_is_back() {
                let _ = s.hold(None);
            }
            if reachable && s.held().is_none() && s.root_swapped() {
                let _ = s.hold(Some(OfflineReason::RootMismatch));
            }
            let reason = match (reachable, s.held()) {
                (false, _) => Some(OfflineReason::Unreachable),
                (true, held) => held,
            };
            // The volume it is on (its capacity and last-seen time too).
            if reachable && s.held().is_none() {
                if let Err(e) = crate::protect::observe(self, &s, true) {
                    tracing::debug!("volume of {}: {e:#}", s.def.label);
                }
            }
            let indexed_at = s.store.meta("last_full_walk").ok().flatten();
            let indexed_at = indexed_at.and_then(|t| t.parse().ok());
            let was_offline = {
                let mut status = s.status.write();
                if matches!(*status, SourceStatus::Indexing { .. }) {
                    continue;
                }
                let was = matches!(*status, SourceStatus::Offline { .. });
                *status = match reason {
                    None => SourceStatus::Online { indexed_at },
                    Some(reason) => SourceStatus::Offline {
                        last_seen: indexed_at,
                        reason,
                    },
                };
                was
            };
            // Back online: a local watcher died with its volume; a fresh one catches up.
            let unarmed = matches!(self.watchers.lock().get(&s.id), Some(None));
            let local = s.def.root.to_local_path().is_some();
            if reason.is_none() && (unarmed || (was_offline && local)) {
                self.arm(&s);
            }
        }
    }
}

pub struct Library {
    pub id: LibraryId,
    pub name: String,
    root: PathBuf,
    pub(crate) shared: Arc<Shared>,
    jobs: Jobs,
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
    /// Fails while the library is open, in this process (including jobs of a closed library
    /// still finishing a step) or another.
    pub fn open(root: &Path, name: &str) -> Result<Library> {
        check_name(name)?;
        let dir = root.join("library").join(name);
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::File::create(dir.join("library.lock"))?;
        if let Err(e) = lock.try_lock() {
            anyhow::bail!("library {name} is already open ({e})");
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
        let rows: Vec<(String, String, Option<String>)> = {
            let conn = db.get()?;
            let mut stmt = conn.prepare("SELECT id, def, volume_id FROM source ORDER BY rowid")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let sources = Arc::new(RwLock::new(Vec::new()));
        for (id, def, volume) in rows {
            let def: SourceDef = serde_json::from_str(&def)?;
            let source = Source::open(
                dir.join("sources").join(&id),
                SourceId(id),
                def,
                volume,
                Arc::downgrade(&sources),
            )?;
            sources.write().push(Arc::new(source));
        }
        let remote_hash_settings = db
            .meta("remote_hash_settings")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let shared = Arc::new(Shared {
            db,
            sources,
            router: RwLock::new(Arc::new(Router::new())),
            busy_until: AtomicU64::new(0),
            hash_on_activity: AtomicBool::new(true),
            remote_hash_settings: RwLock::new(remote_hash_settings),
            remote_hash_io: Mutex::new(()),
            pause_on_battery: AtomicBool::new(true),
            hash_after_walk: AtomicBool::new(true),
            hash_job: Mutex::new(None),
            hash_again: AtomicBool::new(false),
            hash_finishing: AtomicI64::new(0),
            recounting: Mutex::new(()),
            recount_pending: Mutex::default(),
            protection_revision: AtomicU64::new(0),
            utc_offset: AtomicI64::new(0),
            job_events: Mutex::new(Vec::new()),
            jobs: JobState::default(),
            watchers: Mutex::new(HashMap::new()),
            _lock: lock,
        });
        for source in shared.sources.read().iter() {
            *source.owner.write() = Arc::downgrade(&shared);
        }
        let lib = Library {
            id: LibraryId(id),
            name: name.to_owned(),
            root: dir,
            jobs: Jobs::new(shared.clone()),
            shared,
        };
        crate::jobs::prune(&lib.shared.db)?;
        // Resumed by `jobs().resume_all()` once the app has set the router.
        lib.jobs.register("index", IndexJob::restore);
        lib.jobs.register("op", crate::plan::ExecJob::restore);
        lib.jobs.register("hash", crate::HashJob::restore);
        lib.jobs.register(
            crate::SidecarJob::KIND,
            <crate::SidecarJob as crate::Job>::restore,
        );
        lib.jobs.register(
            crate::IntegrityJob::KIND,
            <crate::IntegrityJob as crate::Job>::restore,
        );
        // Stores that came back with tags this library does not know.
        lib.reconcile_tags()?;
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
    /// Sources never overlap: a root inside (or around) another source's root is refused,
    /// also when one of them is reached through a junction, symlink or subst drive (their
    /// resolved paths are compared too; the walk refuses the aliases this cannot see).
    pub fn add_source(&self, def: SourceDef) -> Result<SourceId> {
        anyhow::ensure!(
            (def.kind == SourceKind::Device) == (def.root.scheme == "node"),
            "a device source is a node:// folder, and a node:// folder is a device source"
        );
        anyhow::ensure!(
            def.root.split_archive().is_none(),
            "a source cannot be inside an archive: {}",
            def.root.display()
        );
        // Resolved outside the lock (an unreachable share can take a while to answer).
        let resolve = |root: &VPath| {
            root.to_local_path()
                .and_then(|p| std::fs::canonicalize(p).ok())
        };
        let real = resolve(&def.root);
        let others: Vec<(SourceId, Option<PathBuf>)> = self
            .shared
            .sources
            .read()
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    real.as_ref().and_then(|_| resolve(&s.def.root)),
                )
            })
            .collect();
        let mut sources = self.shared.sources.write();
        for s in sources.iter() {
            let aliased = match (&real, others.iter().find(|(id, _)| *id == s.id)) {
                (Some(a), Some((_, Some(b)))) => a.starts_with(b) || b.starts_with(a),
                _ => false,
            };
            anyhow::ensure!(
                !aliased
                    && s.relative(&def.root).is_none()
                    && relative(&def.root, &s.def.root).is_none(),
                "{} overlaps source {}",
                def.root.display(),
                s.def.label
            );
        }
        let id = SourceId(crate::random_id()?);
        let source = Source::open(
            self.root.join("sources").join(&id.0),
            id.clone(),
            def,
            None,
            Arc::downgrade(&self.shared.sources),
        )?;
        self.shared.db.get()?.execute(
            "INSERT INTO source(id, def, created) VALUES (?1, ?2, ?3)",
            rusqlite::params![id.0, serde_json::to_string(&source.def)?, crate::now()],
        )?;
        *source.owner.write() = Arc::downgrade(&self.shared);
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
        let watcher = self.shared.watchers.lock().remove(id);
        drop(watcher);
        if delete_store {
            let dir = source.dir.clone();
            let deadline = std::time::Instant::now() + REMOVE_WAIT;
            loop {
                source.store.close_idle();
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => break,
                    Err(_) if dir.exists() && std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50));
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

    /// What `remove_source(id, true)` would delete: the store's size on disk, the tags on
    /// its records (Favorites not counted) and its favorites.
    pub fn store_usage(&self, id: &SourceId) -> Result<StoreUsage> {
        let source = self.source(id).with_context(|| format!("no source {id}"))?;
        let bytes = walkdir_size(&source.dir);
        let (tags, favorites) = source.store.get()?.query_row(
            "SELECT coalesce(sum(tag <> ?1), 0), coalesce(sum(tag = ?1), 0) FROM record_tag",
            [crate::FAVORITES],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?;
        Ok(StoreUsage {
            bytes,
            tags,
            favorites,
        })
    }

    pub fn jobs(&self) -> &Jobs {
        &self.jobs
    }

    /// Closes the library: stops watchers and the status poll, and stops jobs at their next
    /// checkpoint (they resume on the next open), waiting up to `timeout`. Callable while
    /// other `Arc<Library>` handles exist; dropping the last one afterwards is then cheap.
    /// False when a job was still in an uninterruptible step (a slow listing or
    /// transfer): it finishes that step on its own and the library stays locked until it
    /// has. Dropping an open library does the same with a 30 s timeout.
    pub fn close(&self, timeout: Duration) -> bool {
        self.shared.jobs.closing.store(true, Ordering::SeqCst);
        let watchers: Vec<_> = self.shared.watchers.lock().drain().collect();
        drop(watchers); // joins their threads
        crate::jobs::shutdown(&self.shared, timeout)
    }

    /// Indexes a source as a durable job (resumed after a restart).
    pub fn index(&self, id: &SourceId) -> Result<JobId> {
        anyhow::ensure!(self.source(id).is_some(), "no source {id}");
        self.jobs.spawn(Box::new(IndexJob { source: id.clone() }))
    }

    /// Keeps a source current (`Indexer::watch`, owned by the library until `unwatch`,
    /// `remove_source` or `close`); a completed walk schedules hashing. A local source
    /// whose root is away is armed by `refresh_status` once it is back, and re-armed after
    /// a reconnect (its old watcher died with the volume).
    pub fn watch(&self, id: &SourceId) -> Result<()> {
        let src = self.source(id).with_context(|| format!("no source {id}"))?;
        self.shared
            .watchers
            .lock()
            .entry(id.clone())
            .or_insert(None);
        let reachable = match src.def.root.to_local_path() {
            Some(p) => p.is_dir(),
            None => true,
        };
        if reachable && src.held().is_none() {
            self.shared.arm(&src);
        }
        Ok(())
    }

    /// Stops keeping a source current (waits for an in-flight change or walk to stop).
    pub fn unwatch(&self, id: &SourceId) {
        let watcher = self.shared.watchers.lock().remove(id);
        drop(watcher);
    }

    /// Whether hashing starts after each completed walk (index jobs and watchers); default
    /// on.
    pub fn set_hash_after_walk(&self, on: bool) {
        self.shared.hash_after_walk.store(on, Ordering::SeqCst);
    }

    /// Seconds east of UTC (local time) for `dm:` dates in `LibrarySearcher` queries;
    /// default 0.
    pub fn set_utc_offset(&self, secs: i64) {
        self.shared.utc_offset.store(secs, Ordering::SeqCst);
    }

    pub(crate) fn utc_offset(&self) -> i64 {
        self.shared.utc_offset.load(Ordering::SeqCst)
    }

    /// The newest `limit` operation log entries (redacted when written), newest first.
    pub fn op_log(&self, limit: usize) -> Result<Vec<crate::OpLogEntry>> {
        crate::oplog::entries(&self.shared, limit)
    }

    /// Content ids the index holds for `entries` (listed children of folder `dir`, relative
    /// to the root of source `id`): for files whose size and modification time still match
    /// their record and whose bytes did not drift, else None. A device source holds no
    /// confirmed content ids (its host's claims are not passed on).
    pub fn content_ids(
        &self,
        id: &SourceId,
        dir: &str,
        entries: &[keel_vfs::Entry],
    ) -> Result<Vec<Option<[u8; 32]>>> {
        use rusqlite::OptionalExtension;
        let src = self.source(id).with_context(|| format!("no source {id}"))?;
        let c = src.store.get()?;
        let Some((parent, _)) = crate::index::resolve(&c, dir, src.nocase())? else {
            return Ok(vec![None; entries.len()]);
        };
        let mut stmt = c.prepare_cached(
            "SELECT size, mtime, cas_id FROM record
             WHERE parent = ?1 AND name = ?2 AND kind = 0 AND cas_id IS NOT NULL
                 AND drift IS NULL",
        )?;
        entries
            .iter()
            .map(|e| {
                let row: Option<(i64, Option<i64>, Vec<u8>)> = stmt
                    .query_row(rusqlite::params![parent, e.name], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()?;
                Ok(row
                    .filter(|(size, mtime, _)| {
                        *size as u64 == e.size && *mtime == e.modified.map(crate::fsid::unix_ns)
                    })
                    .and_then(|(_, _, cas)| cas.try_into().ok()))
            })
            .collect()
    }

    /// Appends finished operations (redacted like every entry) in one transaction: what a
    /// busy logger batches (the library database syncs every commit).
    pub fn log_ops(&self, entries: &[crate::OpDone]) -> Result<()> {
        crate::oplog::record_done(&self.shared, entries)
    }

    /// Appends a finished operation (redacted like every entry); returns its id.
    pub fn log_op(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        result: &str,
        ok: bool,
    ) -> Result<i64> {
        let id = crate::oplog::record(&self.shared, kind, payload, result)?;
        crate::oplog::set_result(&self.shared, id, result, ok)?;
        Ok(id)
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
        // From the trigger-kept `counts` row and two partial indexes (no table scan).
        let counts = |s: &Source| -> Result<(u64, SourceStats)> {
            let c = s.store.get()?;
            let (records, files, bytes, folders, hashed, walked) = c.query_row(
                "SELECT records, files, bytes,
                     (SELECT count(*) FROM record WHERE kind = 1 AND parent IS NOT NULL),
                     (SELECT count(*) FROM record WHERE sampled_hash IS NOT NULL),
                     (SELECT value FROM meta WHERE key = 'last_full_walk')
                 FROM counts",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)? as u64,
                        r.get::<_, i64>(4)? as u64,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )?;
            Ok((
                records,
                SourceStats {
                    id: s.id.clone(),
                    label: s.def.label.clone(),
                    files,
                    folders,
                    bytes,
                    hashed_files: hashed,
                    last_walk: walked.and_then(|t| t.parse().ok()),
                    offline: matches!(*s.status.read(), SourceStatus::Offline { .. }),
                },
            ))
        };
        for s in &sources {
            match counts(s) {
                Ok((records, one)) => {
                    stats.records += records;
                    stats.files += one.files;
                    stats.bytes += one.bytes;
                    stats.per_source.push(one);
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

    /// Counts the distinct content across every source (a scan of all stores) and keeps
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
    /// online or offline (sources being indexed are left alone); a local root with a
    /// different folder than the indexed one is held offline (`OfflineReason::RootMismatch`)
    /// until the indexed folder is back. Watched sources that came back are re-armed.
    /// Returns at once.
    pub fn refresh_status(&self) -> std::thread::JoinHandle<()> {
        let shared = self.shared.clone();
        std::thread::spawn(move || shared.refresh())
    }

    /// Runs `refresh_status` every `every` until the library closes (so watchers re-arm
    /// when a drive comes back).
    pub fn poll_status(&self, every: Duration) -> Result<()> {
        let weak = Arc::downgrade(&self.shared);
        std::thread::Builder::new()
            .name("keel-library-status".into())
            .spawn(move || loop {
                let mut waited = Duration::ZERO;
                while waited < every {
                    let tick = Duration::from_millis(100).min(every - waited);
                    std::thread::sleep(tick);
                    waited += tick;
                    match Weak::upgrade(&weak) {
                        Some(lib) if !lib.closing() => {}
                        _ => return,
                    }
                }
                match Weak::upgrade(&weak) {
                    Some(lib) if !lib.closing() => lib.refresh(),
                    _ => return,
                }
            })?;
        Ok(())
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        self.close(crate::jobs::CLOSE_TIMEOUT);
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
            hash_shares: false,
        }
    }

    /// A source at `root` (`sftp://host/dir`, `cloud://account/`).
    pub(crate) fn remote(label: &str, root: &str) -> SourceDef {
        SourceDef {
            root: VPath::parse(root).unwrap(),
            kind: if root.starts_with("cloud:") {
                SourceKind::Cloud
            } else {
                SourceKind::Folder
            },
            ..folder(label, Path::new(""))
        }
    }

    #[test]
    fn device_sources_are_node_folders_and_only_those() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "t").unwrap();
        let mut local_device = folder("d", root.path());
        local_device.kind = SourceKind::Device;
        assert!(lib.add_source(local_device).is_err());
        let mut node_folder = folder("n", root.path());
        node_folder.root = VPath::parse("node://peer/source").unwrap();
        assert!(lib.add_source(node_folder.clone()).is_err());
        node_folder.kind = SourceKind::Device;
        assert!(lib.add_source(node_folder).is_ok());
    }

    #[test]
    fn open_add_reopen_list_remove() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let (a, b) = (files.path().join("a"), files.path().join("b"));
        let lib = Library::open(data.path(), "main").unwrap();
        let id = lib.add_source(folder("A", &a)).unwrap();
        lib.add_source(folder("B", &b)).unwrap();
        let lib_id = lib.id.clone();
        drop(lib);

        let lib = Library::open(data.path(), "main").unwrap();
        assert_eq!(lib.id, lib_id, "id is stable across opens");
        let labels: Vec<_> = lib.sources().into_iter().map(|s| s.label).collect();
        assert_eq!(labels, ["A", "B"]);
        assert_eq!(
            lib.sources()[0].status,
            SourceStatus::Online { indexed_at: None }
        );
        let listed = Library::list_in(data.path());
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].name.as_str(), listed[0].sources), ("main", 2));

        let store = lib.source(&id).unwrap().store_dir().to_owned();
        assert!(store.join("source.db").is_file());
        lib.remove_source(&id, true).unwrap();
        assert!(!store.exists());
        assert_eq!(lib.sources().len(), 1);
        assert!(lib.remove_source(&id, false).is_err());
        drop(lib);
        assert_eq!(
            Library::open(data.path(), "main").unwrap().sources().len(),
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
        assert!(err.to_string().contains("already open"), "{err}");
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
        // Per source: a folder below the root, a hashed file and the last walk.
        src.store
            .get()
            .unwrap()
            .execute_batch(
                "INSERT INTO record(name, path, kind, size, fs_id, gen, parent)
                     VALUES ('docs', 'docs', 1, 0, 'd', 1, 1);
                 UPDATE record SET sampled_hash = x'09' WHERE name = 'x';",
            )
            .unwrap();
        src.store.set_meta("last_full_walk", "1700000000").unwrap();
        let stats = lib.stats();
        let per: Vec<_> = stats
            .per_source
            .iter()
            .map(|s| {
                (
                    s.label.as_str(),
                    s.files,
                    s.folders,
                    s.bytes,
                    s.hashed_files,
                    s.last_walk,
                    s.offline,
                )
            })
            .collect();
        assert_eq!(
            per,
            [
                ("a", 1, 1, 100, 1, Some(1_700_000_000), false),
                ("b", 2, 0, 15, 0, None, false)
            ]
        );
        assert_eq!(stats.per_source[0].id, src.id);
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
            SourceStatus::Offline {
                last_seen: Some(_),
                ..
            }
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
