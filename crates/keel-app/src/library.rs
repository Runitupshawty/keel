//! The library in the app (spec 2.10, Task 29): opening it off the UI thread, the index
//! behind the `library://` provider, sidebar data, library job rows, tags, plans
//! (validate → preview → execute) and the duplicate finder. The library is in this
//! process or in the profile's keel-daemon (`backend`: the window attaches to a running
//! daemon); every call that reads a store or the daemon runs on a worker; only
//! `sources()`, `note_activity()`, `refresh_status()` and `Jobs::subscribe` of an
//! in-process library (which return at once) run on the UI thread. Drawing is in
//! `library_ui`.

use crate::backend::{Attach, Event, Executed, LibraryBackend, Planned, Remote, VolumeChange};
use crate::keys::Action;
use crate::state::{AppState, Msg};
use crate::tab::TabKind;
use crate::worker;
use anyhow::Context as _;
use crossbeam_channel::{Receiver, Sender};
use keel_core::{
    Indexer, JobEvent, JobId, JobStatus, Library, LibraryHit, LibraryStats, LibrarySummary,
    OfflineReason, OnConflict, Op, Plan, ProtectionSummary, RecordRef, SourceDef, SourceId,
    SourceKind, SourceStatus, SourceSummary, Tag, TagId, View, Volume, VolumeKind, VolumeState,
    Warning, WatchConfig, WatchHandle, FAVORITES,
};
use keel_search::{Hit, Query, Searcher};
use keel_vfs::library as vlib;
use keel_vfs::{Entry, Kind, Router, VPath};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The library search tab's saved queries for the sidebar's Favorites and Recents.
pub const FAVORITES_QUERY: &str = "is:favorite";
pub const RECENTS_QUERY: &str = "is:recent";
/// `Library::close` on exit waits this long for jobs to checkpoint.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Sources, stats and tags are refreshed this often (stats every `STATS_EVERY`).
const META_EVERY: Duration = Duration::from_secs(30);
const STATS_EVERY: Duration = Duration::from_secs(5);
const STATUS_EVERY: Duration = Duration::from_secs(60);
/// A watcher that could not start (source offline) is tried again after this long.
const WATCH_RETRY: Duration = Duration::from_secs(300);
/// Finished library job rows leave the jobs panel after this long (failures stay).
const ROW_KEPT: Duration = Duration::from_secs(5);
/// The duplicate finder shows at most this many groups (biggest first).
pub(crate) const MAX_GROUPS: usize = 500;
/// The daemon's sources are read this often (more often while something runs).
const SOURCES_EVERY: Duration = Duration::from_secs(5);
const SOURCES_BUSY: Duration = Duration::from_secs(1);

/// The Overview tab's folder (no provider: it is never listed).
pub fn overview_path() -> VPath {
    VPath {
        scheme: "keel".into(),
        authority: "overview".into(),
        path: "/".into(),
    }
}

/// Source labels by id, for tab titles and breadcrumbs of `library://` folders.
static LABELS: RwLock<Vec<(String, String)>> = parking_lot::const_rwlock(Vec::new());

pub fn label_of(source: &str) -> Option<String> {
    LABELS
        .read()
        .iter()
        .find(|(id, _)| id == source)
        .map(|(_, l)| l.clone())
}

/// Settings → Library → Hashing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Hashing {
    /// Only while the user is idle (and not on battery).
    #[default]
    IdleOnly,
    /// Also while the user works; pauses on battery.
    PauseOnBattery,
    Off,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LibrarySettings {
    pub enabled: bool,
    /// The library under `<data dir>/library/<name>/`.
    pub name: String,
    pub hashing: Hashing,
    pub hash_remote: bool,
    pub hash_cloud: bool,
    pub remote_hash_max_bytes: u64,
    /// Remote and cloud sources are re-walked this often (local sources are watched live
    /// and reconciled every 6 hours).
    pub rescan_minutes: u64,
    /// Details view: the Tags column.
    pub tags_column: bool,
    // --- Task 33 ---
    /// Integrity checks re-hash this percentage of each source's hashed files.
    pub integrity_pct: f64,
    /// Days between integrity checks (0: off).
    pub integrity_days: u32,
    /// Without a running keel-daemon, start one and attach to it instead of opening the
    /// library in this window (so the CLI, `keel mcp` and other windows keep working).
    pub daemon: bool,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        let remote = keel_core::RemoteHashSettings::default();
        Self {
            enabled: true,
            name: keel_api::config::DEFAULT_LIBRARY.into(),
            hashing: Hashing::default(),
            hash_remote: remote.hash_remote,
            hash_cloud: remote.hash_cloud,
            remote_hash_max_bytes: remote.remote_hash_max_bytes,
            rescan_minutes: 15,
            tags_column: true,
            integrity_pct: keel_core::DEFAULT_SAMPLE_PCT,
            integrity_days: 7,
            daemon: false,
        }
    }
}

impl LibrarySettings {
    fn remote_hash_settings(&self) -> keel_core::RemoteHashSettings {
        keel_core::RemoteHashSettings {
            hash_remote: self.hash_remote,
            hash_cloud: self.hash_cloud,
            remote_hash_max_bytes: self.remote_hash_max_bytes,
        }
    }
}

/// A sidebar / overview / dialog command (`Action::Library`).
#[derive(Clone, Debug, PartialEq)]
pub enum LibCmd {
    Overview,
    OpenSource(SourceId),
    IndexNow(SourceId),
    /// A source held offline because another folder (or nothing) is at its root: index
    /// whatever is there now (`Indexer::adopt_root`).
    AdoptRoot(SourceId),
    /// Asks first.
    RemoveSource(SourceId),
    RemoveConfirmed(SourceId),
    PauseHashing(bool),
    AddSource,
    /// The Add source wizard's answer.
    Register(SourceDef),
    /// First-run toast: Documents as a source.
    AddDocuments,
    /// Favorites, Recents, a tag or a saved view as a library search tab.
    Query(String),
    TagPicker,
    ToggleFavorite,
    /// Tag (or untag) the tag picker's targets (else the active pane's).
    SetTag {
        tag: TagId,
        on: bool,
    },
    /// Tag (or untag) these paths (the details view's favorite star).
    TagPaths {
        paths: Vec<VPath>,
        tag: TagId,
        on: bool,
    },
    CreateTag {
        name: String,
        color: String,
    },
    Duplicates,
    /// Duplicate finder: plan deleting every record of group `group` but `keep`.
    KeepOne {
        group: usize,
        keep: usize,
    },
    /// The shown plan is confirmed.
    Execute,
    CancelJob(JobId),
    RebuildIndex,
    /// Settings: open (creating) another library.
    Switch(String),
    /// Toggle `library.enabled`.
    Enable(bool),
    // --- Task 33 ---
    /// Drive inventory: Archived / Lost / Retired, or Online/Offline (automatic again).
    SetVolumeState {
        volume: String,
        state: VolumeState,
    },
    /// "Mark as backup".
    SetBackup {
        volume: String,
        on: bool,
    },
    /// The failure-domain field: set by hand, or blank for the detected one.
    SetDomain {
        volume: String,
        domain: String,
    },
    /// Overview → Protection: re-hash a sample now.
    CheckIntegrity,
    /// The daemon went away: connect again (starting it when it is not running).
    Reconnect,
    /// The daemon went away: open the library in this window.
    OpenHere,
    /// Close and open again (Settings → Library → background daemon turned on).
    Restart,
}

/// How `LibraryUi::open` reaches the library.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Via {
    /// A running daemon, else (`spawn`) a daemon started now, else this process.
    Auto { spawn: bool },
    /// After the attached daemon went away (Reconnect, Open in this window): once the
    /// stopping daemon has let go of the library (`library.lock`, up to `wait`), this
    /// process (`here`) or a daemon (a running one, else one started now). The lost
    /// connection is kept until this succeeds.
    Replace { here: bool, wait: Duration },
}

/// Attached: input is reported to the daemon at most this often (each report pauses its
/// idle jobs for 5 s, so a busy user keeps them paused).
pub const ACTIVITY_EVERY: Duration = Duration::from_secs(4);

/// How long Reconnect and Open in this window wait for a stopping daemon to close the
/// library (it says `daemon.stopping` first, then closes, waiting up to `CLOSE_WAIT` for
/// jobs).
pub const RELEASE_WAIT: Duration = Duration::from_secs(keel_api::host::CLOSE_WAIT.as_secs() + 5);

/// What a `library.changed` of `kind` (the operation, any client's) makes the window read
/// again.
#[derive(Debug, PartialEq)]
pub enum Refresh {
    Nothing,
    /// The protection card, volume table and copies badges.
    Protection,
    /// Listings (Recents moved).
    Recents,
    /// Sources, tags, badges and listings.
    All,
}

pub fn refresh_for(kind: &str) -> Refresh {
    match kind {
        // They start a job or set a policy: the job's end refreshes what it changed.
        "hashing.set" | "integrity.check" | "media.index" | "sources.index" | "jobs.cancel" => {
            Refresh::Nothing
        }
        // Devices follow `net.event` and their own poll.
        "devices.settings_set" => Refresh::Nothing,
        "volumes.set" | "protection.recount" => Refresh::Protection,
        "recents.note" => Refresh::Recents,
        _ => Refresh::All,
    }
}

/// Answers from library workers (`Msg::Library`).
pub enum LibMsg {
    Opened {
        result: Result<Opened, String>,
        first_run: bool,
    },
    /// The daemon's sources.
    Sources(Vec<SourceSummary>),
    /// From the daemon connection `id`'s subscription.
    Remote(u64, Event),
    Stats(LibraryStats),
    /// The library's remote hashing policy (read when it opens).
    RemoteHashing(keel_core::RemoteHashSettings),
    Meta {
        tags: Vec<Tag>,
        views: Vec<View>,
        tagged: HashMap<VPath, Vec<TagId>>,
    },
    Watching(SourceId, Option<WatchHandle>),
    /// A job this app started (index, hash, operation); None: it failed to start.
    Spawned {
        kind: &'static str,
        id: Option<JobId>,
    },
    Planned(Result<Planned, String>),
    /// `Plan::execute` refused: the sources changed; show this fresh plan.
    Changed(Planned),
    Dups(Result<Vec<DupGroup>, String>),
    Libraries(Vec<LibrarySummary>),
    /// The add-source wizard's folder picker answered.
    Picked(Option<String>),
    /// Job kinds by id (for rows that arrived as bare events).
    Kinds(Vec<(JobId, String)>),
    Closed,
    // --- Task 33 ---
    Protection(ProtectionSummary, Vec<Volume>),
    /// Copies badges of the files in some folders (by real path).
    Badges(HashMap<VPath, Badge>),
}

