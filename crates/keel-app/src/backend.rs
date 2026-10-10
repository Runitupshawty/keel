//! Where the window's library lives (spec 2.10, "Clients"): in this process
//! ([`LibraryBackend::InProcess`], the window owns the library) or in the profile's
//! keel-daemon ([`LibraryBackend::Daemon`]: the window is one more client, so the CLI,
//! `keel mcp` and other windows keep working while it is open).
//!
//! On open the window first looks for the profile's daemon (`client::CONNECT_WAIT`);
//! without one it starts `keel-daemon --profile <name>` when Settings → Library → "Run the
//! library in a background daemon" is on, else it opens the library itself ([`attach`]).
//! While attached it never opens the library: every read and change goes through the API
//! (`keel_api::OPS`), file operations through `plan` + `execute` behind the window's own
//! preview dialog, job progress through `subscribe`. Every call here blocks: workers only.

use crate::library::{offline_text, real_of, record_of, source_of, Badge, DupGroup, DupRecord};
use crate::library::{JobRow, MAX_BADGES, MAX_GROUPS};
use anyhow::{bail, Context as _, Result};
use keel_api::client::Client;
use keel_api::config::HostConfig;
use keel_api::types as api;
use keel_api::ApiError;
use keel_core::{
    Indexer, JobEvent, JobId, JobStatus, Library, LibraryStats, OfflineReason, Op, Plan,
    PlanChanged, ProtectionSummary, SourceDef, SourceId, SourceKind, SourceStatus, SourceSummary,
    Tag, TagId, View, Volume, VolumeKind, VolumeState, FAVORITES,
};
use keel_vfs::{Entry, VPath};
use parking_lot::{Mutex, RwLock};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a started daemon gets to answer.
pub const SPAWN_WAIT: Duration = Duration::from_secs(15);

/// The library the window works with.
#[derive(Clone)]
pub enum LibraryBackend {
    InProcess(Arc<Library>),
    Daemon(Arc<Remote>),
}

/// A connection to the profile's keel-daemon: a few idle clients (workers call in
/// parallel; a connection that broke is not reused, at most `MAX_IDLE` are kept) and the
/// last source list.
pub struct Remote {
    name: String,
    idle: Mutex<Vec<Client>>,
    /// The `subscribe` connection (closed by `close_events`, or when this goes).
    events: Mutex<Option<Arc<keel_api::socket::Closable>>>,
    /// Told apart from an earlier connection (a stale "lost" message is ignored).
    pub id: u64,
    pub version: api::VersionInfo,
    sources: RwLock<Vec<SourceSummary>>,
}

static NEXT_REMOTE: AtomicU64 = AtomicU64::new(1);

/// Idle connections kept for the next calls (more workers than this connect anew).
const MAX_IDLE: usize = 4;

/// The profile's daemon settings (socket name), from the window's folders.
pub fn host_config(profile: &str) -> Result<HostConfig> {
    let config = crate::settings::config_dir().context("no configuration folder")?;
    let data = crate::settings::data_dir().context("no data folder")?;
    Ok(HostConfig::read(profile, config, data))
}

impl Remote {
    /// Connects to the daemon on `name` and reads its version.
    pub fn connect(name: &str) -> Result<Arc<Remote>> {
        let mut client = Client::connect(name)?;
        let version = client
            .call("version", Value::Null)
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        Ok(Arc::new(Remote {
            name: name.to_owned(),
            idle: Mutex::new(vec![client]),
            events: Mutex::new(None),
            id: NEXT_REMOTE.fetch_add(1, Ordering::Relaxed),
            version: serde_json::from_value(version)?,
            sources: RwLock::default(),
        }))
    }

    /// One call, its typed error kept (`execute`'s PLAN_CHANGED carries a fresh preview).
    pub fn call_raw(&self, method: &str, params: Value) -> keel_api::Result<Value> {
        let client = self.idle.lock().pop();
        let mut client = match client {
            Some(c) => c,
            None => Client::connect(&self.name)
                .map_err(|e| ApiError::failed(format!("keel-daemon: {e}")))?,
        };
        let result = client.call(method, params);
        // Transport errors are the client's own ("keel-daemon: …"): that one is dropped.
        let broken = matches!(&result, Err(e) if e.message.starts_with("keel-daemon: "));
        if !broken {
            self.put_back(client);
        }
        result
    }

    /// Keeps `client` for the next call unless `MAX_IDLE` already wait.
    fn put_back(&self, client: Client) {
        let mut idle = self.idle.lock();
        if idle.len() < MAX_IDLE {
            idle.push(client);
        }
    }

