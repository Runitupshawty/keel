//! The parallel walk: every folder is one rayon task that lists it, appends its children
//! to the arena in one batch (so siblings are contiguous and parents come before their
//! children) and spawns its subfolders. Totals are summed afterwards, in one pass from the
//! deepest items up.

use super::filter::Compiled;
use super::mtp::{self, Backend, Session};
use super::priority::Priority;
use super::sys::{self, Raw};
use super::watch::Applied;
use super::{
    attr, flag, DeviceRoot, LinkKind, Node, NodeId, NodeKind, Root, RootInfo, ScanHandle, ScanInfo,
    ScanOptions, Space, Tree, NONE, TOP,
};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Folders deeper than this below a root are not entered ([`flag::TOO_DEEP`]).
pub(super) const MAX_DEPTH: usize = 512;

pub(super) fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Hard-link candidates: (identity, node).
type Candidates = Vec<(u128, NodeId)>;

/// The tree being built, shared by the walker threads.
struct Arena {
    nodes: Mutex<Vec<Node>>,
    exts: Mutex<Exts>,
    ids: Mutex<Candidates>,
}

struct Exts {
    list: Vec<Box<str>>,
    index: HashMap<Box<str>, u32>,
}

impl Exts {
    fn new(mut list: Vec<Box<str>>) -> Self {
        if list.is_empty() {
            list.push("".into());
        }
        let index = (list.iter().enumerate())
            .map(|(i, e)| (e.clone(), i as u32))
            .collect();
        Exts { list, index }
    }

    fn intern(&mut self, e: &str) -> u32 {
        if let Some(&i) = self.index.get(e) {
            return i;
        }
        let i = self.list.len() as u32;
        self.list.push(e.into());
        self.index.insert(e.into(), i);
        i
    }
}

impl Arena {
    fn new(nodes: Vec<Node>, exts: Vec<Box<str>>) -> Self {
        Arena {
            nodes: Mutex::new(nodes),
            exts: Mutex::new(Exts::new(exts)),
            ids: Mutex::new(Vec::new()),
        }
    }

    fn into_parts(self) -> (Vec<Node>, Vec<Box<str>>, Candidates) {
        (
            self.nodes.into_inner(),
            self.exts.into_inner().list,
            self.ids.into_inner(),
        )
    }
}

/// The folders above the one being listed (loop detection for followed links).
struct Anc<'p> {
    id: u128,
    up: Option<&'p Anc<'p>>,
}

/// A folder to walk.
struct Job {
    node: NodeId,
    path: PathBuf,
    cluster: u64,
    /// A drive root: gets free and unknown items.
    drive: bool,
}

/// A portable device's folder (or storage) to list.
struct DeviceJob {
    node: NodeId,
    device_id: String,
    /// The object id on the device.
    object: String,
    /// Its display path (`mtp://device/storage/...`).
    path: PathBuf,
    depth: usize,
    /// A storage root with space stated: gets free and unknown items.
    drive: bool,
}

/// One folder's children before they go into the arena.
#[derive(Default)]
struct Batch {
    nodes: Vec<Node>,
    exts: Vec<(usize, String)>,
    ids: Vec<(u128, usize)>,
    subdirs: Vec<(usize, Job)>,
    files: u64,
    folders: u64,
    bytes: u64,
}

struct Walker<'a> {
    opts: &'a ScanOptions,
    filters: &'a Compiled,
    handle: &'a ScanHandle,
    arena: &'a Arena,
    /// Identities of the folders above where the walk starts (a refreshed branch's).
    base: Vec<u128>,
    unreadable: AtomicU64,
    excluded: AtomicU64,
    /// A device walk ran out of time.
    timed_out: AtomicBool,
}

/// A file's extension as the file types list groups it: lowercase, after the last dot,
/// none for `.bashrc`, a trailing dot, or more than 16 characters or a space after it.
pub(super) fn ext_of(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 16 || ext.contains(' ') {
        return None;
    }
    Some(ext.to_lowercase())
}

fn pool(opts: &ScanOptions) -> Result<rayon::ThreadPool> {
    let n = match opts.threads {
        0 => std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(2, 16),
        n => n.clamp(1, 64),
    };
    let priority = opts.priority;
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        // One task per folder level: deep trees nest deeply.
        .stack_size(32 << 20)
        .thread_name(|i| format!("keel-du-{i}"))
        .start_handler(move |_| {
            if priority != Priority::Normal && !super::priority::apply(priority) {
                tracing::debug!("du: scan priority {priority:?} not applied");
            }
        })
        .build()
        .context("scan threads")
}

impl<'a> Walker<'a> {
    fn new(
        opts: &'a ScanOptions,
        filters: &'a Compiled,
        handle: &'a ScanHandle,
        arena: &'a Arena,
        base: Vec<u128>,
    ) -> Self {
        Walker {
            opts,
            filters,
            handle,
            arena,
            base,
            unreadable: AtomicU64::new(0),
            excluded: AtomicU64::new(0),
            timed_out: AtomicBool::new(false),
        }
    }

    fn looped(&self, mut anc: Option<&Anc<'_>>, id: u128) -> bool {
        while let Some(a) = anc {
            if a.id == id {
                return true;
            }
            anc = a.up;
        }
        self.base.contains(&id)
    }

    fn mark(&self, node: NodeId, f: u16) {
        self.arena.nodes.lock()[node as usize].flags |= f;
    }

    fn skip(&self) {
        self.excluded.fetch_add(1, Ordering::Relaxed);
    }

