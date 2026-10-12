//! Disk usage scans (Phase 17 Task 81): one or many roots (folders, drives, or all local
//! fixed drives) walked in parallel into one arena tree, the data behind `keel du --scan`
//! and the Disk Usage window.
//!
//! - **Parallel:** a work-stealing pool (rayon); every folder is one task, so a wide drive
//!   keeps every thread busy and several roots scan at once.
//! - **Size on disk** (`allocated`): Windows takes the allocation size the directory
//!   enumeration reports (no per-file open), and for compressed and sparse files
//!   `GetCompressedFileSizeW` rounded up to the volume's cluster; elsewhere
//!   `st_blocks * 512`.
//! - **Hard links** count once (by volume + file id) when `count_hard_links_once` is on
//!   (the default): the first name in tree order counts, the others are flagged
//!   [`flag::HARDLINK_DUP`] and add nothing to their folders.
//! - **Links** (symlinks, junctions, mount points) are listed as [`NodeKind::Link`] items
//!   and not followed unless the option for their kind says so; a followed link that leads
//!   back to a folder it is in is not entered ([`flag::LOOP`]).
//! - **Cloud placeholders** are never read: files offline or recalled on access count by
//!   their stated size with nothing on disk, and folders recalled on open are not listed
//!   ([`flag::CLOUD`]), so a scan never downloads anything.
//! - **Filters** leave items out of the tree entirely (see [`Filters`]).
//! - Free and unknown space are pseudo items of a drive root ([`NodeKind::Free`],
//!   [`NodeKind::Unknown`] = used − scanned on-disk size); they are not in any total.
//!
//! Node ids are stable until [`Tree::compact`] (saving and loading compact). Refreshing a
//! branch marks its old items [`NodeKind::Dead`] and appends the new ones.

pub mod export;
#[cfg(test)]
mod export_tests;
mod file;
mod filter;
pub mod report;
mod sys;
#[cfg(test)]
mod tests;
mod walk;

pub use file::{load, load_from, save, save_to, FORMAT, VERSION};
pub use filter::Filters;

use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// An item's index in [`Tree`]'s arena.
pub type NodeId = u32;
/// No node (the top item's parent, an empty folder's first child).
pub const NONE: NodeId = u32::MAX;
/// A time that is not known.
pub const UNKNOWN_TIME: i64 = i64::MIN;

/// What an item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NodeKind {
    /// The one item above the roots (node 0).
    Top = 0,
    /// A scanned root: a folder or a drive.
    Root = 1,
    Dir = 2,
    File = 3,
    /// A symlink, junction or mount point that was not followed (or could not be).
    Link = 4,
    /// A drive's free space (pseudo item).
    Free = 5,
    /// A drive's used space the scan did not find (pseudo item).
    Unknown = 6,
    /// Removed by a refresh; skipped everywhere.
    Dead = 7,
}

impl NodeKind {
    pub(crate) fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Top,
            1 => Self::Root,
            2 => Self::Dir,
            3 => Self::File,
            4 => Self::Link,
            5 => Self::Free,
            6 => Self::Unknown,
            7 => Self::Dead,
            _ => return None,
        })
    }

    /// The name used in JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Root => "root",
            Self::Dir => "dir",
            Self::File => "file",
            Self::Link => "link",
            Self::Free => "free",
            Self::Unknown => "unknown",
            Self::Dead => "dead",
        }
    }
}

