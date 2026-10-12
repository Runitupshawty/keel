//! The parallel walk: every folder is one rayon task that lists it, appends its children
//! to the arena in one batch (so siblings are contiguous and parents come before their
//! children) and spawns its subfolders. Totals are summed afterwards, in one pass from the
//! deepest items up.

use super::filter::Compiled;
use super::sys::{self, Raw};
use super::{
    attr, flag, LinkKind, Node, NodeId, NodeKind, Root, RootInfo, ScanHandle, ScanInfo,
    ScanOptions, Tree, NONE, TOP,
};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Folders deeper than this below a root are not entered ([`flag::TOO_DEEP`]).
pub(super) const MAX_DEPTH: usize = 512;

pub(super) fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// The tree being built, shared by the walker threads.
struct Arena {
    nodes: Mutex<Vec<Node>>,
    exts: Mutex<Exts>,
    /// Hard-link candidates: (identity, node).
    ids: Mutex<Vec<(u128, NodeId)>>,
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

    fn into_parts(self) -> (Vec<Node>, Vec<Box<str>>, Vec<(u128, NodeId)>) {
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

fn pool(threads: usize) -> Result<rayon::ThreadPool> {
    let n = match threads {
        0 => std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(2, 16),
        n => n.clamp(1, 64),
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        // One task per folder level: deep trees nest deeply.
        .stack_size(32 << 20)
        .thread_name(|i| format!("keel-du-{i}"))
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
                self.mark(job.node, flag::UNREADABLE);
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
        let path = dir.join(&raw.name);
        if self.filters.excludes(&name, &path, raw.attrs) {
            return self.skip();
        }
        if let Some(kind) = raw.link {
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
                b.subdirs.push((b.nodes.len(), Job { node: NONE, path, cluster, drive: false }));
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
        n.allocated = sys::allocated(raw.attrs, raw.size, raw.alloc, &path, cluster);
        n.flags |= file_flags(raw.attrs);
        if let Some(e) = ext_of(&n.name) {
            b.exts.push((b.nodes.len(), e));
        }
        // Unix tells which files have other names; a Windows listing does not.
        if self.opts.count_hard_links_once && raw.nlink != 1 {
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
                b.subdirs.push((b.nodes.len(), Job { node: NONE, path, cluster, drive: false }));
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
    let filters = Compiled::new(&opts.filters)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for r in roots {
        let found = match r {
            Root::Path(p) => vec![std::path::absolute(p).with_context(|| p.display().to_string())?],
            Root::AllLocalDrives => sys::local_drives(),
        };
        for p in found {
            let k = super::filter::key(&p);
            if !paths.iter().any(|q| super::filter::key(q) == k) {
                paths.push(p);
            }
        }
    }
    anyhow::ensure!(!paths.is_empty(), "nothing to scan: no local drives were found");
    anyhow::ensure!(paths.len() < 1 << 20, "too many roots");
    let pool = pool(opts.threads)?;
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
        });
        jobs.push(Job {
            node: id,
            path: p.clone(),
            cluster: sys::cluster_size(p),
            drive: space.is_some(),
        });
    }
    nodes[0].first_child = 1;
    nodes[0].child_count = paths.len() as u32;
    let arena = Arena::new(nodes, Vec::new());
    let walker = Walker::new(opts, &filters, handle, &arena, Vec::new());
    pool.install(|| jobs.into_par_iter().for_each(|j| walker.walk(j, 0, None)));
    let info = ScanInfo {
        started,
        elapsed: Duration::ZERO,
        complete: true,
        unreadable: walker.unreadable.load(Ordering::Relaxed),
        excluded: walker.excluded.load(Ordering::Relaxed),
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
    let complete = !tree.nodes().any(|(_, n)| n.has(flag::INCOMPLETE));
    tree.info.complete = complete;
    tree.info.elapsed = handle.finish();
    Ok(tree)
}

/// Groups files by identity: a group of two or more gets an entry in `tree.links`, and
/// every name but the first in tree order is a duplicate. `cands` are the new candidates;
/// files already known to be linked are added here.
pub(super) fn link_up(tree: &mut Tree, mut cands: Vec<(u128, NodeId)>) {
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
    handle.finish();
}

pub(super) fn refresh(tree: &mut Tree, id: NodeId, handle: &ScanHandle) -> Result<()> {
    let node = tree.get(id).context("no such item in the scan")?.clone();
    match node.kind {
        NodeKind::Dead => anyhow::bail!("the item was removed by an earlier refresh"),
        NodeKind::Top => {
            *tree = tree.rescan(handle)?;
            return Ok(());
        }
        NodeKind::Free | NodeKind::Unknown => {
            handle.start();
            for r in &mut tree.roots {
                if r.node == node.parent && r.space.is_some() {
                    r.space = sys::space(&r.path).or(r.space);
                }
            }
            tree.update_pseudo();
            handle.finish();
            return Ok(());
        }
        _ => {}
    }
    let path = tree.path(id).context("the item has no path")?;
    let opts = tree.options.clone();
    let filters = Compiled::new(&opts.filters)?;
    let pool = pool(opts.threads)?;
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
    // The folders above, for loop detection when links are followed.
    let mut base = Vec::new();
    let mut up = node.parent;
    while let Some(n) = tree.get(up) {
        if n.kind == NodeKind::Top {
            break;
        }
        if let Some(id) = tree.path(up).and_then(|p| sys::target(&p).ok()?.id) {
            base.push(id);
        }
        up = n.parent;
    }
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
