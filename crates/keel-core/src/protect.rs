//! Protection (spec 2.10, Task 33): volumes and failure domains, per-record redundancy,
//! backup state, the protection summary and the drive inventory.
//!
//! Every source is on one volume (a partition, a share, a cloud account, a host), and every
//! volume is in one failure domain: the physical disk behind it when known (two partitions
//! of one disk are one domain), the server of a share, the cloud account, the SFTP host;
//! else the volume itself. Redundancy counts files (hard links of one file once) and
//! distinct failure domains, so two copies on one disk never read as two independent
//! copies. Copies on lost or retired volumes do not count; copies on offline or archived
//! volumes count, flagged. A paired device's word that it holds a content (the content id
//! its listing claims) is never a copy: it shows in the hover list, flagged `claimed`.

use crate::library::{RecordRef, Shared, Source, SourceKind, SourceStatus};
use crate::Library;
use anyhow::{Context, Result};
use keel_vfs::VPath;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VolumeKind {
    Fixed,
    Removable,
    Network,
    Cloud,
    /// A paired device, or any other remote source.
    Device,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VolumeState {
    /// A source on it is reachable.
    Online,
    /// None of its sources is reachable (unplugged, host down).
    Offline,
    /// Kept offline on purpose (a drive on a shelf): its copies count, flagged offline.
    Archived,
    /// Gone: its copies no longer count.
    Lost,
    /// Taken out of service: its copies no longer count.
    Retired,
}

impl VolumeState {
    /// Whether copies on such a volume count as copies.
    pub fn counts(self) -> bool {
        !matches!(self, VolumeState::Lost | VolumeState::Retired)
    }

    /// Whether its copies are out of reach now.
    pub fn offline(self) -> bool {
        matches!(self, VolumeState::Offline | VolumeState::Archived)
    }

    /// Stored form: Online/Offline follow the sources ('auto').
    fn key(self) -> &'static str {
        match self {
            VolumeState::Online | VolumeState::Offline => "auto",
            VolumeState::Archived => "archived",
            VolumeState::Lost => "lost",
            VolumeState::Retired => "retired",
        }
    }
}

impl VolumeKind {
    fn key(self) -> &'static str {
        match self {
            VolumeKind::Fixed => "fixed",
            VolumeKind::Removable => "removable",
            VolumeKind::Network => "network",
            VolumeKind::Cloud => "cloud",
            VolumeKind::Device => "device",
        }
    }
    fn parse(s: &str) -> VolumeKind {
        match s {
            "removable" => VolumeKind::Removable,
            "network" => VolumeKind::Network,
            "cloud" => VolumeKind::Cloud,
            "device" => VolumeKind::Device,
            _ => VolumeKind::Fixed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// Volume GUID path (Windows), filesystem UUID (`uuid:…`), `dev:…`, `cloud:<account>`,
    /// `sftp:<host>`; `source:<id>` for a source whose volume was never seen.
    pub id: String,
    pub label: String,
    pub kind: VolumeKind,
    /// Copies sharing it are one copy as far as failures go: `disk:<serial>` (the physical
    /// disk; `disk:<a>+disk:<b>` for a volume spanning disks), `net:<server>`, the cloud
    /// account or host id, else the volume id. The one set by hand when `domain_set`.
    pub failure_domain: String,
    /// The failure domain was set by hand (`Library::set_failure_domain`), not detected.
    pub domain_set: bool,
    pub state: VolumeState,
    /// Unix seconds a source on it was last reachable (0: never).
    pub last_seen: i64,
    /// Marked as a backup volume ("Mark as backup").
    pub backup: bool,
    /// (used, total) bytes as last seen (local volumes).
    pub capacity: Option<(u64, u64)>,
}

/// One file holding a content, where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyAt {
    pub record: RecordRef,
    pub path: VPath,
    pub source_label: String,
    pub volume: Volume,
    /// A device source's file that its device says holds the content; unverified, so it
    /// never counts as a copy.
    #[serde(default)]
    pub claimed: bool,
}