    /// Lists `job`'s folder, adds its children and walks its subfolders (in parallel).
    fn walk(&self, job: Job, depth: usize, anc: Option<&Anc<'_>>) {
        if !self.handle.checkpoint() {
            self.mark(job.node, flag::INCOMPLETE);
            return;
        }
        self.handle.at(&job.path);
        let listing = match sys::list(&job.path) {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!("du: {}: {e}", job.path.display());
                self.unreadable.fetch_add(1, Ordering::Relaxed);
                let denied = e.kind() == std::io::ErrorKind::PermissionDenied;
                self.mark(
                    job.node,
                    flag::UNREADABLE | if denied { flag::DENIED } else { 0 },
                );
                return;
            }
        };
        let here;
        let anc = match listing.id {
            Some(id) => {
                here = Anc { id, up: anc };
                Some(&here)
            }
            None => anc,
        };
        let mut b = Batch {
            nodes: Vec::with_capacity(listing.entries.len() + 2),
            ..Batch::default()
        };
        for raw in listing.entries {
            self.entry(raw, &job.path, job.cluster, depth, anc, &mut b);
        }
        if job.drive && self.opts.free_and_unknown {
            b.nodes.push(Node::new(NodeKind::Free, "<Free space>"));
            b.nodes.push(Node::new(NodeKind::Unknown, "<Unknown>"));
        }
        let (files, folders, bytes) = (b.files, b.folders, b.bytes);
        let subdirs = std::mem::take(&mut b.subdirs);
        let Some(first) = self.push(job.node, b) else {
            return;
        };
        self.handle.add(files, folders, bytes);
        let jobs: Vec<Job> = subdirs
            .into_iter()
            .map(|(i, mut j)| {
                j.node = first + i as NodeId;
                j
            })
            .collect();
        jobs.into_par_iter()
            .for_each(|j| self.walk(j, depth + 1, anc));
    }

    /// One listed entry of the folder `dir` (at `depth`) into `b`.
    fn entry(
        &self,
        raw: Raw,
        dir: &Path,
        cluster: u64,
        depth: usize,
        anc: Option<&Anc<'_>>,
        b: &mut Batch,
    ) {
        let name = raw.name.to_string_lossy().into_owned();
        // Joined only when needed: most files never need their full path.
        let joined = || dir.join(&raw.name);
        if self.filters.excludes(&name, raw.attrs, joined) {
            return self.skip();
        }
        if let Some(kind) = raw.link {
            let path = joined();
            return self.link(kind, raw, name, path, cluster, depth, anc, b);
        }
        let deep = depth + 1 > MAX_DEPTH;
        if raw.dir {
            let mut n = Node::new(NodeKind::Dir, name);
            n.attributes = raw.attrs;
            n.modified = raw.mtime;
            b.folders += 1;
            if raw.cloud() {
                n.flags |= flag::CLOUD;
            } else if deep {
                n.flags |= flag::TOO_DEEP;
            } else {
                b.subdirs.push((
                    b.nodes.len(),
                    Job {
                        node: NONE,
                        path: joined(),
                        cluster,
                        drive: false,
                    },
                ));
            }
            b.nodes.push(n);
            return;
        }
        if self.filters.excludes_file(raw.size, raw.mtime) {
            return self.skip();
        }
        let mut n = Node::new(NodeKind::File, name);
        n.attributes = raw.attrs;
        n.modified = raw.mtime;
        n.size = raw.size;
        n.allocated = sys::allocated(raw.attrs, raw.size, raw.alloc, joined, cluster);
        n.flags |= file_flags(raw.attrs);
        if let Some(e) = ext_of(&n.name) {
            b.exts.push((b.nodes.len(), e));
        }
        // Unix tells which files have other names; a Windows listing does not. When links
        // are followed, a file can also be reached by a path through a link (one name, but
        // seen twice), so every file is a candidate then.
        let follows = self.opts.follow_symlinks
            || self.opts.follow_junctions
            || self.opts.follow_mount_points;
        if self.opts.count_hard_links_once && (raw.nlink != 1 || follows) {
            if let Some(id) = raw.id {
                b.ids.push((id, b.nodes.len()));
            }
        }
        b.files += 1;
        b.bytes += raw.size;
        b.nodes.push(n);
    }

    /// A symlink, junction or mount point: listed as a link unless its kind is followed;
    /// a followed one is what it leads to (a folder that is not one of its own ancestors,
    /// or a file).
    #[allow(clippy::too_many_arguments)]
    fn link(
        &self,
        kind: LinkKind,
        raw: Raw,
        name: String,
        path: PathBuf,
        cluster: u64,
        depth: usize,
        anc: Option<&Anc<'_>>,
        b: &mut Batch,
    ) {
        if self.opts.exclude_reparse_points {
            return self.skip();
        }
        let follow = match kind {
            LinkKind::Symlink => self.opts.follow_symlinks,
            LinkKind::Junction => self.opts.follow_junctions,
            LinkKind::MountPoint => self.opts.follow_mount_points,
        };
        let mut n = Node::new(NodeKind::Link, name);
        n.attributes = raw.attrs;
        n.modified = raw.mtime;
        n.flags = kind.flag();
        if !follow {
            b.nodes.push(n);
            return;
        }
        let t = match sys::target(&path) {
            Ok(t) => t,
            Err(_) => {
                // Dangling, or the target cannot be read.
                n.flags |= flag::UNREADABLE;
                b.nodes.push(n);
                return;
            }
        };
        n.flags |= flag::FOLLOWED;
        n.modified = t.mtime;
        if t.dir {
            if t.id.is_some_and(|id| self.looped(anc, id)) {
                n.flags |= flag::LOOP;
                b.nodes.push(n);
                return;
            }
            n.kind = NodeKind::Dir;
            b.folders += 1;
            if t.attrs & attr::CLOUD != 0 {
                n.flags |= flag::CLOUD;
            } else if depth + 1 > MAX_DEPTH {
                n.flags |= flag::TOO_DEEP;
            } else {
                // The target may be on another volume.
                let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let cluster = sys::cluster_size(&real);
                b.subdirs.push((
                    b.nodes.len(),
                    Job {
                        node: NONE,
                        path,
                        cluster,
                        drive: false,
                    },
                ));
            }
            b.nodes.push(n);
            return;
        }
        if self.filters.excludes_file(t.size, t.mtime) {
            return self.skip();
        }
        n.kind = NodeKind::File;
        n.size = t.size;
        n.allocated = if t.attrs & attr::CLOUD != 0 {
            0
        } else {
            sys::round_up(t.alloc.unwrap_or(t.size), cluster)
        };
        n.flags |= file_flags(t.attrs);
        if let Some(e) = ext_of(&n.name) {
            b.exts.push((b.nodes.len(), e));
        }
        // The file also has its own name somewhere: always a candidate.
        if self.opts.count_hard_links_once {
            if let Some(id) = t.id {
                b.ids.push((id, b.nodes.len()));
            }
        }
        b.files += 1;
        b.bytes += t.size;
        b.nodes.push(n);
    }

    /// Walks device folders, one device connection per job, one request at a time.
    fn devices(&self, backend: &dyn Backend, jobs: Vec<DeviceJob>, deadline: Option<Instant>) {
        for job in jobs {
            match backend.open(&job.device_id) {
                Ok(mut s) => self.device(&mut *s, job, deadline),
                Err(e) => {
                    tracing::debug!("du: {}: {e:#}", job.path.display());
                    self.unreadable.fetch_add(1, Ordering::Relaxed);
                    self.mark(job.node, flag::UNREADABLE);
                }
            }
        }
    }

    /// Lists a device folder and everything below it, depth first, with an explicit stack
    /// (no recursion); stops at a cancel or the deadline (the folders not listed are
    /// flagged incomplete). An object listed a second time (a device bug that would loop)
    /// is left out.
    fn device(&self, s: &mut dyn Session, start: DeviceJob, deadline: Option<Instant>) {
        let late = || deadline.is_some_and(|d| Instant::now() >= d);
        let stop = || self.handle.is_cancelled() || late();
        let mut seen: HashSet<String> = HashSet::from([start.object.clone()]);
        let mut stack = vec![start];
        while let Some(job) = stack.pop() {
            if !self.handle.checkpoint() || stop() {
                if late() && !self.handle.is_cancelled() {
                    self.timed_out.store(true, Ordering::Relaxed);
                }
                self.mark(job.node, flag::INCOMPLETE);
                continue;
            }
            self.handle.at(&job.path);
            let objects = match s.children(&job.object, &stop) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("du: {}: {e:#}", job.path.display());
                    self.unreadable.fetch_add(1, Ordering::Relaxed);
                    self.mark(job.node, flag::UNREADABLE);
                    continue;
                }
            };
            if stop() {
                // Part of a listing: dropped, the folder is not scanned.
                stack.push(job);
                continue;
            }
            let mut b = Batch {
                nodes: Vec::with_capacity(objects.len() + 2),
                ..Batch::default()
            };
            // The object ids of `b.subdirs`, in order.
            let mut folders = Vec::new();
            for o in objects {
                if !seen.insert(o.id.clone()) {
                    tracing::debug!("du: {}: object {} listed again", job.path.display(), o.id);
                    continue;
                }
                let e = o.entry();
                let raw = Raw {
                    name: e.name.into(),
                    dir: e.folder,
                    attrs: e.attrs,
                    link: None,
                    size: e.size,
                    // A device has no clusters: what a file takes is its length.
                    alloc: Some(e.size),
                    mtime: e.modified,
                    id: None,
                    nlink: 0,
                };
                let before = b.subdirs.len();
                self.entry(raw, &job.path, 1, job.depth, None, &mut b);
                if b.subdirs.len() > before {
                    folders.push(o.id);
                }
            }
            if job.drive && self.opts.free_and_unknown {
                b.nodes.push(Node::new(NodeKind::Free, "<Free space>"));
                b.nodes.push(Node::new(NodeKind::Unknown, "<Unknown>"));
            }
            let (files, dirs, bytes) = (b.files, b.folders, b.bytes);
            let subdirs = std::mem::take(&mut b.subdirs);
            let Some(first) = self.push(job.node, b) else {
                continue;
            };
            self.handle.add(files, dirs, bytes);
            // Reversed: the first folder is listed first.
            for ((i, sub), object) in subdirs.into_iter().zip(folders).rev() {
                stack.push(DeviceJob {
                    node: first + i as NodeId,
                    device_id: job.device_id.clone(),
                    object,
                    path: sub.path,
                    depth: job.depth + 1,
                    drive: false,
                });
            }
        }
    }

    /// Appends `b`'s nodes as the children of `parent`; their first id (None when the
    /// arena is full: `parent` is flagged incomplete).
    fn push(&self, parent: NodeId, mut b: Batch) -> Option<NodeId> {
        if !b.exts.is_empty() {
            let mut x = self.arena.exts.lock();
            for (i, e) in &b.exts {
                b.nodes[*i].ext = x.intern(e);
            }
        }
        for n in &mut b.nodes {
            n.parent = parent;
        }
        let first = {
            let mut nodes = self.arena.nodes.lock();
            let first = nodes.len();
            if first + b.nodes.len() >= NONE as usize {
                nodes[parent as usize].flags |= flag::INCOMPLETE;
                return None;
            }
            let p = &mut nodes[parent as usize];
            if b.nodes.is_empty() {
                p.first_child = NONE;
                p.child_count = 0;
            } else {
                p.first_child = first as NodeId;
                p.child_count = b.nodes.len() as u32;
            }
            nodes.extend(b.nodes);
            first as NodeId
        };
        if !b.ids.is_empty() {
            (self.arena.ids.lock()).extend(b.ids.iter().map(|&(id, i)| (id, first + i as NodeId)));
        }
        Some(first)
    }
}