/// [`Node::flags`] bits.
pub mod flag {
    /// A further name of a hard-linked file already counted elsewhere: adds nothing.
    pub const HARDLINK_DUP: u16 = 1;
    /// A folder that could not be listed (counted as empty).
    pub const UNREADABLE: u16 = 1 << 1;
    /// A cloud placeholder: a file not on disk, or a folder not listed (never hydrated).
    pub const CLOUD: u16 = 1 << 2;
    /// A followed link leading back to a folder it is in: not entered.
    pub const LOOP: u16 = 1 << 3;
    /// A folder deeper than 512 levels: not entered.
    pub const TOO_DEEP: u16 = 1 << 4;
    /// Not walked because the scan was cancelled.
    pub const INCOMPLETE: u16 = 1 << 5;
    /// NTFS-compressed.
    pub const COMPRESSED: u16 = 1 << 6;
    /// Sparse.
    pub const SPARSE: u16 = 1 << 7;
    /// A file with other names (hard links) in the scan; [`super::Node::link`] says which.
    pub const HARDLINK: u16 = 1 << 8;
    /// A symbolic link (followed or not).
    pub const SYMLINK: u16 = 1 << 9;
    /// A junction (followed or not).
    pub const JUNCTION: u16 = 1 << 10;
    /// A mount point: a volume mounted in a folder (followed or not).
    pub const MOUNT: u16 = 1 << 11;
    /// A link that was followed: the item is what it leads to.
    pub const FOLLOWED: u16 = 1 << 12;
    /// Every known bit (a loaded scan keeps only these).
    pub const ALL: u16 = (1 << 13) - 1;
}

/// File attributes, Windows' bits; synthesized elsewhere (read-only from the write bits,
/// hidden from a leading dot or the macOS hidden flag, directory, reparse for a symlink).
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const SPARSE: u32 = 0x200;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const COMPRESSED: u32 = 0x800;
    pub const OFFLINE: u32 = 0x1000;
    pub const RECALL_ON_OPEN: u32 = 0x4_0000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    /// Any of these: a cloud placeholder whose data is not on this disk.
    pub const CLOUD: u32 = OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS;
}

/// The kinds of links a scan may follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Symlink,
    Junction,
    MountPoint,
}

impl LinkKind {
    fn flag(self) -> u16 {
        match self {
            Self::Symlink => flag::SYMLINK,
            Self::Junction => flag::JUNCTION,
            Self::MountPoint => flag::MOUNT,
        }
    }
}

/// One item. For folders (and roots and the top item) the sizes and counts are of the
/// whole subtree; for a file they are its own.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub name: Box<str>,
    pub parent: NodeId,
    /// The children are `first_child .. first_child + child_count` (some may be Dead).
    pub first_child: NodeId,
    pub child_count: u32,
    pub kind: NodeKind,
    pub flags: u16,
    /// [`attr`] bits.
    pub attributes: u32,
    /// The extension's index in [`Tree::exts`] (0: none); files only.
    pub ext: u32,
    /// Hard-link identity: 1 + its index in [`Tree::link_id`]'s table, 0 when the file has
    /// no other name in the scan.
    pub link: u32,
    /// Bytes (a file's length; a folder's total).
    pub size: u64,
    /// Bytes on disk.
    pub allocated: u64,
    /// Files below (a file: 0).
    pub files: u64,
    /// Folders below.
    pub folders: u64,
    /// Files, folders and links below.
    pub items: u64,
    /// The item's own last-write time (unix ns, [`UNKNOWN_TIME`] when not known).
    pub modified: i64,
    /// The newest last-write time in the subtree (a file's own).
    pub newest: i64,
}

impl Node {
    pub(crate) fn new(kind: NodeKind, name: impl Into<Box<str>>) -> Self {
        Node {
            name: name.into(),
            parent: NONE,
            first_child: NONE,
            child_count: 0,
            kind,
            flags: 0,
            attributes: 0,
            ext: 0,
            link: 0,
            size: 0,
            allocated: 0,
            files: 0,
            folders: 0,
            items: 0,
            modified: UNKNOWN_TIME,
            newest: UNKNOWN_TIME,
        }
    }

    /// The top item, a root or a folder.
    pub fn is_container(&self) -> bool {
        matches!(self.kind, NodeKind::Top | NodeKind::Root | NodeKind::Dir)
    }

    /// Free or unknown space.
    pub fn is_pseudo(&self) -> bool {
        matches!(self.kind, NodeKind::Free | NodeKind::Unknown)
    }

    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }
}

/// What to scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Root {
    /// A folder or a drive (`C:\`, `/`).
    Path(PathBuf),
    /// Every local fixed drive (Windows), or every user mount that is not removable or a
    /// network share (elsewhere).
    AllLocalDrives,
}