/// How safe a record's content is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redundancy {
    /// Files holding the content on volumes that count (not lost or retired); hard links
    /// of one file count once. 1 for a record without a content id yet.
    pub copies: u64,
    /// Distinct failure domains among those copies.
    pub failure_domains: u64,
    /// A copy is on a backup volume and the copies span two or more failure domains.
    pub backed_up: bool,
    /// Copies (counted above) on offline or archived volumes.
    pub offline_copies: u64,
    /// Every record holding it (lost and retired volumes included, for the hover list),
    /// then the device files claimed to hold it (`claimed`, not counted).
    pub locations: Vec<CopyAt>,
}

/// The Overview's protection card.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionSummary {
    /// Hashed contents held by one file only.
    pub single_copy: u64,
    /// Contents held by two or more files, all in one failure domain.
    pub single_domain: u64,
    /// Contents without a copy on a backup volume in a second failure domain.
    pub unbacked: u64,
    /// Files whose bytes changed while size, mtime and change time did not.
    pub drifted: u64,
    /// Files not hashed yet (hashing off, paused or still running; shares, remote sources
    /// whose hashing is off, remote files over the size cap): in none of the counts above,
    /// their copies are unknown.
    pub unchecked: u64,
    pub offline_volumes: u64,
    /// (volume, used, total) for volumes whose capacity is known.
    pub capacity: Vec<(Volume, u64, u64)>,
}

/// The counters part of `ProtectionSummary`, kept in `library.db` meta.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Counters {
    single_copy: u64,
    single_domain: u64,
    unbacked: u64,
    drifted: u64,
    #[serde(default)]
    unchecked: u64,
}

const COUNTERS: &str = "protection";

/// Quiet time after the last applied change before the protection counters are recounted.
pub const RECOUNT_DEBOUNCE: Duration = Duration::from_secs(5);
/// How often the recount thread looks at the clock, a running walk and `closing`.
const RECOUNT_TICK: Duration = Duration::from_millis(100);

/// Changes applied since the last recount (`schedule_recount`).
#[derive(Default)]
pub(crate) struct PendingRecount {
    /// The last change.
    last: Option<Instant>,
    /// Changed sources, with the generation they were applied to.
    sources: HashMap<crate::SourceId, u64>,
    /// A `recount_when_quiet` thread is waiting.
    worker: bool,
}

#[cfg(test)]
impl PendingRecount {
    /// Nothing pending and no thread waiting.
    pub(crate) fn idle(&self) -> bool {
        self.last.is_none() && self.sources.is_empty() && !self.worker
    }
}

/// After a change applied outside a full walk (a watcher event, an executed operation):
/// the counters are recounted once nothing changed for `RECOUNT_DEBOUNCE`, on a thread of
/// their own (one at a time per library, started here when none waits).
pub(crate) fn schedule_recount(lib: &Arc<Shared>, src: &Source) {
    if lib.closing() || src.removed.load(Ordering::SeqCst) {
        return;
    }
    let mut pending = lib.recount_pending.lock();
    pending.last = Some(Instant::now());
    pending
        .sources
        .insert(src.id.clone(), src.generation.load(Ordering::SeqCst));
    if pending.worker {
        return;
    }
    let weak = Arc::downgrade(lib);
    pending.worker = std::thread::Builder::new()
        .name("keel-protection".into())
        .spawn(move || recount_when_quiet(&weak))
        .map_err(|e| tracing::warn!("protection recount thread: {e}"))
        .is_ok();
}