fn file_flags(attrs: u32) -> u16 {
    let mut f = 0;
    if attrs & attr::COMPRESSED != 0 {
        f |= flag::COMPRESSED;
    }
    if attrs & attr::SPARSE != 0 {
        f |= flag::SPARSE;
    }
    if attrs & attr::CLOUD != 0 {
        f |= flag::CLOUD;
    }
    f
}

pub(super) fn scan(roots: &[Root], opts: &ScanOptions, handle: &ScanHandle) -> Result<Tree> {
    scan_with(roots, opts, handle, &mtp::System)
}

/// When a device walk must stop.
fn deadline(opts: &ScanOptions) -> Option<Instant> {
    (opts.device_time_limit_secs > 0)
        .then(|| Instant::now().checked_add(Duration::from_secs(opts.device_time_limit_secs)))
        .flatten()
}

/// Each device root's device name and storage, asked before anything is walked.
fn resolve(devices: &dyn Backend, roots: &[DeviceRoot]) -> Result<Vec<(String, mtp::Storage)>> {
    let mut out = Vec::with_capacity(roots.len());
    for d in roots {
        let mut s = devices
            .open(&d.device_id)
            .context("the portable device is not connected")?;
        let storage = s
            .storage(&d.storage_id)
            .context("the storage is no longer on the portable device")?;
        out.push((s.name(), storage));
    }
    Ok(out)
}