/// How a scan walks. Kept with the tree (a refresh walks the same way) and in scan files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanOptions {
    /// A file with several names counts once (default on).
    pub count_hard_links_once: bool,
    pub follow_symlinks: bool,
    pub follow_junctions: bool,
    /// Also crossing into another filesystem mounted in a folder (Unix: any mount below).
    pub follow_mount_points: bool,
    /// Leave links out of the tree entirely (not even listed).
    pub exclude_reparse_points: bool,
    pub filters: Filters,
    /// Walker threads; 0 picks one per CPU (2 to 16).
    pub threads: usize,
    /// Free and unknown pseudo items for drive roots (default on).
    pub free_and_unknown: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            count_hard_links_once: true,
            follow_symlinks: false,
            follow_junctions: false,
            follow_mount_points: false,
            exclude_reparse_points: false,
            filters: Filters::default(),
            threads: 0,
            free_and_unknown: true,
        }
    }
}

/// A scanned root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootInfo {
    pub node: NodeId,
    pub path: PathBuf,
    /// The drive's size and free space when the root is a drive (and free and unknown
    /// items are on).
    pub space: Option<Space>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

/// About the scan as a whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanInfo {
    /// When it started (unix seconds).
    pub started: i64,
    /// How long it ran (pauses left out).
    pub elapsed: Duration,
    /// False when it was cancelled (some folders are flagged [`flag::INCOMPLETE`]).
    pub complete: bool,
    /// Folders that could not be listed.
    pub unreadable: u64,
    /// Items the filters left out.
    pub excluded: u64,
}

/// A finished (or cancelled) scan.
#[derive(Clone, Debug)]
pub struct Tree {
    pub(crate) nodes: Vec<Node>,
    pub(crate) exts: Vec<Box<str>>,
    pub(crate) links: Vec<u128>,
    pub(crate) roots: Vec<RootInfo>,
    pub options: ScanOptions,
    pub info: ScanInfo,
}

/// The top item's id.
pub const TOP: NodeId = 0;