/// An opened library: where it is, its running jobs and their events, and why a wanted
/// daemon is not used.
pub struct Opened {
    pub backend: LibraryBackend,
    pub jobs: Vec<(JobId, JobRow)>,
    pub events: Option<Receiver<JobEvent>>,
    /// A daemon was started for this window.
    pub spawned: bool,
    pub note: Option<String>,
}

/// One library job in the jobs panel.
#[derive(Clone, Debug, PartialEq)]
pub struct JobRow {
    pub kind: String,
    pub status: JobStatus,
    pub progress: f32,
    pub ended: Option<Instant>,
}

impl JobRow {
    pub fn active(&self) -> bool {
        matches!(self.status, JobStatus::Queued | JobStatus::Running)
    }

    pub fn title(&self) -> &str {
        match self.kind.as_str() {
            "index" => "Library: indexing",
            "hash" => "Library: hashing contents",
            "op" => "Library: file operation",
            keel_net::spacedrop::KIND => "Spacedrop: sending",
            "sidecar" => "Library: media thumbnails",
            "integrity" => "Library: checking integrity",
            _ => "Library job",
        }
    }
}

/// A duplicate group with each record resolved for display.
#[derive(Clone, Debug, PartialEq)]
pub struct DupGroup {
    pub size: u64,
    pub records: Vec<DupRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DupRecord {
    pub path: VPath,
    pub source: String,
    pub offline: bool,
}

/// Bytes freed by keeping one copy of each group.
pub fn reclaimable(groups: &[DupGroup]) -> u64 {
    groups
        .iter()
        .map(|g| g.size * (g.records.len() as u64).saturating_sub(1))
        .sum()
}

/// Duplicate groups of at least `min_size` bytes, each record resolved (worker only).
pub fn load_dups(lib: &Library, min_size: u64) -> anyhow::Result<Vec<DupGroup>> {
    let mut out = Vec::new();
    for g in lib.duplicates(min_size)?.into_iter().take(MAX_GROUPS) {
        let mut records = Vec::new();
        for r in g.records {
            if let Some(hit) = lib.record(&r)? {
                records.push(DupRecord {
                    path: hit.path,
                    source: hit.source_label,
                    offline: matches!(hit.status, SourceStatus::Offline { .. }),
                });
            }
        }
        if records.len() > 1 {
            out.push(DupGroup {
                size: g.size,
                records,
            });
        }
    }
    Ok(out)
}

/// The plan for "Keep this one": delete every other record of the group.
pub fn keep_one(group: &DupGroup, keep: usize) -> Op {
    Op::Delete {
        paths: (group.records.iter().enumerate())
            .filter(|(i, _)| *i != keep)
            .map(|(_, r)| r.path.clone())
            .collect(),
    }
}

/// Where a source row's status dot and text come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dot {
    Online,
    Indexing,
    Offline,
    Error,
}

/// One sidebar row of the Sources section.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceRow {
    pub id: SourceId,
    pub label: String,
    pub dot: Dot,
    /// Short status text ("indexing 1,200 / 5,000", "offline since …").
    pub detail: String,
    /// Held offline over its root (offer "Adopt new root").
    pub adopt: bool,
}

/// Why a source is offline, for its tooltip and read errors.
pub fn offline_text(last_seen: Option<i64>, reason: OfflineReason) -> String {
    let seen = when(last_seen);
    match reason {
        OfflineReason::Unreachable => format!("offline (last seen {seen})"),
        OfflineReason::RootMismatch => format!(
            "offline (last seen {seen}): a different folder is at its root; \
             right-click → Adopt new root to index it"
        ),
        OfflineReason::Empty => format!(
            "offline (last seen {seen}): its root is empty (unmounted?); \
             right-click → Adopt new root to index it anyway"
        ),
    }
}

pub fn source_rows(sources: &[SourceSummary]) -> Vec<SourceRow> {
    sources
        .iter()
        .map(|s| {
            let (dot, detail) = match &s.status {
                SourceStatus::Online { indexed_at: None } => {
                    (Dot::Online, "not indexed yet".into())
                }
                SourceStatus::Online { indexed_at } => {
                    (Dot::Online, format!("indexed {}", when(*indexed_at)))
                }
                SourceStatus::Indexing { done, total: 0 } => {
                    (Dot::Indexing, format!("indexing {}", count(*done)))
                }
                SourceStatus::Indexing { done, total } => (
                    Dot::Indexing,
                    format!("indexing {} / {}", count(*done), count(*total)),
                ),
                SourceStatus::Offline { last_seen, reason } => {
                    (Dot::Offline, offline_text(*last_seen, *reason))
                }
                SourceStatus::Error(e) => (Dot::Error, e.clone()),
            };
            SourceRow {
                id: s.id.clone(),
                label: s.label.clone(),
                dot,
                detail,
                adopt: matches!(
                    s.status,
                    SourceStatus::Offline {
                        reason: OfflineReason::RootMismatch | OfflineReason::Empty,
                        ..
                    }
                ),
            }
        })
        .collect()
}

/// `1234567` -> `1,234,567`.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A unix time as local `YYYY-MM-DD HH:MM`, "never" for None.
pub fn when(t: Option<i64>) -> String {
    t.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "never".into())
}

/// The source whose root holds `p` (a real path), with `p` relative to it.
pub fn source_of<'a>(
    sources: &'a [SourceSummary],
    p: &VPath,
) -> Option<(&'a SourceSummary, String)> {
    sources.iter().find_map(|s| {
        let root = &s.root;
        if root.scheme != p.scheme || root.authority != p.authority || p.split_archive().is_some() {
            return None;
        }
        let fold = |x: &str| {
            let x = x.trim_end_matches('/');
            if cfg!(windows) && root.scheme == "file" {
                x.to_lowercase()
            } else {
                x.to_owned()
            }
        };
        let (r, q) = (fold(&root.path), fold(&p.path));
        let rest = q.strip_prefix(&r)?;
        if !(rest.is_empty() || rest.starts_with('/')) {
            return None;
        }
        // The original spelling of the remainder.
        let rel = p.path.trim_end_matches('/')[r.len()..].trim_matches('/');
        Some((s, rel.to_owned()))
    })
}

/// The real path of `p`: a `library://` path through its source's root, others as is.
pub fn real_of(sources: &[SourceSummary], p: &VPath) -> Option<VPath> {
    match vlib::split(p) {
        Some((id, rel)) => {
            let s = sources.iter().find(|s| s.id.0 == id)?;
            Some(if rel.is_empty() {
                s.root.clone()
            } else {
                s.root.join(rel)
            })
        }
        None => Some(p.clone()),
    }
}

/// `p` (real or `library://`) as `(source, path relative to it)`.
pub fn locate(sources: &[SourceSummary], p: &VPath) -> Option<(SourceId, String)> {
    match vlib::split(p) {
        Some((id, rel)) => Some((SourceId(id.into()), rel.to_owned())),
        None => source_of(sources, p).map(|(s, rel)| (s.id.clone(), rel)),
    }
}

/// The record of `(source, rel)` (worker only: lists its folder).
pub fn record_of(lib: &Library, source: &SourceId, rel: &str) -> anyhow::Result<RecordRef> {
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let children = lib.list_children(source, parent)?;
    let hit = children
        .iter()
        .find(|h| h.name == name)
        .or_else(|| children.iter().find(|h| h.name.eq_ignore_ascii_case(name)))
        .with_context(|| format!("{name} is not in the library index yet"))?;
    Ok(hit.record.clone())
}

/// One line per warning, for the preview dialog.
pub fn warning_text(w: &Warning, sources: &[SourceSummary]) -> String {
    let files = |n: u64, one: &str, many: &str| {
        if n == 1 {
            format!("1 file {one}")
        } else {
            format!("{} files {many}", count(n))
        }
    };
    match w {
        Warning::LastCopy { path, files: n } => format!(
            "{} ({})",
            files(
                *n,
                "is the last copy of its content",
                "are the last copies of their content"
            ),
            path.name()
        ),
        Warning::OfflineSource { label, .. } => format!(
            "Source offline: {label} (previewed from its last index; it must be online to run)"
        ),
        Warning::NotIndexed { path } => {
            format!(
                "{} is not indexed: previewed from the folder itself",
                path.name()
            )
        }
        Warning::Exists { path, on_conflict } => format!(
            "{} already exists there: {}",
            path.name(),
            match on_conflict {
                OnConflict::Skip => "skipped",
                OnConflict::Overwrite => "overwritten",
                OnConflict::RenameNew => "both kept",
            }
        ),
        Warning::Permanent { path } => {
            let place = source_of(sources, path)
                .map(|(s, _)| s.label.clone())
                .unwrap_or_else(|| path.authority.clone());
            format!("Permanent delete on {place}: there is no trash there")
        }
        Warning::ContentUnverified { path, files: n } => format!(
            "{} not hashed yet, so other copies are unknown ({})",
            files(*n, "is", "are"),
            path.name()
        ),
        // --- Task 33 ---
        Warning::SingleDomain { path, files: n } => format!(
            "{} the only copy outside one failure domain: every copy left would be on one \
             disk, account or host ({})",
            files(*n, "is", "are"),
            path.name()
        ),
        Warning::CopiesOffline { path, files: n } => format!(
            "{} other copies only on offline or archived drives ({})",
            files(*n, "has its", "have their"),
            path.name()
        ),
        Warning::RewritesArchive { path, bytes } => format!(
            "Rewrites the {} archive {}: it is written again beside itself, then replaces itself",
            crate::view_details::size_text_of(*bytes),
            path.name()
        ),
    }
}

/// The dialog's headline: "Delete 3 files (1.2 MB)".
pub fn plan_summary(plan: &Plan) -> String {
    let (files, bytes) = plan
        .changes
        .iter()
        .fold((0, 0), |(f, b), c| (f + c.files, b + c.bytes));
    let verb = match plan.op {
        Op::Copy { .. } => "Copy",
        Op::Move { .. } => "Move",
        Op::Delete { .. } => "Delete",
        Op::Rename { .. } => "Rename",
    };
    let what = if files == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", count(files))
    };
    format!(
        "{verb} {what} ({})",
        humansize::format_size(bytes, humansize::DECIMAL)
    )
}

/// The add-source wizard.
pub struct AddSource {
    pub root: String,
    pub label: String,
    pub include_hidden: bool,
    pub ignore: String,
    /// The OS folder picker is open (on a worker).
    pub picking: bool,
}

/// The tag picker (Ctrl+Shift+T) for the targets.
pub struct TagPicker {
    pub targets: Vec<VPath>,
    pub new_name: String,
    pub new_color: [u8; 3],
}