/// A device storage's space, asked again (None when the device does not say).
fn device_space(devices: &dyn Backend, d: &DeviceRoot) -> Option<Space> {
    mtp::on_device_thread(|| {
        let mut s = devices.open(&d.device_id).ok()?;
        s.storage(&d.storage_id).ok()?.space
    })
}

pub(super) fn scan_with(
    roots: &[Root],
    opts: &ScanOptions,
    handle: &ScanHandle,
    devices: &dyn Backend,
) -> Result<Tree> {
    let filters = Compiled::new(&opts.filters)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut devs: Vec<DeviceRoot> = Vec::new();
    for r in roots {
        let found = match r {
            Root::Path(p) => vec![std::path::absolute(p).with_context(|| p.display().to_string())?],
            Root::AllLocalDrives => sys::local_drives(),
            Root::Mtp {
                device_id,
                storage_id,
            } => {
                let d = DeviceRoot {
                    device_id: device_id.clone(),
                    storage_id: storage_id.clone(),
                };
                if !devs.contains(&d) {
                    devs.push(d);
                }
                continue;
            }
        };
        for p in found {
            let k = super::filter::key(&p);
            if !paths.iter().any(|q| super::filter::key(q) == k) {
                paths.push(p);
            }
        }
    }
    anyhow::ensure!(
        !paths.is_empty() || !devs.is_empty(),
        "nothing to scan: no local drives were found"
    );
    anyhow::ensure!(paths.len() + devs.len() < 1 << 20, "too many roots");
    let storages = match devs.is_empty() {
        true => Vec::new(),
        false => mtp::on_device_thread(|| resolve(devices, &devs))?,
    };
    let pool = pool(opts)?;
    handle.start();
    let started = now_ns().div_euclid(1_000_000_000);
    let mut nodes = vec![Node::new(NodeKind::Top, "")];
    let mut infos = Vec::new();
    let mut jobs = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let t = sys::target(p).with_context(|| p.display().to_string())?;
        anyhow::ensure!(t.dir, "not a folder: {}", p.display());
        let id = (i + 1) as NodeId;
        let mut n = Node::new(NodeKind::Root, p.display().to_string());
        n.parent = TOP;
        n.attributes = t.attrs;
        n.modified = t.mtime;
        nodes.push(n);
        let space = (opts.free_and_unknown && sys::is_drive_root(p))
            .then(|| sys::space(p))
            .flatten();
        infos.push(RootInfo {
            node: id,
            path: p.clone(),
            space,
            device: None,
        });
        jobs.push(Job {
            node: id,
            path: p.clone(),
            cluster: sys::cluster_size(p),
            drive: space.is_some(),
        });
    }
    let mut device_jobs = Vec::new();
    for (d, (device, storage)) in devs.iter().zip(storages) {
        let id = nodes.len() as NodeId;
        let base = format!(
            "mtp://{}/{}",
            mtp::clean_name(&device),
            mtp::clean_name(&storage.name)
        );
        // Two alike devices: each root keeps a path of its own.
        let mut path = PathBuf::from(&base);
        let mut k = 2;
        while (infos.iter()).any(|r| super::filter::key(&r.path) == super::filter::key(&path)) {
            path = PathBuf::from(format!("{base} ({k})"));
            k += 1;
        }
        let mut n = Node::new(NodeKind::Root, path.display().to_string());
        n.parent = TOP;
        n.attributes = attr::DIRECTORY;
        nodes.push(n);
        let space = storage.space.filter(|_| opts.free_and_unknown);
        infos.push(RootInfo {
            node: id,
            path: path.clone(),
            space,
            device: Some(d.clone()),
        });
        device_jobs.push(DeviceJob {
            node: id,
            device_id: d.device_id.clone(),
            object: d.storage_id.clone(),
            path,
            depth: 0,
            drive: space.is_some(),
        });
    }
    nodes[0].first_child = 1;
    nodes[0].child_count = infos.len() as u32;
    let arena = Arena::new(nodes, Vec::new());
    let walker = Walker::new(opts, &filters, handle, &arena, Vec::new());
    let walk_paths = || pool.install(|| jobs.into_par_iter().for_each(|j| walker.walk(j, 0, None)));
    if device_jobs.is_empty() {
        walk_paths();
    } else {
        // Devices are listed on a thread of their own while the folders are walked.
        let until = deadline(opts);
        std::thread::scope(|s| {
            let folders = s.spawn(walk_paths);
            mtp::on_device_thread(|| walker.devices(devices, device_jobs, until));
            if let Err(p) = folders.join() {
                std::panic::resume_unwind(p);
            }
        });
    }
    let info = ScanInfo {
        started,
        elapsed: Duration::ZERO,
        complete: true,
        unreadable: walker.unreadable.load(Ordering::Relaxed),
        excluded: walker.excluded.load(Ordering::Relaxed),
        timed_out: walker.timed_out.load(Ordering::Relaxed),
        denied: 0,
    };
    drop(walker);
    let (nodes, exts, ids) = arena.into_parts();
    let mut tree = Tree {
        nodes,
        exts,
        links: Vec::new(),
        roots: infos,
        options: opts.clone(),
        info,
    };
    link_up(&mut tree, ids);
    tree.recompute();
    tree.info.denied = tree.count_denied();
    let complete = !tree.nodes().any(|(_, n)| n.has(flag::INCOMPLETE));
    tree.info.complete = complete;
    tree.info.elapsed = handle.finish();
    Ok(tree)
}