/// The `schedule_recount` thread. It holds the library only while it looks (an unused
/// library closes at once) and ends with nothing pending or when the library closes. A
/// source being walked defers the recount; a completed walk recounts at its end, so its
/// requests for the generation before it are dropped. A failed walk leaves them pending.
fn recount_when_quiet(lib: &std::sync::Weak<Shared>) {
    loop {
        std::thread::sleep(RECOUNT_TICK);
        let Some(lib) = lib.upgrade() else { return };
        {
            let mut pending = lib.recount_pending.lock();
            if lib.closing() {
                *pending = PendingRecount::default();
                return;
            }
            if pending
                .last
                .is_some_and(|last| last.elapsed() < RECOUNT_DEBOUNCE)
            {
                continue;
            }
            let sources = lib.sources.read();
            pending.sources.retain(|id, gen| {
                sources.iter().any(|s| {
                    &s.id == id
                        && !s.removed.load(Ordering::SeqCst)
                        && s.generation.load(Ordering::SeqCst) == *gen
                })
            });
            let walking = sources.iter().any(|s| {
                pending.sources.contains_key(&s.id) && s.pending_gen.load(Ordering::SeqCst) != 0
            });
            if walking {
                continue;
            }
            pending.last = None;
            if pending.sources.is_empty() {
                pending.worker = false;
                return;
            }
            pending.sources.clear();
        }
        if let Err(e) = recount(&lib) {
            if !e.is::<crate::Cancelled>() {
                tracing::warn!("protection after changes: {e:#}");
            }
        }
    }
}

/// What a root's volume is, seen now.
struct Observed {
    id: String,
    label: String,
    kind: VolumeKind,
    domain: String,
    capacity: Option<(u64, u64)>,
}

fn observe_root(root: &VPath) -> Option<Observed> {
    let Some(path) = root.to_local_path() else {
        let (kind, prefix) = match root.scheme.as_str() {
            "cloud" => (VolumeKind::Cloud, "cloud"),
            "sftp" => (VolumeKind::Network, "sftp"),
            other => (VolumeKind::Device, other),
        };
        // The account / host is the volume and its own failure domain.
        let id = format!("{prefix}:{}", root.authority);
        return Some(Observed {
            label: root.authority.clone(),
            domain: id.clone(),
            id,
            kind,
            capacity: None,
        });
    };
    let v = keel_vfs::volume_info(&path)?;
    let label = if v.label.is_empty() {
        // Windows system drives often have no label: the drive letter.
        match path.components().next() {
            Some(std::path::Component::Prefix(p)) => p.as_os_str().to_string_lossy().into(),
            _ => v.id.clone(),
        }
    } else {
        v.label
    };
    Some(Observed {
        domain: v.disk.unwrap_or_else(|| v.id.clone()),
        kind: match v.kind {
            keel_vfs::VolumeType::Fixed => VolumeKind::Fixed,
            keel_vfs::VolumeType::Removable => VolumeKind::Removable,
            keel_vfs::VolumeType::Network => VolumeKind::Network,
        },
        capacity: (v.total > 0).then_some((v.used, v.total)),
        id: v.id,
        label,
    })
}

/// Records which volume `src`'s root is on now, updating the volume's facts, and its
/// last-seen time when `reachable` (a local root is reachable when its volume can be told).
/// Returns its id.
pub(crate) fn observe(lib: &Shared, src: &Source, reachable: bool) -> Result<Option<String>> {
    if src.removed.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let Some(o) = observe_root(&src.def.root) else {
        return Ok(None);
    };
    let local = src.def.root.to_local_path().is_some();
    let seen = if reachable || local { crate::now() } else { 0 };
    let c = lib.db.get()?;
    c.execute(
        "INSERT INTO volume(id, label, kind, domain, last_seen, used, total)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET label = excluded.label, kind = excluded.kind,
             domain = excluded.domain, last_seen = max(last_seen, excluded.last_seen),
             used = coalesce(excluded.used, used), total = coalesce(excluded.total, total)",
        params![
            o.id,
            o.label,
            o.kind.key(),
            o.domain,
            seen,
            o.capacity.map(|c| c.0 as i64),
            o.capacity.map(|c| c.1 as i64)
        ],
    )?;
    if src.volume_id.read().as_deref() != Some(o.id.as_str()) {
        c.execute(
            "UPDATE source SET volume_id = ?2 WHERE id = ?1",
            params![src.id.0, o.id],
        )?;
        *src.volume_id.write() = Some(o.id.clone());
    }
    Ok(Some(o.id))
}