pub struct DupFinder {
    pub groups: Option<Result<Vec<DupGroup>, String>>,
}

/// The validate → preview dialog.
pub struct PlanDialog {
    pub plan: Planned,
    /// The sources changed since the first preview (this is the fresh one).
    pub changed: bool,
    pub running: bool,
}

enum Watch {
    Starting,
    Live(WatchHandle),
    Failed(Instant),
}

type Slot = Arc<RwLock<Option<LibraryBackend>>>;

pub struct LibraryUi {
    /// Where the open library is (None: closed).
    pub backend: Option<LibraryBackend>,
    /// The library when it is open in this process (the node and the media view use it).
    pub lib: Option<Arc<Library>>,
    /// Attached: the daemon went away; nothing is written until Reconnect or Open in this
    /// window.
    pub lost: bool,
    /// Attached: when the daemon was last told the user is working (`activity.note`).
    activity_sent: Option<Instant>,
    /// Attached to a daemon this window started.
    pub spawned: bool,
    /// How long Reconnect / Open in this window wait for a stopping daemon (`RELEASE_WAIT`).
    pub release_wait: Duration,
    /// Why the daemon the settings asked for is not used.
    pub note: Option<String>,
    next_sources: Instant,
    /// Shared with the `library://` provider.
    slot: Slot,
    pub opening: bool,
    pub error: Option<String>,
    pub sources: Vec<SourceSummary>,
    pub stats: LibraryStats,
    pub tags: Vec<Tag>,
    pub views: Vec<View>,
    /// Tags (Favorites included) by real path.
    pub tagged: HashMap<VPath, Vec<TagId>>,
    pub jobs: BTreeMap<JobId, JobRow>,
    events: Option<Receiver<JobEvent>>,
    watchers: HashMap<SourceId, Watch>,
    /// Index jobs being started (no watcher may start meanwhile).
    starting_index: usize,
    pub hash_paused: bool,
    next_meta: Instant,
    next_stats: Instant,
    next_status: Instant,
    pub libraries: Vec<LibrarySummary>,
    pub add: Option<AddSource>,
    pub picker: Option<TagPicker>,
    pub dups: Option<DupFinder>,
    /// Latest duplicate summary (groups, reclaimable bytes) for the Overview.
    pub dup_summary: Option<(usize, u64)>,
    pub plan: Option<PlanDialog>,
    /// Entries copied or cut in a library view (real paths): Paste plans the transfer.
    pub clip: Option<(Vec<VPath>, bool)>,
    /// Settings → Library: the new library's name.
    pub new_name: String,
    /// Commands from Settings → Library, run after the window (`AppState::settings_ui`).
    pub pending: Vec<LibCmd>,
    /// The hashing policy last applied.
    policy: Hashing,
    remote_hash_settings: keel_core::RemoteHashSettings,
    /// Media work was asked for while a sidecar job ran: started again when it ends (a
    /// running job does not see records changed behind its cursor, nor other sources).
    media_again: bool,
    // --- Task 33 ---
    /// The Overview's protection card and volume table (None until first read).
    pub protection: Option<ProtectionSummary>,
    /// `Library::protection_revision` last seen (in process).
    protection_revision: u64,
    pub volumes: Vec<Volume>,
    /// Details view copies badges by real path; folders wanted (`badge`), and asked for.
    pub badges: HashMap<VPath, Badge>,
    badge_wanted: parking_lot::Mutex<HashSet<VPath>>,
    badge_asked: HashSet<VPath>,
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl LibraryUi {
    /// Registers the `library://` provider (it answers "not open" until the library is).
    pub fn new(router: &Arc<Router>, tx: Sender<Msg>, ctx: egui::Context) -> Self {
        let slot: Slot = Arc::default();
        router.register(Arc::new(vlib::LibraryProvider::new(
            Arc::new(AppIndex(slot.clone())),
            Arc::downgrade(router),
        )));
        let now = Instant::now();
        Self {
            backend: None,
            lib: None,
            lost: false,
            activity_sent: None,
            spawned: false,
            release_wait: RELEASE_WAIT,
            note: None,
            next_sources: now,
            slot,
            opening: false,
            error: None,
            sources: Vec::new(),
            stats: LibraryStats::default(),
            tags: Vec::new(),
            views: Vec::new(),
            tagged: HashMap::new(),
            jobs: BTreeMap::new(),
            events: None,
            watchers: HashMap::new(),
            starting_index: 0,
            hash_paused: false,
            next_meta: now,
            next_stats: now,
            next_status: now,
            libraries: Vec::new(),
            add: None,
            picker: None,
            dups: None,
            dup_summary: None,
            plan: None,
            clip: None,
            new_name: String::new(),
            pending: Vec::new(),
            policy: Hashing::default(),
            remote_hash_settings: Default::default(),
            media_again: false,
            protection: None,
            protection_revision: 0,
            volumes: Vec::new(),
            badges: HashMap::new(),
            badge_wanted: parking_lot::Mutex::new(HashSet::new()),
            badge_asked: HashSet::new(),
            tx,
            ctx,
        }
    }

    pub fn is_open(&self) -> bool {
        self.backend.is_some()
    }

    /// The daemon this window is attached to.
    pub fn remote(&self) -> Option<&Arc<Remote>> {
        self.backend.as_ref().and_then(LibraryBackend::remote)
    }

    /// Opens library `name` on a worker: attaches to the profile's daemon (`via`), else
    /// opens it here (creating it on first run), sets the router and resumes jobs left by
    /// the last session.
    pub fn open(&mut self, name: &str, router: Arc<Router>, via: Via) {
        if self.opening {
            return;
        }
        self.opening = true;
        self.error = None;
        let (tx, ctx, name) = (self.tx.clone(), self.ctx.clone(), name.to_owned());
        let profile = crate::cli::profile();
        worker::spawn("keel-library-open", move || {
            let msg = match open_library(&name, router, &profile, via, &tx, &ctx) {
                Ok((opened, first_run)) => LibMsg::Opened {
                    result: Ok(opened),
                    first_run,
                },
                Err(e) => LibMsg::Opened {
                    result: Err(format!("{e:#}")),
                    first_run: false,
                },
            };
            worker::send(&tx, &ctx, Msg::Library(msg));
        });
    }

    /// Takes the open library out (watchers stop); for closing or switching. A daemon's
    /// library stays open there.
    fn take(&mut self) -> Option<(Option<Arc<Library>>, Vec<WatchHandle>)> {
        if let Some(r) = self.remote() {
            r.close_events();
        }
        self.backend.take()?;
        let lib = self.lib.take();
        self.lost = false;
        self.spawned = false;
        *self.slot.write() = None;
        self.events = None;
        self.sources.clear();
        self.protection = None;
        self.volumes.clear();
        self.forget_badges();
        self.jobs.clear();
        self.tagged.clear();
        self.tags.clear();
        self.views.clear();
        let handles = std::mem::take(&mut self.watchers)
            .into_values()
            .filter_map(|w| match w {
                Watch::Live(h) => Some(h),
                _ => None,
            })
            .collect();
        Some((lib, handles))
    }

    /// Exit: stops watchers and closes the library, waiting up to `CLOSE_TIMEOUT`; a
    /// daemon keeps running (and its library open).
    pub fn close_now(&mut self) {
        if let Some((lib, handles)) = self.take() {
            drop(handles);
            if lib.is_some_and(|lib| !lib.close(CLOSE_TIMEOUT)) {
                tracing::warn!("library: a job was still busy at exit");
            }
        }
    }

    /// Closes on a worker (switching libraries, turning the library off); lets go of a
    /// daemon.
    fn close_later(&mut self) {
        if let Some((lib, handles)) = self.take() {
            let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
            worker::spawn("keel-library-close", move || {
                drop(handles);
                if let Some(lib) = lib {
                    lib.close(CLOSE_TIMEOUT);
                }
                worker::send(&tx, &ctx, Msg::Library(LibMsg::Closed));
            });
        }
    }

    /// Runs `f` with the library on a worker; its message (if any) comes back. Nothing
    /// runs while the daemon is lost.
    fn spawn(
        &self,
        name: &str,
        f: impl FnOnce(&LibraryBackend) -> Option<LibMsg> + Send + 'static,
    ) -> bool {
        let Some(backend) = self.backend.clone().filter(|_| !self.lost) else {
            return false;
        };
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        worker::spawn(name, move || {
            if let Some(msg) = f(&backend) {
                worker::send(&tx, &ctx, Msg::Library(msg));
            }
        })
    }

    /// Like `spawn`, but an error becomes a toast.
    fn spawn_try(
        &self,
        name: &str,
        what: &'static str,
        f: impl FnOnce(&LibraryBackend) -> anyhow::Result<Option<LibMsg>> + Send + 'static,
    ) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.spawn(name, move |lib| match f(lib) {
            Ok(msg) => msg,
            Err(e) => {
                worker::send(&tx, &ctx, Msg::Toast(format!("{what}: {e:#}")));
                None
            }
        });
    }

    pub fn refresh_meta(&mut self) {
        self.next_meta = Instant::now() + META_EVERY;
        self.spawn("keel-library-meta", |b| match b.meta() {
            Ok((tags, views, tagged)) => Some(LibMsg::Meta {
                tags,
                views,
                tagged,
            }),
            Err(e) => {
                tracing::warn!("library tags: {e:#}");
                None
            }
        });
    }

    fn refresh_stats(&mut self) {
        self.next_stats = Instant::now() + STATS_EVERY;
        self.spawn("keel-library-stats", |b| match b.stats() {
            Ok(stats) => Some(LibMsg::Stats(stats)),
            Err(e) => {
                tracing::warn!("library stats: {e:#}");
                None
            }
        });
        // --- Task 33 ---
        self.spawn("keel-library-protection", |b| match b.protection() {
            Ok((p, v)) => Some(LibMsg::Protection(p, v)),
            Err(e) => {
                tracing::warn!("protection: {e:#}");
                None
            }
        });
    }

    pub fn refresh_dups(&mut self) {
        self.spawn("keel-library-dups", |b| {
            Some(LibMsg::Dups(b.dups(1).map_err(|e| format!("{e:#}"))))
        });
    }

    /// The daemon's sources, on a worker.
    fn refresh_sources(&mut self) {
        self.next_sources = Instant::now() + SOURCES_EVERY;
        self.spawn("keel-library-sources", |b| match b.sources() {
            Ok(s) => Some(LibMsg::Sources(s)),
            Err(e) => {
                tracing::warn!("library sources: {e:#}");
                None
            }
        });
    }