/// Groups files by identity: a group of two or more gets an entry in `tree.links`, and
/// every name but the first in tree order is a duplicate. `cands` are the new candidates;
/// files already known to be linked are added here.
pub(super) fn link_up(tree: &mut Tree, mut cands: Candidates) {
    for (i, n) in tree.nodes.iter().enumerate() {
        if n.link != 0 && n.kind != NodeKind::Dead {
            if let Some(&id) = tree.links.get(n.link as usize - 1) {
                cands.push((id, i as NodeId));
            }
        }
    }
    for n in &mut tree.nodes {
        n.link = 0;
        n.flags &= !(flag::HARDLINK | flag::HARDLINK_DUP);
    }
    tree.links.clear();
    if !tree.options.count_hard_links_once {
        return;
    }
    cands.sort_unstable();
    cands.dedup();
    let mut i = 0;
    while i < cands.len() {
        let mut j = i + 1;
        while j < cands.len() && cands[j].0 == cands[i].0 {
            j += 1;
        }
        if j - i > 1 {
            tree.links.push(cands[i].0);
            let link = tree.links.len() as u32;
            for (k, &(_, node)) in cands[i..j].iter().enumerate() {
                let n = &mut tree.nodes[node as usize];
                n.link = link;
                n.flags |= flag::HARDLINK;
                if k > 0 {
                    n.flags |= flag::HARDLINK_DUP;
                }
            }
        }
        i = j;
    }
}

/// Marks `id`'s descendants Dead (and `id` too when `this`).
fn kill(tree: &mut Tree, id: NodeId, this: bool) {
    let mut stack = vec![id];
    while let Some(at) = stack.pop() {
        stack.extend(tree.children(at));
        if at != id || this {
            tree.nodes[at as usize].kind = NodeKind::Dead;
        }
    }
}

fn finish(tree: &mut Tree, handle: &ScanHandle) {
    tree.recompute();
    let complete = !tree.nodes().any(|(_, n)| n.has(flag::INCOMPLETE));
    tree.info.complete = complete;
    tree.info.unreadable = (tree.nodes())
        .filter(|(_, n)| n.is_container() && n.has(flag::UNREADABLE))
        .count() as u64;
    tree.info.denied = tree.count_denied();
    handle.finish();
}

/// The identities of `from` and the folders above it, for loop detection when links are
/// followed.
fn ancestor_ids(tree: &Tree, from: NodeId) -> Vec<u128> {
    let mut base = Vec::new();
    let mut up = from;
    while let Some(n) = tree.get(up) {
        if n.kind == NodeKind::Top {
            break;
        }
        if let Some(id) = tree.path(up).and_then(|p| sys::target(&p).ok()?.id) {
            base.push(id);
        }
        up = n.parent;
    }
    base
}

pub(super) fn refresh(tree: &mut Tree, id: NodeId, handle: &ScanHandle) -> Result<()> {
    refresh_with(tree, id, handle, &mtp::System)
}