impl Tree {
    /// The item `id` (panics when out of range).
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id as usize)
    }

    /// Arena slots, Dead ones included (ids are below this).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Items that are not Dead.
    pub fn live(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.kind != NodeKind::Dead)
            .count()
    }

    pub fn nodes(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        (self.nodes.iter().enumerate())
            .filter(|(_, n)| n.kind != NodeKind::Dead)
            .map(|(i, n)| (i as NodeId, n))
    }

    /// The live children of `id`, in scan order.
    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let (first, count) = match self.get(id) {
            Some(n) if n.first_child != NONE => (n.first_child, n.child_count),
            _ => (0, 0),
        };
        (first..first + count).filter(|&c| self.nodes[c as usize].kind != NodeKind::Dead)
    }

    /// The live children of `id`, biggest first (then by name); pseudo items last.
    pub fn sorted_children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = self.children(id).collect();
        out.sort_by(|&a, &b| {
            let (a, b) = (self.node(a), self.node(b));
            (a.is_pseudo().cmp(&b.is_pseudo()))
                .then(b.size.cmp(&a.size))
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// The scanned roots.
    pub fn roots(&self) -> &[RootInfo] {
        &self.roots
    }

    /// Lowercase extensions (index 0 is "": no extension).
    pub fn exts(&self) -> &[Box<str>] {
        &self.exts
    }

    /// A file's extension, "" for none.
    pub fn ext(&self, id: NodeId) -> &str {
        self.get(id)
            .and_then(|n| self.exts.get(n.ext as usize))
            .map_or("", |e| e)
    }

    /// A hard-linked file's identity (volume + file id), shared by all its names.
    pub fn link_id(&self, id: NodeId) -> Option<u128> {
        let link = self.get(id)?.link;
        (link > 0).then(|| self.links.get(link as usize - 1).copied())?
    }

    /// Levels below its root (a root: 0).
    pub fn depth(&self, mut id: NodeId) -> usize {
        let mut d = 0;
        while let Some(n) = self.get(id) {
            if matches!(n.kind, NodeKind::Root | NodeKind::Top) {
                break;
            }
            d += 1;
            id = n.parent;
        }
        d
    }

    /// The root an item is in.
    pub fn root_of(&self, mut id: NodeId) -> Option<&RootInfo> {
        while let Some(n) = self.get(id) {
            if n.kind == NodeKind::Root {
                return self.roots.iter().find(|r| r.node == id);
            }
            id = n.parent;
        }
        None
    }

    /// An item's path (None for the top item and pseudo items).
    pub fn path(&self, id: NodeId) -> Option<PathBuf> {
        let mut names = Vec::new();
        let mut at = id;
        loop {
            let n = self.get(at)?;
            match n.kind {
                NodeKind::Root => break,
                NodeKind::Top | NodeKind::Free | NodeKind::Unknown => return None,
                _ => names.push(&*n.name),
            }
            at = n.parent;
        }
        let root = self.roots.iter().find(|r| r.node == at)?;
        Some(names.iter().rev().fold(root.path.clone(), |p, n| p.join(n)))
    }

    /// The item at `path` (a root or below one).
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        const SEPS: [char; 2] = ['/', std::path::MAIN_SEPARATOR];
        let want = filter::key(path);
        for r in &self.roots {
            let root = filter::key(&r.path);
            let rest = match want.strip_prefix(&root) {
                Some("") => "",
                Some(rest) if root.ends_with(SEPS) => rest,
                // "C:\data" is not a prefix of "C:\database".
                Some(rest) => match rest.strip_prefix(SEPS) {
                    Some(rest) => rest,
                    None => continue,
                },
                None => continue,
            };
            let mut at = r.node;
            for part in rest.split(SEPS).filter(|p| !p.is_empty()) {
                at = self.children(at).find(|&c| {
                    let n = self.node(c);
                    !n.is_pseudo() && filter::name_key(&n.name) == part
                })?;
            }
            return Some(at);
        }
        None
    }

    /// Scans the same roots again with the same options.
    pub fn rescan(&self, handle: &ScanHandle) -> anyhow::Result<Tree> {
        let roots: Vec<Root> = self
            .roots
            .iter()
            .map(|r| Root::Path(r.path.clone()))
            .collect();
        scan(&roots, &self.options, handle)
    }

    /// Scans `id` again: a folder's subtree, or one file or link; an item that is gone is
    /// removed; the top item rescans everything. Totals above it are updated.
    pub fn refresh(&mut self, id: NodeId, handle: &ScanHandle) -> anyhow::Result<()> {
        walk::refresh(self, id, handle)
    }

    /// Drops Dead items; ids change. Returns the old id → new id map (NONE for dropped).
    pub fn compact(&mut self) -> Vec<NodeId> {
        let (order, remap) = self.live_order();
        let mut nodes = Vec::with_capacity(order.len());
        for &old in &order {
            let mut n = self.nodes[old as usize].clone();
            n.parent = if n.parent == NONE {
                NONE
            } else {
                remap[n.parent as usize]
            };
            n.first_child = NONE;
            n.child_count = 0;
            nodes.push(n);
        }
        for i in 1..nodes.len() {
            let p = nodes[i].parent as usize;
            if nodes[p].child_count == 0 {
                nodes[p].first_child = i as NodeId;
            }
            nodes[p].child_count += 1;
        }
        for r in &mut self.roots {
            r.node = remap[r.node as usize];
        }
        self.nodes = nodes;
        remap
    }

    /// Live ids in an order where parents come first and each folder's children are
    /// contiguous, and the old → new id map.
    pub(crate) fn live_order(&self) -> (Vec<NodeId>, Vec<NodeId>) {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut remap = vec![NONE; self.nodes.len()];
        if self.nodes.is_empty() {
            return (order, remap);
        }
        // Breadth first from the top: children of one folder land next to each other.
        order.push(TOP);
        remap[0] = 0;
        let mut i = 0;
        while i < order.len() {
            let id = order[i];
            for c in self.children(id) {
                remap[c as usize] = order.len() as NodeId;
                order.push(c);
            }
            i += 1;
        }
        (order, remap)
    }

    /// Sums every folder's subtree from its children (files, links), deepest first.
    pub(crate) fn recompute(&mut self) {
        for n in &mut self.nodes {
            n.newest = n.modified;
            if n.is_container() {
                n.size = 0;
                n.allocated = 0;
                n.files = 0;
                n.folders = 0;
                n.items = 0;
            }
        }
        // Parents always come before their children in the arena.
        for i in (1..self.nodes.len()).rev() {
            let n = &self.nodes[i];
            if n.kind == NodeKind::Dead || n.is_pseudo() || n.parent == NONE {
                continue;
            }
            let parent = n.parent as usize;
            let dup = n.has(flag::HARDLINK_DUP);
            let (size, allocated, files, folders, items, newest) = match n.kind {
                NodeKind::File if dup => (0, 0, 1, 0, 1, n.newest),
                NodeKind::File => (n.size, n.allocated, 1, 0, 1, n.newest),
                NodeKind::Link => (0, 0, 0, 0, 1, UNKNOWN_TIME),
                _ => (
                    n.size,
                    n.allocated,
                    n.files,
                    n.folders.saturating_add(1),
                    n.items.saturating_add(1),
                    n.newest,
                ),
            };
            // Saturating: a loaded scan file may claim anything.
            let p = &mut self.nodes[parent];
            p.size = p.size.saturating_add(size);
            p.allocated = p.allocated.saturating_add(allocated);
            p.files = p.files.saturating_add(files);
            p.folders = p.folders.saturating_add(folders);
            p.items = p.items.saturating_add(items);
            p.newest = p.newest.max(newest);
        }
        self.update_pseudo();
    }

    /// Free space and unknown (used − scanned on disk) of each drive root.
    pub(crate) fn update_pseudo(&mut self) {
        for r in &self.roots {
            let Some(space) = r.space else { continue };
            let root = &self.nodes[r.node as usize];
            let scanned = root.allocated;
            let (first, count) = (root.first_child, root.child_count);
            if first == NONE {
                continue;
            }
            for c in first..first + count {
                let n = &mut self.nodes[c as usize];
                let bytes = match n.kind {
                    NodeKind::Free => space.free,
                    NodeKind::Unknown => {
                        (space.total.saturating_sub(space.free)).saturating_sub(scanned)
                    }
                    _ => continue,
                };
                n.size = bytes;
                n.allocated = bytes;
            }
        }
    }

    /// The roots with their biggest `top` children, for `keel du --scan` and its JSON.
    pub fn summary(&self, top: usize) -> Summary {
        self.summary_by(top, false)
    }

    /// [`summary`](Self::summary), the children ranked by size on disk when `size_on_disk`.
    pub fn summary_by(&self, top: usize, size_on_disk: bool) -> Summary {
        let item = |id: NodeId| {
            let n = self.node(id);
            ItemSummary {
                name: n.name.to_string(),
                path: self.path(id).map(|p| p.display().to_string()),
                kind: n.kind.as_str(),
                bytes: n.size,
                allocated: n.allocated,
                files: n.files,
                folders: n.folders,
                items: n.items,
                newest: secs(n.newest),
                notes: notes(n.flags),
            }
        };
        let roots = self
            .roots
            .iter()
            .map(|r| {
                let n = self.node(r.node);
                let mut kids: Vec<NodeId> = self
                    .sorted_children(r.node)
                    .into_iter()
                    .filter(|&c| !self.node(c).is_pseudo())
                    .collect();
                if size_on_disk {
                    // Stable: equal sizes on disk keep the order by size, then name.
                    kids.sort_by(|&a, &b| self.node(b).allocated.cmp(&self.node(a).allocated));
                }
                let pseudo = |k: NodeKind| {
                    self.children(r.node)
                        .find(|&c| self.node(c).kind == k)
                        .map(|c| self.node(c).size)
                };
                RootSummary {
                    path: r.path.display().to_string(),
                    bytes: n.size,
                    allocated: n.allocated,
                    files: n.files,
                    folders: n.folders,
                    items: n.items,
                    newest: secs(n.newest),
                    free: pseudo(NodeKind::Free),
                    unknown: pseudo(NodeKind::Unknown),
                    notes: notes(n.flags),
                    truncated: kids.len() > top,
                    children: kids.into_iter().take(top).map(item).collect(),
                }
            })
            .collect();
        let t = self.node(TOP);
        Summary {
            complete: self.info.complete,
            elapsed_ms: self.info.elapsed.as_millis() as u64,
            bytes: t.size,
            allocated: t.allocated,
            files: t.files,
            folders: t.folders,
            items: t.items,
            unreadable: self.info.unreadable,
            excluded: self.info.excluded,
            roots,
        }
    }
}