    pub fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let v = self
            .call_raw(method, params)
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        serde_json::from_value(v).with_context(|| format!("{method}: unexpected answer"))
    }

    /// A previewed operation the user already asked for (a click is the confirmation):
    /// its preview, then `execute` of exactly that preview.
    pub fn apply(&self, method: &str, params: Value) -> Result<api::Executed> {
        let preview: api::PlanPreview = self.call(method, params)?;
        self.call(
            "execute",
            json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
        )
    }

    /// The sources last read.
    pub fn cached_sources(&self) -> Vec<SourceSummary> {
        self.sources.read().clone()
    }

    /// The daemon's sources (kept for `library://` reads).
    pub fn sources(&self) -> Result<Vec<SourceSummary>> {
        let list: Vec<api::SourceInfo> = self.call("sources.list", json!({}))?;
        let list: Vec<SourceSummary> = list.iter().map(source_summary).collect();
        *self.sources.write() = list.clone();
        Ok(list)
    }

    /// Job progress, library changes and device events from a connection of their own
    /// (`subscribe`, one per `Remote`: a new one closes the last): job events come back on
    /// the returned channel, the rest through `on` (each with a repaint). When the daemon
    /// goes away, `on(Event::Lost)`; never after `close_events`.
    pub fn subscribe(
        &self,
        on: impl Fn(Event) + Send + 'static,
    ) -> Result<crossbeam_channel::Receiver<JobEvent>> {
        let conn = keel_api::socket::connect_closable(&self.name, keel_api::client::CONNECT_WAIT)?;
        (&*conn).write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"subscribe\"}\n")?;
        (&*conn).flush()?;
        if let Some(old) = self.events.lock().replace(conn.clone()) {
            old.close();
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("keel-daemon-events".into())
            .spawn(move || {
                let mut lines = BufReader::new(&*conn).lines();
                loop {
                    let Some(Ok(line)) = lines.next() else {
                        if !conn.is_closed() {
                            on(Event::Lost);
                        }
                        return;
                    };
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let params = &v["params"];
                    match v["method"].as_str() {
                        Some("job.progress") => {
                            let ev = JobEvent {
                                id: params["id"].as_i64().unwrap_or_default(),
                                status: job_status(params["status"].as_str().unwrap_or("")),
                                progress: params["progress"].as_f64().unwrap_or(0.0) as f32,
                            };
                            if tx.send(ev).is_err() {
                                return; // the window let go of this connection
                            }
                            on(Event::Jobs);
                        }
                        Some("library.changed") => {
                            let kind = params["kind"].as_str().unwrap_or_default();
                            // A sidecar job's own news: nothing to refresh (the window
                            // asks for the thumbnails it shows, made on demand).
                            if kind != "media.index" || params["method"] != "job" {
                                on(Event::Changed(kind.to_owned()))
                            }
                        }
                        Some("daemon.stopping") if !conn.is_closed() => return on(Event::Lost),
                        Some("net.event") => on(Event::Net(params.clone())),
                        _ => {}
                    }
                }
            })?;
        Ok(rx)
    }

    /// Ends the subscription (the window lets go of this daemon).
    pub fn close_events(&self) {
        if let Some(conn) = self.events.lock().take() {
            conn.close();
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.close_events();
    }
}

/// What the subscription reports besides job progress.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A job moved (its event is on the job channel).
    Jobs,
    /// A change by any client (`library.changed`): its kind is the operation (`tags.add`,
    /// `recents.note`, ...; empty from an older daemon), see `library::refresh_for`.
    Changed(String),

    /// A device event (`net.event` params).
    Net(Value),
    /// The connection closed: the daemon stopped.
    Lost,
}

/// `keel_api::Backend` over a [`Remote`] (for `DaemonProvider`).
pub struct Rpc(pub Arc<Remote>);

impl keel_api::Backend for Rpc {
    fn call(&mut self, method: &str, params: Value) -> keel_api::Result<Value> {
        self.0.call_raw(method, params)
    }
}

/// How the library is reached when it opens.
#[derive(Debug, PartialEq)]
pub enum Attach<R> {
    /// The daemon (started by this call when `spawned`).
    Daemon { remote: R, spawned: bool },
    /// In this process; `note` says why a daemon was wanted but not used.
    InProcess { note: Option<String> },
}

/// A started daemon: why it has already ended, if it has (`Spawned::exited`).
pub type Exited = Box<dyn FnMut() -> Option<String>>;