    fn refresh_libraries(&self) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        worker::spawn("keel-library-list", move || {
            worker::send(&tx, &ctx, Msg::Library(LibMsg::Libraries(Library::list())));
        });
    }

    /// Task 36: sends `paths` to `peer` as a Spacedrop job (a row in the jobs panel).
    pub fn send_drop(&self, node: Arc<keel_net::Node>, peer: keel_net::PeerId, paths: Vec<VPath>) {
        let Some(lib) = self.lib.clone() else { return };
        self.spawn("keel-drop-send", move |_| {
            let id = keel_net::spacedrop::send(&node, &lib, peer, paths)
                .map_err(|e| tracing::warn!("spacedrop: {e:#}"))
                .ok();
            Some(LibMsg::Spawned { kind: "drop", id })
        });
    }

    /// Settings → Library → Hashing.
    pub fn apply_hashing(&mut self, settings: &LibrarySettings) {
        let policy = settings.hashing;
        let remote = settings.remote_hash_settings();
        if self.backend.is_none() || (self.policy == policy && self.remote_hash_settings == remote)
        {
            return;
        }
        self.policy = policy;
        // The remote policy is the library's: sent only when the user changed it here.
        let changed = self.remote_hash_settings != remote;
        self.remote_hash_settings = remote;
        self.sync_hashing(changed.then_some(remote));
    }

    /// Off (by policy or paused by hand) cancels a running hash job and keeps walks from
    /// starting one; on starts one. A daemon gets the policy through `hashing.set`.
    /// `remote`: a new remote policy for the library (None keeps the library's own).
    pub(crate) fn sync_hashing(&mut self, remote: Option<keel_core::RemoteHashSettings>) {
        let on = self.policy != Hashing::Off && !self.hash_paused;
        let idle_only = self.policy == Hashing::IdleOnly;
        if self.remote().is_some() {
            self.spawn("keel-library-hashing", move |b| {
                let id = (b.set_hashing(on, idle_only, remote))
                    .map_err(|e| tracing::warn!("hashing: {e:#}"))
                    .ok()
                    .flatten();
                id.map(|id| LibMsg::Spawned {
                    kind: "hash",
                    id: Some(id),
                })
            });
            return;
        }
        if let Some(lib) = &self.lib {
            if let Some(Err(e)) = remote.map(|r| lib.set_remote_hash_settings(r)) {
                tracing::warn!("remote hashing settings: {e:#}");
                return;
            }
            lib.set_hash_after_walk(on);
            lib.set_hash_idle_only(idle_only);
        }
        if on {
            self.start_hashing();
        } else {
            let running: Vec<JobId> = (self.jobs.iter())
                .filter(|(_, j)| j.kind == "hash" && j.active())
                .map(|(id, _)| *id)
                .collect();
            for id in running {
                self.spawn("keel-library-cancel", move |b| {
                    let _ = b.cancel(id);
                    None
                });
            }
        }
    }

    /// Starts hashing unless it is off, paused or already running.
    fn start_hashing(&mut self) {
        if self.policy == Hashing::Off
            || self.hash_paused
            || self.jobs.values().any(|j| j.kind == "hash" && j.active())
        {
            return;
        }
        let idle_only = self.policy == Hashing::IdleOnly;
        self.spawn("keel-library-hash", move |b| {
            Some(LibMsg::Spawned {
                kind: "hash",
                id: b.hash(idle_only).ok(),
            })
        });
    }

    /// Task 32: thumbnails and metadata for every source's photos and videos (the sidecar
    /// job, idle priority); while one runs, again once it has ended.
    fn start_media(&mut self) {
        if self
            .jobs
            .values()
            .any(|j| j.kind == "sidecar" && j.active())
        {
            self.media_again = true;
            return;
        }
        self.media_again = false;
        let ids: Vec<SourceId> = (self.sources.iter())
            .filter(|s| s.root.scheme == "file")
            .map(|s| s.id.clone())
            .collect();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.spawn("keel-library-media", move |b| {
            for id in ids {
                let id = b.media_job(&id).ok();
                let msg = Msg::Library(LibMsg::Spawned {
                    kind: "sidecar",
                    id,
                });
                worker::send(&tx, &ctx, msg);
            }
            None
        });
    }

    /// A watcher per source once no index job runs (both would walk the source). In this
    /// process only: keel-daemon watches its sources itself.
    fn sync_watchers(&mut self, rescan: Duration) {
        let Some(lib) = self.lib.clone() else { return };
        let indexing =
            self.starting_index > 0 || self.jobs.values().any(|j| j.kind == "index" && j.active());
        let ids: Vec<SourceId> = self.sources.iter().map(|s| s.id.clone()).collect();
        // Removed sources: stop their watchers (dropping one waits for its walk to stop).
        let gone: Vec<SourceId> = (self.watchers.keys())
            .filter(|id| !ids.contains(id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(Watch::Live(h)) = self.watchers.remove(&id) {
                worker::spawn("keel-unwatch", move || drop(h));
            }
        }
        if indexing {
            return;
        }
        for id in ids {
            let retry = match self.watchers.get(&id) {
                None => true,
                Some(Watch::Failed(at)) => at.elapsed() >= WATCH_RETRY,
                Some(_) => false,
            };
            if !retry {
                continue;
            }
            self.watchers.insert(id.clone(), Watch::Starting);
            let (tx, ctx, lib) = (self.tx.clone(), self.ctx.clone(), lib.clone());
            worker::spawn("keel-library-watch", move || {
                let cfg = WatchConfig {
                    poll: rescan,
                    ..WatchConfig::default()
                };
                let handle = lib.source(&id).and_then(|src| {
                    Indexer::watch_with(&src, &lib.router(), cfg)
                        .map_err(|e| tracing::warn!("watch {}: {e:#}", src.def.label))
                        .ok()
                });
                worker::send(&tx, &ctx, Msg::Library(LibMsg::Watching(id, handle)));
            });
        }
    }
}

/// The `library://` provider's view of the library: listings from the index (through
/// `DaemonProvider` when attached), reads from the real files (the daemon runs on this
/// machine).
struct AppIndex(Slot);

impl AppIndex {
    fn backend(&self) -> anyhow::Result<LibraryBackend> {
        self.0.read().clone().context("the library is not open")
    }
}

impl vlib::LibraryIndex for AppIndex {
    fn children(&self, source: &str, rel: &str) -> anyhow::Result<Vec<Entry>> {
        self.backend()?.children(source, rel)
    }

    fn resolve(&self, source: &str, rel: &str) -> anyhow::Result<VPath> {
        self.backend()?.resolve(source, rel)
    }
}

/// Where `open` finds the library (worker only): the daemon when one runs (or is started
/// for `via`), else this process. Returns it with whether it was created now.
fn open_library(
    name: &str,
    router: Arc<Router>,
    profile: &str,
    via: Via,
    tx: &Sender<Msg>,
    ctx: &egui::Context,
) -> anyhow::Result<(Opened, bool)> {
    let cfg = crate::backend::host_config(profile)?;
    let socket = cfg.socket_name();
    let mut connect = || Remote::connect(&socket).ok();
    let lock_dir = cfg.data_dir.join("library").join(name);
    let release = match via {
        Via::Replace { wait, .. } => wait,
        _ => Duration::ZERO,
    };
    // A stopping daemon closes the library after it said so: a new one waits for that.
    let mut spawn = || -> anyhow::Result<crate::backend::Exited> {
        released(&lock_dir, release)?;
        let mut started = crate::backend::spawn_daemon(&cfg)?;
        Ok(Box::new(move || started.exited()))
    };
    let attach = match via {
        Via::Replace { here: true, wait } => {
            released(&lock_dir, wait)?;
            crate::backend::attach(None, None, crate::backend::SPAWN_WAIT)
        }
        Via::Auto { spawn: wanted } => crate::backend::attach(
            Some(&mut connect),
            wanted.then_some(&mut spawn as &mut dyn FnMut() -> _),
            crate::backend::SPAWN_WAIT,
        ),
        Via::Replace { here: false, .. } => crate::backend::attach(
            Some(&mut connect),
            Some(&mut spawn as &mut dyn FnMut() -> _),
            crate::backend::SPAWN_WAIT,
        ),
    };
    let note = match attach {
        Attach::Daemon { remote, spawned } => {
            let backend = LibraryBackend::Daemon(remote.clone());
            let (tx, ctx, id) = (tx.clone(), ctx.clone(), remote.id);
            let events = remote.subscribe(move |ev| match ev {
                Event::Jobs => ctx.request_repaint(),
                ev => worker::send(&tx, &ctx, Msg::Library(LibMsg::Remote(id, ev))),
            })?;
            remote.sources()?;
            let jobs = backend.jobs()?.into_iter().filter(|(_, j)| j.active());
            let opened = Opened {
                jobs: jobs.collect(),
                backend,
                events: Some(events),
                spawned,
                note: None,
            };
            return Ok((opened, false));
        }
        Attach::InProcess { note } => note,
    };
    let root = crate::settings::data_dir().context("no data folder")?;
    let first_run = !root.join("library").join(name).exists();
    // Owner-only when created here (else it inherits the drive's permissions).
    keel_api::private::create_dir_all(&root)?;
    let lib = Library::open(&root, name)?;
    lib.set_router(router);
    keel_net::spacedrop::register(&lib); // Task 36
    lib.set_utc_offset(chrono::Local::now().offset().local_minus_utc().into());
    lib.jobs().resume_all()?;
    let lib = Arc::new(lib);
    let events = lib.jobs().subscribe();
    let backend = LibraryBackend::InProcess(lib);
    let jobs = backend.jobs()?.into_iter().filter(|(_, j)| j.active());
    let opened = Opened {
        jobs: jobs.collect(),
        backend,
        events: Some(events),
        spawned: false,
        note,
    };
    Ok((opened, first_run))
}

/// Waits up to `wait` until nobody holds `library.lock` in `dir` (a stopping daemon still
/// closes the library after it said `daemon.stopping`).
pub(crate) fn released(dir: &std::path::Path, wait: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("library.lock"));
        // No lock file: no library to hold. Dropping the file lets go of the lock again.
        if file.is_err() || file.is_ok_and(|f| f.try_lock().is_ok()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the stopping keel-daemon still holds the library: try again in a moment"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn entry_of(h: LibraryHit) -> Entry {
    let modified = h
        .modified
        .filter(|t| *t >= 0)
        .map(|t| std::time::UNIX_EPOCH + Duration::from_secs(t as u64));
    Entry {
        hidden: h.name.starts_with('.'),
        path: h.path,
        name: h.name,
        kind: if h.is_dir { Kind::Dir } else { Kind::File },
        size: h.size,
        modified,
        is_link: false,
        encrypted: false,
        ext: String::new(),
    }
}