fn secs(ns: i64) -> Option<i64> {
    (ns != UNKNOWN_TIME).then(|| ns.div_euclid(1_000_000_000))
}

fn notes(flags: u16) -> Vec<&'static str> {
    [
        (flag::HARDLINK_DUP, "counted elsewhere (hard link)"),
        (flag::HARDLINK, "hard link"),
        (flag::UNREADABLE, "unreadable"),
        (flag::CLOUD, "cloud placeholder"),
        (flag::LOOP, "link loop"),
        (flag::TOO_DEEP, "too deep"),
        (flag::INCOMPLETE, "not scanned (cancelled)"),
        (flag::COMPRESSED, "compressed"),
        (flag::SPARSE, "sparse"),
        (flag::SYMLINK, "symlink"),
        (flag::JUNCTION, "junction"),
        (flag::MOUNT, "mount point"),
        (flag::FOLLOWED, "followed"),
    ]
    .into_iter()
    .filter(|(f, _)| flags & f != 0)
    .map(|(_, s)| s)
    .collect()
}

/// [`Tree::summary`].
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub complete: bool,
    pub elapsed_ms: u64,
    pub bytes: u64,
    pub allocated: u64,
    pub files: u64,
    pub folders: u64,
    pub items: u64,
    pub unreadable: u64,
    pub excluded: u64,
    pub roots: Vec<RootSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RootSummary {
    pub path: String,
    pub bytes: u64,
    pub allocated: u64,
    pub files: u64,
    pub folders: u64,
    pub items: u64,
    /// Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<&'static str>,
    pub children: Vec<ItemSummary>,
    /// More, smaller children were left out.
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ItemSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub kind: &'static str,
    pub bytes: u64,
    pub allocated: u64,
    pub files: u64,
    pub folders: u64,
    pub items: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<&'static str>,
}

