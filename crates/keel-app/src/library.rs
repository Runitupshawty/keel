//! The library in the app (spec 2.10, Task 29): opening it off the UI thread, the index
//! behind the `library://` provider, sidebar data, library job rows, tags, plans
//! (validate → preview → execute) and the duplicate finder. Every keel-core call that
//! reads a store runs on a worker; only `sources()`, `activity()`, `refresh_status()` and
//! `Jobs::subscribe` (which return at once) run on the UI thread. Drawing is in
//! `library_ui`.

use crate::keys::Action;
use crate::state::{AppState, Msg};
use crate::tab::TabKind;
use crate::worker;
use anyhow::Context as _;
use crossbeam_channel::{Receiver, Sender};
use keel_core::{
    Indexer, JobEvent, JobId, JobInfo, JobStatus, Library, LibraryHit, LibraryStats,
    LibrarySummary, OnConflict, Op, Plan, PlanChanged, RecordRef, SourceDef, SourceId, SourceKind,
    SourceStatus, SourceSummary, Tag, TagId, View, Warning, WatchConfig, WatchHandle, FAVORITES,
};
use keel_search::{Hit, Query, Searcher};
use keel_vfs::library as vlib;
use keel_vfs::{Entry, Kind, Router, VPath};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};
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
/// Idle-only hashing: input within this long counts as busy.
const IDLE_AFTER: Duration = Duration::from_secs(5);
/// A watcher that could not start (source offline) is tried again after this long.
const WATCH_RETRY: Duration = Duration::from_secs(300);
/// Finished library job rows leave the jobs panel after this long (failures stay).
const ROW_KEPT: Duration = Duration::from_secs(5);
/// The duplicate finder shows at most this many groups (biggest first).
const MAX_GROUPS: usize = 500;

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
    /// Remote and cloud sources are re-walked this often (local sources are watched live
    /// and reconciled every 6 hours).
    pub rescan_minutes: u64,
    /// Details view: the Tags column.
    pub tags_column: bool,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            name: "james".into(),
            hashing: Hashing::default(),
            rescan_minutes: 15,
            tags_column: true,
        }
    }
}

/// A sidebar / overview / dialog command (`Action::Library`).
#[derive(Clone, Debug, PartialEq)]
pub enum LibCmd {
    Overview,
    OpenSource(SourceId),
    IndexNow(SourceId),
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
}

/// Answers from library workers (`Msg::Library`).
pub enum LibMsg {
    Opened {
        result: Result<Arc<Library>, String>,
        first_run: bool,
        jobs: Vec<JobInfo>,
    },
    Stats(LibraryStats),
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
    Planned(Result<Plan, String>),
    /// `Plan::execute` refused: the sources changed; show this fresh plan.
    Changed(Plan),
    Dups(Result<Vec<DupGroup>, String>),
    Libraries(Vec<LibrarySummary>),
    /// The add-source wizard's folder picker answered.
    Picked(Option<String>),
    /// Job kinds by id (for rows that arrived as bare events).
    Kinds(Vec<(JobId, String)>),
    Closed,
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
    pub record: RecordRef,
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
                    record: r,
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
                SourceStatus::Offline { last_seen } => (
                    Dot::Offline,
                    format!("offline (last seen {})", when(*last_seen)),
                ),
                SourceStatus::Error(e) => (Dot::Error, e.clone()),
            };
            SourceRow {
                id: s.id.clone(),
                label: s.label.clone(),
                dot,
                detail,
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
    pub plan: Plan,
    /// The sources changed since the first preview (this is the fresh one).
    pub changed: bool,
    pub running: bool,
}

enum Watch {
    Starting,
    Live(WatchHandle),
    Failed(Instant),
}

type Slot = Arc<RwLock<Option<Arc<Library>>>>;