/// The search tab's Library backend: `LibrarySearcher` (or the daemon's `search`), plus
/// the sidebar's Favorites and Recents queries.
pub struct LibSearch(pub LibraryBackend);

impl Searcher for LibSearch {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let lib = match &self.0 {
            LibraryBackend::InProcess(lib) => lib,
            daemon => return daemon.search(q),
        };
        let hits = match q.text.trim() {
            FAVORITES_QUERY => lib.favorites()?,
            RECENTS_QUERY => lib.recents(q.max as usize)?,
            _ => return keel_core::LibrarySearcher(lib.clone()).query(q),
        };
        Ok(hits
            .into_iter()
            .map(|h| Hit {
                path: h.path,
                is_dir: h.is_dir,
                size: h.size,
                modified: entry_of_time(h.modified),
            })
            .collect())
    }

    fn available(&self) -> bool {
        true
    }

    fn status(&self) -> Option<String> {
        match &self.0 {
            LibraryBackend::InProcess(lib) => keel_core::LibrarySearcher(lib.clone()).status(),
            LibraryBackend::Daemon(_) => None,
        }
    }

    fn name(&self) -> &'static str {
        "Library"
    }
}

pub(crate) fn entry_of_time(t: Option<i64>) -> Option<std::time::SystemTime> {
    t.filter(|t| *t >= 0)
        .map(|t| std::time::UNIX_EPOCH + Duration::from_secs(t as u64))
}

impl AppState {
    /// The library side of `drain`.
    pub fn library_msg(&mut self, msg: LibMsg) {
        let l = &mut self.library;
        match msg {
            LibMsg::Opened { result, first_run } => {
                l.opening = false;
                match result {
                    Ok(opened) => {
                        // Reconnect / Open in this window: the lost connection goes now.
                        drop(l.take());
                        if let LibraryBackend::Daemon(r) = &opened.backend {
                            tracing::info!(
                                "library: attached to keel-daemon (pid {})",
                                r.version.pid
                            );
                            l.set_sources(r.cached_sources());
                        }
                        if let Some(note) = &opened.note {
                            self.toasts
                                .error(format!("Library: {note}; opened in this window"));
                        }
                        *l.slot.write() = Some(opened.backend.clone());
                        l.events = opened.events;
                        l.jobs = opened.jobs.into_iter().collect();
                        l.lib = match &opened.backend {
                            LibraryBackend::InProcess(lib) => Some(lib.clone()),
                            LibraryBackend::Daemon(_) => None,
                        };
                        l.backend = Some(opened.backend);
                        l.spawned = opened.spawned;
                        l.note = opened.note;
                        l.lost = false;
                        l.sync_sources();
                        l.refresh_meta();
                        l.refresh_stats();
                        l.refresh_dups();
                        l.refresh_libraries();
                        self.library.policy = self.settings.library.hashing;
                        // The library's remote policy wins over settings.toml (a client's
                        // `hashing.set` may have changed it): read here, never pushed.
                        self.library.remote_hash_settings =
                            self.settings.library.remote_hash_settings();
                        self.library.sync_hashing(None);
                        self.library.spawn("keel-library-hashing-policy", |b| {
                            match b.remote_hashing() {
                                Ok(Some(r)) => Some(LibMsg::RemoteHashing(r)),
                                Ok(None) => None,
                                Err(e) => {
                                    tracing::warn!("remote hashing policy: {e:#}");
                                    None
                                }
                            }
                        });
                        self.library.start_media(); // Task 32
                        if first_run {
                            self.toasts.offer(
                                "Keel can index your files into a library, even offline",
                                "Add your Documents folder as a source",
                                Action::Library(LibCmd::AddDocuments),
                            );
                        }
                        self.relist_library_tabs();
                    }
                    Err(e) => {
                        self.toasts.error(format!("Library: {e}"));
                        l.error = Some(e);
                    }
                }
            }
            LibMsg::Sources(sources) => l.set_sources(sources),
            LibMsg::Remote(id, ev) => {
                if l.remote().is_none_or(|r| r.id != id) {
                    return; // an earlier connection
                }
                match ev {
                    Event::Lost => {
                        if !l.lost {
                            l.lost = true;
                            self.toasts.error(
                                "keel-daemon stopped: the library is read-only until you \
                                 reconnect or open it in this window",
                            );
                        }
                    }
                    Event::Changed(kind) => match refresh_for(&kind) {
                        Refresh::Nothing => {}
                        Refresh::Protection => l.protection_changed(),
                        Refresh::Recents => self.relist_library_tabs(),
                        Refresh::All => {
                            l.refresh_sources();
                            l.refresh_meta_soon();
                            l.forget_badges();
                            self.relist_library_tabs();
                        }
                    },
                    Event::Net(v) => self.devices_event(v),
                    Event::Jobs => {}
                }
            }
            LibMsg::Stats(stats) => l.stats = stats,
            LibMsg::RemoteHashing(r) => {
                // Adopted on both sides: `apply_hashing` sees no change to send back.
                l.remote_hash_settings = r;
                let s = &mut self.settings.library;
                (s.hash_remote, s.hash_cloud, s.remote_hash_max_bytes) =
                    (r.hash_remote, r.hash_cloud, r.remote_hash_max_bytes);
            }
            LibMsg::Meta {
                tags,
                views,
                tagged,
            } => {
                l.tags = tags;
                l.views = views;
                l.tagged = tagged;
            }
            LibMsg::Watching(id, handle) => {
                let known = l.sources.iter().any(|s| s.id == id) && l.lib.is_some();
                match handle {
                    Some(h) if known => {
                        l.watchers.insert(id, Watch::Live(h));
                    }
                    Some(h) => {
                        worker::spawn("keel-unwatch", move || drop(h));
                    }
                    None => {
                        l.watchers.insert(id, Watch::Failed(Instant::now()));
                    }
                }
            }
            LibMsg::Spawned { kind, id } => {
                if kind == "index" {
                    l.starting_index = l.starting_index.saturating_sub(1);
                }
                match id {
                    Some(id) => {
                        l.jobs.entry(id).or_insert(JobRow {
                            kind: kind.into(),
                            status: JobStatus::Running,
                            progress: 0.0,
                            ended: None,
                        });
                    }
                    None if kind != "hash" && kind != "sidecar" => {
                        self.toasts.error(format!("Could not start {kind}"))
                    }
                    None => {}
                }
            }
            LibMsg::Planned(Ok(plan)) => {
                l.plan = Some(PlanDialog {
                    plan,
                    changed: false,
                    running: false,
                })
            }
            LibMsg::Planned(Err(e)) => self.toasts.error(e),
            LibMsg::Changed(plan) => {
                l.plan = Some(PlanDialog {
                    plan,
                    changed: true,
                    running: false,
                })
            }
            LibMsg::Dups(result) => {
                if let Ok(groups) = &result {
                    l.dup_summary = Some((groups.len(), reclaimable(groups)));
                }
                if let Some(d) = &mut l.dups {
                    d.groups = Some(result);
                }
            }
            LibMsg::Libraries(list) => l.libraries = list,
            LibMsg::Picked(picked) => {
                if let Some(add) = &mut l.add {
                    add.picking = false;
                    if let Some(root) = picked {
                        add.label = label_for(&root);
                        add.root = root;
                    }
                }
            }
            LibMsg::Kinds(kinds) => {
                for (id, kind) in kinds {
                    if let Some(row) = l.jobs.get_mut(&id).filter(|r| r.kind.is_empty()) {
                        row.kind = kind;
                    }
                }
            }
            LibMsg::Protection(p, v) => {
                l.protection = Some(p);
                l.volumes = v;
            }
            LibMsg::Badges(b) => l.badges.extend(b),
            LibMsg::Closed => {
                if self.settings.library.enabled && !l.is_open() {
                    let name = self.settings.library.name.clone();
                    let spawn = self.settings.library.daemon;
                    l.open(&name, self.router.clone(), Via::Auto { spawn });
                }
            }
        }
    }