pub(super) fn refresh_with(
    tree: &mut Tree,
    id: NodeId,
    handle: &ScanHandle,
    devices: &dyn Backend,
) -> Result<()> {
    let node = tree.get(id).context("no such item in the scan")?.clone();
    match node.kind {
        NodeKind::Dead => anyhow::bail!("the item was removed by an earlier refresh"),
        NodeKind::Top => {
            *tree = scan_with(&tree.scan_roots(), &tree.options, handle, devices)?;
            return Ok(());
        }
        NodeKind::Free | NodeKind::Unknown => {
            handle.start();
            for r in &mut tree.roots {
                if r.node == node.parent && r.space.is_some() {
                    let now = match &r.device {
                        Some(d) => device_space(devices, d),
                        None => sys::space(&r.path),
                    };
                    r.space = now.or(r.space);
                }
            }
            tree.update_pseudo();
            handle.finish();
            return Ok(());
        }
        _ => {}
    }
    if let Some(d) = tree.root_of(id).and_then(|r| r.device.clone()) {
        return refresh_device(tree, id, d, handle, devices);
    }
    let path = tree.path(id).context("the item has no path")?;
    let opts = tree.options.clone();
    let filters = Compiled::new(&opts.filters)?;
    let pool = pool(&opts)?;
    handle.start();
    if node.kind != NodeKind::Root {
        if let Err(e) = std::fs::symlink_metadata(&path) {
            if e.kind() == std::io::ErrorKind::NotFound {
                kill(tree, id, true);
                finish(tree, handle);
                return Ok(());
            }
        }
    }
    if !node.is_container() {
        return refresh_entry(tree, id, &path, &opts, &filters, handle);
    }
    let depth = tree.depth(id);
    let base = ancestor_ids(tree, node.parent);
    kill(tree, id, false);
    {
        let n = &mut tree.nodes[id as usize];
        n.flags &= !(flag::UNREADABLE | flag::INCOMPLETE | flag::TOO_DEEP);
        n.first_child = NONE;
        n.child_count = 0;
    }
    let mut drive = false;
    if node.kind == NodeKind::Root {
        if let Some(r) = tree.roots.iter_mut().find(|r| r.node == id) {
            if r.space.is_some() {
                r.space = sys::space(&r.path).or(r.space);
                drive = true;
            }
        }
    }
    let arena = Arena::new(
        std::mem::take(&mut tree.nodes),
        std::mem::take(&mut tree.exts),
    );
    let walker = Walker::new(&opts, &filters, handle, &arena, base);
    let job = Job {
        node: id,
        cluster: sys::cluster_size(&path),
        path,
        drive,
    };
    pool.install(|| walker.walk(job, depth, None));
    tree.info.excluded += walker.excluded.load(Ordering::Relaxed);
    drop(walker);
    let (nodes, exts, ids) = arena.into_parts();
    tree.nodes = nodes;
    tree.exts = exts;
    link_up(tree, ids);
    finish(tree, handle);
    Ok(())
}

/// An item on a device: its folder (a file's folder) is found again by its names from the
/// storage down and listed again; a folder that is gone is removed.
fn refresh_device(
    tree: &mut Tree,
    id: NodeId,
    dev: DeviceRoot,
    handle: &ScanHandle,
    devices: &dyn Backend,
) -> Result<()> {
    let target = match tree.node(id).is_container() {
        true => id,
        false => tree.node(id).parent,
    };
    let mut names = Vec::new();
    let mut at = target;
    while tree.node(at).kind == NodeKind::Dir {
        names.push(tree.node(at).name.to_string());
        at = tree.node(at).parent;
    }
    names.reverse();
    let is_root = tree.node(target).kind == NodeKind::Root;
    let path = tree.path(target).context("the item has no path")?;
    let depth = tree.depth(target);
    let opts = tree.options.clone();
    let filters = Compiled::new(&opts.filters)?;
    handle.start();
    // The folder's object id, by name from the storage down (and the storage's space).
    let found = mtp::on_device_thread(|| -> Result<(Option<String>, Option<Space>)> {
        let mut s =
            (devices.open(&dev.device_id)).context("the portable device is not connected")?;
        let space = match is_root {
            true => s.storage(&dev.storage_id)?.space,
            false => None,
        };
        let mut object = dev.storage_id.clone();
        for name in &names {
            let want = super::filter::name_key(name);
            let next = (s.children(&object, &|| false)?).into_iter().find(|k| {
                let e = k.entry();
                e.folder && super::filter::name_key(&e.name) == want
            });
            match next {
                Some(k) => object = k.id,
                None => return Ok((None, space)),
            }
        }
        Ok((Some(object), space))
    });
    let (object, space) = match found {
        Ok(v) => v,
        Err(e) => {
            handle.finish();
            return Err(e);
        }
    };
    let Some(object) = object else {
        // Gone from the device.
        kill(tree, target, true);
        finish(tree, handle);
        return Ok(());
    };
    kill(tree, target, false);
    {
        let n = &mut tree.nodes[target as usize];
        n.flags &= !(flag::UNREADABLE | flag::INCOMPLETE | flag::TOO_DEEP);
        n.first_child = NONE;
        n.child_count = 0;
    }
    let mut drive = false;
    if is_root {
        if let Some(r) = tree.roots.iter_mut().find(|r| r.node == target) {
            if r.space.is_some() {
                r.space = space.or(r.space);
                drive = true;
            }
        }
    }
    let arena = Arena::new(
        std::mem::take(&mut tree.nodes),
        std::mem::take(&mut tree.exts),
    );
    let walker = Walker::new(&opts, &filters, handle, &arena, Vec::new());
    let job = DeviceJob {
        node: target,
        device_id: dev.device_id,
        object,
        path,
        depth,
        drive,
    };
    let until = deadline(&opts);
    mtp::on_device_thread(|| walker.devices(devices, vec![job], until));
    tree.info.excluded += walker.excluded.load(Ordering::Relaxed);
    tree.info.timed_out |= walker.timed_out.load(Ordering::Relaxed);
    drop(walker);
    let (nodes, exts, ids) = arena.into_parts();
    tree.nodes = nodes;
    tree.exts = exts;
    link_up(tree, ids);
    finish(tree, handle);
    Ok(())
}