/// After a completed walk: the volume, then the counters.
pub(crate) fn after_walk(lib: &Shared, src: &Source) {
    if let Err(e) = observe(lib, src, true).and_then(|_| recount(lib)) {
        tracing::warn!("protection after a walk of {}: {e:#}", src.def.label);
    }
}

/// Volumes by id, with Online/Offline derived from their sources' status.
pub(crate) struct Volumes {
    by_id: HashMap<String, Volume>,
}

impl Volumes {
    /// Sources never seen on a volume are looked at first (their root, if reachable).
    pub(crate) fn load(lib: &Shared) -> Result<Volumes> {
        let sources: Vec<Arc<Source>> = lib.sources.read().clone();
        for s in &sources {
            if s.volume_id.read().is_none() {
                if let Err(e) = observe(lib, s, false) {
                    tracing::debug!("volume of {}: {e:#}", s.def.label);
                }
            }
        }
        let mut online: HashSet<String> = HashSet::new();
        for s in &sources {
            let up = !matches!(*s.status.read(), SourceStatus::Offline { .. });
            if let (true, Some(v)) = (up, s.volume_id.read().clone()) {
                online.insert(v);
            }
        }
        let c = lib.db.get()?;
        let mut stmt = c.prepare(
            "SELECT id, label, kind, coalesce(domain_set, domain), state, last_seen, backup, used,
                 total, domain_set IS NOT NULL
             FROM volume",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let state: String = r.get(4)?;
            let used: Option<i64> = r.get(7)?;
            let total: Option<i64> = r.get(8)?;
            Ok(Volume {
                state: match state.as_str() {
                    "archived" => VolumeState::Archived,
                    "lost" => VolumeState::Lost,
                    "retired" => VolumeState::Retired,
                    _ if online.contains(&id) => VolumeState::Online,
                    _ => VolumeState::Offline,
                },
                label: r.get(1)?,
                kind: VolumeKind::parse(&r.get::<_, String>(2)?),
                failure_domain: r.get(3)?,
                domain_set: r.get(9)?,
                last_seen: r.get(5)?,
                backup: r.get(6)?,
                capacity: used.zip(total).map(|(u, t)| (u as u64, t as u64)),
                id,
            })
        })?;
        let by_id = rows
            .map(|v| v.map(|v| (v.id.clone(), v)))
            .collect::<rusqlite::Result<_>>()?;
        Ok(Volumes { by_id })
    }

    /// `src`'s volume; a source never seen on one is its own volume and domain.
    fn of(&self, src: &Source) -> Volume {
        if let Some(v) = src
            .volume_id
            .read()
            .as_ref()
            .and_then(|id| self.by_id.get(id))
        {
            return v.clone();
        }
        let (state, last_seen) = match *src.status.read() {
            SourceStatus::Offline { last_seen, .. } => (VolumeState::Offline, last_seen),
            _ => (VolumeState::Online, None),
        };
        let id = format!("source:{}", src.id);
        Volume {
            label: src.def.label.clone(),
            kind: match src.def.kind {
                SourceKind::Share => VolumeKind::Network,
                SourceKind::Cloud => VolumeKind::Cloud,
                SourceKind::Device => VolumeKind::Device,
                SourceKind::Folder | SourceKind::Drive => VolumeKind::Fixed,
            },
            failure_domain: id.clone(),
            domain_set: false,
            id,
            state,
            last_seen: last_seen.unwrap_or(0),
            backup: false,
            capacity: None,
        }
    }
}

