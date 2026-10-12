//! Keel's own Windows file index. Every fixed NTFS volume is read straight from the
//! MFT (administrator only): the raw `FILE` records through the volume handle
//! ([`crate::mft`]: names, hard links, sizes, dates, attributes, streams), or, when
//! that read is refused or fails, `FSCTL_ENUM_USN_DATA` (names and attributes only).
//! The elevated helper only reads: it hands the raw records to the unelevated
//! process, which parses them and builds the index ([`crate::wire`], `receive`).
//! The index is persisted to `<cache>/index/<serial>.db`. Any later start loads that db and catches up from
//! the USN journal, then polls it every 2 s; this works without elevation too
//! (`FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` on a root-folder handle, names restored by
//! opening each new file by id). A changed journal id or a truncated journal means
//! a rebuild (elevated) or a stale-index note (not elevated). With no saved index
//! and no elevation it falls back to walking the user's folders (`walk`);
//! [`request_full_index`] runs the MFT read elevated (`keel --index-service`).

mod acl;
mod db;
mod index;
mod pattern;
mod receive;
mod service;
mod svcwin;
mod usn;
mod walk;
mod win;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use parking_lot::{Mutex, RwLock};

use crate::{mft, wire};
use crate::{Hit, Query, SearchState, Searcher};
use db::Meta;
use index::{rank, Found, Index, Info};
use usn::Changes;
use win::{Journal, Volume, VolumeId};

pub use acl::set_hide_unlistable;
pub use svcwin::{
    install_service, request_service_change, run_service, service_state, uninstall_service,
    ServiceState,
};
pub(crate) use win::file_meta;
pub use win::is_elevated;

/// Shown while only the user folders are indexed.
pub const FALLBACK_STATUS: &str = "Indexing user folders only; run 'Index all drives' as \
     administrator for everything.";
/// Shown when a saved volume index went stale (journal recreated or wrapped) and
/// only an elevated rebuild can refresh it.
pub const FROZEN_STATUS: &str = "The drive index is out of date; run 'Index all drives' as \
     administrator to refresh it.";
const POLL: Duration = Duration::from_secs(2);
/// Longest the walk index stays write-locked while watcher changes are applied;
/// searches get the lock in between batches.
const BATCH: Duration = Duration::from_millis(20);
/// Least time between two full re-walks after lost watcher events.
const RESCAN_GAP: Duration = Duration::from_secs(60);
/// Hits beyond this many skip the per-file size/date lookup (a huge `max`).
const STAT_LIMIT: usize = 2_000;
/// Most changed files whose size and dates one journal poll looks up by file id; the
/// rest keep what the index had (a huge batch, such as an archive unpacked).
const STAT_BATCH: usize = 20_000;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
/// Least time between two tries to reach the index service.
const SERVICE_RETRY: Duration = Duration::from_secs(60);
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;

/// Where the index lives: `KEEL_INDEX_DIR`, else `KEEL_CONFIG_DIR\index` (portable
/// and test setups), else `%TEMP%\keel-test-index` inside a cargo test binary, else
/// `%LOCALAPPDATA%\Keel\index`.
pub fn index_dir() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|d| !d.is_empty());
    if let Some(dir) = var("KEEL_INDEX_DIR") {
        return Some(dir.into());
    }
    if let Some(dir) = var("KEEL_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("index"));
    }
    if in_test_binary() {
        return Some(std::env::temp_dir().join("keel-test-index"));
    }
    Some(
        directories::BaseDirs::new()?
            .cache_dir()
            .join("Keel")
            .join("index"),
    )
}

// ponytail: cargo puts test binaries in `target\<profile>\deps`, so no test (here or
// in keel-app, which builds whole Apps) can reach the real index; an explicit
// `KEEL_INDEX_DIR` still wins.
fn in_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.file_name().map(|d| d == "deps"))
        .unwrap_or(false)
}

/// Indexes every fixed NTFS volume in full: relaunches this executable as `keel
/// --index-service <pipe>` through the UAC prompt, and builds each volume's index
/// here, unelevated, from the raw MFT records the helper sends over the pipe
/// ([`crate::wire`]), writing `<serial>.db.svc`; a running [`NtfsSearcher`] picks
/// them up within seconds. Blocks: call off the UI thread.
pub fn request_full_index() -> anyhow::Result<()> {
    let dir = index_dir().ok_or_else(|| anyhow!("no cache folder"))?;
    std::fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe()?;
    let starts = starts(&win::fixed_ntfs_volumes());
    if starts.is_empty() {
        bail!("no fixed NTFS volume to index");
    }
    let (name, pipe) = win::helper_pipe().context("creating the index helper's pipe")?;
    let launch = {
        let name = name.clone();
        std::thread::spawn(move || {
            let code = win::run_elevated(&exe, &format!("--index-service \"{name}\""));
            // The helper is gone: wake the accept below if it never connected.
            win::poke_pipe(&name);
            code
        })
    };
    let received = match win::accept_pipe(&pipe) {
        Ok(()) => receive_all(&dir, starts, pipe),
        Err(e) => {
            drop(pipe);
            Err(e.into())
        }
    };
    let code = launch
        .join()
        .map_err(|_| anyhow!("the index service launcher panicked"))?
        .context("starting the elevated index service")?;
    match received {
        Err(e) if code != 0 => Err(e.context(format!("the index service exited with code {code}"))),
        other => other,
    }
}

/// Reads the helper's stream from `pipe` and saves each volume it sends as
/// `<serial>.db.svc` in `dir`. Errors when no volume could be saved; problems with
/// single volumes are logged.
fn receive_all(
    dir: &Path,
    starts: Vec<receive::Start>,
    mut pipe: impl std::io::Read,
) -> anyhow::Result<()> {
    let mut saved = 0;
    let mut problems: Vec<String> = Vec::new();
    let mut rx = receive::Receiver::new(starts, |id, index, meta| {
        let db = db_path(dir, id).with_extension("db.svc");
        match db::save_full(&db, &index, &meta) {
            Ok(()) => saved += 1,
            Err(e) => problems.push(format!("saving {}: {e:#}", db.display())),
        }
    });
    let mut buf = Vec::new();
    let read: anyhow::Result<()> = loop {
        match wire::read_frame(&mut pipe, &mut buf) {
            Ok(true) => {
                if let Err(e) = rx.frame(&buf) {
                    break Err(e);
                }
            }
            Ok(false) if rx.complete => break Ok(()),
            Ok(false) => break Err(anyhow!("the index service stopped before it was done")),
            Err(e) => break Err(e.into()),
        }
    };
    // Closing the pipe stops a helper still writing.
    drop(pipe);
    let failed = std::mem::take(&mut rx.failed);
    drop(rx);
    if let Err(e) = read {
        problems.push(format!("{e:#}"));
    }
    problems.extend(failed);
    for p in &problems {
        warn(p);
    }
    match problems.into_iter().next() {
        Some(first) if saved == 0 => Err(anyhow!(first)),
        _ if saved == 0 => Err(anyhow!("the index service read no volume")),
        _ => Ok(()),
    }
}

/// Body of `keel --index-service <pipe>` (elevated): connects to the pipe the
/// unelevated Keel made and sends it every fixed NTFS volume's raw MFT records
/// ([`wire::send_volume`]). This process only locates the MFT and blanks what the
/// index never reads; it builds nothing and writes no file.
pub fn run_index_service(pipe: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    if !is_elevated() {
        bail!("the index service needs administrator rights");
    }
    let name = pipe
        .to_str()
        .ok_or_else(|| anyhow!("not an index helper pipe"))?;
    let pipe = win::open_helper_pipe(name).context("connecting to Keel")?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, pipe);
    let mut send = |frame: &[u8]| out.write_all(frame);
    for id in win::fixed_ntfs_volumes() {
        match Volume::open(id.letter) {
            Ok(volume) if volume.privileged() => {
                wire::send_volume(&volume, id.letter, id.serial, &mut send)?;
            }
            _ => {
                let header = wire::Frame::Volume {
                    letter: id.letter,
                    serial: id.serial,
                };
                send(&header.encode())?;
                send(&wire::Frame::Failed("cannot open the volume".into()).encode())?;
            }
        }
    }
    send(&wire::Frame::Done.encode())?;
    out.flush()?;
    Ok(())
}

/// Each volume's journal position and root folder, taken before the MFT is read so
/// changes made meanwhile are replayed afterwards. A volume this process cannot ask
/// is left out.
fn starts(ids: &[VolumeId]) -> Vec<receive::Start> {
    ids.iter()
        .filter_map(|&id| {
            let volume = Volume::open(id.letter).ok()?;
            match start_of(id, &volume) {
                Ok(start) => Some(start),
                Err(e) => {
                    warn(&format!("indexing {}: {e:#}", id.drive()));
                    None
                }
            }
        })
        .collect()
}