/// The open-time decision: a running daemon wins; else one is started when `spawn` is
/// given (Settings → Library → "Run the library in a background daemon", or Reconnect)
/// and waited for up to `wait` (less when it exits first); else the library opens in this
/// process. `connect` None skips the daemon ("Open in this window").
pub fn attach<R>(
    connect: Option<&mut dyn FnMut() -> Option<R>>,
    spawn: Option<&mut dyn FnMut() -> Result<Exited>>,
    wait: Duration,
) -> Attach<R> {
    let Some(connect) = connect else {
        return Attach::InProcess { note: None };
    };
    if let Some(remote) = connect() {
        return Attach::Daemon {
            remote,
            spawned: false,
        };
    }
    let Some(spawn) = spawn else {
        return Attach::InProcess { note: None };
    };
    let mut exited = match spawn() {
        Ok(exited) => exited,
        Err(e) => {
            return Attach::InProcess {
                note: Some(format!("keel-daemon did not start: {e:#}")),
            }
        }
    };
    let deadline = Instant::now() + wait;
    loop {
        if let Some(remote) = connect() {
            return Attach::Daemon {
                remote,
                spawned: true,
            };
        }
        if let Some(why) = exited() {
            return Attach::InProcess { note: Some(why) };
        }
        if Instant::now() >= deadline {
            return Attach::InProcess {
                note: Some("keel-daemon did not answer in time".into()),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A plan waiting in the preview dialog.
#[derive(Clone, Debug)]
pub enum Planned {
    Local(Plan),
    Daemon(api::PlanPreview),
}

impl Planned {
    /// The dialog's headline.
    pub fn summary(&self) -> String {
        match self {
            Planned::Local(p) => crate::library::plan_summary(p),
            Planned::Daemon(p) => p.summary.clone(),
        }
    }

    /// One line per change.
    pub fn changes(&self) -> Vec<String> {
        use humansize::{format_size, DECIMAL};
        match self {
            Planned::Local(plan) => plan
                .changes
                .iter()
                .map(|c| {
                    let verb = match c.action {
                        keel_core::Action::Copy => "Copy",
                        keel_core::Action::Move => "Move",
                        keel_core::Action::Delete => "Delete",
                        keel_core::Action::Rename => "Rename",
                    };
                    let to = c.to.as_ref().map(|t| format!(" → {}", t.display()));
                    format!(
                        "{verb} {}{}  ({}, {})",
                        c.from.display(),
                        to.unwrap_or_default(),
                        crate::jobs::items(c.files as usize),
                        format_size(c.bytes, DECIMAL)
                    )
                })
                .collect(),
            Planned::Daemon(p) => p
                .changes
                .iter()
                .map(|c| {
                    let mut verb = c.action.clone();
                    if let Some(first) = verb.get_mut(..1) {
                        first.make_ascii_uppercase();
                    }
                    let to = c.to.as_ref().map(|t| format!(" → {t}"));
                    let size = match (c.files, c.bytes) {
                        (Some(f), Some(b)) => format!(
                            "  ({}, {})",
                            crate::jobs::items(f as usize),
                            format_size(b, DECIMAL)
                        ),
                        _ => String::new(),
                    };
                    let path = c.path.as_deref().unwrap_or_default();
                    format!("{verb} {path}{}{size}", to.unwrap_or_default())
                })
                .collect(),
        }
    }

    /// One line per warning.
    pub fn warnings(&self, sources: &[SourceSummary]) -> Vec<String> {
        match self {
            Planned::Local(p) => p
                .warnings
                .iter()
                .map(|w| crate::library::warning_text(w, sources))
                .collect(),
            Planned::Daemon(p) => p.warnings.iter().map(|w| w.message.clone()).collect(),
        }
    }
}

/// Tags, saved views and the tags of every tagged path (by real path).
pub type Meta = (Vec<Tag>, Vec<View>, HashMap<VPath, Vec<TagId>>);

/// What executing a plan did.
pub enum Executed {
    Job(JobId),
    /// The sources changed since the preview: confirm this fresh one.
    Changed(Planned),
}

/// A volume change from the volume table.
pub enum VolumeChange {
    State(VolumeState),
    Backup(bool),
    Domain(String),
}

impl LibraryBackend {
    pub fn is_daemon(&self) -> bool {
        matches!(self, LibraryBackend::Daemon(_))
    }

    pub fn remote(&self) -> Option<&Arc<Remote>> {
        match self {
            LibraryBackend::Daemon(r) => Some(r),
            LibraryBackend::InProcess(_) => None,
        }
    }

    pub fn sources(&self) -> Result<Vec<SourceSummary>> {
        match self {
            LibraryBackend::InProcess(lib) => Ok(lib.sources()),
            LibraryBackend::Daemon(r) => r.sources(),
        }
    }

    /// The indexed children of a `library://` folder (`DaemonProvider` lists the daemon's).
    pub fn children(&self, source: &str, rel: &str) -> Result<Vec<Entry>> {
        match self {
            LibraryBackend::InProcess(lib) => {
                let hits = lib.list_children(&SourceId(source.into()), rel)?;
                Ok(hits.into_iter().map(crate::library::entry_of).collect())
            }
            LibraryBackend::Daemon(r) => {
                use keel_vfs::Provider;
                let provider = keel_api::DaemonProvider::new("library", Rpc(r.clone()));
                provider.list(&keel_vfs::library::path(source, rel))
            }
        }
    }

    /// The real path of a `library://` path, or why it cannot be reached now.
    pub fn resolve(&self, source: &str, rel: &str) -> Result<VPath> {
        let (label, root, status) = match self {
            LibraryBackend::InProcess(lib) => {
                let src = lib
                    .source(&SourceId(source.into()))
                    .context("this source is no longer in the library")?;
                let status = src.status.read().clone();
                (src.def.label.clone(), src.def.root.clone(), status)
            }
            LibraryBackend::Daemon(r) => {
                let sources = r.sources.read();
                let s = (sources.iter())
                    .find(|s| s.id.0 == source)
                    .context("this source is no longer in the library")?;
                (s.label.clone(), s.root.clone(), s.status.clone())
            }
        };
        if let SourceStatus::Offline { last_seen, reason } = status {
            bail!("{label} is {}", offline_text(last_seen, reason));
        }
        Ok(if rel.is_empty() { root } else { root.join(rel) })
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        match self {
            LibraryBackend::InProcess(lib) => Ok(lib.stats()),
            LibraryBackend::Daemon(r) => {
                let s: api::LibraryStats = r.call("library.stats", json!({}))?;
                Ok(LibraryStats {
                    sources: s.sources,
                    offline_sources: s.offline_sources,
                    records: s.records,
                    files: s.files,
                    bytes: s.bytes,
                    unique_content: s.unique_content,
                    running_jobs: s.running_jobs,
                    per_source: s
                        .per_source
                        .into_iter()
                        .map(|p| keel_core::SourceStats {
                            id: SourceId(p.id),
                            label: p.label,
                            files: p.files,
                            folders: p.folders,
                            bytes: p.bytes,
                            hashed_files: p.hashed_files,
                            last_walk: p.last_walk,
                            offline: p.offline,
                        })
                        .collect(),
                })
            }
        }
    }

    pub fn protection(&self) -> Result<(ProtectionSummary, Vec<Volume>)> {
        match self {
            LibraryBackend::InProcess(lib) => Ok((lib.protection_summary()?, lib.volumes()?)),
            LibraryBackend::Daemon(r) => {
                let p: api::Protection = r.call("protection.summary", json!({}))?;
                let v: Vec<api::VolumeInfo> = r.call("volumes.list", json!({}))?;
                let summary = ProtectionSummary {
                    single_copy: p.single_copy,
                    single_domain: p.single_domain,
                    unbacked: p.unbacked,
                    drifted: p.drifted,
                    unchecked: p.unchecked,
                    offline_volumes: p.offline_volumes,
                    capacity: Vec::new(),
                };
                Ok((summary, v.into_iter().map(volume).collect()))
            }
        }
    }

    /// Tags, saved views and the tags of every tagged path (by real path).
    pub fn meta(&self) -> Result<Meta> {
        let mut tagged: HashMap<VPath, Vec<TagId>> = HashMap::new();
        match self {
            LibraryBackend::InProcess(lib) => {
                let tags = lib.tags().unwrap_or_default();
                let views = lib.views().unwrap_or_default();
                for tag in tags.iter().map(|t| t.id).chain([FAVORITES]) {
                    for hit in lib.records_with_tag(tag).unwrap_or_default() {
                        tagged.entry(hit.path).or_default().push(tag);
                    }
                }
                Ok((tags, views, tagged))
            }
            LibraryBackend::Daemon(r) => {
                let tags: Vec<api::TagInfo> = r.call("tags.list", json!({}))?;
                let views: Vec<api::ViewInfo> = r.call("views.list", json!({}))?;
                let paths: Vec<api::TaggedPath> = r.call("tags.tagged", json!({}))?;
                for p in paths {
                    tagged.insert(vpath_of(&p.path), p.tags);
                }
                let tags = (tags.into_iter())
                    .map(|t| Tag {
                        id: t.id,
                        name: t.name,
                        color: t.color,
                        parent: t.parent,
                    })
                    .collect();
                let views = (views.into_iter())
                    .map(|v| View {
                        id: v.id,
                        name: v.name,
                        query: v.query,
                        layout: v.layout,
                    })
                    .collect();
                Ok((tags, views, tagged))
            }
        }
    }

    /// Duplicate groups of at least `min_size` bytes, each copy resolved for display.
    pub fn dups(&self, min_size: u64) -> Result<Vec<DupGroup>> {
        match self {
            LibraryBackend::InProcess(lib) => crate::library::load_dups(lib, min_size),
            LibraryBackend::Daemon(r) => {
                let groups: Vec<api::DupGroup> = r.call(
                    "duplicates",
                    json!({"min_size": min_size.max(1), "max": MAX_GROUPS}),
                )?;
                let sources = r.sources.read().clone();
                Ok(groups
                    .into_iter()
                    .map(|g| DupGroup {
                        size: g.size,
                        records: (g.paths.iter())
                            .map(|p| {
                                let path = vpath_of(p);
                                let source = source_of(&sources, &path).map(|(s, _)| s);
                                DupRecord {
                                    source: source.map(|s| s.label.clone()).unwrap_or_default(),
                                    offline: source.is_some_and(|s| {
                                        matches!(s.status, SourceStatus::Offline { .. })
                                    }),
                                    path,
                                }
                            })
                            .collect(),
                    })
                    .filter(|g| g.records.len() > 1)
                    .collect())
            }
        }
    }

    /// Settings → Library → Hashing; `on` also starts hashing (or keeps the running job).
    /// `remote`: a new remote policy (None keeps the library's).
    pub fn set_hashing(
        &self,
        on: bool,
        idle_only: bool,
        remote: Option<keel_core::RemoteHashSettings>,
    ) -> Result<Option<JobId>> {
        match self {
            LibraryBackend::InProcess(lib) => {
                if let Some(remote) = remote {
                    lib.set_remote_hash_settings(remote)?;
                }
                lib.set_hash_after_walk(on);
                lib.set_hash_idle_only(idle_only);
                Ok(None)
            }
            LibraryBackend::Daemon(r) => {
                let mut params = json!({"on": on, "idle_only": idle_only});
                if let Some(remote) = remote {
                    params["remote"] = json!(remote.hash_remote);
                    params["cloud"] = json!(remote.hash_cloud);
                    params["max_remote_bytes"] = json!(remote.remote_hash_max_bytes);
                }
                Ok(r.apply("hashing.set", params)?.job)
            }
        }
    }

    /// The library's remote hashing policy (None: a daemon from before 0.12 does not say).
    pub fn remote_hashing(&self) -> Result<Option<keel_core::RemoteHashSettings>> {
        match self {
            LibraryBackend::InProcess(lib) => Ok(Some(lib.remote_hash_settings())),
            LibraryBackend::Daemon(r) => {
                let s: api::LibraryStats = r.call("library.stats", json!({}))?;
                Ok(s.hashing.map(|h| keel_core::RemoteHashSettings {
                    hash_remote: h.remote,
                    hash_cloud: h.cloud,
                    remote_hash_max_bytes: h.max_remote_bytes,
                }))
            }
        }
    }

    /// Starts hashing now; a daemon keeps its `idle_only` policy (the window's).
    pub fn hash(&self, idle_only: bool) -> Result<JobId> {
        match self {
            LibraryBackend::InProcess(lib) => lib.hash(),
            LibraryBackend::Daemon(r) => r
                .apply("hashing.set", json!({"on": true, "idle_only": idle_only}))?
                .job
                .context("no hash job"),
        }
    }

    pub fn media_job(&self, id: &SourceId) -> Result<JobId> {
        match self {
            LibraryBackend::InProcess(lib) => lib.media_job(id),
            LibraryBackend::Daemon(r) => job(r.apply("media.index", json!({"id": id.0}))?),
        }
    }

    pub fn integrity(&self, pct: f64) -> Result<JobId> {
        match self {
            LibraryBackend::InProcess(lib) => lib.integrity(None, pct),
            LibraryBackend::Daemon(r) => {
                job(r.apply("integrity.check", json!({ "sample_pct": pct }))?)
            }
        }
    }

    pub fn schedule_integrity(&self, pct: f64, days: u32) -> Result<Option<JobId>> {
        match self {
            LibraryBackend::InProcess(lib) => {
                lib.schedule_integrity(pct, Duration::from_secs(u64::from(days) * 24 * 60 * 60))
            }
            LibraryBackend::Daemon(r) => Ok(r
                .apply(
                    "integrity.check",
                    json!({"sample_pct": pct, "due_days": days}),
                )?
                .job),
        }
    }

    /// Indexes a source; `adopt`: first accept whatever is at its root.
    pub fn index(&self, id: &SourceId, adopt: bool) -> Result<JobId> {
        match self {
            LibraryBackend::InProcess(lib) => {
                if let (Some(src), true) = (lib.source(id), adopt) {
                    Indexer::adopt_root(&src)?;
                }
                lib.index(id)
            }
            LibraryBackend::Daemon(r) => {
                job(r.apply("sources.index", json!({"id": id.0, "adopt": adopt}))?)
            }
        }
    }

    pub fn add_source(&self, def: SourceDef) -> Result<JobId> {
        match self {
            LibraryBackend::InProcess(lib) => {
                let id = lib.add_source(def)?;
                lib.index(&id)
            }
            LibraryBackend::Daemon(r) => {
                let kind = match def.kind {
                    SourceKind::Folder => "folder",
                    SourceKind::Drive => "drive",
                    SourceKind::Share => "share",
                    SourceKind::Cloud => "cloud",
                    SourceKind::Device => "device",
                };
                let added = r.apply(
                    "sources.add",
                    json!({"root": def.root.display(), "label": def.label, "kind": kind,
                        "include_hidden": def.include_hidden, "ignore": def.ignore}),
                )?;
                let id = (added.result.as_ref())
                    .and_then(|v| v["id"].as_str())
                    .context("sources.add: no id")?
                    .to_owned();
                self.index(&SourceId(id), false)
            }
        }
    }

    /// Forgets a source and deletes its index store (the files are not touched).
    pub fn remove_source(&self, id: &SourceId) -> Result<()> {
        match self {
            LibraryBackend::InProcess(lib) => lib.remove_source(id, true),
            LibraryBackend::Daemon(r) => {
                r.apply("sources.remove", json!({"id": id.0, "delete_store": true}))?;
                Ok(())
            }
        }
    }

    pub fn plan(&self, op: Op) -> Result<Planned> {
        match self {
            LibraryBackend::InProcess(lib) => Ok(Planned::Local(
                keel_core::validate_preview_execute(lib, op)?,
            )),
            LibraryBackend::Daemon(r) => Ok(Planned::Daemon(r.call("plan", plan_params(&op))?)),
        }
    }

    /// Runs the confirmed plan as a job.
    pub fn execute(&self, planned: Planned) -> Result<Executed> {
        match (self, planned) {
            (LibraryBackend::InProcess(lib), Planned::Local(plan)) => {
                match plan.execute(lib, true) {
                    Ok(id) => Ok(Executed::Job(id)),
                    Err(e) => match e.downcast::<PlanChanged>() {
                        Ok(PlanChanged(fresh)) => Ok(Executed::Changed(Planned::Local(*fresh))),
                        Err(e) => Err(e),
                    },
                }
            }
            (LibraryBackend::Daemon(r), Planned::Daemon(p)) => {
                let params = json!({"plan_id": p.plan_id, "input_hash": p.input_hash});
                match r.call_raw("execute", params) {
                    Ok(v) => job(serde_json::from_value(v)?).map(Executed::Job),
                    Err(e) if e.code == ApiError::PLAN_CHANGED => {
                        let fresh = e.data.context("no fresh preview")?;
                        Ok(Executed::Changed(Planned::Daemon(serde_json::from_value(
                            fresh,
                        )?)))
                    }
                    Err(e) => bail!("{}", e.message),
                }
            }
            _ => bail!("the library changed since the preview: preview again"),
        }
    }

    pub fn cancel(&self, id: JobId) -> Result<()> {
        match self {
            LibraryBackend::InProcess(lib) => lib.jobs().cancel(id),
            LibraryBackend::Daemon(r) => {
                r.apply("jobs.cancel", json!({ "id": id }))?;
                Ok(())
            }
        }
    }

    /// An opened file counts for Recents.
    pub fn note_open(&self, sources: &[SourceSummary], path: &VPath) -> Result<()> {
        match self {
            LibraryBackend::InProcess(lib) => {
                let (source, rel) =
                    crate::library::locate(sources, path).context("not in a library source")?;
                lib.note_open(&record_of(lib, &source, &rel)?)
            }
            LibraryBackend::Daemon(r) => {
                let real = real_of(sources, path).context("not in a library source")?;
                r.call::<api::Done>("recents.note", json!({"path": real.display()}))?;
                Ok(())
            }
        }
    }

    /// Creates a tag (and puts it on `targets`, when given).
    pub fn create_tag(
        &self,
        sources: &[SourceSummary],
        name: &str,
        color: &str,
        targets: Option<&[VPath]>,
    ) -> Result<()> {
        match self {
            LibraryBackend::InProcess(lib) => {
                let tag = lib.create_tag(name, Some(color), None)?;
                if let Some(targets) = targets {
                    crate::library::apply_tag(lib, sources, targets, tag, true)?;
                }
                Ok(())
            }
            LibraryBackend::Daemon(r) => {
                // No paths: `tags.add` only creates the tag.
                let paths = real_paths(sources, targets.unwrap_or_default())?;
                r.apply(
                    "tags.add",
                    json!({"tag": name, "paths": paths, "color": color}),
                )?;
                Ok(())
            }
        }
    }

    /// Tags (or untags) `targets` (real or `library://` paths); `tags` names the tag.
    pub fn set_tag(
        &self,
        sources: &[SourceSummary],
        tags: &[Tag],
        targets: &[VPath],
        tag: TagId,
        on: bool,
    ) -> Result<()> {
        match self {
            LibraryBackend::InProcess(lib) => {
                crate::library::apply_tag(lib, sources, targets, tag, on)
            }
            LibraryBackend::Daemon(r) => {
                let paths = real_paths(sources, targets)?;
                if tag == FAVORITES {
                    r.apply("favorites.set", json!({"paths": paths, "on": on}))?;
                } else {
                    let name = (tags.iter().find(|t| t.id == tag))
                        .map(|t| t.name.clone())
                        .context("that tag is gone")?;
                    let method = if on { "tags.add" } else { "tags.remove" };
                    r.apply(method, json!({"tag": name, "paths": paths}))?;
                }
                Ok(())
            }
        }
    }

    pub fn set_volume(&self, volume: &str, change: VolumeChange) -> Result<()> {
        match self {
            LibraryBackend::InProcess(lib) => match change {
                VolumeChange::State(s) => lib.set_volume_state(volume, s),
                VolumeChange::Backup(on) => lib.set_backup(volume, on),
                VolumeChange::Domain(d) => lib.set_failure_domain(volume, Some(&d)),
            },
            LibraryBackend::Daemon(r) => {
                let mut params = json!({ "volume": volume });
                match change {
                    VolumeChange::State(s) => {
                        params["state"] = json!(crate::library::state_text(s));
                    }
                    VolumeChange::Backup(on) => params["backup"] = json!(on),
                    VolumeChange::Domain(d) => params["failure_domain"] = json!(d),
                }
                r.apply("volumes.set", params)?;
                Ok(())
            }
        }
    }

    /// Copies badges of the files in `dirs` (real paths), by real path.
    pub fn badges(&self, sources: &[SourceSummary], dirs: &[VPath]) -> HashMap<VPath, Badge> {
        let mut out = HashMap::new();
        for dir in dirs {
            match self {
                LibraryBackend::InProcess(lib) => {
                    let Some((source, rel)) = crate::library::locate(sources, dir) else {
                        continue;
                    };
                    let Ok(children) = lib.list_children(&source, &rel) else {
                        continue;
                    };
                    let files: Vec<_> = (children.into_iter())
                        .filter(|h| !h.is_dir)
                        .take(MAX_BADGES)
                        .collect();
                    let records: Vec<_> = files.iter().map(|h| h.record.clone()).collect();
                    // The volumes are read once per folder, not once per file.
                    let Ok(all) = lib.redundancies(&records) else {
                        continue;
                    };
                    for (h, r) in files.into_iter().zip(all) {
                        if let Some(r) = r {
                            out.insert(h.path, crate::library::badge_of(&r.into()));
                        }
                    }
                }
                LibraryBackend::Daemon(r) => {
                    let Some(real) = real_of(sources, dir) else {
                        continue;
                    };
                    let Ok(files) = r.call::<Vec<api::FileCopies>>(
                        "redundancy.folder",
                        json!({"path": real.display()}),
                    ) else {
                        continue;
                    };
                    for f in files {
                        out.insert(vpath_of(&f.path), crate::library::badge_of(&f.copies));
                    }
                }
            }
        }
        out
    }

    /// Kinds of every job (for rows that arrived as bare events).
    pub fn job_kinds(&self) -> Result<Vec<(JobId, String)>> {
        Ok(self
            .jobs()?
            .into_iter()
            .map(|(id, row)| (id, row.kind))
            .collect())
    }

    /// Every job as a jobs panel row.
    pub fn jobs(&self) -> Result<Vec<(JobId, JobRow)>> {
        let row = |kind: String, status, progress| JobRow {
            kind,
            status,
            progress,
            ended: None,
        };
        Ok(match self {
            LibraryBackend::InProcess(lib) => (lib.jobs().list()?.into_iter())
                .map(|j| (j.id, row(j.kind, j.status, j.progress)))
                .collect(),
            LibraryBackend::Daemon(r) => (r.call::<Vec<api::JobInfo>>("jobs.list", json!({}))?)
                .into_iter()
                .map(|j| (j.id, row(j.kind, job_status(&j.status), j.progress)))
                .collect(),
        })
    }

    /// Library search hits: a query, Favorites or Recents.
    pub fn search(&self, q: &keel_search::Query) -> Result<Vec<keel_search::Hit>> {
        use crate::library::{FAVORITES_QUERY, RECENTS_QUERY};
        let LibraryBackend::Daemon(r) = self else {
            bail!("in-process search goes through LibSearch");
        };
        let hits: Vec<api::Hit> = match q.text.trim() {
            FAVORITES_QUERY => r.call("favorites.list", json!({}))?,
            RECENTS_QUERY => r.call("recents", json!({ "limit": q.max.max(1) }))?,
            text => r.call("search", json!({"query": text, "max": q.max.max(1)}))?,
        };
        Ok(hits
            .into_iter()
            .map(|h| keel_search::Hit {
                path: vpath_of(&h.path),
                is_dir: h.is_dir,
                size: h.size,
                modified: crate::library::entry_of_time(h.modified),
            })
            .collect())
    }
}

/// A started job's id.
fn job(done: api::Executed) -> Result<JobId> {
    done.job.context("no job was started")
}

/// Real paths of `targets` as the API takes them.
fn real_paths(sources: &[SourceSummary], targets: &[VPath]) -> Result<Vec<String>> {
    targets
        .iter()
        .map(|t| {
            real_of(sources, t)
                .map(|p| p.display())
                .with_context(|| format!("{} is not in a library source", t.display()))
        })
        .collect()
}

/// An API path: a URI, else a local path.
pub fn vpath_of(s: &str) -> VPath {
    match s.contains("://") {
        true => VPath::parse(s).unwrap_or_else(|_| VPath::local(s)),
        false => VPath::local(s),
    }
}

pub fn job_status(s: &str) -> JobStatus {
    match s {
        "queued" => JobStatus::Queued,
        "running" => JobStatus::Running,
        "done" => JobStatus::Done,
        "cancelled" => JobStatus::Cancelled,
        _ => JobStatus::Failed,
    }
}

/// `sources.list` as the window's sidebar reads it.
pub fn source_summary(s: &api::SourceInfo) -> SourceSummary {
    let detail = s.detail.as_deref().unwrap_or_default();
    let status = match s.status.as_str() {
        "online" => SourceStatus::Online {
            indexed_at: s.indexed_at,
        },
        "indexing" => {
            // "<done> of about <total> records"
            let mut n = detail
                .split_whitespace()
                .filter_map(|w| w.parse::<u64>().ok());
            SourceStatus::Indexing {
                done: n.next().unwrap_or(0),
                total: n.next().unwrap_or(0),
            }
        }
        "offline" => SourceStatus::Offline {
            last_seen: s.last_seen,
            reason: match detail {
                "rootmismatch" => OfflineReason::RootMismatch,
                "empty" => OfflineReason::Empty,
                _ => OfflineReason::Unreachable,
            },
        },
        _ => SourceStatus::Error(detail.to_owned()),
    };
    SourceSummary {
        id: SourceId(s.id.clone()),
        label: s.label.clone(),
        root: vpath_of(&s.root),
        kind: match s.kind {
            api::SourceKindName::Folder => SourceKind::Folder,
            api::SourceKindName::Drive => SourceKind::Drive,
            api::SourceKindName::Share => SourceKind::Share,
            api::SourceKindName::Cloud => SourceKind::Cloud,
            api::SourceKindName::Device => SourceKind::Device,
        },
        status,
        generation: s.generation,
    }
}

fn volume(v: api::VolumeInfo) -> Volume {
    Volume {
        id: v.id,
        label: v.label,
        kind: match v.kind {
            api::VolumeKindName::Fixed => VolumeKind::Fixed,
            api::VolumeKindName::Removable => VolumeKind::Removable,
            api::VolumeKindName::Network => VolumeKind::Network,
            api::VolumeKindName::Cloud => VolumeKind::Cloud,
            api::VolumeKindName::Device => VolumeKind::Device,
        },
        failure_domain: v.failure_domain,
        domain_set: v.domain_set,
        state: volume_state(v.state),
        last_seen: v.last_seen,
        backup: v.backup,
        capacity: v.used.zip(v.total),
    }
}

pub fn volume_state(s: api::VolumeStateName) -> VolumeState {
    match s {
        api::VolumeStateName::Online => VolumeState::Online,
        api::VolumeStateName::Offline => VolumeState::Offline,
        api::VolumeStateName::Archived => VolumeState::Archived,
        api::VolumeStateName::Lost => VolumeState::Lost,
        api::VolumeStateName::Retired => VolumeState::Retired,
    }
}

/// An `Op` as `plan` parameters.
fn plan_params(op: &Op) -> Value {
    let conflict = |c: &keel_core::OnConflict| match c {
        keel_core::OnConflict::Skip => "skip",
        keel_core::OnConflict::Overwrite => "overwrite",
        keel_core::OnConflict::RenameNew => "rename_new",
    };
    let paths = |v: &[VPath]| v.iter().map(VPath::display).collect::<Vec<_>>();
    match op {
        Op::Copy {
            src,
            dst_dir,
            on_conflict,
        } => json!({"op": "copy", "paths": paths(src), "to": dst_dir.display(),
            "on_conflict": conflict(on_conflict)}),
        Op::Move {
            src,
            dst_dir,
            on_conflict,
        } => json!({"op": "move", "paths": paths(src), "to": dst_dir.display(),
            "on_conflict": conflict(on_conflict)}),
        Op::Delete { paths: p } => json!({"op": "delete", "paths": paths(p)}),
        Op::Rename { path, new_name } => {
            json!({"op": "rename", "paths": [path.display()], "new_name": new_name})
        }
    }
}

/// Starts `keel-daemon --profile=<name>` detached (it outlives the window), logging to
/// `<config dir>/daemon.log`.
pub fn spawn_daemon(cfg: &HostConfig) -> Result<Spawned> {
    let (child, log) = crate::commands::spawn_daemon(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Spawned {
        child: Some(child),
        log,
    })
}

/// A keel-daemon this window started. Dropped while it runs, it is reaped when it ends
/// (no zombie on Unix).
pub struct Spawned {
    child: Option<std::process::Child>,
    log: std::path::PathBuf,
}

impl Spawned {
    /// Why it already ended (its exit status and the end of its log), else None.
    pub fn exited(&mut self) -> Option<String> {
        let status = self.child.as_mut()?.try_wait().ok().flatten()?;
        self.child = None;
        Some(match log_tail(&self.log) {
            tail if tail.is_empty() => format!("keel-daemon exited ({status})"),
            tail => format!("keel-daemon exited ({status}): {tail}"),
        })
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(mut child) = self.child.take() {
            let reaper = std::thread::Builder::new().name("keel-daemon-reaper".into());
            let _ = reaper.spawn(move || child.wait());
        }
    }
}

/// The last two lines of `log` (read from its last 4 KB), at most 300 characters.
fn log_tail(log: &std::path::Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut text = Vec::new();
    if let Ok(mut f) = std::fs::File::open(log) {
        let len = f.metadata().map_or(0, |m| m.len());
        let _ = f.seek(SeekFrom::Start(len.saturating_sub(4096)));
        let _ = f.read_to_end(&mut text);
    }
    let text = String::from_utf8_lossy(&text);
    let mut lines: Vec<&str> = (text.lines().map(str::trim))
        .filter(|l| !l.is_empty())
        .rev()
        .take(2)
        .collect();
    lines.reverse();
    let tail = lines.join(" | ");
    match tail.char_indices().nth(300) {
        Some((i, _)) => format!("{}...", &tail[..i]),
        None => tail,
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