/// One record holding a content.
struct Held {
    src: Arc<Source>,
    id: i64,
    path: String,
    /// The file (native id: hard links share it), else the record.
    file: String,
    /// Only claimed by a device (`remote_cas`).
    claimed: bool,
}

/// The records holding confirmed content `cas` (drifted ones no longer do), then the device
/// files claimed to hold it.
fn holders(lib: &Shared, cas: &[u8]) -> Result<Vec<Held>> {
    let sources: Vec<Arc<Source>> = lib.sources.read().clone();
    let (mut out, mut claims) = (Vec::new(), Vec::new());
    for s in sources {
        let claimed = s.is_device();
        let c = s.store.get()?;
        let mut stmt = c.prepare_cached(if claimed {
            "SELECT id, path, ?2 || ':' || id FROM record
             WHERE remote_cas = ?1 AND kind = 0 ORDER BY id"
        } else {
            "SELECT id, path, CASE WHEN substr(fs_id, 1, 2) <> 'h:' THEN fs_id
                 ELSE ?2 || ':' || id END
             FROM record WHERE cas_id = ?1 AND kind = 0 AND drift IS NULL ORDER BY id"
        })?;
        let rows = stmt.query_map(params![cas, s.id.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        for row in rows {
            let (id, path, file) = row?;
            let held = Held {
                src: s.clone(),
                id,
                path,
                file,
                claimed,
            };
            if claimed {
                claims.push(held);
            } else {
                out.push(held);
            }
        }
    }
    out.append(&mut claims);
    Ok(out)
}

/// (copies, failure domains, backed up, offline copies) of copies given as (file, volume).
pub(crate) fn tally<'a>(
    copies: impl IntoIterator<Item = (&'a str, &'a Volume)>,
) -> (u64, u64, bool, u64) {
    let (mut files, mut domains, mut offline) = (HashSet::new(), HashSet::new(), HashSet::new());
    let mut backup = false;
    for (file, v) in copies.into_iter().filter(|(_, v)| v.state.counts()) {
        files.insert(file);
        domains.insert(v.failure_domain.as_str());
        if v.state.offline() {
            offline.insert(file);
        }
        backup |= v.backup;
    }
    let d = domains.len() as u64;
    (
        files.len() as u64,
        d,
        backup && d >= 2,
        offline.len() as u64,
    )
}

/// Copies of content `cas` per record, with their volume: what `Library::record_copies`
/// returns, plus each file key (for `tally`).
fn copies_of(lib: &Shared, vols: &Volumes, cas: &[u8]) -> Result<Vec<(String, CopyAt)>> {
    Ok(holders(lib, cas)?
        .into_iter()
        .map(|h| {
            (
                h.file,
                CopyAt {
                    record: RecordRef {
                        source: h.src.id.clone(),
                        id: h.id,
                    },
                    path: h.src.absolute(&h.path),
                    source_label: h.src.def.label.clone(),
                    volume: vols.of(&h.src),
                    claimed: h.claimed,
                },
            )
        })
        .collect())
}

/// What a delete leaves of a content (`after_delete`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Left {
    /// No counted copy survives.
    pub none: bool,
    /// The surviving copies fall from two or more failure domains to one.
    pub one_domain: bool,
    /// Copies survive, all of them on offline or archived volumes.
    pub offline: bool,
}

/// The resolved path of a local copy (None: remote, or not reachable now).
fn canonical(p: &VPath) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(p.to_local_path()?).ok()
}