/// Scans `roots` into one tree. Fails before walking when a root is missing or not a
/// folder, or a filter does not compile; folders that cannot be listed below a root are
/// flagged, not errors. A cancelled scan returns what it found (`info.complete` false).
pub fn scan(roots: &[Root], options: &ScanOptions, handle: &ScanHandle) -> anyhow::Result<Tree> {
    walk::scan(roots, options, handle)
}

/// The local fixed drives: `C:\`-style roots on Windows; elsewhere the user mounts that are
/// not removable media or network shares.
pub fn local_drives() -> Vec<PathBuf> {
    sys::local_drives()
}

/// Controls a running scan (pause, resume, cancel) and reads its progress; cheap to clone
/// and safe to use from other threads. Use a new one per scan: a cancelled handle stays
/// cancelled.
#[derive(Clone, Default)]
pub struct ScanHandle(Arc<Shared>);

#[derive(Default)]
struct Shared {
    cancel: AtomicBool,
    paused: AtomicBool,
    state: Mutex<Clock>,
    wake: Condvar,
    files: AtomicU64,
    folders: AtomicU64,
    bytes: AtomicU64,
    current: Mutex<PathBuf>,
    /// Milliseconds after the start of the last `current` update.
    current_at: AtomicU64,
}

#[derive(Default)]
struct Clock {
    started: Option<Instant>,
    paused_at: Option<Instant>,
    paused_for: Duration,
    finished: Option<Duration>,
}

impl Clock {
    fn elapsed(&self) -> Duration {
        if let Some(d) = self.finished {
            return d;
        }
        let Some(start) = self.started else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        let paused = self.paused_for + self.paused_at.map_or(Duration::ZERO, |p| now - p);
        (now - start).saturating_sub(paused)
    }
}