pub struct LibraryUi {
    pub lib: Option<Arc<Library>>,
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
    last_input: Instant,
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
            lib: None,
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
            last_input: now,
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
            tx,
            ctx,
        }
    }

    pub fn is_open(&self) -> bool {
        self.lib.is_some()
    }

    /// Opens library `name` on a worker (creating it on first run), sets the router and
    /// resumes jobs left by the last session.
    pub fn open(&mut self, name: &str, router: Arc<Router>) {
        if self.opening {
            return;
        }
        self.opening = true;
        self.error = None;
        let (tx, ctx, name) = (self.tx.clone(), self.ctx.clone(), name.to_owned());
        worker::spawn("keel-library-open", move || {
            let opened = (|| -> anyhow::Result<(Library, bool, Vec<JobInfo>)> {
                let root = keel_core::data_dir().context("no data folder")?;
                let first_run = !root.join("library").join(&name).exists();
                let lib = Library::open(&root, &name)?;
                lib.set_router(router);
                lib.jobs().resume_all()?;
                let jobs = lib.jobs().list()?;
                Ok((lib, first_run, jobs))
            })();
            let msg = match opened {
                Ok((lib, first_run, jobs)) => LibMsg::Opened {
                    result: Ok(Arc::new(lib)),
                    first_run,
                    jobs,
                },
                Err(e) => LibMsg::Opened {
                    result: Err(format!("{e:#}")),
                    first_run: false,
                    jobs: Vec::new(),
                },
            };
            worker::send(&tx, &ctx, Msg::Library(msg));
        });
    }

    /// Takes the open library out (watchers stop); for closing or switching.
    fn take(&mut self) -> Option<(Arc<Library>, Vec<WatchHandle>)> {
        let lib = self.lib.take()?;
        *self.slot.write() = None;
        self.events = None;
        self.sources.clear();
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

    /// Exit: stops watchers and closes the library, waiting up to `CLOSE_TIMEOUT`.
    pub fn close_now(&mut self) {
        if let Some((lib, handles)) = self.take() {
            drop(handles);
            match Arc::try_unwrap(lib) {
                Ok(lib) => {
                    if !lib.close(CLOSE_TIMEOUT) {
                        tracing::warn!("library: a job was still busy at exit");
                    }
                }
                // A worker still holds it; its drop closes it.
                Err(_) => tracing::info!("library: closed by its last user"),
            }
        }
    }

    /// Closes on a worker (switching libraries, turning the library off).
    fn close_later(&mut self) {
        if let Some((lib, handles)) = self.take() {
            let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
            worker::spawn("keel-library-close", move || {
                drop(handles);
                if let Ok(lib) = Arc::try_unwrap(lib) {
                    lib.close(CLOSE_TIMEOUT);
                }
                worker::send(&tx, &ctx, Msg::Library(LibMsg::Closed));
            });
        }
    }

    /// Runs `f` with the library on a worker; its message (if any) comes back.
    fn spawn(
        &self,
        name: &str,
        f: impl FnOnce(&Library) -> Option<LibMsg> + Send + 'static,
    ) -> bool {
        let Some(lib) = self.lib.clone() else {
            return false;
        };
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        worker::spawn(name, move || {
            if let Some(msg) = f(&lib) {
                worker::send(&tx, &ctx, Msg::Library(msg));
            }
        })
    }

    /// Like `spawn`, but an error becomes a toast.
    fn spawn_try(
        &self,
        name: &str,
        what: &'static str,
        f: impl FnOnce(&Library) -> anyhow::Result<Option<LibMsg>> + Send + 'static,
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
        self.spawn("keel-library-meta", |lib| {
            let tags = lib.tags().unwrap_or_default();
            let views = lib.views().unwrap_or_default();
            let mut tagged: HashMap<VPath, Vec<TagId>> = HashMap::new();
            for tag in tags.iter().map(|t| t.id).chain([FAVORITES]) {
                for hit in lib.records_with_tag(tag).unwrap_or_default() {
                    tagged.entry(hit.path).or_default().push(tag);
                }
            }
            Some(LibMsg::Meta {
                tags,
                views,
                tagged,
            })
        });
    }

    fn refresh_stats(&mut self) {
        self.next_stats = Instant::now() + STATS_EVERY;
        self.spawn("keel-library-stats", |lib| Some(LibMsg::Stats(lib.stats())));
    }

    pub fn refresh_dups(&mut self) {
        self.spawn("keel-library-dups", |lib| {
            Some(LibMsg::Dups(
                load_dups(lib, 1).map_err(|e| format!("{e:#}")),
            ))
        });
    }

    fn refresh_libraries(&self) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        worker::spawn("keel-library-list", move || {
            worker::send(&tx, &ctx, Msg::Library(LibMsg::Libraries(Library::list())));
        });
    }

    /// Settings → Library → Hashing: off cancels a running hash job.
    pub fn apply_hashing(&mut self, policy: Hashing) {
        if self.lib.is_none() || self.policy == policy {
            return;
        }
        self.policy = policy;
        if policy != Hashing::Off {
            self.start_hashing(policy);
        } else {
            let running: Vec<JobId> = (self.jobs.iter())
                .filter(|(_, j)| j.kind == "hash" && j.active())
                .map(|(id, _)| *id)
                .collect();
            for id in running {
                self.spawn("keel-library-cancel", move |lib| {
                    let _ = lib.jobs().cancel(id);
                    None
                });
            }
        }
    }

    /// Starts hashing unless it is off or already running.
    fn start_hashing(&mut self, policy: Hashing) {
        if policy == Hashing::Off || self.jobs.values().any(|j| j.kind == "hash" && j.active()) {
            return;
        }
        self.spawn("keel-library-hash", |lib| {
            Some(LibMsg::Spawned {
                kind: "hash",
                id: lib.hash().ok(),
            })
        });
    }

    /// A watcher per source once no index job runs (both would walk the source).
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