/// For `plan`: per deleted content, what survives. `deleted`: the records being deleted,
/// by (source, id). A surviving record with the file key of a deleted one is a hard link
/// only when it resolves to another path: the same path is the same file seen through an
/// alias (a junction, symlink or subst drive), deleted with it.
pub(crate) fn after_delete(
    lib: &Shared,
    vols: &Volumes,
    cas: &[u8],
    deleted: &HashSet<(String, i64)>,
) -> Result<Left> {
    let mut all = copies_of(lib, vols, cas)?;
    // A device's claims are not copies.
    all.retain(|(_, c)| !c.claimed);
    let gone = |c: &CopyAt| deleted.contains(&(c.record.source.0.clone(), c.record.id));
    // The deleted files' resolved paths, by file key.
    let mut gone_at: HashMap<&str, Vec<Option<std::path::PathBuf>>> = HashMap::new();
    for (f, c) in all.iter().filter(|(_, c)| gone(c)) {
        gone_at
            .entry(f.as_str())
            .or_default()
            .push(canonical(&c.path));
    }
    let kept = |(f, c): &&(String, CopyAt)| {
        !gone(c)
            && gone_at.get(f.as_str()).is_none_or(|at| {
                // Only a path known on both sides and different is another file.
                canonical(&c.path)
                    .is_some_and(|p| at.iter().all(|a| a.as_ref().is_some_and(|a| *a != p)))
            })
    };
    let (_, before, ..) = tally(all.iter().map(|(f, c)| (f.as_str(), &c.volume)));
    let (left, after, _, offline) = tally(
        all.iter()
            .filter(kept)
            .map(|(f, c)| (f.as_str(), &c.volume)),
    );
    Ok(Left {
        none: left == 0,
        one_domain: left > 0 && after == 1 && before >= 2,
        offline: left > 0 && offline == left,
    })
}