    /// Per frame: job events, source status, periodic refreshes, watchers, hashing policy.
    pub fn library_tick(&mut self) {
        let busy_input = self
            .ctx
            .input(|i| !i.events.is_empty() || i.pointer.is_moving());
        let rescan = Duration::from_secs(self.settings.library.rescan_minutes.max(1) * 60);
        let l = &mut self.library;
        if l.backend.is_none() || l.lost {
            return;
        }
        let lib = l.lib.clone();
        l.follow_recounts();
        let now = Instant::now();
        // Media and integrity jobs pause for 5 s after each input; hashing too when idle
        // only (`sync_hashing`). Attached, the daemon is told (`activity.note`) at most
        // every `ACTIVITY_EVERY`.
        match (lib.as_ref(), l.remote().cloned()) {
            (Some(lib), _) if busy_input => lib.note_activity(),
            (None, Some(remote))
                if busy_input
                    && l.activity_sent
                        .is_none_or(|t| t.elapsed() >= ACTIVITY_EVERY) =>
            {
                l.activity_sent = Some(now);
                worker::spawn("keel-daemon-activity", move || {
                    if let Err(e) =
                        remote.call::<keel_api::types::Done>("activity.note", serde_json::json!({}))
                    {
                        tracing::debug!("activity.note: {e:#}");
                    }
                });
            }
            _ => {}
        }
        let mut ended: Vec<String> = Vec::new();
        let mut unknown = false;
        if let Some(rx) = &l.events {
            while let Ok(ev) = rx.try_recv() {
                let row = l.jobs.entry(ev.id).or_insert_with(|| {
                    unknown = true;
                    JobRow {
                        kind: String::new(),
                        status: ev.status,
                        progress: ev.progress,
                        ended: None,
                    }
                });
                row.status = ev.status;
                row.progress = ev.progress;
                if !row.active() && row.ended.is_none() {
                    row.ended = Some(now);
                    ended.push(row.kind.clone());
                    if ev.status == JobStatus::Failed {
                        self.toasts.error(format!("{} failed", row.title()));
                    }
                }
            }
        }
        if unknown {
            // Kinds of jobs this app did not start.
            l.spawn("keel-library-jobs", |b| {
                Some(LibMsg::Kinds(b.job_kinds().ok()?))
            });
        }
        l.jobs.retain(|_, j| {
            j.status == JobStatus::Failed || j.ended.is_none_or(|t| t.elapsed() < ROW_KEPT)
        });
        if l.jobs
            .values()
            .any(|j| j.ended.is_some() && j.status != JobStatus::Failed)
        {
            self.ctx.request_repaint_after(ROW_KEPT);
        }
        l.sync_sources();
        if l.remote().is_some() && (now >= l.next_sources || !ended.is_empty()) {
            l.refresh_sources();
            let busy = l.jobs.values().any(JobRow::active)
                || (l.sources.iter()).any(|s| matches!(s.status, SourceStatus::Indexing { .. }));
            if busy {
                l.next_sources = now + SOURCES_BUSY;
            }
        }
        if !ended.is_empty() {
            l.refresh_meta();
            l.refresh_stats();
            if ended.iter().any(|k| k == "index") {
                l.start_hashing();
                l.start_media(); // Task 32
            } else if l.media_again && ended.iter().any(|k| k == "sidecar") {
                l.start_media();
            }
            // The Overview's duplicate summary (and an open finder) follow new content ids.
            if ended.iter().any(|k| k == "hash") {
                l.refresh_dups();
            }
            // --- Task 33 ---: copies badges follow content ids, volumes and drift.
            if ended
                .iter()
                .any(|k| matches!(k.as_str(), "hash" | "integrity" | "index"))
            {
                l.forget_badges();
            }
            self.relist_library_tabs();
        }
        let l = &mut self.library;
        if now >= l.next_meta {
            l.refresh_meta();
        }
        let overview = self
            .panes
            .iter()
            .any(|p| matches!(p.tabs[p.active].kind, TabKind::Overview));
        if now >= l.next_stats && (overview || now >= l.next_meta) {
            l.refresh_stats();
        }
        if now >= l.next_status {
            l.next_status = now + STATUS_EVERY;
            // keel-daemon checks its sources itself.
            if let Some(lib) = &lib {
                drop(lib.refresh_status());
            }
            // --- Task 33 --- (keel-daemon keeps its own schedule, from the same settings)
            let (pct, days) = (
                self.settings.library.integrity_pct,
                self.settings.library.integrity_days,
            );
            let due = days > 0 && lib.is_some();
            if due && !l.jobs.values().any(|j| j.kind == "integrity" && j.active()) {
                l.spawn("keel-library-integrity", move |b| {
                    let id = b.schedule_integrity(pct, days).ok().flatten()?;
                    Some(LibMsg::Spawned {
                        kind: "integrity",
                        id: Some(id),
                    })
                });
            }
        }
        l.ask_badges();
        l.sync_watchers(rescan);
        let indexing = l
            .sources
            .iter()
            .any(|s| matches!(s.status, SourceStatus::Indexing { .. }));
        if indexing || l.jobs.values().any(JobRow::active) {
            self.ctx.request_repaint_after(Duration::from_millis(500));
        } else {
            self.ctx.request_repaint_after(STATS_EVERY);
        }
    }

    /// Indexes a source now; `adopt`: first accept whatever is at its root.
    fn index_now(&mut self, id: SourceId, adopt: bool) {
        // Its watcher stops first (both would walk the source).
        let watch = self.library.watchers.remove(&id);
        self.library.starting_index += 1;
        self.library.spawn("keel-library-index", move |b| {
            drop(watch);
            let started = b.index(&id, adopt);
            if let Err(e) = &started {
                tracing::warn!("index {}: {e:#}", id.0);
            }
            Some(LibMsg::Spawned {
                kind: "index",
                id: started.ok(),
            })
        });
    }

    /// Relists every tab on a `library://` folder (its index changed).
    pub fn relist_library_tabs(&mut self) {
        for p in 0..2 {
            for t in 0..self.panes[p].tabs.len() {
                if self.panes[p].tabs[t].dir.scheme == vlib::SCHEME {
                    self.list(p, t);
                }
            }
        }
    }

    /// Real paths of the targets, or a toast when one is outside every source.
    fn library_targets(&mut self, p: usize) -> Option<Vec<VPath>> {
        let paths: Vec<VPath> = self
            .tab(p)
            .targets()
            .iter()
            .map(|e| e.path.clone())
            .collect();
        if paths.is_empty() {
            return None;
        }
        let sources = &self.library.sources;
        match paths
            .iter()
            .map(|x| real_of(sources, x))
            .collect::<Option<Vec<_>>>()
        {
            Some(real) => Some(real),
            None => {
                self.toasts.error("That source is no longer in the library");
                None
            }
        }
    }

    fn plan(&mut self, op: Op) {
        if self.library.plan.is_some() {
            return self.toasts.error("Another preview is open");
        }
        self.library
            .spawn_try("keel-library-plan", "Preview", move |b| {
                Ok(Some(LibMsg::Planned(
                    b.plan(op).map_err(|e| format!("{e:#}")),
                )))
            });
    }

    /// Library-aware handling of an action: None when handled here.
    pub fn library_intercept(&mut self, p: usize, action: Action) -> Option<Action> {
        if let Action::Library(cmd) = action {
            self.library_cmd(p, cmd);
            return None;
        }
        if !self.library.is_open() {
            return Some(action);
        }
        let in_library = self.tab(p).dir.scheme == vlib::SCHEME;
        let targets: Vec<VPath> = (self.tab(p).targets().iter())
            .map(|e| e.path.clone())
            .collect();
        let here = self.tab(p).dir.clone();
        let sources = &self.library.sources;
        let all_in_sources = |paths: &[VPath]| {
            !paths.is_empty() && paths.iter().all(|x| locate(sources, x).is_some())
        };
        // The daemon is gone: nothing in the library is written until the user chooses.
        if self.library.lost {
            let touches = in_library
                || match &action {
                    Action::Delete | Action::Cut => all_in_sources(&targets),
                    Action::RenameTo { from, .. } => locate(sources, from).is_some(),
                    Action::Drop { paths, dst, .. } => {
                        all_in_sources(paths) || locate(sources, dst).is_some()
                    }
                    Action::Paste => self.library.clip.is_some(),
                    _ => false,
                };
            let writes = matches!(
                action,
                Action::Delete
                    | Action::RenameTo { .. }
                    | Action::Drop { .. }
                    | Action::Paste
                    | Action::Cut
                    | Action::NewFolder
                    | Action::NewFile
            );
            if touches && writes || (writes && locate(sources, &here).is_some()) {
                self.toasts.error(LOST);
                return None;
            }
        }
        match action {
            Action::Delete => {
                let paths: Vec<VPath> = self
                    .tab(p)
                    .targets()
                    .iter()
                    .map(|e| e.path.clone())
                    .collect();
                if !(in_library || all_in_sources(&paths)) {
                    return Some(Action::Delete);
                }
                let paths = self.library_targets(p)?;
                self.plan(Op::Delete { paths });
                None
            }
            Action::RenameTo { from, to } if from.scheme == vlib::SCHEME => {
                if let Some(path) = real_of(sources, &from) {
                    self.plan(Op::Rename { path, new_name: to });
                }
                None
            }
            Action::Copy | Action::Cut if in_library => {
                let cut = action == Action::Cut;
                let paths = self.library_targets(p)?;
                let n = paths.len();
                self.clipboard.set_paths(paths.clone(), cut);
                self.library.clip = Some((paths, cut));
                let verb = if cut { "Cut" } else { "Copied" };
                self.toasts
                    .info(format!("{verb} {}", crate::jobs::items(n)));
                None
            }
            Action::Paste | Action::NewFolder | Action::NewFile if in_library => {
                self.toasts
                    .error("Library views are read-only: open the real folder to add files");
                None
            }
            Action::Paste
                if self.library.clip.is_some()
                    && !self.clipboard.changed_outside()
                    && !self.tab(p).is_search() =>
            {
                let (src, cut) = self.library.clip.clone()?;
                if cut {
                    self.library.clip = None;
                }
                let dst_dir = self.tab(p).dir.clone();
                let on_conflict = OnConflict::Skip;
                self.plan(if cut {
                    Op::Move {
                        src,
                        dst_dir,
                        on_conflict,
                    }
                } else {
                    Op::Copy {
                        src,
                        dst_dir,
                        on_conflict,
                    }
                });
                None
            }
            Action::Drop { paths, from, dst } => {
                if dst.scheme == vlib::SCHEME {
                    self.toasts.error("Drop onto a real folder");
                    return None;
                }
                let from_library = paths.iter().any(|x| x.scheme == vlib::SCHEME);
                if !(from_library || all_in_sources(&paths))
                    || from.as_ref().is_some_and(|(_, dir)| *dir == dst)
                {
                    return Some(Action::Drop { paths, from, dst });
                }
                let src = paths
                    .iter()
                    .map(|x| real_of(sources, x))
                    .collect::<Option<Vec<_>>>()?;
                let shift = self.ctx.input(|i| i.modifiers.shift);
                let mv = crate::pane::drop_moves(from.as_ref().map(|(pane, _)| *pane), p, shift);
                let on_conflict = OnConflict::Skip;
                self.plan(if mv {
                    Op::Move {
                        src,
                        dst_dir: dst,
                        on_conflict,
                    }
                } else {
                    Op::Copy {
                        src,
                        dst_dir: dst,
                        on_conflict,
                    }
                });
                None
            }
            Action::Enter => {
                // Opened files count for Recents.
                let target = self.tab(p).targets().first().map(|e| (*e).clone());
                if let Some(e) = target.filter(|e| e.kind != Kind::Dir) {
                    if locate(sources, &e.path).is_some() {
                        let sources = sources.clone();
                        self.library.spawn("keel-library-opened", move |b| {
                            let _ = b.note_open(&sources, &e.path);
                            None
                        });
                    }
                }
                Some(Action::Enter)
            }
            Action::OpenLocation if self.tab(p).library_search => {
                // A library hit opens its library folder (browsable offline too).
                let e = self.tab(p).targets().first().map(|e| (*e).clone())?;
                let (source, rel) = locate(sources, &e.path)?;
                let (parent, name) = rel.rsplit_once('/').unwrap_or(("", &rel));
                let dir = vlib::path(&source.0, parent);
                let q = if self.dual { 1 - p } else { p };
                let new_tab = !self.dual || self.tab(q).is_search();
                let name = name.to_owned();
                self.reveal_in(q, dir, name, new_tab);
                None
            }
            other => Some(other),
        }
    }