/// The `library://` provider's view of the library.
struct AppIndex(Slot);

impl AppIndex {
    fn lib(&self) -> anyhow::Result<Arc<Library>> {
        self.0.read().clone().context("the library is not open")
    }
}

impl vlib::LibraryIndex for AppIndex {
    fn children(&self, source: &str, rel: &str) -> anyhow::Result<Vec<Entry>> {
        let lib = self.lib()?;
        let hits = lib.list_children(&SourceId(source.into()), rel)?;
        Ok(hits.into_iter().map(entry_of).collect())
    }

    fn resolve(&self, source: &str, rel: &str) -> anyhow::Result<VPath> {
        let lib = self.lib()?;
        let src = lib
            .source(&SourceId(source.into()))
            .context("this source is no longer in the library")?;
        let status = src.status.read().clone();
        if let SourceStatus::Offline { last_seen } = status {
            anyhow::bail!(
                "{} is offline (last seen {})",
                src.def.label,
                when(last_seen)
            );
        }
        Ok(src.absolute(rel))
    }
}

fn entry_of(h: LibraryHit) -> Entry {
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

/// The search tab's Library backend: `LibrarySearcher`, plus the sidebar's Favorites and
/// Recents queries.
pub struct LibSearch(pub Arc<Library>);

impl Searcher for LibSearch {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let hits = match q.text.trim() {
            FAVORITES_QUERY => self.0.favorites()?,
            RECENTS_QUERY => self.0.recents(q.max as usize)?,
            _ => return keel_core::LibrarySearcher(self.0.clone()).query(q),
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
        keel_core::LibrarySearcher(self.0.clone()).status()
    }

    fn name(&self) -> &'static str {
        "Library"
    }
}

fn entry_of_time(t: Option<i64>) -> Option<std::time::SystemTime> {
    t.filter(|t| *t >= 0)
        .map(|t| std::time::UNIX_EPOCH + Duration::from_secs(t as u64))
}