/// A snapshot of a scan's progress.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub files: u64,
    pub folders: u64,
    /// Bytes of the files found so far (hard links not yet told apart).
    pub bytes: u64,
    /// Running time, pauses left out.
    pub elapsed: Duration,
    pub files_per_sec: f64,
    pub bytes_per_sec: f64,
    /// The folder being listed (recently).
    pub current: PathBuf,
    pub paused: bool,
    pub cancelled: bool,
    pub done: bool,
}

impl ScanHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Workers stop before the next folder until [`resume`](Self::resume). May be called
    /// before the scan starts.
    pub fn pause(&self) {
        let mut st = self.0.state.lock();
        if !self.0.paused.swap(true, Ordering::SeqCst) {
            st.paused_at = Some(Instant::now());
        }
    }

    pub fn resume(&self) {
        let mut st = self.0.state.lock();
        if self.0.paused.swap(false, Ordering::SeqCst) {
            if let Some(at) = st.paused_at.take() {
                st.paused_for += at.elapsed();
            }
        }
        self.0.wake.notify_all();
    }

    /// Stops the scan after the folders being listed; it returns what it found.
    pub fn cancel(&self) {
        let _st = self.0.state.lock();
        self.0.cancel.store(true, Ordering::SeqCst);
        self.0.wake.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        self.0.paused.load(Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancel.load(Ordering::SeqCst)
    }

    pub fn progress(&self) -> Progress {
        let st = self.0.state.lock();
        let elapsed = st.elapsed();
        let done = st.finished.is_some();
        drop(st);
        let files = self.0.files.load(Ordering::Relaxed);
        let bytes = self.0.bytes.load(Ordering::Relaxed);
        let secs = elapsed.as_secs_f64().max(1e-3);
        Progress {
            files,
            folders: self.0.folders.load(Ordering::Relaxed),
            bytes,
            elapsed,
            files_per_sec: files as f64 / secs,
            bytes_per_sec: bytes as f64 / secs,
            current: self.0.current.lock().clone(),
            paused: self.is_paused(),
            cancelled: self.is_cancelled(),
            done,
        }
    }

    pub(crate) fn start(&self) {
        let mut st = self.0.state.lock();
        st.started = Some(Instant::now());
        st.finished = None;
        st.paused_for = Duration::ZERO;
        if st.paused_at.is_some() {
            st.paused_at = Some(Instant::now());
        }
        self.0.files.store(0, Ordering::Relaxed);
        self.0.folders.store(0, Ordering::Relaxed);
        self.0.bytes.store(0, Ordering::Relaxed);
        self.0.current_at.store(0, Ordering::Relaxed);
    }

    pub(crate) fn finish(&self) -> Duration {
        let mut st = self.0.state.lock();
        let d = st.elapsed();
        st.finished = Some(d);
        d
    }

    /// False when cancelled; blocks while paused.
    pub(crate) fn checkpoint(&self) -> bool {
        if self.is_cancelled() {
            return false;
        }
        if self.0.paused.load(Ordering::SeqCst) {
            let mut st = self.0.state.lock();
            while self.0.paused.load(Ordering::SeqCst) && !self.is_cancelled() {
                self.0.wake.wait(&mut st);
            }
        }
        !self.is_cancelled()
    }

    pub(crate) fn add(&self, files: u64, folders: u64, bytes: u64) {
        self.0.files.fetch_add(files, Ordering::Relaxed);
        self.0.folders.fetch_add(folders, Ordering::Relaxed);
        self.0.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Updates the current folder at most every 50 ms.
    pub(crate) fn at(&self, path: &Path) {
        let ms = {
            let Some(st) = self.0.state.try_lock() else {
                return;
            };
            st.started.map_or(0, |s| s.elapsed().as_millis() as u64)
        };
        let last = self.0.current_at.load(Ordering::Relaxed);
        if (last == 0 || ms >= last + 50)
            && self
                .0
                .current_at
                .compare_exchange(last, ms.max(1), Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            if let Some(mut c) = self.0.current.try_lock() {
                c.clear();
                c.push(path);
            }
        }
    }
}