fn start_of(id: VolumeId, volume: &Volume) -> anyhow::Result<receive::Start> {
    let journal = match volume.query_journal() {
        Ok(j) => j,
        // No journal: a snapshot that is never updated.
        Err(e) if e.raw_os_error() == Some(ERROR_JOURNAL_NOT_ACTIVE) => Journal {
            id: 0,
            next_usn: 0,
            lowest_valid_usn: 0,
        },
        Err(e) => return Err(e.into()),
    };
    Ok(receive::Start {
        id,
        root: win::root_frn(id.letter)?,
        journal,
    })
}

fn db_path(dir: &Path, id: VolumeId) -> PathBuf {
    dir.join(format!("{:08X}.db", id.serial))
}

/// Copies finished `*.db.svc` files over their `*.db` (the copy is owned by this
/// user) and returns the db paths replaced.
fn adopt(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut adopted = Vec::new();
    for svc in read.flatten().map(|e| e.path()) {
        if svc.extension().is_none_or(|e| e != "svc") {
            continue;
        }
        let db = svc.with_extension(""); // X.db.svc -> X.db
        let tmp = svc.with_extension("adopt");
        let ok = std::fs::copy(&svc, &tmp).is_ok() && {
            for stale in ["db-wal", "db-shm"] {
                let _ = std::fs::remove_file(db.with_extension(stale));
            }
            std::fs::rename(&tmp, &db).is_ok()
        };
        if ok {
            let _ = std::fs::remove_file(&svc);
            adopted.push(db);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    adopted
}

/// System files in records 0 to 15 (`$MFT` to `$Extend`; 5 is the root folder) are
/// not indexed; what lives under `$Extend` is then unreachable and never listed.
const FIRST_USER_RECORD: u64 = 16;
const ROOT_RECORD: u64 = 5;

/// Puts one file read from the MFT in the index (its first name; the others are
/// counted as hard links). Every entry is stored whatever the user may list: the
/// per-user filter (a folder the user cannot open hides what is under it) is applied
/// to results by the index service, branch `p15-service`, not here.
fn add_entry(index: &mut Index, entry: &mft::Entry) {
    let number = entry.frn & mft::RECORD_MASK;
    if number < FIRST_USER_RECORD && number != ROOT_RECORD {
        return;
    }
    let Some(name) = entry.names.first() else {
        return;
    };
    index.upsert(entry.frn, name.parent, &name.name, entry.is_dir);
    let times = &entry.standard;
    // Data kept inside the record takes no clusters; Windows reports its size rounded
    // up to 8 bytes, and so does the index, matching the journal's lookups by id.
    // A WOF-compressed file takes the clusters of its compressed stream.
    let allocated = if entry.data.resident {
        entry.data.size.next_multiple_of(8)
    } else if entry.reparse == mft::IO_REPARSE_TAG_WOF && entry.wof > 0 {
        entry.wof
    } else {
        entry.data.allocated
    };
    index.set_info(
        entry.frn,
        Info {
            size: entry.data.size,
            allocated,
            created: times.created as i64,
            modified: times.modified as i64,
            accessed: times.accessed as i64,
            changed: 0,
            attributes: entry.attributes(),
            links: entry.links(),
            streams: entry.streams,
            reparse: entry.reparse,
        },
    );
}

/// Reads one volume's MFT into a fresh index, in this (elevated) process: the same
/// raw records and the same checks as through the helper's pipe, else
/// `FSCTL_ENUM_USN_DATA`. The journal position is taken first, so changes made
/// during the read are replayed afterwards.
fn build_volume(id: VolumeId) -> anyhow::Result<(Index, Meta)> {
    let volume = Volume::open(id.letter)?;
    if !volume.privileged() {
        bail!(
            "reading the MFT of {} needs administrator rights",
            id.drive()
        );
    }
    let start = start_of(id, &volume)?;
    let mut built = None;
    let mut rx = receive::Receiver::new(vec![start], |_, index, meta| built = Some((index, meta)));
    wire::send_volume(&volume, id.letter, id.serial, &mut |frame| {
        rx.frame(&frame[4..])
            .map_err(|e| std::io::Error::other(format!("{e:#}")))
    })?;
    let failed = rx.failed.pop();
    drop(rx);
    built.ok_or_else(|| anyhow!(failed.unwrap_or_else(|| "nothing was read".into())))
}

/// [`build_volume`] through the index service (not elevated): the service sends the
/// MFT's raw records (it parses nothing) and this process parses, checks and indexes
/// them exactly as it does for the one-shot helper. The journal position is taken
/// first, so changes made during the read are replayed afterwards.
fn build_volume_remote(
    id: VolumeId,
    client: &mut service::Client<std::fs::File>,
) -> std::io::Result<(Index, Meta)> {
    let journal = match client.query_journal(id.letter) {
        Ok(j) => j,
        Err(e) if e.raw_os_error() == Some(ERROR_JOURNAL_NOT_ACTIVE) => Journal {
            id: 0,
            next_usn: 0,
            lowest_valid_usn: 0,
        },
        Err(e) => return Err(e),
    };
    let root = win::root_frn(id.letter)?;
    client.read_mft(receive::Start { id, root, journal })
}

/// What a saved index needs to become current.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resume {
    /// Replay the journal from this USN.
    From(i64),
    /// The journal was recreated or has wrapped past the saved position.
    Rebuild,
}

pub(crate) fn resume(meta: &Meta, journal: &Journal) -> Resume {
    if meta.journal_id != journal.id
        || meta.next_usn < journal.lowest_valid_usn
        || meta.next_usn > journal.next_usn
    {
        Resume::Rebuild
    } else {
        Resume::From(meta.next_usn)
    }
}

enum Part {
    Volume {
        index: Index,
        id: VolumeId,
        meta: Meta,
        db: PathBuf,
    },
    Walk(walk::Walk),
}

impl Part {
    fn index(&self) -> &Index {
        match self {
            Part::Volume { index, .. } => index,
            Part::Walk(walk) => &walk.index,
        }
    }

    fn serial(&self) -> Option<u32> {
        match self {
            Part::Volume { id, .. } => Some(id.serial),
            Part::Walk(_) => None,
        }
    }
}

struct Shared {
    dir: PathBuf,
    /// One index per NTFS volume, or the single user-folder walk.
    parts: RwLock<Vec<Part>>,
    status: Mutex<Option<String>>,
    ready: AtomicBool,
    /// Entries found so far by the user-folder walk (before it is `ready`).
    done: AtomicUsize,
    /// An index was built or swapped in since the last `take_ready`.
    fresh: AtomicBool,
}

impl Shared {
    /// A finished index is in `parts`: searchable now, and the app is told.
    fn finished(&self) {
        self.ready.store(true, Ordering::Relaxed);
        self.fresh.store(true, Ordering::Release);
    }
}

/// Searcher over Keel's own index (see the module docs).
pub struct NtfsSearcher {
    shared: Arc<Shared>,
    /// Review focus 2: folders this user may list ([`set_hide_unlistable`]).
    access: acl::FolderAccess,
}

fn load_volume(dir: &Path, id: VolumeId) -> Option<Part> {
    let db = db_path(dir, id);
    let (mut index, meta) = db::load(&db).ok()??;
    index.prefix = id.drive();
    Some(Part::Volume {
        index,
        id,
        meta,
        db,
    })
}

impl NtfsSearcher {
    /// Loads saved volume indexes from `dir` now (seconds for millions of entries:
    /// call off the UI thread), then, on a background thread, catches up from the
    /// journal, rebuilds (elevated) or walks the user folders (not elevated and
    /// nothing saved), and keeps the index current.
    pub fn open(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        adopt(&dir);
        let parts: Vec<Part> = win::fixed_ntfs_volumes()
            .into_iter()
            .filter_map(|id| load_volume(&dir, id))
            .collect();
        let shared = Arc::new(Shared {
            dir,
            ready: AtomicBool::new(!parts.is_empty()),
            parts: RwLock::new(parts),
            status: Mutex::new(None),
            done: AtomicUsize::new(0),
            fresh: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&shared);
        let _ = std::thread::Builder::new()
            .name("keel-index".into())
            .spawn(move || run(weak));
        Self {
            shared,
            access: acl::FolderAccess::new(acl::can_list),
        }
    }

    /// Entries indexed so far, over all volumes (or the user-folder walk).
    pub fn len(&self) -> usize {
        self.shared
            .parts
            .read()
            .iter()
            .map(|p| p.index().len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Searcher for NtfsSearcher {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let matcher = pattern::compile(query)?;
        let max = query.max as usize;
        // Hidden entries make room for the next ones: fetch more when filtering.
        let filter = acl::enabled();
        let fetch = if filter { max.saturating_mul(2) } else { max };
        let mut found: Vec<Found> = {
            let parts = self.shared.parts.read();
            if parts.is_empty() {
                let done = self.shared.done.load(Ordering::Relaxed);
                bail!("Keel is indexing your files ({done} so far); search works when it is done");
            }
            parts
                .iter()
                .flat_map(|part| part.index().search(&matcher, fetch))
                .collect()
        };
        found.sort_by(rank);
        if filter {
            self.access.retain(&mut found, max);
        }
        found.truncate(max);
        let mut hits: Vec<Hit> = found
            .into_iter()
            .map(|f| Hit {
                path: keel_vfs::VPath::local(&f.path),
                is_dir: f.is_dir,
                size: 0,
                modified: None,
                snippet: None,
            })
            .collect();
        // Size and date come from the file system: a few hundred lookups, in parallel.
        if query.meta && hits.len() <= STAT_LIMIT {
            crate::fill_meta(&mut hits);
        }
        Ok(hits)
    }

    /// True once any index (a volume or the user-folder walk) is loaded.
    fn available(&self) -> bool {
        self.shared.ready.load(Ordering::Relaxed)
    }

    fn status(&self) -> Option<String> {
        self.shared.status.lock().clone()
    }

    fn name(&self) -> &'static str {
        "Keel index"
    }

    /// Indexing (with the walk's count so far) until the first index is in.
    fn state(&self) -> SearchState {
        if self.available() {
            SearchState::Ready
        } else {
            SearchState::Indexing {
                done: self.shared.done.load(Ordering::Relaxed),
            }
        }
    }

    fn take_ready(&self) -> bool {
        self.shared.fresh.swap(false, Ordering::Acquire)
    }
}

/// Background thread state.
struct Worker {
    elevated: bool,
    handles: Vec<(u32, Volume)>,
    /// Volumes whose journal this process cannot read (not elevated).
    frozen: Vec<u32>,
    watcher: Option<notify::RecommendedWatcher>,
    events: Option<std::sync::mpsc::Receiver<notify::Result<notify::Event>>>,
    /// The watcher lost events: walk the user folders again (at most every
    /// [`RESCAN_GAP`]).
    rescan: bool,
    rescanned: Option<Instant>,
    /// The index service, when installed and this process is not elevated.
    service: Option<service::Client<std::fs::File>>,
    /// When connecting to it was last tried.
    service_tried: Option<Instant>,
    /// The service was connected at the last poll.
    served: bool,
}

/// The background thread: bring saved volumes current (or build them when
/// elevated), else walk the user folders; then poll every [`POLL`] until the
/// searcher is dropped.
fn run(weak: Weak<Shared>) {
    let mut worker = Worker {
        elevated: is_elevated(),
        handles: Vec::new(),
        frozen: Vec::new(),
        watcher: None,
        events: None,
        rescan: false,
        rescanned: None,
        service: None,
        service_tried: None,
        served: false,
    };
    let mut first = true;
    loop {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        let started = Instant::now();
        // Output of an elevated `--index-service` run replaces what we have.
        let adopted = adopt(&shared.dir);
        // The index service just became reachable (installed while Keel runs): build
        // what is missing or stale through it.
        let served = worker.service().is_some();
        let new_service = served && !worker.served;
        worker.served = served;
        if new_service {
            worker.frozen.clear();
        }
        if first || !adopted.is_empty() || new_service {
            first = false;
            worker.refresh(&shared, &adopted);
        }
        worker.poll(&shared);
        drop(shared);
        std::thread::sleep(POLL.saturating_sub(started.elapsed()));
    }
}

impl Worker {
    /// (Re)loads saved volumes, rebuilds missing or stale ones when elevated, and
    /// switches between the volume indexes and the user-folder walk.
    fn refresh(&mut self, shared: &Shared, adopted: &[PathBuf]) {
        let mut stale = Vec::new();
        for id in win::fixed_ntfs_volumes() {
            let db = db_path(&shared.dir, id);
            let have = has_volume(shared, id.serial);
            if !have || adopted.contains(&db) {
                if let Some(part) = load_volume(&shared.dir, id) {
                    replace(shared, part);
                    self.frozen.retain(|&s| s != id.serial);
                }
            }
            if self.volume(id.serial).is_none() {
                if let Ok(volume) = Volume::open(id.letter) {
                    self.handles.push((id.serial, volume));
                }
            }
            let meta = shared.parts.read().iter().find_map(|p| match p {
                Part::Volume { id: v, meta, .. } if v.serial == id.serial => Some(meta.clone()),
                _ => None,
            });
            let journal = self.volume(id.serial).and_then(|v| v.query_journal().ok());
            let rebuild = match (&meta, &journal) {
                (None, _) => true,
                (Some(meta), Some(journal)) => resume(meta, journal) == Resume::Rebuild,
                (Some(_), None) => false,
            };
            if rebuild {
                stale.push(id);
            }
        }
        for id in stale {
            self.rebuild(shared, id);
        }
        if shared.parts.read().iter().any(|p| p.serial().is_some()) {
            shared.parts.write().retain(|p| p.serial().is_some());
            self.watcher = None;
            self.events = None;
            if self.frozen.is_empty() {
                *shared.status.lock() = None;
            }
        } else if self.watcher.is_none() {
            *shared.status.lock() = Some(FALLBACK_STATUS.into());
            self.start_walk(shared);
        }
        let ready = !shared.parts.read().is_empty();
        shared.ready.store(ready, Ordering::Relaxed);
    }

    fn volume(&self, serial: u32) -> Option<&Volume> {
        self.handles
            .iter()
            .find(|(s, _)| *s == serial)
            .map(|(_, v)| v)
    }

    /// Re-reads a volume's MFT when elevated, or through the index service when it
    /// reads this volume; otherwise marks its index stale.
    fn rebuild(&mut self, shared: &Shared, id: VolumeId) {
        let built = if self.elevated {
            build_volume(id)
        } else {
            match self.remote(id, |client| build_volume_remote(id, client)) {
                Some(built) => built.map_err(anyhow::Error::from),
                None => {
                    if has_volume(shared, id.serial) {
                        self.freeze(shared, id.serial);
                    }
                    return;
                }
            }
        };
        let db = db_path(&shared.dir, id);
        match built {
            Ok((index, meta)) => {
                if let Err(e) = db::save_full(&db, &index, &meta) {
                    warn(&format!("saving {}: {e:#}", db.display()));
                }
                replace(
                    shared,
                    Part::Volume {
                        index,
                        id,
                        meta,
                        db,
                    },
                );
                self.frozen.retain(|&s| s != id.serial);
            }
            Err(e) => warn(&format!("indexing {}: {e:#}", id.drive())),
        }
    }

    /// The index service's connection: made (and the service started) when it is
    /// installed, at most every [`SERVICE_RETRY`]; never for an elevated process,
    /// which reads the volumes itself.
    fn service(&mut self) -> Option<&mut service::Client<std::fs::File>> {
        if self.elevated {
            return None;
        }
        if self.service.as_ref().is_some_and(|c| c.broken()) {
            self.service = None;
        }
        if self.service.is_none()
            && self
                .service_tried
                .is_none_or(|at| at.elapsed() >= SERVICE_RETRY)
        {
            self.service_tried = Some(Instant::now());
            match svcwin::connect_service() {
                Ok(client) => self.service = Some(client),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn(&format!("connecting to the index service: {e}")),
            }
        }
        self.service.as_mut()
    }

    /// Runs `f` with the index service when it reads volume `id` (same letter and
    /// serial as this process sees). None when there is no service for it.
    fn remote<T>(
        &mut self,
        id: VolumeId,
        f: impl FnOnce(&mut service::Client<std::fs::File>) -> std::io::Result<T>,
    ) -> Option<std::io::Result<T>> {
        let client = self.service()?;
        if !client.serves(id) {
            // Volumes come and go: ask again, once.
            if client.refresh_volumes().is_err() || !client.serves(id) {
                return None;
            }
        }
        Some(f(client))
    }

    /// Whether journal reads for `id` go through the service: this process cannot
    /// read the volume itself and the service reads it.
    fn via_service(&mut self, id: VolumeId) -> bool {
        let privileged = self.volume(id.serial).is_some_and(|v| v.privileged());
        !privileged && self.remote(id, |_| Ok(())).is_some()
    }

    fn freeze(&mut self, shared: &Shared, serial: u32) {
        if !self.frozen.contains(&serial) {
            self.frozen.push(serial);
        }
        *shared.status.lock() = Some(FROZEN_STATUS.into());
    }

    /// Applies new journal records (and watcher events for the walk), saving the
    /// changed rows and the new journal position.
    fn poll(&mut self, shared: &Shared) {
        let volumes: Vec<(VolumeId, u64, i64)> = shared
            .parts
            .read()
            .iter()
            .filter_map(|p| match p {
                Part::Volume { id, meta, .. } if meta.journal_id != 0 => {
                    Some((*id, meta.journal_id, meta.next_usn))
                }
                _ => None,
            })
            .collect();
        for (id, journal_id, from) in volumes {
            if self.frozen.contains(&id.serial) {
                continue;
            }
            if self.volume(id.serial).is_none() {
                self.freeze(shared, id.serial);
                continue;
            }
            let remote = self.via_service(id);
            let read = if remote {
                self.remote(id, |c| c.read_journal(id.letter, journal_id, from))
                    .unwrap_or_else(|| Err(std::io::Error::other("the index service went away")))
            } else {
                self.volume(id.serial).map_or_else(
                    || Err(std::io::ErrorKind::NotFound.into()),
                    |v| v.read_journal(journal_id, from),
                )
            };
            let (mut records, next) = match read {
                Ok(read) => read,
                Err(e) if e.raw_os_error() == Some(ERROR_JOURNAL_ENTRY_DELETED) => {
                    self.rebuild(shared, id);
                    continue;
                }
                Err(_) => {
                    self.freeze(shared, id.serial);
                    continue;
                }
            };
            if records.is_empty() && next == from {
                continue;
            }
            let stats: HashMap<u64, usn::Stat> = if remote {
                // The service's journal read has the names; it also looks up which
                // deleted names are hard links of a file that is still there.
                let deleted: Vec<u64> = records
                    .iter()
                    .filter(|r| r.reason & usn::REASON_FILE_DELETE != 0)
                    .map(|r| r.frn)
                    .collect();
                let alive = self
                    .remote(id, |c| c.stat(id.letter, &deleted))
                    .and_then(Result::ok)
                    .unwrap_or_default();
                usn::keep_linked(&mut records, |frn| alive.contains_key(&frn));
                let wanted = usn::to_stat(&records, STAT_BATCH);
                self.remote(id, |c| c.stat(id.letter, &wanted))
                    .and_then(Result::ok)
                    .unwrap_or_default()
            } else {
                let Some(volume) = self.volume(id.serial) else {
                    continue;
                };
                if !volume.privileged() {
                    let parts = shared.parts.read();
                    if let Some(part) = parts.iter().find(|p| p.serial() == Some(id.serial)) {
                        usn::fill_names(&mut records, part.index(), |frn| volume.name_of(frn));
                    }
                }
                usn::keep_linked(&mut records, |frn| volume.name_of(frn).is_some());
                // Sizes and dates of the changed files, read before the index is locked.
                usn::to_stat(&records, STAT_BATCH)
                    .into_iter()
                    .filter_map(|frn| Some((frn, volume.stat(frn)?)))
                    .collect()
            };
            let mut changes = Changes::default();
            if let Some(Part::Volume { index, meta, .. }) = shared
                .parts
                .write()
                .iter_mut()
                .find(|p| p.serial() == Some(id.serial))
            {
                usn::apply_with(index, &records, &stats, &mut changes);
                meta.next_usn = next;
            }
            if let Some(Part::Volume {
                index, meta, db, ..
            }) = shared
                .parts
                .read()
                .iter()
                .find(|p| p.serial() == Some(id.serial))
            {
                if let Err(e) = db::save_changes(db, index, &changes, meta) {
                    warn(&format!("saving {}: {e:#}", db.display()));
                }
            }
        }
        self.poll_walk(shared);
    }

    /// Applies the watcher's events to the user-folder walk. The disk is read (new
    /// folders walked) before the index is locked, then the changes go in batches.
    fn poll_walk(&mut self, shared: &Shared) {
        let Some(events) = &self.events else {
            return;
        };
        let events: Vec<_> = events.try_iter().collect();
        if events.is_empty() && !self.rescan {
            return;
        }
        let deep = match shared.parts.read().first() {
            Some(Part::Walk(walk)) => walk.deep.clone(),
            _ => return,
        };
        let ops = walk::plan(&deep, events);
        self.rescan |= ops.contains(&walk::Op::Rescan);
        if self.rescan && self.rescanned.is_none_or(|at| at.elapsed() >= RESCAN_GAP) {
            // Lost events: walk again off the lock (the old index keeps serving), then
            // swap. Events arriving meanwhile stay queued for the next poll.
            self.rescan = false;
            self.rescanned = Some(Instant::now());
            let (deep, shallow) = walk::roots();
            let fresh = new_walk(&deep, &shallow, &AtomicUsize::new(0));
            if let Some(slot @ Part::Walk(_)) = shared.parts.write().first_mut() {
                *slot = Part::Walk(fresh);
            }
            shared.finished();
            return;
        }
        apply_walk_ops(&shared.parts, &ops);
    }

    /// Indexes the user folders, watching first so nothing created meanwhile is lost.
    fn start_walk(&mut self, shared: &Shared) {
        use notify::{RecursiveMode, Watcher};
        let (deep, shallow) = walk::roots();
        let (tx, rx) = std::sync::mpsc::channel();
        self.watcher = notify::recommended_watcher(tx).ok().map(|mut w| {
            for d in &deep {
                let _ = w.watch(d, RecursiveMode::Recursive);
            }
            for s in &shallow {
                let _ = w.watch(s, RecursiveMode::NonRecursive);
            }
            w
        });
        self.events = Some(rx);
        shared.done.store(0, Ordering::Relaxed);
        let walk = new_walk(&deep, &shallow, &shared.done);
        shared.parts.write().push(Part::Walk(walk));
        shared.finished();
    }
}

/// Walks the user folders into a new index (seconds to minutes for a big home
/// folder; no lock), counting entries in `progress`.
fn new_walk(deep: &[PathBuf], shallow: &[PathBuf], progress: &AtomicUsize) -> walk::Walk {
    let mut walk = walk::Walk::new(deep);
    for s in shallow {
        walk.add_tree(s, Some(1), progress);
    }
    for d in deep {
        walk.add_tree(d, None, progress);
    }
    walk
}

/// Applies `ops` to the walk index, write-locking it for about [`BATCH`] at a time
/// and handing the lock to waiting searches in between. Returns the longest hold.
fn apply_walk_ops(parts: &RwLock<Vec<Part>>, ops: &[walk::Op]) -> Duration {
    let mut longest = Duration::ZERO;
    let mut rest = ops;
    while !rest.is_empty() {
        let mut guard = parts.write();
        let Some(Part::Walk(walk)) = guard.first_mut() else {
            break;
        };
        let started = Instant::now();
        let mut n = 0;
        while n < rest.len() && (n == 0 || started.elapsed() < BATCH) {
            walk.apply(&rest[n]);
            n += 1;
        }
        longest = longest.max(started.elapsed());
        rest = &rest[n..];
        parking_lot::RwLockWriteGuard::unlock_fair(guard);
    }
    longest
}

fn has_volume(shared: &Shared, serial: u32) -> bool {
    shared
        .parts
        .read()
        .iter()
        .any(|p| p.serial() == Some(serial))
}

/// Puts `part` in place of the same volume's index, or adds it.
fn replace(shared: &Shared, part: Part) {
    let mut parts = shared.parts.write();
    match parts.iter_mut().find(|p| p.serial() == part.serial()) {
        Some(slot) => *slot = part,
        None => parts.push(part),
    }
    drop(parts);
    shared.finished();
}

// ponytail: stderr, not tracing; keel-search has no logging dependency yet.
fn warn(message: &str) {
    eprintln!("keel-index: {message}");
}

#[cfg(test)]
mod tests {
    use super::index::Column;
    use super::usn::tests::record_bytes;
    use super::usn::{parse_buffer, REASON_FILE_DELETE, REASON_RENAME_OLD_NAME};
    use super::*;

    const CREATE: u32 = 0x100;
    const RENAME_NEW: u32 = 0x2000;
    const CLOSE: u32 = 0x8000_0000;
    const DIR: u32 = 0x10;

    #[test]
    fn resume_rebuilds_on_new_journal_or_truncation() {
        let meta = Meta {
            journal_id: 7,
            next_usn: 1_000,
            root: 5,
            volume: "C:".into(),
            full: false,
        };
        let journal = |id, lowest, next| Journal {
            id,
            next_usn: next,
            lowest_valid_usn: lowest,
        };
        assert_eq!(resume(&meta, &journal(7, 0, 5_000)), Resume::From(1_000));
        assert_eq!(
            resume(&meta, &journal(7, 1_000, 1_000)),
            Resume::From(1_000)
        );
        assert_eq!(resume(&meta, &journal(8, 0, 5_000)), Resume::Rebuild);
        assert_eq!(resume(&meta, &journal(7, 2_000, 5_000)), Resume::Rebuild);
        assert_eq!(resume(&meta, &journal(7, 0, 500)), Resume::Rebuild);
    }

    #[test]
    fn catch_up_applies_create_rename_and_delete() {
        let mut index = Index::new("C:", 5);
        index.upsert(10, 5, "Users", true);
        index.upsert(11, 10, "old.txt", false);
        index.upsert(12, 10, "gone.txt", false);
        // A journal read: a folder and a file created in it, old.txt renamed and
        // moved into it (old-name record, then new-name record), gone.txt deleted,
        // and a create-then-delete that never needs a row.
        let mut buf = 99_u64.to_le_bytes().to_vec();
        for (frn, parent, reason, attrs, name) in [
            (20, 10, CREATE, DIR, "Projects"),
            (20, 10, CREATE | CLOSE, DIR, "Projects"),
            (21, 20, CREATE, 0, "plan.md"),
            (11, 10, REASON_RENAME_OLD_NAME, 0, "old.txt"),
            (11, 20, RENAME_NEW, 0, "new.txt"),
            (11, 20, RENAME_NEW | CLOSE, 0, "new.txt"),
            (12, 10, REASON_FILE_DELETE | CLOSE, 0, "gone.txt"),
            (30, 10, CREATE, 0, "~tmp"),
            (30, 10, CREATE | REASON_FILE_DELETE | CLOSE, 0, "~tmp"),
        ] {
            buf.extend(record_bytes(3, frn, parent, reason, attrs, name));
        }
        let (next, records) = parse_buffer(&buf).unwrap();
        assert_eq!(next, 99);
        let mut changes = Changes::default();
        usn::apply(&mut index, &records, &mut changes);

        assert_eq!(index.path(21).unwrap(), r"C:\Users\Projects\plan.md");
        assert_eq!(index.path(11).unwrap(), r"C:\Users\Projects\new.txt");
        assert!(index.get(12).is_none());
        assert!(index.get(30).is_none());
        assert_eq!(index.len(), 4);
        let mut upserts: Vec<u64> = changes.upserts.iter().copied().collect();
        upserts.sort();
        assert_eq!(upserts, [11, 20, 21]);
        let mut removes: Vec<u64> = changes.removes.iter().copied().collect();
        removes.sort();
        assert_eq!(removes, [12, 30]);
    }

    /// Finding 22: while the first index is built, the searcher reports Indexing
    /// with a count (not "unavailable"), and announces the finished index once.
    #[test]
    fn indexing_is_reported_and_completion_announced() {
        let shared = Arc::new(Shared {
            dir: PathBuf::new(),
            parts: RwLock::new(Vec::new()),
            status: Mutex::new(Some(FALLBACK_STATUS.into())),
            ready: AtomicBool::new(false),
            done: AtomicUsize::new(1234),
            fresh: AtomicBool::new(false),
        });
        let searcher = NtfsSearcher {
            shared: shared.clone(),
            access: acl::FolderAccess::new(acl::can_list),
        };
        assert_eq!(searcher.state(), SearchState::Indexing { done: 1234 });
        let err = searcher.query(&Query::default()).unwrap_err().to_string();
        assert!(err.contains("indexing") && err.contains("1234"), "{err}");
        assert!(!searcher.take_ready());

        let mut walk = walk::Walk::new(&[]);
        walk.add(Path::new(r"C:\KeelReadyNeedle.txt"), false);
        shared.parts.write().push(Part::Walk(walk));
        shared.finished();
        assert_eq!(searcher.state(), SearchState::Ready);
        assert!(searcher.take_ready());
        assert!(!searcher.take_ready(), "once");
        let hits = searcher
            .query(&Query {
                text: "keelreadyneedle".into(),
                ..Query::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// Finding 3: a 10k-file tree deleted from a 1M-entry walk index, as one folder
    /// event and as one event per file; the write lock is held < 50 ms per batch, and
    /// planning (which walks new folders) needs no lock at all.
    #[test]
    fn walk_changes_hold_the_index_lock_briefly() {
        use notify::event::{CreateKind, RemoveKind};
        use notify::{Event, EventKind};
        let mut big = walk::Walk::new(&[]);
        for d in 0..1_000 {
            for f in 0..1_000 {
                big.add(Path::new(&format!(r"C:\t\d{d}\f{f}.txt")), false);
            }
        }
        for tree in ["one", "each"] {
            for f in 0..10_000 {
                let path = format!(r"C:\t\{tree}\s{}\f{f}.txt", f % 100);
                big.add(Path::new(&path), false);
            }
        }
        assert!(big.index.len() > 1_000_000);
        let parts = RwLock::new(vec![Part::Walk(big)]);
        let remove = |path: String| {
            Ok(Event::new(EventKind::Remove(RemoveKind::Any)).add_path(PathBuf::from(path)))
        };
        let one = vec![remove(r"C:\t\one".into())];
        let mut each: Vec<_> = (0..10_000)
            .map(|f| remove(format!(r"C:\t\each\s{}\f{f}.txt", f % 100)))
            .collect();
        each.extend((0..100).map(|s| remove(format!(r"C:\t\each\s{s}"))));
        each.push(remove(r"C:\t\each".into()));
        for events in [one, each] {
            let ops = walk::plan(&[], events);
            let longest = apply_walk_ops(&parts, &ops);
            println!("{} ops, longest write-lock hold {longest:?}", ops.len());
            assert!(longest < Duration::from_millis(50), "lock held {longest:?}");
        }
        assert_eq!(parts.read()[0].index().len(), 1_001_002);

        // A folder created under a deep root is walked while a search holds the lock.
        let root = std::env::temp_dir().join(format!("keel-walk-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("new").join("deeper")).unwrap();
        std::fs::write(root.join("new").join("deeper").join("x.txt"), b"x").unwrap();
        let deep = walk::Walk::new(std::slice::from_ref(&root)).deep;
        let created =
            Ok(Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.join("new")));
        let ops = {
            let _search = parts.write();
            walk::plan(&deep, [created])
        };
        assert_eq!(ops.len(), 3, "{ops:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Size, dates and links of a file by its reference number, as a journal poll
    /// looks them up.
    #[test]
    fn stat_by_file_id_reads_size_dates_and_links() {
        let dir = std::env::temp_dir().join(format!("keel-stat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.bin");
        std::fs::write(&file, vec![7u8; 10_000]).unwrap();
        std::fs::hard_link(&file, dir.join("b.bin")).unwrap();
        let letter = std::fs::canonicalize(&dir)
            .unwrap()
            .display()
            .to_string()
            .trim_start_matches(r"\\?\")
            .chars()
            .next()
            .unwrap();
        let volume = Volume::open(letter).unwrap();
        let frn = win::frn_of(&file).unwrap();
        let stat = volume.stat(frn).unwrap();
        assert_eq!(stat.size, 10_000);
        assert!(stat.allocated >= 10_000, "{stat:?}");
        assert_eq!(stat.links, 2);
        assert!(
            stat.created > 0 && stat.modified >= stat.created,
            "{stat:?}"
        );
        assert_eq!(stat.attributes & 0x10, 0);
        let folder = volume.stat(win::frn_of(&dir).unwrap()).unwrap();
        assert_ne!(folder.attributes & 0x10, 0);
        assert_eq!((folder.size, folder.allocated), (0, 0));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            volume.stat(0x0000_FFFF_FFFF_FFF0).is_none(),
            "no such record"
        );
    }

    /// A journal batch with sizes looked up: the file's size reaches every folder
    /// above, a delete takes it away again, and folder dates follow.
    #[test]
    fn journal_batches_keep_folder_totals() {
        let mut index = Index::new("C:", 5);
        index.upsert(5, 5, ".", true);
        index.upsert(10, 5, "Users", true);
        let rec = |frn, parent, reason, attrs, name: &str, time| usn::Record {
            frn,
            parent,
            usn: 0,
            reason,
            is_dir: attrs & DIR != 0,
            name: name.into(),
            time,
            attributes: attrs,
        };
        let records = vec![
            rec(20, 10, CREATE, DIR, "Projects", 1_000),
            rec(21, 20, CREATE, 0x20, "plan.md", 1_001),
            rec(21, 20, 0x2 | CLOSE, 0x20, "plan.md", 1_002),
        ];
        let stats: HashMap<u64, usn::Stat> = usn::to_stat(&records, 10)
            .into_iter()
            .map(|frn| {
                let size = if frn == 21 { 5_000 } else { 0 };
                let stat = usn::Stat {
                    size,
                    allocated: size.div_ceil(4096) * 4096,
                    modified: 1_002,
                    created: 1_001,
                    attributes: if frn == 20 { DIR } else { 0x20 },
                    links: 1,
                    ..usn::Stat::default()
                };
                (frn, stat)
            })
            .collect();
        let mut changes = Changes::default();
        usn::apply_with(&mut index, &records, &stats, &mut changes);
        let info = index.info(21).unwrap();
        assert_eq!(
            (info.size, info.changed, info.attributes),
            (5_000, 1_002, 0x20)
        );
        let users = index.folder(10).unwrap();
        assert_eq!((users.size, users.files, users.folders), (5_000, 1, 1));
        assert_eq!(users.allocated, 8_192);
        assert_eq!(users.newest, 1_002);
        assert_eq!(
            index.info(20).unwrap().changed,
            1_001,
            "dated when plan.md came"
        );

        let gone = [rec(
            21,
            20,
            REASON_FILE_DELETE | CLOSE,
            0x20,
            "plan.md",
            2_000,
        )];
        usn::apply_with(&mut index, &gone, &HashMap::new(), &mut changes);
        let users = index.folder(10).unwrap();
        assert_eq!((users.size, users.files), (0, 0));
        assert_eq!(users.newest, 2_000);
        assert!(changes.removes.contains(&21));
    }

    /// Elevated only: the raw MFT read finds the files `FSCTL_ENUM_USN_DATA` lists on
    /// every fixed NTFS volume, with the sizes the file system reports. Prints counts
    /// and system file names only.
    #[test]
    #[ignore = "reads every fixed volume's MFT; needs administrator"]
    fn live_raw_mft_matches_enumeration() {
        if !is_elevated() {
            println!("skipped: not elevated");
            return;
        }
        for id in win::fixed_ntfs_volumes() {
            let volume = Volume::open(id.letter).unwrap();
            // FILETIME now: files created after this were not there for the raw read.
            let unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            let read_start = (unix.as_nanos() / 100) as i64 + 116_444_736_000_000_000;
            let started = Instant::now();
            let mut raw: HashMap<u64, (u64, String, bool, u64, u64)> = HashMap::new();
            crate::raw::read_mft(&volume, |e| {
                let mut index = Index::new("", 0);
                add_entry(&mut index, &e);
                let allocated = index.info(e.frn).map_or(0, |i| i.allocated);
                let name = &e.names[0];
                let row = (
                    name.parent,
                    name.name.clone(),
                    e.is_dir,
                    e.data.size,
                    allocated,
                );
                raw.insert(e.frn, row);
            })
            .unwrap();
            let raw_time = started.elapsed();
            let started = Instant::now();
            let mut listed: HashMap<u64, (u64, String)> = HashMap::new();
            volume
                .enumerate(|records| {
                    for r in records {
                        listed.insert(r.frn, (r.parent, r.name));
                    }
                })
                .unwrap();
            println!(
                "{}: raw read {} files in {raw_time:.2?}; enumeration {} in {:.2?}",
                id.drive(),
                raw.len(),
                listed.len(),
                started.elapsed()
            );
            let only_raw: Vec<u64> = raw
                .keys()
                .filter(|f| !listed.contains_key(f))
                .copied()
                .collect();
            let only_listed: Vec<u64> = listed
                .keys()
                .filter(|f| !raw.contains_key(f))
                .copied()
                .collect();
            let system = |names: Vec<&String>| -> Vec<String> {
                names
                    .into_iter()
                    .filter(|n| n.starts_with('$'))
                    .take(20)
                    .cloned()
                    .collect()
            };
            println!(
                "only in the raw read: {} (system: {:?}); only enumerated: {} (system: {:?})",
                only_raw.len(),
                system(only_raw.iter().map(|f| &raw[f].1).collect()),
                only_listed.len(),
                system(only_listed.iter().map(|f| &listed[f].1).collect()),
            );
            let same_parent = listed
                .iter()
                .filter(|(f, (p, _))| raw.get(f).is_some_and(|r| r.0 == *p))
                .count();
            println!("same parent: {same_parent} of {}", listed.len());
            // Enumerated but not read: only a miss when the file is older than the read.
            let missed = only_listed
                .iter()
                .filter(|f| volume.stat(**f).is_some_and(|s| s.created < read_start))
                .count();
            println!("enumerated, not read, and older than the read: {missed}");
            let (mut checked, mut sizes, mut allocated) = (0, 0, 0);
            let mut samples = Vec::new();
            for (frn, (_, _, _, size, alloc)) in raw.iter().filter(|(_, row)| !row.2).take(5_000) {
                let Some(stat) = volume.stat(*frn) else {
                    continue;
                };
                checked += 1;
                sizes += usize::from(stat.size != *size);
                if stat.allocated != *alloc {
                    allocated += 1;
                    if samples.len() < 12 {
                        samples.push((*size, *alloc, stat.allocated, stat.attributes));
                    }
                }
            }
            println!(
                "{checked} files checked by id: {sizes} sizes and {allocated} sizes on disk differ"
            );
            println!("size on disk (size, read, by id, attributes): {samples:x?}");
            assert!(missed <= listed.len() / 10_000 + 16);
            assert!(sizes <= checked / 100 + 5);
            assert!(allocated <= checked / 100 + 5);
        }
    }

    /// The helper's pipe: the helper side checks who serves it and writes; this side
    /// reads what it wrote. A name naming another process, or not a helper pipe, is
    /// refused; a poke wakes an accept nobody else will answer.
    #[test]
    fn the_helper_pipe_carries_frames_and_checks_its_server() {
        use std::io::{Read, Write};
        let (name, mut pipe) = win::helper_pipe().unwrap();
        assert!(name.starts_with(win::HELPER_PIPE));
        let writer = {
            let name = name.clone();
            std::thread::spawn(move || {
                let mut out = win::open_helper_pipe(&name).unwrap();
                out.write_all(&wire::Frame::Done.encode()).unwrap();
            })
        };
        win::accept_pipe(&pipe).unwrap();
        let mut got = Vec::new();
        pipe.read_to_end(&mut got).unwrap();
        writer.join().unwrap();
        assert_eq!(got, wire::Frame::Done.encode());

        // A pipe served by this process under a name that claims another process:
        // someone squatting the name the helper was given.
        let (name, pipe) = win::helper_pipe().unwrap();
        let (_, nonce) = name[win::HELPER_PIPE.len()..].split_once('-').unwrap();
        let lie = format!("{}{}-{nonce}", win::HELPER_PIPE, std::process::id() + 4);
        let squat = win::serve_pipe(&lie).unwrap();
        let err = win::open_helper_pipe(&lie).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        drop(squat);
        for bad in [
            r"\\.\pipe\other".to_string(),
            format!(r"{name}\..\x"),
            format!("{}{}-xyz", win::HELPER_PIPE, std::process::id()),
            r"C:\Windows\Temp\x".to_string(),
        ] {
            let err = win::open_helper_pipe(&bad).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{bad}");
        }
        // Nobody connects: the poke ends the wait.
        win::poke_pipe(&name);
        win::accept_pipe(&pipe).unwrap();
    }

    /// The unelevated side of a full index: a helper stream for one volume is saved
    /// as `<serial>.db.svc`, which `adopt` and `db::load` take; a stream that stops
    /// early with nothing saved is an error.
    #[test]
    fn received_volumes_are_saved_for_adoption() {
        use crate::raw::tests::{file, mft_zero, runs, Image, RECORD};
        let dir = std::env::temp_dir().join(format!("keel-ntfs-receive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let size = 32 * RECORD;
        let mut image = Image::new(24, size);
        image.put(&[(10, 8)], 0, &mft_zero(size, runs(&[(Some(10), 8)]), None));
        image.put(&[(10, 8)], 17, &file("KeelReceived.txt"));
        let id = VolumeId {
            letter: 'K',
            serial: 0x0123_4567,
        };
        let start = receive::Start {
            id,
            root: 5 | 5 << 48,
            journal: Journal {
                id: 3,
                next_usn: 40,
                lowest_valid_usn: 0,
            },
        };
        let mut stream = Vec::new();
        wire::send_volume(&image, 'K', id.serial, &mut |f| {
            stream.extend(f);
            Ok(())
        })
        .unwrap();
        stream.extend(wire::Frame::Done.encode());
        receive_all(&dir, vec![start], &stream[..]).unwrap();
        assert_eq!(adopt(&dir), [dir.join("01234567.db")]);
        let (index, meta) = db::load(&dir.join("01234567.db")).unwrap().unwrap();
        assert!(meta.full);
        assert_eq!((meta.journal_id, meta.next_usn), (3, 40));
        let path = index.path(17 | 1 << 48).unwrap();
        assert!(path.ends_with("KeelReceived.txt"), "{path}");

        let cut = &stream[..stream.len() - 20];
        assert!(receive_all(&dir, vec![start], cut).is_err());
        assert!(adopt(&dir).is_empty(), "nothing saved");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tests_never_use_the_real_index_folder() {
        let dir = index_dir().unwrap();
        let real = directories::BaseDirs::new().map(|b| b.cache_dir().join("Keel"));
        assert!(
            real.is_none_or(|real| !dir.starts_with(real)),
            "{}",
            dir.display()
        );
    }

    #[test]
    fn adopt_moves_service_output_into_place() {
        let dir = std::env::temp_dir().join(format!("keel-ntfs-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0000ABCD.db"), b"old").unwrap();
        std::fs::write(dir.join("0000ABCD.db-wal"), b"stale").unwrap();
        std::fs::write(dir.join("0000ABCD.db.svc"), b"new").unwrap();
        let adopted = adopt(&dir);
        assert_eq!(adopted, [dir.join("0000ABCD.db")]);
        assert_eq!(std::fs::read(dir.join("0000ABCD.db")).unwrap(), b"new");
        assert!(!dir.join("0000ABCD.db.svc").exists());
        assert!(!dir.join("0000ABCD.db-wal").exists());
        assert!(adopt(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn searcher_answers_from_a_saved_index_without_elevation() {
        // Uses the real volumes' serials so `open` loads the db; checks only what we
        // wrote, under a path no real file has.
        // Elevated, the worker would start re-reading every real volume's MFT.
        let Some(id) = win::fixed_ntfs_volumes().into_iter().next() else {
            return;
        };
        if is_elevated() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("keel-ntfs-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut index = Index::new(id.drive(), 5);
        index.upsert(10, 5, "KeelOpenNeedleDir", true);
        index.upsert(11, 10, "KeelOpenNeedle.txt", false);
        let meta = Meta {
            journal_id: 0,
            next_usn: 0,
            root: 5,
            volume: id.drive(),
            full: false,
        };
        db::save_full(&db_path(&dir, id), &index, &meta).unwrap();
        let searcher = NtfsSearcher::open(dir.clone());
        assert!(searcher.available());
        assert_eq!(searcher.state(), SearchState::Ready);
        let hits = searcher
            .query(&Query {
                text: "keelopenneedle".into(),
                ..Query::default()
            })
            .unwrap();
        let paths: Vec<String> = hits.iter().map(|h| h.path.display()).collect();
        assert_eq!(
            paths,
            [
                format!(r"{}\KeelOpenNeedleDir", id.drive()),
                format!(r"{}\KeelOpenNeedleDir\KeelOpenNeedle.txt", id.drive()),
            ]
        );
        assert!(hits[0].is_dir);
        let folders = crate::folder_index(&searcher).unwrap();
        assert!(folders.iter().any(|f| f.ends_with(r"\KeelOpenNeedleDir")));
        drop(searcher);
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `cargo test -p keel-search --release -- --ignored --nocapture perf_two_million`
    #[test]
    #[ignore]
    fn perf_two_million_entries() {
        let words = [
            "report", "invoice", "photo", "build", "src", "main", "lib", "test", "keel", "notes",
            "draft", "final", "backup", "data", "cache", "index", "video", "music",
        ];
        let exts = [
            "txt", "pdf", "rs", "jpg", "docx", "json", "md", "dll", "exe", "png",
        ];
        let mut index = Index::new("C:", 5);
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let built = Instant::now();
        let mut dirs = vec![5u64];
        for frn in 6..2_000_006u64 {
            let r = rand();
            let parent = dirs[(r % dirs.len() as u64) as usize];
            let w1 = words[(r >> 8) as usize % words.len()];
            let w2 = words[(r >> 16) as usize % words.len()];
            let is_dir = r % 10 == 0;
            let name = if is_dir {
                format!("{w1}_{w2}_{}", r >> 40 & 0xFFF)
            } else {
                format!(
                    "{w1}-{w2}-{}.{}",
                    r >> 32 & 0xFFFF,
                    exts[(r >> 24) as usize % exts.len()]
                )
            };
            index.upsert(frn, parent, &name, is_dir);
            if !is_dir {
                let size = r >> 44;
                index.set_info(
                    frn,
                    Info {
                        size,
                        allocated: size.next_multiple_of(4096),
                        modified: (r >> 20 & 0xFFFF_FFFF) as i64,
                        ..Info::default()
                    },
                );
            }
            if is_dir {
                dirs.push(frn);
            }
        }
        println!(
            "built 2M entries with sizes and dates (folder totals kept as they came) in {:?}",
            built.elapsed()
        );
        let started = Instant::now();
        index.begin_bulk();
        index.settle();
        println!("folder totals recounted in {:?}", started.elapsed());
        let started = Instant::now();
        index.set_fast_sort(&[Column::Name, Column::Size, Column::Modified]);
        index.settle();
        println!(
            "fast-sort orders (name, size, modified) built in {:?}",
            started.elapsed()
        );
        let started = Instant::now();
        for n in 0..1_000u64 {
            let frn = 6 + rand() % 2_000_000;
            match n % 10 {
                0 => {
                    index.remove_at(frn, 1 << 40);
                }
                1 => {
                    if let Some((parent, _, is_dir)) = index.get(frn) {
                        index.upsert_at(frn, parent, &format!("renamed-{n}.txt"), is_dir, 1 << 40);
                    }
                }
                _ => {
                    index.set_info(
                        frn,
                        Info {
                            size: n,
                            modified: 1 << 40,
                            ..Info::default()
                        },
                    );
                }
            }
        }
        let changed = started.elapsed();
        let started = Instant::now();
        index.settle();
        println!(
            "1,000 changes: applied in {changed:?}, orders and totals settled in {:?}",
            started.elapsed()
        );
        let mut some: Vec<u64> = (0..200_000).map(|_| 6 + rand() % 2_000_000).collect();
        let mut again = some.clone();
        let started = Instant::now();
        index.sort_frns(Column::Size, true, &mut some);
        println!(
            "sort 200,000 results by size: {:?} through the order",
            started.elapsed()
        );
        index.set_fast_sort(&[]);
        let started = Instant::now();
        index.sort_frns(Column::Size, true, &mut again);
        println!("the same without fast sort: {:?}", started.elapsed());
        assert_eq!(some, again);
        for text in [
            "keel",
            "invoice-report-12",
            "*.pdf",
            "regex:^photo.*7\\.jpg$",
            "final folder:",
            "zzzz-no-match",
            "a",
            "e",
            "notes in:C:\\report",
        ] {
            let m = pattern::compile(&Query {
                text: text.into(),
                ..Query::default()
            })
            .unwrap();
            let started = Instant::now();
            let found = index.search(&m, 500);
            let took = started.elapsed();
            println!("{text:>28}: {:>4} hits in {took:?}", found.len());
        }
        let m = pattern::compile(&Query {
            folders_only: true,
            max: 200_000,
            ..Query::default()
        })
        .unwrap();
        let started = Instant::now();
        let folders = index.search(&m, 200_000);
        println!("folder index: {} in {:?}", folders.len(), started.elapsed());
    }

    /// A normal user's view of the journal: readable, but without names, which
    /// `fill_names` restores. `cargo test -p keel-search -- --ignored --nocapture live_journal`
    #[test]
    #[ignore]
    fn live_journal_without_elevation() {
        if is_elevated() {
            println!("skipped: elevated");
            return;
        }
        let id = win::fixed_ntfs_volumes()[0];
        let volume = Volume::open(id.letter).unwrap();
        assert!(!volume.privileged());
        let journal = volume.query_journal().unwrap();
        let probe = std::env::temp_dir().join("keel-journal-probe.txt");
        std::fs::write(&probe, b"x").unwrap();
        let (mut records, _) = volume.read_journal(journal.id, journal.next_usn).unwrap();
        assert!(records.iter().all(|r| r.name.is_empty()));
        usn::fill_names(&mut records, &Index::new(id.drive(), 5), |frn| {
            volume.name_of(frn)
        });
        std::fs::remove_file(&probe).unwrap();
        assert!(records.iter().any(|r| r.name == "keel-journal-probe.txt"));
        assert!(
            volume.enumerate(|_| {}).is_err(),
            "the MFT read needs elevation"
        );
    }

    /// Reads the real MFT of every fixed NTFS volume, then exercises C: (queries, db
    /// round trip, journal catch-up). Needs an elevated run:
    /// `cargo test -p keel-search --release -- --ignored --nocapture live_mft`
    #[test]
    #[ignore]
    fn live_mft_enumeration() {
        if !is_elevated() {
            println!("skipped: not elevated");
            return;
        }
        let volumes = win::fixed_ntfs_volumes();
        let mut indexes = Vec::new();
        for id in &volumes {
            let started = Instant::now();
            let (index, meta) = build_volume(*id).unwrap();
            println!(
                "{} ({:08X}): {} entries in {:.2?} (journal {:x}, next usn {})",
                id.drive(),
                id.serial,
                index.len(),
                started.elapsed(),
                meta.journal_id,
                meta.next_usn
            );
            indexes.push((*id, index, meta));
        }
        let (id, index, meta) = &indexes[0];
        assert!(index.len() > 10_000);
        let windows = index
            .entries()
            .find(|(_, parent, name, _)| {
                *parent == meta.root && name.eq_ignore_ascii_case("Windows")
            })
            .map(|(frn, ..)| index.path(frn).unwrap());
        assert_eq!(
            windows.unwrap().to_lowercase(),
            format!(r"{}\windows", id.drive()).to_lowercase()
        );

        for text in [
            "notepad.exe",
            "*.dll",
            "keel",
            r"regex:^kernel32\.dll$",
            "a",
            "e",
            "folder: system",
            r"dll in:C:\Windows\System32",
            "zzqqxx-nothing",
        ] {
            let m = pattern::compile(&Query {
                text: text.into(),
                ..Query::default()
            })
            .unwrap();
            let started = Instant::now();
            let found = index.search(&m, 500);
            println!(
                "{text:>30}: {:>4} hits in {:.2?}",
                found.len(),
                started.elapsed()
            );
        }
        let m = pattern::compile(&Query {
            folders_only: true,
            max: 200_000,
            ..Query::default()
        })
        .unwrap();
        let started = Instant::now();
        let folders = index.search(&m, 200_000);
        println!(
            "folder index: {} in {:.2?}",
            folders.len(),
            started.elapsed()
        );

        let path = std::env::temp_dir().join("keel-live-mft.db");
        let started = Instant::now();
        db::save_full(&path, index, meta).unwrap();
        println!(
            "db save: {:.2?}, {} MB",
            started.elapsed(),
            path.metadata().unwrap().len() >> 20
        );
        let started = Instant::now();
        let (loaded, _) = db::load(&path).unwrap().unwrap();
        println!(
            "db load: {} entries in {:.2?}",
            loaded.len(),
            started.elapsed()
        );
        assert_eq!(loaded.len(), index.len());
        let _ = std::fs::remove_file(path);

        // Catch-up: create, rename and delete a probe file in the temp folder, then
        // replay that volume's journal (the temp folder may be a junction elsewhere).
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = PathBuf::from(dir.display().to_string().trim_start_matches(r"\\?\"));
        let (id, mut index, mut meta) = indexes
            .into_iter()
            .find(|(id, ..)| dir.display().to_string().starts_with(&id.drive()))
            .unwrap();
        let volume = Volume::open(id.letter).unwrap();
        let (records, next) = volume.read_journal(meta.journal_id, meta.next_usn).unwrap();
        let mut changes = Changes::default();
        usn::apply(&mut index, &records, &mut changes);
        meta.next_usn = next;
        println!("journal since enumeration: {} records", records.len());
        let (a, b) = (dir.join("keel-live-a.txt"), dir.join("keel-live-b.txt"));
        std::fs::write(&a, b"x").unwrap();
        println!(
            "captured: {}",
            volume.raw_journal_hex(meta.journal_id, meta.next_usn)
        );
        std::fs::rename(&a, &b).unwrap();
        let (records, next) = volume.read_journal(meta.journal_id, meta.next_usn).unwrap();
        usn::apply(&mut index, &records, &mut changes);
        let find = |index: &Index, name: &str| {
            let m = pattern::compile(&Query {
                text: name.into(),
                ..Query::default()
            })
            .unwrap();
            index
                .search(&m, 10)
                .into_iter()
                .map(|f| f.path)
                .collect::<Vec<_>>()
        };
        assert!(find(&index, "keel-live-a.txt").is_empty());
        let found = find(&index, "keel-live-b.txt");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].to_lowercase(),
            b.display().to_string().to_lowercase()
        );
        std::fs::remove_file(&b).unwrap();
        let (records, _) = volume.read_journal(meta.journal_id, next).unwrap();
        usn::apply(&mut index, &records, &mut changes);
        assert!(find(&index, "keel-live-b.txt").is_empty());
        println!("catch-up: create, rename and delete replayed");
    }

    /// The `--index-service` round trip in two runs over `%TEMP%\keel-index-test`:
    /// elevated, the helper's side runs on a thread and sends every volume over a
    /// real pipe, and this side writes `<serial>.db.svc`; then, not elevated,
    /// `NtfsSearcher::open` adopts and loads them and the queries are timed.
    /// `cargo test -p keel-search --release -- --ignored --nocapture live_index_service`
    #[test]
    #[ignore]
    fn live_index_service() {
        let dir = std::env::temp_dir().join("keel-index-test");
        if is_elevated() {
            std::fs::create_dir_all(&dir).unwrap();
            let started = Instant::now();
            let (name, pipe) = win::helper_pipe().unwrap();
            let helper = {
                let name = name.clone();
                std::thread::spawn(move || run_index_service(Path::new(&name)))
            };
            win::accept_pipe(&pipe).unwrap();
            receive_all(&dir, starts(&win::fixed_ntfs_volumes()), pipe).unwrap();
            helper.join().unwrap().unwrap();
            println!("index service: {:.2?}", started.elapsed());
            return;
        }
        let started = Instant::now();
        let searcher = NtfsSearcher::open(dir);
        println!(
            "open: {} entries in {:.2?}",
            searcher.len(),
            started.elapsed()
        );
        if searcher.is_empty() {
            println!("skipped: run this test elevated first");
            return;
        }
        for text in [
            "notepad.exe",
            "*.dll",
            "keel",
            r"regex:^kernel32\.dll$",
            "a",
            "e",
            "ab",
            "report",
            "folder: system",
            r"dll in:C:\Windows\System32",
            "zzqqxx-nothing",
        ] {
            let q = Query {
                text: text.into(),
                ..Query::default()
            };
            let matcher = pattern::compile(&q).unwrap();
            let parts = searcher.shared.parts.read();
            let started = Instant::now();
            let found: usize = parts
                .iter()
                .map(|p| p.index().search(&matcher, 500).len())
                .sum();
            let matched = started.elapsed();
            drop(parts);
            let started = Instant::now();
            let hits = searcher.query(&q).unwrap();
            println!(
                "{text:>30}: {found:>4} matched in {matched:>9.2?}; query() {:>3} hits in {:.2?}",
                hits.len(),
                started.elapsed()
            );
        }
        let started = Instant::now();
        let folders = crate::folder_index(&searcher).unwrap();
        println!(
            "folder_index: {} in {:.2?}",
            folders.len(),
            started.elapsed()
        );
    }

    /// `query()` time over the user-folder walk of this machine, per pattern: cold,
    /// then warm file-system cache, then `meta: false`. Patterns via `KEEL_PERF_QUERIES`
    /// (comma-separated). `cargo test -p keel-search --release -- --ignored --nocapture
    /// perf_query`
    #[test]
    #[ignore]
    fn perf_query_times() {
        let dir = std::env::temp_dir().join(format!("keel-index-perf-{}", std::process::id()));
        let searcher = NtfsSearcher::open(dir.clone());
        let started = Instant::now();
        while !searcher.available() {
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("{} entries in {:.2?}", searcher.len(), started.elapsed());
        let patterns = std::env::var("KEEL_PERF_QUERIES")
            .unwrap_or_else(|_| "*.dll,*.json,report,keel,a,*.png".into());
        for text in patterns.split(',') {
            let mut times = Vec::new();
            for meta in [true, true, false] {
                let started = Instant::now();
                let hits = searcher
                    .query(&Query {
                        text: text.into(),
                        meta,
                        ..Query::default()
                    })
                    .unwrap();
                times.push(format!(
                    "{:>4} hits {:>9.2?}",
                    hits.len(),
                    started.elapsed()
                ));
            }
            println!("{text:>10}: {}", times.join("  |  "));
        }
        drop(searcher);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The user-folder fallback on this machine: walk time and size.
    /// `cargo test -p keel-search --release -- --ignored --nocapture live_user_folder`
    #[test]
    #[ignore]
    fn live_user_folder_fallback() {
        if is_elevated() {
            println!("skipped: elevated (would read the MFT instead)");
            return;
        }
        let dir = std::env::temp_dir().join(format!("keel-index-empty-{}", std::process::id()));
        let started = Instant::now();
        let searcher = NtfsSearcher::open(dir.clone());
        while !searcher.available() {
            std::thread::sleep(Duration::from_millis(100));
        }
        println!(
            "walked {} entries in {:.2?}",
            searcher.len(),
            started.elapsed()
        );
        assert_eq!(searcher.status().as_deref(), Some(FALLBACK_STATUS));
        assert_eq!(searcher.state(), SearchState::Ready);
        assert!(searcher.take_ready(), "the finished walk is announced once");
        assert!(!searcher.take_ready());
        let home = directories::UserDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();
        let needle = home.join(format!("keel-fallback-{}.txt", std::process::id()));
        std::fs::write(&needle, b"x").unwrap();
        let name = needle.file_name().unwrap().to_string_lossy().into_owned();
        let query = Query {
            text: name.clone(),
            ..Query::default()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while searcher.query(&query).unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        let started = Instant::now();
        let hits = searcher.query(&query).unwrap();
        println!(
            "watcher picked up {name}; query in {:.2?}",
            started.elapsed()
        );
        std::fs::remove_file(&needle).unwrap();
        assert_eq!(hits.len(), 1);
        drop(searcher);
        let _ = std::fs::remove_dir_all(dir);
    }
}