    pub fn library_cmd(&mut self, p: usize, cmd: LibCmd) {
        let always = matches!(
            cmd,
            LibCmd::Switch(_)
                | LibCmd::Enable(_)
                | LibCmd::Overview
                | LibCmd::Reconnect
                | LibCmd::OpenHere
                | LibCmd::Restart
        );
        if !self.library.is_open() && !always {
            return self.toasts.error(if self.library.opening {
                "The library is still opening"
            } else {
                "The library is off (Settings → Library)"
            });
        }
        if self.library.lost && !always && !matches!(cmd, LibCmd::OpenSource(_) | LibCmd::Query(_))
        {
            return self.toasts.error(LOST);
        }
        match cmd {
            LibCmd::Overview => {
                let pane = &mut self.panes[p];
                match pane.tabs.iter().position(|t| t.kind == TabKind::Overview) {
                    Some(t) => pane.active = t,
                    None => {
                        pane.tabs.push(crate::tab::Tab::new(overview_path()));
                        pane.active = pane.tabs.len() - 1;
                    }
                }
                self.library.refresh_stats();
                self.library.refresh_dups();
            }
            LibCmd::OpenSource(id) => self.run(p, Action::Navigate(vlib::path(&id.0, ""))),
            LibCmd::IndexNow(id) => self.index_now(id, false),
            LibCmd::AdoptRoot(id) => self.index_now(id, true),
            LibCmd::RemoveSource(id) => {
                let label = label_of(&id.0).unwrap_or_default();
                self.dialog = Some(crate::dialogs::Dialog::Confirm {
                    text: format!(
                        "Remove {label} from the library? Its index (tags included) is deleted; \
                         the files themselves are not touched."
                    ),
                    on_yes: Action::Library(LibCmd::RemoveConfirmed(id)),
                });
            }
            LibCmd::RemoveConfirmed(id) => {
                let watch = self.library.watchers.remove(&id);
                self.library
                    .spawn_try("keel-library-remove", "Remove source", move |b| {
                        drop(watch);
                        b.remove_source(&id)?;
                        Ok(Some(LibMsg::Sources(b.sources()?)))
                    });
            }
            LibCmd::PauseHashing(on) => {
                self.library.hash_paused = on;
                self.library.sync_hashing(None);
            }
            LibCmd::AddSource => {
                let root = self
                    .tab(p)
                    .dir
                    .to_local_path()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default();
                self.library.add = Some(AddSource {
                    label: label_for(&root),
                    root,
                    include_hidden: false,
                    ignore: String::new(),
                    picking: false,
                });
            }
            LibCmd::AddDocuments => {
                let Some(docs) =
                    directories::UserDirs::new().and_then(|u| u.document_dir().map(VPath::local))
                else {
                    return self.toasts.error("No Documents folder found");
                };
                self.library_cmd(
                    p,
                    LibCmd::Register(SourceDef {
                        label: "Documents".into(),
                        root: docs,
                        kind: SourceKind::Folder,
                        include_hidden: false,
                        ignore: Vec::new(),
                        poll_secs: None,
                        hash_shares: false,
                    }),
                );
            }
            LibCmd::Register(def) => {
                self.library.add = None;
                self.library.starting_index += 1;
                let label = def.label.clone();
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                self.library.spawn("keel-library-add", move |b| {
                    let id = match b.add_source(def) {
                        Ok(id) => {
                            if let (true, Ok(s)) = (b.is_daemon(), b.sources()) {
                                worker::send(&tx, &ctx, Msg::Library(LibMsg::Sources(s)));
                            }
                            Some(id)
                        }
                        Err(e) => {
                            let text = format!("Add {label}: {e:#}");
                            worker::send(&tx, &ctx, Msg::Toast(text));
                            None
                        }
                    };
                    Some(LibMsg::Spawned { kind: "index", id })
                });
            }
            LibCmd::Query(text) => {
                let mut tab = crate::tab::Tab::search(self.tab(p).dir.clone());
                tab.library_search = true;
                if let TabKind::Search { query, due, .. } = &mut tab.kind {
                    *query = text;
                    *due = Some(Instant::now());
                }
                let pane = &mut self.panes[p];
                pane.tabs.push(tab);
                pane.active = pane.tabs.len() - 1;
                self.ctx.request_repaint();
            }
            LibCmd::TagPicker => {
                if let Some(targets) = self.library_targets(p) {
                    self.library.picker = Some(TagPicker {
                        targets,
                        new_name: String::new(),
                        new_color: [0x3b, 0x82, 0xf6],
                    });
                }
            }
            LibCmd::ToggleFavorite => {
                let Some(targets) = self.library_targets(p) else {
                    return;
                };
                let on = !targets.iter().all(|t| {
                    self.library
                        .tagged
                        .get(t)
                        .is_some_and(|tags| tags.contains(&FAVORITES))
                });
                self.set_tag(targets, FAVORITES, on);
            }
            LibCmd::SetTag { tag, on } => {
                let targets = match &self.library.picker {
                    Some(picker) => picker.targets.clone(),
                    None => match self.library_targets(p) {
                        Some(t) => t,
                        None => return,
                    },
                };
                self.set_tag(targets, tag, on);
            }
            LibCmd::TagPaths { paths, tag, on } => self.set_tag(paths, tag, on),
            LibCmd::CreateTag { name, color } => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let targets = self.library.picker.as_ref().map(|x| x.targets.clone());
                let sources = self.library.sources.clone();
                self.library
                    .spawn_try("keel-library-tag", "New tag", move |b| {
                        b.create_tag(&sources, &name, &color, targets.as_deref())?;
                        Ok(None)
                    });
                self.library.refresh_meta_soon();
            }
            LibCmd::Duplicates => {
                self.library.dups = Some(DupFinder { groups: None });
                self.library.refresh_dups();
            }
            LibCmd::KeepOne { group, keep } => {
                let op = (self.library.dups.as_ref())
                    .and_then(|d| d.groups.as_ref()?.as_ref().ok()?.get(group))
                    .map(|g| keep_one(g, keep));
                if let Some(op) = op {
                    self.plan(op);
                }
            }
            LibCmd::Execute => {
                let Some(dialog) = &mut self.library.plan else {
                    return;
                };
                if dialog.running {
                    return;
                }
                dialog.running = true;
                let plan = dialog.plan.clone();
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                self.library
                    .spawn("keel-library-execute", move |b| match b.execute(plan) {
                        Ok(Executed::Job(id)) => Some(LibMsg::Spawned {
                            kind: "op",
                            id: Some(id),
                        }),
                        Ok(Executed::Changed(fresh)) => Some(LibMsg::Changed(fresh)),
                        Err(e) => {
                            worker::send(&tx, &ctx, Msg::Toast(format!("{e:#}")));
                            None
                        }
                    });
                // The dialog closes; a refused plan comes back as `Changed`.
                self.library.plan = None;
            }
            LibCmd::CancelJob(id) => {
                self.library.spawn("keel-library-cancel", move |b| {
                    let _ = b.cancel(id);
                    None
                });
            }
            LibCmd::RebuildIndex => {
                let ids: Vec<SourceId> =
                    self.library.sources.iter().map(|s| s.id.clone()).collect();
                for id in ids {
                    self.library_cmd(p, LibCmd::IndexNow(id));
                }
            }
            LibCmd::Switch(name) => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                if let Some(r) = self.library.remote().filter(|r| r.version.library != name) {
                    return self.toasts.error(format!(
                        "keel-daemon serves library {}: stop it (keel daemon stop) to switch",
                        r.version.library
                    ));
                }
                self.settings.library.name = name.clone();
                self.settings.library.enabled = true;
                if self.library.is_open() {
                    // Reopened on `Closed`.
                    self.library.close_later();
                } else {
                    let spawn = self.settings.library.daemon;
                    self.library
                        .open(&name, self.router.clone(), Via::Auto { spawn });
                }
            }
            // --- Task 33 ---
            LibCmd::SetVolumeState { volume, state } => {
                self.set_volume(volume, VolumeChange::State(state))
            }
            LibCmd::SetBackup { volume, on } => self.set_volume(volume, VolumeChange::Backup(on)),
            LibCmd::SetDomain { volume, domain } => {
                self.set_volume(volume, VolumeChange::Domain(domain))
            }
            LibCmd::CheckIntegrity => {
                let pct = self.settings.library.integrity_pct;
                self.library.spawn("keel-library-integrity", move |b| {
                    Some(LibMsg::Spawned {
                        kind: "integrity",
                        id: b.integrity(pct).ok(),
                    })
                });
            }
            LibCmd::Enable(on) => {
                self.settings.library.enabled = on;
                if on && !self.library.is_open() {
                    let name = self.settings.library.name.clone();
                    let spawn = self.settings.library.daemon;
                    self.library
                        .open(&name, self.router.clone(), Via::Auto { spawn });
                } else if !on {
                    self.library.close_later();
                }
            }
            LibCmd::Reconnect | LibCmd::OpenHere => {
                // The lost connection stays (read-only, its banner) until the library opens
                // again (`LibMsg::Opened`).
                let name = self.settings.library.name.clone();
                let here = cmd == LibCmd::OpenHere;
                let wait = self.library.release_wait;
                let via = match self.library.lost || !self.library.is_open() {
                    true => Via::Replace { here, wait },
                    false => return,
                };
                self.library.open(&name, self.router.clone(), via);
            }
            LibCmd::Restart => {
                if self.library.is_open() {
                    self.library.close_later();
                }
            }
        }
    }

    fn set_volume(&mut self, volume: String, change: VolumeChange) {
        self.library
            .spawn_try("keel-library-volume", "Volume", move |b| {
                b.set_volume(&volume, change)?;
                Ok(None)
            });
        self.library.protection_changed();
    }

    fn set_tag(&mut self, targets: Vec<VPath>, tag: TagId, on: bool) {
        let (sources, tags) = (self.library.sources.clone(), self.library.tags.clone());
        self.library.spawn_try("keel-library-tag", "Tag", move |b| {
            b.set_tag(&sources, &tags, &targets, tag, on)?;
            Ok(None)
        });
        self.library.refresh_meta_soon();
    }
}

/// Why nothing is written while an attached daemon is gone.
pub const LOST: &str =
    "keel-daemon stopped: reconnect, or open the library in this window, to make changes";

impl LibraryUi {
    /// Tags change on a worker; read them back right after.
    fn refresh_meta_soon(&mut self) {
        self.next_meta = Instant::now() + Duration::from_millis(300);
        self.ctx.request_repaint_after(Duration::from_millis(300));
    }

    /// Sources of an in-process library (instant), their labels for titles, and nothing
    /// else (a daemon's come from `refresh_sources`).
    fn sync_sources(&mut self) {
        let Some(lib) = &self.lib else { return };
        let sources = lib.sources();
        self.set_sources(sources);
    }

    fn set_sources(&mut self, sources: Vec<SourceSummary>) {
        self.sources = sources;
        // Kept for removed sources too (ids are random; a stale label harms nothing).
        let mut labels = LABELS.write();
        for s in &self.sources {
            // A source on a device also names that device's shared source (tab titles).
            crate::devices::note_source(&s.root, &s.label, false);
            match labels.iter_mut().find(|(id, _)| *id == s.id.0) {
                Some(l) if l.1 != s.label => l.1 = s.label.clone(),
                Some(_) => {}
                None => labels.push((s.id.0.clone(), s.label.clone())),
            }
        }
    }