impl AppState {
    /// The library side of `drain`.
    pub fn library_msg(&mut self, msg: LibMsg) {
        let l = &mut self.library;
        match msg {
            LibMsg::Opened {
                result,
                first_run,
                jobs,
            } => {
                l.opening = false;
                match result {
                    Ok(lib) => {
                        *l.slot.write() = Some(lib.clone());
                        l.events = Some(lib.jobs().subscribe());
                        l.jobs = jobs
                            .into_iter()
                            .filter(|j| matches!(j.status, JobStatus::Queued | JobStatus::Running))
                            .map(|j| {
                                (
                                    j.id,
                                    JobRow {
                                        kind: j.kind,
                                        status: j.status,
                                        progress: j.progress,
                                        ended: None,
                                    },
                                )
                            })
                            .collect();
                        l.lib = Some(lib);
                        l.sync_sources();
                        l.refresh_meta();
                        l.refresh_stats();
                        l.refresh_dups();
                        l.refresh_libraries();
                        let policy = self.settings.library.hashing;
                        self.library.policy = policy;
                        self.library.start_hashing(policy);
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
            LibMsg::Stats(stats) => l.stats = stats,
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
                    None if kind != "hash" => self.toasts.error(format!("Could not start {kind}")),
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
            LibMsg::Closed => {
                if self.settings.library.enabled && !l.is_open() {
                    let name = self.settings.library.name.clone();
                    l.open(&name, self.router.clone());
                }
            }
        }
    }

    /// Per frame: job events, source status, periodic refreshes, watchers, hashing policy.
    pub fn library_tick(&mut self) {
        let busy_input = self
            .ctx
            .input(|i| !i.events.is_empty() || i.pointer.is_moving());
        let policy = self.settings.library.hashing;
        let rescan = Duration::from_secs(self.settings.library.rescan_minutes.max(1) * 60);
        let l = &mut self.library;
        if busy_input {
            l.last_input = Instant::now();
        }
        let Some(lib) = l.lib.clone() else { return };
        let now = Instant::now();
        // Hashing pauses while the user works (idle only) or when paused by hand.
        let idle = l.last_input.elapsed() >= IDLE_AFTER;
        let busy = l.hash_paused || (policy == Hashing::IdleOnly && !idle);
        lib.activity()
            .store(busy, std::sync::atomic::Ordering::Relaxed);
        if policy == Hashing::IdleOnly && !idle {
            self.ctx.request_repaint_after(IDLE_AFTER);
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
            l.spawn("keel-library-jobs", |lib| {
                let list = lib.jobs().list().ok()?;
                Some(LibMsg::Kinds(
                    list.into_iter().map(|j| (j.id, j.kind)).collect(),
                ))
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
        if !ended.is_empty() {
            l.refresh_meta();
            l.refresh_stats();
            if ended.iter().any(|k| k == "index") {
                l.start_hashing(policy);
            }
            // The Overview's duplicate summary (and an open finder) follow new content ids.
            if ended.iter().any(|k| k == "hash") {
                l.refresh_dups();
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
            drop(lib.refresh_status());
        }
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
            .spawn_try("keel-library-plan", "Preview", move |lib| {
                Ok(Some(LibMsg::Planned(
                    keel_core::validate_preview_execute(lib, op).map_err(|e| format!("{e:#}")),
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
        let sources = &self.library.sources;
        let all_in_sources = |paths: &[VPath]| {
            !paths.is_empty() && paths.iter().all(|x| locate(sources, x).is_some())
        };
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
                    if let Some((source, rel)) = locate(sources, &e.path) {
                        self.library.spawn("keel-library-opened", move |lib| {
                            if let Ok(r) = record_of(lib, &source, &rel) {
                                let _ = lib.note_open(&r);
                            }
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
        if !self.library.is_open()
            && !matches!(
                cmd,
                LibCmd::Switch(_) | LibCmd::Enable(_) | LibCmd::Overview
            )
        {
            return self.toasts.error(if self.library.opening {
                "The library is still opening"
            } else {
                "The library is off (Settings → Library)"
            });
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
            LibCmd::IndexNow(id) => {
                // Its watcher stops first (both would walk the source).
                let watch = self.library.watchers.remove(&id);
                self.library.starting_index += 1;
                self.library.spawn("keel-library-index", move |lib| {
                    drop(watch);
                    Some(LibMsg::Spawned {
                        kind: "index",
                        id: lib.index(&id).ok(),
                    })
                });
            }
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
                    .spawn_try("keel-library-remove", "Remove source", move |lib| {
                        drop(watch);
                        lib.remove_source(&id, true)?;
                        Ok(None)
                    });
            }
            LibCmd::PauseHashing(on) => self.library.hash_paused = on,
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
                    }),
                );
            }
            LibCmd::Register(def) => {
                self.library.add = None;
                self.library.starting_index += 1;
                let label = def.label.clone();
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                self.library.spawn("keel-library-add", move |lib| {
                    let id = match lib.add_source(def) {
                        Ok(id) => lib.index(&id).ok(),
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
                    .spawn_try("keel-library-tag", "New tag", move |lib| {
                        let tag = lib.create_tag(&name, Some(&color), None)?;
                        if let Some(targets) = targets {
                            apply_tag(lib, &sources, &targets, tag, true)?;
                        }
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
                    .spawn("keel-library-execute", move |lib| match plan.execute(lib) {
                        Ok(id) => {
                            worker::send(
                                &tx,
                                &ctx,
                                Msg::Library(LibMsg::Spawned {
                                    kind: "op",
                                    id: Some(id),
                                }),
                            );
                            None
                        }
                        Err(e) => match e.downcast::<PlanChanged>() {
                            Ok(PlanChanged(fresh)) => Some(LibMsg::Changed(*fresh)),
                            Err(e) => {
                                worker::send(&tx, &ctx, Msg::Toast(format!("{e:#}")));
                                None
                            }
                        },
                    });
                // The dialog closes; a refused plan comes back as `Changed`.
                self.library.plan = None;
            }
            LibCmd::CancelJob(id) => {
                self.library.spawn("keel-library-cancel", move |lib| {
                    let _ = lib.jobs().cancel(id);
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
                self.settings.library.name = name.clone();
                self.settings.library.enabled = true;
                if self.library.is_open() {
                    // Reopened on `Closed`.
                    self.library.close_later();
                } else {
                    self.library.open(&name, self.router.clone());
                }
            }
            LibCmd::Enable(on) => {
                self.settings.library.enabled = on;
                if on && !self.library.is_open() {
                    let name = self.settings.library.name.clone();
                    self.library.open(&name, self.router.clone());
                } else if !on {
                    self.library.close_later();
                }
            }
        }
    }

    fn set_tag(&mut self, targets: Vec<VPath>, tag: TagId, on: bool) {
        let sources = self.library.sources.clone();
        self.library
            .spawn_try("keel-library-tag", "Tag", move |lib| {
                apply_tag(lib, &sources, &targets, tag, on)?;
                Ok(None)
            });
        self.library.refresh_meta_soon();
    }
}

impl LibraryUi {
    /// Tags change on a worker; read them back right after.
    fn refresh_meta_soon(&mut self) {
        self.next_meta = Instant::now() + Duration::from_millis(300);
        self.ctx.request_repaint_after(Duration::from_millis(300));
    }

    /// Sources (instant), their labels for titles, and nothing else.
    fn sync_sources(&mut self) {
        let Some(lib) = &self.lib else { return };
        self.sources = lib.sources();
        // Kept for removed sources too (ids are random; a stale label harms nothing).
        let mut labels = LABELS.write();
        for s in &self.sources {
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
        *self.slot.write() = Some(lib.clone());
        self.events = Some(lib.jobs().subscribe());
        self.lib = Some(lib);
        self.sync_sources();
    }
}

/// Tags `targets` (real or `library://` paths): worker only.
fn apply_tag(
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