/// Recomputes the protection counters over every store (a scan; at walk, hash and
/// integrity ends and when volume states change).
pub(crate) fn recount(lib: &Shared) -> Result<()> {
    let _one_at_a_time = lib.recounting.lock();
    anyhow::ensure!(!lib.closing(), crate::Cancelled);
    let vols = Volumes::load(lib)?;
    let sources: Vec<Arc<Source>> = lib.sources.read().clone();
    // A private scratch database (SQLite spills it to a temp file).
    let scratch = Connection::open("")?;
    scratch.execute_batch("CREATE TABLE c(content, file TEXT, domain TEXT, backup INTEGER)")?;
    let (mut drifted, mut unchecked) = (0u64, 0u64);
    for s in &sources {
        anyhow::ensure!(!lib.closing(), crate::Cancelled);
        let v = vols.of(s);
        // A device's claims are not copies.
        if !v.state.counts() || s.is_device() {
            continue;
        }
        let path = s.store_dir().join("source.db");
        scratch.execute("ATTACH DATABASE ?1 AS s", [path.to_string_lossy()])?;
        // A file whose sampled hash is unique (no content id: no other file can hold it)
        // is its own content; files never hashed are not counted.
        let copied = scratch
            .execute(
                "INSERT INTO c SELECT coalesce(cas_id, 'u:' || ?1 || ':' || id),
                     CASE WHEN substr(fs_id, 1, 2) <> 'h:' THEN fs_id ELSE ?1 || ':' || id END,
                     ?2, ?3
                 FROM s.record WHERE kind = 0 AND drift IS NULL
                     AND (cas_id IS NOT NULL OR sampled_hash IS NOT NULL)",
                params![s.id.0, v.failure_domain, v.backup],
            )
            .and_then(|_| {
                scratch.query_row(
                    "SELECT count(*) FILTER (WHERE drift IS NOT NULL),
                         count(*) FILTER (WHERE kind = 0 AND drift IS NULL AND cas_id IS NULL
                             AND sampled_hash IS NULL)
                     FROM s.record",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
            });
        scratch.execute("DETACH DATABASE s", [])?;
        let (d, u) = copied?;
        drifted += d as u64;
        unchecked += u as u64;
    }
    let (single_copy, single_domain, unbacked) = scratch.query_row(
        "SELECT coalesce(sum(files = 1), 0), coalesce(sum(files > 1 AND domains = 1), 0),
             coalesce(sum(NOT (backup AND domains > 1)), 0)
         FROM (SELECT count(DISTINCT file) files, count(DISTINCT domain) domains,
                   max(backup) backup FROM c GROUP BY content)",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        },
    )?;
    let counters = Counters {
        single_copy,
        single_domain,
        unbacked,
        drifted,
        unchecked,
    };
    anyhow::ensure!(!lib.closing(), crate::Cancelled);
    lib.db
        .set_meta(COUNTERS, &serde_json::to_string(&counters)?)?;
    lib.protection_revision.fetch_add(1, Ordering::Release);
    Ok(())
}

impl Library {
    /// Changes after a completed protection recount (no I/O). Clients re-read the card
    /// and copies badges when this changes; the daemon publishes `protection.recount`.
    pub fn protection_revision(&self) -> u64 {
        self.shared.protection_revision.load(Ordering::Acquire)
    }

    /// The drive inventory: every volume a source was ever seen on, by label.
    pub fn volumes(&self) -> Result<Vec<Volume>> {
        let mut out: Vec<Volume> = Volumes::load(&self.shared)?.by_id.into_values().collect();
        out.sort_by(|a, b| (a.label.to_lowercase(), &a.id).cmp(&(b.label.to_lowercase(), &b.id)));
        Ok(out)
    }

    /// Marks a volume archived, lost or retired; Online or Offline hands it back to its
    /// sources' status. Recounts the protection counters.
    pub fn set_volume_state(&self, volume: &str, state: VolumeState) -> Result<()> {
        let n = self.shared.db.get()?.execute(
            "UPDATE volume SET state = ?2 WHERE id = ?1",
            params![volume, state.key()],
        )?;
        anyhow::ensure!(n == 1, "no volume {volume}");
        recount(&self.shared)
    }

    /// Marks (or unmarks) a volume as a backup. Recounts the protection counters.
    pub fn set_backup(&self, volume: &str, backup: bool) -> Result<()> {
        let n = self.shared.db.get()?.execute(
            "UPDATE volume SET backup = ?2 WHERE id = ?1",
            params![volume, backup],
        )?;
        anyhow::ensure!(n == 1, "no volume {volume}");
        recount(&self.shared)
    }

    /// Sets the failure domain of a volume by hand (two names of one server, a disk the OS
    /// cannot tell apart, LVM or pools the detection splits); None or blank goes back to the
    /// detected one. Recounts the protection counters.
    pub fn set_failure_domain(&self, volume: &str, domain: Option<&str>) -> Result<()> {
        let domain = domain.map(str::trim).filter(|d| !d.is_empty());
        let n = self.shared.db.get()?.execute(
            "UPDATE volume SET domain_set = ?2 WHERE id = ?1",
            params![volume, domain],
        )?;
        anyhow::ensure!(n == 1, "no volume {volume}");
        recount(&self.shared)
    }

    /// Every record holding confirmed content `cas_id` (drifted ones and device claims
    /// excluded), with its volume.
    pub fn record_copies(&self, cas_id: &[u8]) -> Result<Vec<(RecordRef, Volume)>> {
        let vols = Volumes::load(&self.shared)?;
        Ok(copies_of(&self.shared, &vols, cas_id)?
            .into_iter()
            .filter(|(_, c)| !c.claimed)
            .map(|(_, c)| (c.record, c.volume))
            .collect())
    }

    /// The record's content id: `None` before hashing, or when `now` (the file's mtime in
    /// ns and size as it is now) differs from the record. `now: None` (an offline file):
    /// the content id as last indexed.
    pub fn content_id(
        &self,
        record: &RecordRef,
        now: Option<(i64, u64)>,
    ) -> Result<Option<[u8; 32]>> {
        let src = self
            .source(&record.source)
            .with_context(|| format!("no source {}", record.source))?;
        let row: Option<(Option<Vec<u8>>, i64, i64)> = src
            .store
            .get()?
            .query_row(
                "SELECT cas_id, coalesce(mtime, 0), size FROM record WHERE id = ?1",
                [record.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row
            .filter(|(_, m, s)| now.is_none_or(|now| now == (*m, *s as u64)))
            .and_then(|(cas, _, _)| cas?.try_into().ok()))
    }

    /// How many files hold this record's content (hard links of one file once; lost and
    /// retired volumes not counted), in how many failure domains, whether it is backed
    /// up, and where.
    pub fn redundancy(&self, record: &RecordRef) -> Result<Redundancy> {
        let vols = Volumes::load(&self.shared)?;
        self.redundancy_with(&vols, record)
    }

    /// `redundancy` of many records (a folder's badges), reading the volumes once; None for a
    /// record that could not be read.
    pub fn redundancies(&self, records: &[RecordRef]) -> Result<Vec<Option<Redundancy>>> {
        let vols = Volumes::load(&self.shared)?;
        Ok(records
            .iter()
            .map(|r| self.redundancy_with(&vols, r).ok())
            .collect())
    }

    fn redundancy_with(&self, vols: &Volumes, record: &RecordRef) -> Result<Redundancy> {
        let src = self
            .source(&record.source)
            .with_context(|| format!("no source {}", record.source))?;
        let (cas, path): (Option<Vec<u8>>, String) = src
            .store
            .get()?
            .query_row(
                "SELECT cas_id, path FROM record WHERE id = ?1",
                [record.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .with_context(|| format!("no record {}", record.id))?;
        let copies = match &cas {
            Some(cas) => copies_of(&self.shared, vols, cas)?,
            None => Vec::new(),
        };
        if copies.iter().all(|(_, c)| c.claimed) {
            // No content id yet (or only drifted copies or claims): the record itself.
            let volume = vols.of(&src);
            let mut locations = vec![CopyAt {
                record: record.clone(),
                path: src.absolute(&path),
                source_label: src.def.label.clone(),
                volume: volume.clone(),
                claimed: false,
            }];
            locations.extend(copies.into_iter().map(|(_, c)| c));
            return Ok(Redundancy {
                copies: 1,
                failure_domains: 1,
                backed_up: false,
                offline_copies: u64::from(volume.state.offline()),
                locations,
            });
        }
        let (copies_n, failure_domains, backed_up, offline_copies) = tally(
            copies
                .iter()
                .filter(|(_, c)| !c.claimed)
                .map(|(f, c)| (f.as_str(), &c.volume)),
        );
        Ok(Redundancy {
            copies: copies_n,
            failure_domains,
            backed_up,
            offline_copies,
            locations: copies.into_iter().map(|(_, c)| c).collect(),
        })
    }

    /// The Overview's protection card: counters kept by walks, hashing and integrity
    /// checks (no scan here), plus the volumes' states and capacity.
    pub fn protection_summary(&self) -> Result<ProtectionSummary> {
        let counters: Counters = self
            .shared
            .db
            .meta(COUNTERS)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let volumes = self.volumes()?;
        Ok(ProtectionSummary {
            single_copy: counters.single_copy,
            single_domain: counters.single_domain,
            unbacked: counters.unbacked,
            drifted: counters.drifted,
            unchecked: counters.unchecked,
            offline_volumes: volumes
                .iter()
                .filter(|v| v.state == VolumeState::Offline)
                .count() as u64,
            capacity: volumes
                .into_iter()
                .filter_map(|v| v.capacity.map(|(u, t)| (v.clone(), u, t)))
                .collect(),
        })
    }

    /// Recomputes the protection counters now (a scan of every store); walks, hashing and
    /// integrity checks do this when they end.
    pub fn recount_protection(&self) -> Result<()> {
        recount(&self.shared)
    }
}

#[cfg(test)]
#[path = "protect_tests.rs"]
mod tests;