    /// Tests: a library opened by the test.
    #[cfg(test)]
    pub fn set_open(&mut self, lib: Arc<Library>) {
        *self.slot.write() = Some(LibraryBackend::InProcess(lib.clone()));
        self.events = Some(lib.jobs().subscribe());
        self.backend = Some(LibraryBackend::InProcess(lib.clone()));
        self.lib = Some(lib);
        self.sync_sources();
    }

    /// Tests: attached to a daemon (as `open` does).
    #[cfg(test)]
    pub fn set_attached(&mut self, remote: Arc<Remote>) {
        let id = remote.id;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let events = remote
            .subscribe(move |ev| worker::send(&tx, &ctx, Msg::Library(LibMsg::Remote(id, ev))))
            .unwrap();
        self.set_sources(remote.sources().unwrap());
        let backend = LibraryBackend::Daemon(remote);
        *self.slot.write() = Some(backend.clone());
        self.events = Some(events);
        self.backend = Some(backend);
    }
}

/// Tags `targets` (real or `library://` paths): worker only.
pub(crate) fn apply_tag(
    lib: &Library,
    sources: &[SourceSummary],
    targets: &[VPath],
    tag: TagId,
    on: bool,
) -> anyhow::Result<()> {
    let records = targets
        .iter()
        .map(|t| {
            let (source, rel) = locate(sources, t)
                .with_context(|| format!("{} is not in a library source", t.display()))?;
            record_of(lib, &source, &rel)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    lib.set_tag(tag, &records, on)
}

/// A source label from its folder ("D:\Photos" -> "Photos").
pub fn label_for(root: &str) -> String {
    let trimmed = root.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .to_owned()
}

#[cfg(test)]
#[path = "library_tests.rs"]
mod tests;

// --- Task 33 ---: protection card, volume table and copies badges.

impl LibraryUi {
    /// The copies badge of a file (real path); asks for its folder's badges when missing.
    pub fn badge(&self, real: &VPath) -> Option<&Badge> {
        let b = self.badges.get(real);
        if b.is_none() {
            if let Some(dir) = real.parent().filter(|d| !self.badge_asked.contains(d)) {
                self.badge_wanted.lock().insert(dir);
            }
        }
        b
    }

    /// Reads the badges of the folders the details view asked for, on a worker.
    fn ask_badges(&mut self) {
        let wanted: Vec<VPath> = self.badge_wanted.lock().drain().collect();
        let dirs: Vec<VPath> = wanted
            .into_iter()
            .filter(|d| self.badge_asked.insert(d.clone()))
            .collect();
        if dirs.is_empty() {
            return;
        }
        let sources = self.sources.clone();
        self.spawn("keel-library-badges", move |b| {
            Some(LibMsg::Badges(b.badges(&sources, &dirs)))
        });
    }

    fn forget_badges(&mut self) {
        self.badges.clear();
        self.badge_asked.clear();
        self.badge_wanted.lock().clear();
    }

    /// A recount keel-core ran on its own thread (after watcher changes): counts and badges
    /// are read again. Seen at the next tick, at most `STATS_EVERY` later; attached, the
    /// daemon says `protection.recount` instead.
    pub(crate) fn follow_recounts(&mut self) {
        let Some(revision) = self.lib.as_ref().map(|l| l.protection_revision()) else {
            return;
        };
        if revision != self.protection_revision {
            self.protection_revision = revision;
            self.protection_changed();
        }
    }

    /// A volume changed: counts and badges are read again.
    fn protection_changed(&mut self) {
        self.forget_badges();
        self.next_stats = Instant::now() + Duration::from_millis(300);
        self.ctx.request_repaint_after(Duration::from_millis(300));
    }
}

/// Files per folder that get a copies badge (the rest show none).
pub(crate) const MAX_BADGES: usize = 5_000;

/// The details view's copies badge: copies and failure domains, a risk when one domain
/// holds them all.
#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    pub text: String,
    /// How it was computed, and where the copies are.
    pub hover: String,
    /// Every copy in one failure domain.
    pub risk: bool,
}

pub fn badge_of(r: &keel_api::types::Copies) -> Badge {
    let plural = |n: u64, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let mut hover = format!(
        "{} in {}",
        plural(r.copies, "copy", "copies"),
        plural(r.failure_domains, "failure domain", "failure domains")
    );
    if r.offline_copies > 0 {
        hover += &format!(" ({} offline)", r.offline_copies);
    }
    hover += if r.backed_up {
        "; backed up"
    } else {
        "; not backed up"
    };
    for c in &r.locations {
        let mut flags = String::new();
        let state = c.state.map(crate::backend::volume_state);
        if let Some(state) = state.filter(|s| *s != VolumeState::Online) {
            flags = format!(" [{}]", state_text(state));
        }
        if c.backup {
            flags += " [backup]";
        }
        if c.claimed {
            flags += " [claimed by the device, not counted]";
        }
        hover += &format!(
            "\n• {} on {} ({}){flags}",
            c.path, c.volume, c.failure_domain
        );
    }
    hover += "\nCopies count files with the same content (hard links once) on volumes that are \
              not lost or retired; volumes on one disk, cloud account or host are one failure \
              domain. Files not hashed yet show 1.";
    Badge {
        text: format!("{}× · {}", r.copies, r.failure_domains),
        hover,
        risk: r.failure_domains <= 1,
    }
}

pub fn state_text(s: VolumeState) -> &'static str {
    match s {
        VolumeState::Online => "online",
        VolumeState::Offline => "offline",
        VolumeState::Archived => "archived",
        VolumeState::Lost => "lost",
        VolumeState::Retired => "retired",
    }
}

pub fn kind_text(k: VolumeKind) -> &'static str {
    match k {
        VolumeKind::Fixed => "Disk",
        VolumeKind::Removable => "Removable",
        VolumeKind::Network => "Network",
        VolumeKind::Cloud => "Cloud",
        VolumeKind::Device => "Device",
    }
}

/// One row of the Overview's volume table.
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeRow {
    pub id: String,
    pub label: String,
    pub kind: &'static str,
    pub state: VolumeState,
    pub domain: String,
    /// The domain was set by hand.
    pub domain_set: bool,
    pub backup: bool,
    /// "120 GB / 250 GB", or "" when unknown.
    pub usage: String,
    pub last_seen: String,
}

pub fn volume_rows(volumes: &[Volume]) -> Vec<VolumeRow> {
    volumes
        .iter()
        .map(|v| VolumeRow {
            id: v.id.clone(),
            label: v.label.clone(),
            kind: kind_text(v.kind),
            state: v.state,
            domain: v.failure_domain.clone(),
            domain_set: v.domain_set,
            backup: v.backup,
            usage: v
                .capacity
                .map(|(used, total)| {
                    format!(
                        "{} / {}",
                        humansize::format_size(used, humansize::DECIMAL),
                        humansize::format_size(total, humansize::DECIMAL)
                    )
                })
                .unwrap_or_default(),
            last_seen: when((v.last_seen > 0).then_some(v.last_seen)),
        })
        .collect()
}

/// One row of the Overview's per-source table.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceCountRow {
    pub id: SourceId,
    pub label: String,
    pub files: String,
    pub folders: String,
    pub size: String,
    /// "80 %" of the files hashed ("—" without files).
    pub hashed: String,
    pub last_walk: String,
    pub offline: bool,
}

pub fn source_count_rows(per: &[keel_core::SourceStats]) -> Vec<SourceCountRow> {
    per.iter()
        .map(|s| SourceCountRow {
            id: s.id.clone(),
            label: s.label.clone(),
            files: count(s.files),
            folders: count(s.folders),
            size: humansize::format_size(s.bytes, humansize::DECIMAL),
            hashed: match s.files {
                0 => "—".into(),
                n => format!("{} %", s.hashed_files.min(n) * 100 / n),
            },
            last_walk: when(s.last_walk),
            offline: s.offline,
        })
        .collect()
}

/// One line of the protection card.
#[derive(Clone, Debug, PartialEq)]
pub struct ProtectionLine {
    pub text: String,
    /// How the number is computed (hover).
    pub how: &'static str,
    /// Shown as a warning.
    pub warn: bool,
}

/// The protection card's lines, every number explained. While files are not hashed yet the
/// copy counts cover the checked files only, and say so: an unknown is never a plain 0.
pub fn protection_lines(p: &ProtectionSummary) -> Vec<ProtectionLine> {
    let files = |n: u64| {
        if n == 1 {
            "1 file".to_owned()
        } else {
            format!("{} files", count(n))
        }
    };
    let partial = if p.unchecked > 0 {
        " (of those checked)"
    } else {
        ""
    };
    let line = |text: String, how: &'static str, warn: bool| ProtectionLine { text, how, warn };
    let mut lines = Vec::new();
    if p.unchecked > 0 {
        lines.push(line(
            format!("{} not checked yet", files(p.unchecked)),
            "Files without a content hash yet (hashing off, paused or still running; shares \
             and cloud sources are hashed only when asked): whether they have other copies is \
             unknown, so they are in none of the counts below.",
            true,
        ));
    }
    lines.extend([
        line(
            format!("{} with one copy only{partial}", files(p.single_copy)),
            "Hashed contents held by a single file (hard links count once; copies on lost or \
             retired volumes do not count). Files not hashed yet are not counted.",
            p.single_copy > 0,
        ),
        line(
            format!(
                "{} with every copy on one disk{partial}",
                files(p.single_domain)
            ),
            "Contents with two or more copies, all in one failure domain: one physical disk, \
             cloud account or host. One failure loses them all.",
            p.single_domain > 0,
        ),
        line(
            format!("{} not backed up{partial}", files(p.unbacked)),
            "Contents without a copy on a volume marked as backup in a second failure domain. \
             Mark backup drives in the volume table below.",
            false,
        ),
        line(
            format!("{} changed since last check", files(p.drifted)),
            "Integrity checks re-hash a sample of files; drift is a file whose bytes changed \
             although its size and times did not (bit rot, or a tool restoring timestamps).",
            p.drifted > 0,
        ),
        line(
            if p.offline_volumes == 1 {
                "1 volume offline".to_owned()
            } else {
                format!("{} volumes offline", p.offline_volumes)
            },
            "Volumes none of whose sources can be reached now. Their copies still count; mark \
             a drive Archived, Lost or Retired in the volume table.",
            p.offline_volumes > 0,
        ),
    ]);
    lines
}