/// A file or a link: its entry in its folder's listing again.
fn refresh_entry(
    tree: &mut Tree,
    id: NodeId,
    path: &Path,
    opts: &ScanOptions,
    filters: &Compiled,
    handle: &ScanHandle,
) -> Result<()> {
    let parent = tree.node(id).parent;
    let dir = path.parent().context("the item has no folder")?;
    let name = path.file_name().context("the item has no name")?;
    let listing = sys::list(dir).with_context(|| dir.display().to_string())?;
    let Some(raw) = listing.entries.into_iter().find(|r| r.name == name) else {
        kill(tree, id, true);
        finish(tree, handle);
        return Ok(());
    };
    let arena = Arena::new(Vec::new(), Vec::new());
    let walker = Walker::new(opts, filters, handle, &arena, Vec::new());
    let mut b = Batch::default();
    walker.entry(
        raw,
        dir,
        sys::cluster_size(dir),
        tree.depth(parent),
        None,
        &mut b,
    );
    tree.info.excluded += walker.excluded.load(Ordering::Relaxed);
    drop(walker);
    if b.nodes.first().is_some_and(|n| n.kind == NodeKind::Dir) {
        // It is a folder now: its folder is listed again.
        return refresh(tree, parent, handle);
    }
    let Some(mut n) = b.nodes.pop() else {
        // Left out by the filters now.
        kill(tree, id, true);
        finish(tree, handle);
        return Ok(());
    };
    n.parent = parent;
    if let Some((_, e)) = b.exts.first() {
        let mut x = Exts::new(std::mem::take(&mut tree.exts));
        n.ext = x.intern(e);
        tree.exts = x.list;
    }
    tree.nodes[id as usize] = n;
    let ids = b.ids.iter().map(|&(i, _)| (i, id)).collect();
    link_up(tree, ids);
    finish(tree, handle);
    Ok(())
}

/// The File Watcher's changes ([`Tree::apply_changes`]): each path is read again from the
/// disk and the tree follows (see [`super::watch`]).
pub(super) fn apply(tree: &mut Tree, paths: &[PathBuf]) -> Result<Applied> {
    let handle = ScanHandle::new();
    let opts = tree.options.clone();
    let filters = Compiled::new(&opts.filters)?;
    handle.start();
    let before = tree.nodes.len();
    let mut out = Applied::default();
    // A notification may name a path through another spelling of a root (macOS: /private
    // before /var): such paths are put back under the root as scanned.
    let canon: Vec<(PathBuf, PathBuf)> = (tree.roots.iter())
        .filter(|r| r.device.is_none())
        .filter_map(|r| Some((std::fs::canonicalize(&r.path).ok()?, r.path.clone())))
        .filter(|(c, p)| c != p)
        .collect();
    // Per folder: the folder and the names in it to read again.
    let mut groups: Vec<(PathBuf, Vec<OsString>)> = Vec::new();
    let mut group_of: HashMap<String, usize> = HashMap::new();
    let mut add = |dir: &Path, name: &OsStr| {
        let i = *group_of.entry(super::filter::key(dir)).or_insert_with(|| {
            groups.push((dir.to_owned(), Vec::new()));
            groups.len() - 1
        });
        if !groups[i].1.iter().any(|n| n == name) {
            groups[i].1.push(name.to_owned());
        }
    };
    let mut seen = HashSet::new();
    for p in paths {
        let mut p = p.clone();
        for (c, root) in &canon {
            if let Ok(rest) = p.strip_prefix(c) {
                p = root.join(rest);
                break;
            }
        }
        if !seen.insert(super::filter::key(&p)) {
            continue;
        }
        match tree.find(&p) {
            Some(id) => {
                let n = tree.node(id);
                if matches!(n.kind, NodeKind::Top | NodeKind::Root) || n.is_pseudo() {
                    continue;
                }
                if n.is_container() {
                    match std::fs::symlink_metadata(&p) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            kill(tree, id, true);
                            out.removed += 1;
                            continue;
                        }
                        // Still a folder: what changes in it is notified on its own.
                        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => continue,
                        _ => {}
                    }
                }
                if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
                    add(dir, name);
                }
            }
            None => {
                // New: the item right below the deepest folder of the scan above it.
                let mut child = p.as_path();
                while let Some(dir) = child.parent() {
                    if let Some(a) = tree.find(dir) {
                        let n = tree.node(a);
                        let not_listed = flag::CLOUD | flag::TOO_DEEP | flag::LOOP;
                        if let (true, Some(name)) =
                            (n.is_container() && !n.has(not_listed), child.file_name())
                        {
                            add(dir, name);
                        }
                        break;
                    }
                    child = dir;
                }
            }
        }
    }
    // (current id, id before) of the items moved to make room.
    let mut moved: HashMap<NodeId, NodeId> = HashMap::new();
    let mut cands: Candidates = Vec::new();
    for (dir, names) in groups {
        let Some(pid) = tree.find(&dir).filter(|&id| tree.node(id).is_container()) else {
            continue;
        };
        let listing = match sys::list(&dir) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if tree.node(pid).kind != NodeKind::Root {
                    kill(tree, pid, true);
                    out.removed += 1;
                }
                continue;
            }
            Err(e) => {
                tracing::debug!("du: watch: {}: {e}", dir.display());
                continue;
            }
        };
        // Listed now: it is not unreadable any more.
        tree.nodes[pid as usize].flags &= !(flag::UNREADABLE | flag::DENIED);
        let depth = tree.depth(pid);
        let cluster = sys::cluster_size(&dir);
        let existing: HashMap<String, NodeId> = (tree.children(pid))
            .filter(|&c| !tree.node(c).is_pseudo())
            .map(|c| (super::filter::name_key(&tree.node(c).name), c))
            .collect();
        let mut fresh = Vec::new();
        for name in names {
            let raw = listing.entries.iter().find(|r| r.name == name).cloned();
            let had = (existing.get(&super::filter::name_key(&name.to_string_lossy()))).copied();
            match (had, raw) {
                (None, None) => {}
                (Some(id), None) => {
                    kill(tree, id, true);
                    out.removed += 1;
                }
                (None, Some(raw)) => fresh.push(raw),
                (Some(id), Some(raw)) => {
                    let dummy = Arena::new(Vec::new(), Vec::new());
                    let w = Walker::new(&opts, &filters, &handle, &dummy, Vec::new());
                    let mut b = Batch::default();
                    w.entry(raw.clone(), &dir, cluster, depth, None, &mut b);
                    tree.info.excluded += w.excluded.load(Ordering::Relaxed);
                    drop(w);
                    let old = tree.node(id).kind;
                    match b.nodes.pop() {
                        // Left out by the filters now.
                        None => {
                            kill(tree, id, true);
                            out.removed += 1;
                        }
                        // The same kind of item: read again in place.
                        Some(mut n) if n.kind == old && !n.is_container() => {
                            n.parent = pid;
                            if let Some((_, e)) = b.exts.first() {
                                let mut x = Exts::new(std::mem::take(&mut tree.exts));
                                n.ext = x.intern(e);
                                tree.exts = x.list;
                            }
                            tree.nodes[id as usize] = n;
                            cands.extend(b.ids.iter().map(|&(i, _)| (i, id)));
                            out.updated += 1;
                        }
                        // A file that is a folder now, or the other way round.
                        Some(_) => {
                            kill(tree, id, true);
                            out.removed += 1;
                            fresh.push(raw);
                        }
                    }
                }
            }
        }
        if fresh.is_empty() {
            continue;
        }
        let (added, moves, ids) = append(tree, pid, &dir, fresh, &opts, &filters, &handle)?;
        out.added += added;
        cands.extend(ids);
        for (from, to) in moves {
            let first = moved.remove(&from).unwrap_or(from);
            moved.insert(to, first);
        }
    }
    if out.added > 0 || out.removed > 0 {
        // Siblings stay contiguous, but moved children now come after their own children:
        // compacting puts parents first again (and drops what was removed).
        let remap = tree.compact();
        for c in &mut cands {
            c.1 = remap.get(c.1 as usize).copied().unwrap_or(NONE);
        }
        cands.retain(|c| c.1 != NONE);
        let mut map: Vec<NodeId> = remap.iter().take(before).copied().collect();
        for (now, first) in moved {
            if let Some(slot) = map.get_mut(first as usize) {
                *slot = remap.get(now as usize).copied().unwrap_or(NONE);
            }
        }
        out.remap = Some(map);
    }
    if out.is_empty() {
        handle.finish();
    } else {
        link_up(tree, cands);
        finish(tree, &handle);
    }
    Ok(out)
}

/// Adds `raws` (entries of the folder `pid` at `dir`) as new children of `pid`, walking new
/// folders. The folder's live children move to the end of the arena with them, so its
/// children stay contiguous. Returns how many were added, the (old, new) ids of the moved
/// children and the new hard-link candidates.
#[allow(clippy::type_complexity)]
fn append(
    tree: &mut Tree,
    pid: NodeId,
    dir: &Path,
    raws: Vec<Raw>,
    opts: &ScanOptions,
    filters: &Compiled,
    handle: &ScanHandle,
) -> Result<(u64, Vec<(NodeId, NodeId)>, Candidates)> {
    let depth = tree.depth(pid);
    let follows = opts.follow_symlinks || opts.follow_junctions || opts.follow_mount_points;
    let base = match follows {
        true => ancestor_ids(tree, pid),
        false => Vec::new(),
    };
    let old: Vec<NodeId> = tree.children(pid).collect();
    let kept: Vec<Node> = old
        .iter()
        .map(|&c| tree.nodes[c as usize].clone())
        .collect();
    let cluster = sys::cluster_size(dir);
    let pool = pool(opts)?;
    let arena = Arena::new(
        std::mem::take(&mut tree.nodes),
        std::mem::take(&mut tree.exts),
    );
    let walker = Walker::new(opts, filters, handle, &arena, base);
    let mut fresh = Batch::default();
    for raw in raws {
        walker.entry(raw, dir, cluster, depth, None, &mut fresh);
    }
    let added = fresh.nodes.len() as u64;
    let mut moves = Vec::new();
    if added > 0 {
        let k = old.len();
        let mut b = Batch {
            nodes: kept,
            ..Batch::default()
        };
        b.nodes.extend(fresh.nodes);
        b.exts = fresh.exts.into_iter().map(|(i, e)| (i + k, e)).collect();
        b.ids = fresh.ids.into_iter().map(|(id, i)| (id, i + k)).collect();
        let subdirs: Vec<(usize, Job)> = (fresh.subdirs.into_iter())
            .map(|(i, j)| (i + k, j))
            .collect();
        if let Some(first) = walker.push(pid, b) {
            {
                let mut nodes = arena.nodes.lock();
                for (i, &o) in old.iter().enumerate() {
                    let new = first + i as NodeId;
                    nodes[o as usize].kind = NodeKind::Dead;
                    let (f, c) = (
                        nodes[new as usize].first_child,
                        nodes[new as usize].child_count,
                    );
                    if f != NONE {
                        for g in f..f + c {
                            nodes[g as usize].parent = new;
                        }
                    }
                    moves.push((o, new));
                }
            }
            let jobs: Vec<Job> = (subdirs.into_iter())
                .map(|(i, mut j)| {
                    j.node = first + i as NodeId;
                    j
                })
                .collect();
            pool.install(|| {
                jobs.into_par_iter()
                    .for_each(|j| walker.walk(j, depth + 1, None))
            });
        }
    }
    tree.info.excluded += walker.excluded.load(Ordering::Relaxed);
    drop(walker);
    let (nodes, exts, ids) = arena.into_parts();
    tree.nodes = nodes;
    tree.exts = exts;
    Ok((added, moves, ids))
}
