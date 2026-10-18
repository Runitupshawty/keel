//! The in-memory name table: one slot per file reference number, all names in one
//! `\n`-separated buffer so a query is a single regex scan over contiguous memory.
//!
//! Each slot also has metadata columns (size, size on disk, dates, attributes, link and
//! stream counts), stored as one array per column; folders carry totals of everything
//! below them, kept current on every change; and any column can keep a fast-sort order
//! (a permutation of the slots), merged incrementally in [`Index::settle`].

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Range;
use std::sync::OnceLock;

use keel_query::{ChildCounts, DateProp, NodeId, Props, Timestamp, Tree};
use regex::Regex;

/// Hash for file reference numbers (already well spread): one multiply.
#[derive(Default)]
pub(crate) struct FrnHasher(u64);

impl Hasher for FrnHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0 ^ u64::from(b));
        }
    }

    fn write_u64(&mut self, n: u64) {
        let h = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }
}

type FrnMap<V> = HashMap<u64, V, BuildHasherDefault<FrnHasher>>;
type FrnSet = HashSet<u64, BuildHasherDefault<FrnHasher>>;
type SlotMap<V> = HashMap<u32, V, BuildHasherDefault<FrnHasher>>;

const DEAD: u64 = u64::MAX;
const MAX_DEPTH: usize = 512;
/// Name buffers smaller than this are scanned on the calling thread only.
const PARALLEL_BYTES: usize = 1 << 20;
/// Indexes smaller than this build their fast-sort orders on the calling thread only.
const PARALLEL_SORT: usize = 1 << 16;

/// One entry's metadata. Dates are FILETIME (100 ns since 1601), 0 when unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Info {
    /// File size; a folder's is the total of the files below it.
    pub size: u64,
    /// Size on disk; a folder's is the total below it.
    pub allocated: u64,
    pub created: i64,
    pub modified: i64,
    pub accessed: i64,
    /// Recently changed: the last change Keel saw (a journal record for the entry, or
    /// an entry added to, removed from or renamed in the folder).
    pub changed: i64,
    /// Win32 attributes, the directory bit included.
    pub attributes: u32,
    /// Hard links (names in folders).
    pub links: u16,
    /// Alternate data streams.
    pub streams: u16,
    /// Reparse tag, 0 when none.
    pub reparse: u32,
}

/// What is below a folder.
// Sorting and folder totals are read by the query layer (Phase 15 Task 71).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Folder {
    pub size: u64,
    pub allocated: u64,
    pub files: u32,
    pub folders: u32,
    /// The newest change in the folder or below it: the latest modified or recently
    /// changed date of the folder and of everything below.
    pub newest: i64,
}

/// A column the index can sort by.
// Sorting and folder totals are read by the query layer (Phase 15 Task 71).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Column {
    Name,
    Extension,
    Size,
    Allocated,
    Created,
    Modified,
    Accessed,
    Changed,
    Attributes,
}

/// The metadata columns: element `i` belongs to slot `i` (a sort or filter over one
/// column touches only that column's memory).
#[derive(Default)]
struct Columns {
    size: Vec<u64>,
    allocated: Vec<u64>,
    created: Vec<i64>,
    modified: Vec<i64>,
    accessed: Vec<i64>,
    changed: Vec<i64>,
    attributes: Vec<u32>,
    links: Vec<u16>,
    streams: Vec<u16>,
    reparse: Vec<u32>,
}

impl Columns {
    fn get(&self, i: usize) -> Info {
        Info {
            size: self.size[i],
            allocated: self.allocated[i],
            created: self.created[i],
            modified: self.modified[i],
            accessed: self.accessed[i],
            changed: self.changed[i],
            attributes: self.attributes[i],
            links: self.links[i],
            streams: self.streams[i],
            reparse: self.reparse[i],
        }
    }

    /// Sets slot `i`'s row; `i` may be one past the end (a new slot).
    fn set(&mut self, i: usize, v: Info) {
        if i == self.size.len() {
            self.size.push(v.size);
            self.allocated.push(v.allocated);
            self.created.push(v.created);
            self.modified.push(v.modified);
            self.accessed.push(v.accessed);
            self.changed.push(v.changed);
            self.attributes.push(v.attributes);
            self.links.push(v.links);
            self.streams.push(v.streams);
            self.reparse.push(v.reparse);
        } else {
            self.size[i] = v.size;
            self.allocated[i] = v.allocated;
            self.created[i] = v.created;
            self.modified[i] = v.modified;
            self.accessed[i] = v.accessed;
            self.changed[i] = v.changed;
            self.attributes[i] = v.attributes;
            self.links[i] = v.links;
            self.streams[i] = v.streams;
            self.reparse[i] = v.reparse;
        }
    }

    /// When an entry last changed, for folder totals.
    fn touched(&self, i: usize) -> i64 {
        self.modified[i].max(self.changed[i])
    }
}

/// Counts below a folder (its size totals live in the size columns).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Agg {
    files: u32,
    folders: u32,
    /// Newest change strictly below.
    newest: i64,
}

/// What one entry adds to every folder above it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Contrib {
    size: u64,
    allocated: u64,
    files: u32,
    folders: u32,
    newest: i64,
}

impl Contrib {
    /// What turning `old` into `self` adds above. Sums wrap, so adding and taking away
    /// the same amount always cancels; the newest date only grows.
    fn since(self, old: Contrib) -> Contrib {
        Contrib {
            size: self.size.wrapping_sub(old.size),
            allocated: self.allocated.wrapping_sub(old.allocated),
            files: self.files.wrapping_sub(old.files),
            folders: self.folders.wrapping_sub(old.folders),
            newest: self.newest,
        }
    }

    /// Taking `self` away.
    fn negated(self) -> Contrib {
        Contrib {
            newest: i64::MIN,
            ..Contrib::default().since(self)
        }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    frn: u64,
    parent: u64,
    /// Byte offset of the name in `Index::names`.
    at: u32,
    len: u32,
    is_dir: bool,
}

/// Every name on one volume (or the user-folder walk), keyed by file reference
/// number, with parent links to rebuild paths.
pub(crate) struct Index {
    /// What paths start with: "C:" for a volume, "" for the walk index (whose
    /// top-level entries are drives named "C:").
    pub prefix: String,
    pub root: u64,
    slots: Vec<Slot>,
    free: Vec<u32>,
    by_frn: FrnMap<u32>,
    /// Names, each followed by `\n`; renamed or deleted names stay as garbage
    /// until `compact`.
    names: String,
    /// (offset of a name in `names`, slot), ascending by offset.
    owners: Vec<(u32, u32)>,
    garbage: usize,
    /// Metadata, one row per slot.
    columns: Columns,
    /// Counts below each folder, by slot.
    folders: SlotMap<Agg>,
    /// Frns that entries name as their parent but that are not in the index (yet, or
    /// any more): when one is added, the totals of what is below it must be recounted.
    missing: FrnSet,
    /// Folder totals need a full recount at the next [`Index::settle`] (a bulk load, a
    /// parent cycle, or a newest date that only a recount can lower).
    stale: bool,
    /// Fast-sort orders: each column's live slots in sort order, current as of the
    /// last `settle`.
    orders: Vec<(Column, Vec<u32>)>,
    /// Slots changed since the last `settle` (kept only while there are orders).
    dirty: Vec<u32>,
    /// Every order must be rebuilt at the next `settle`.
    reorder: bool,
    /// Folder slots above the entry being changed (a reused buffer).
    above: Vec<u32>,
}

/// A query result from one index: rank key plus the full path.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub quality: u8,
    pub path: String,
    pub is_dir: bool,
}

/// Case-insensitive (ASCII) ordering without allocating.
pub(crate) fn cmp_ci(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// Result order: match quality, then path.
pub(crate) fn rank(a: &Found, b: &Found) -> Ordering {
    a.quality
        .cmp(&b.quality)
        .then_with(|| cmp_ci(&a.path, &b.path))
}

/// A compiled query (see `pattern::compile`).
pub(crate) use crate::walkindex::Matcher;

impl Index {
    pub fn new(prefix: impl Into<String>, root: u64) -> Self {
        Self {
            prefix: prefix.into(),
            root,
            slots: Vec::new(),
            free: Vec::new(),
            by_frn: FrnMap::default(),
            names: String::new(),
            owners: Vec::new(),
            garbage: 0,
            columns: Columns::default(),
            folders: SlotMap::default(),
            missing: FrnSet::default(),
            stale: false,
            orders: Vec::new(),
            dirty: Vec::new(),
            reorder: false,
            above: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.by_frn.len()
    }

    fn name_of(&self, slot: &Slot) -> &str {
        &self.names[slot.at as usize..(slot.at + slot.len) as usize]
    }

    fn push_name(&mut self, slot: u32, name: &str) -> (u32, u32) {
        let at = self.names.len() as u32;
        // NTFS POSIX-namespace names may hold '\n', which would split the record.
        if name.contains('\n') {
            self.names.push_str(&name.replace('\n', " "));
        } else {
            self.names.push_str(name);
        }
        let len = self.names.len() as u32 - at;
        self.names.push('\n');
        self.owners.push((at, slot));
        (at, len)
    }

    /// Adds or updates an entry; true when anything changed.
    pub fn upsert(&mut self, frn: u64, parent: u64, name: &str, is_dir: bool) -> bool {
        self.upsert_at(frn, parent, name, is_dir, 0)
    }

    /// Adds, renames or moves an entry after a change at `when` (FILETIME, 0 when
    /// unknown), keeping folder totals current; true when anything changed. The
    /// recently-changed dates of the entry and of the folders it left and entered move
    /// up to `when`.
    pub fn upsert_at(
        &mut self,
        frn: u64,
        parent: u64,
        name: &str,
        is_dir: bool,
        when: i64,
    ) -> bool {
        let Some(&i) = self.by_frn.get(&frn) else {
            self.insert(frn, parent, name, is_dir, when);
            return true;
        };
        let slot = self.slots[i as usize];
        let same_name = self.name_of(&slot) == name;
        if same_name && slot.parent == parent && slot.is_dir == is_dir {
            return false;
        }
        let old_up = self.up(i);
        let moved = slot.parent != parent || slot.is_dir != is_dir;
        if moved {
            let before = self.contrib(i);
            // The folder it leaves keeps its newest date: exact only when `when` is newer.
            if before.newest > when {
                self.stale = true;
            }
            self.add_up(old_up, before.negated());
        }
        if slot.is_dir != is_dir {
            // A file that became a folder, or back: recount.
            self.stale = true;
            if is_dir {
                self.folders.insert(i, Agg::default());
            } else {
                self.folders.remove(&i);
            }
        }
        let (at, len) = if same_name {
            (slot.at, slot.len)
        } else {
            self.garbage += slot.len as usize + 1;
            self.push_name(i, name)
        };
        self.slots[i as usize] = Slot {
            frn,
            parent,
            at,
            len,
            is_dir,
        };
        let new_up = self.up(i);
        if moved {
            self.note_parent(new_up);
            let after = self.contrib(i);
            self.add_up(new_up, after);
            self.bump(new_up, when);
        }
        self.bump(old_up, when);
        self.bump_slot(i, when);
        self.touch(i);
        self.maybe_compact();
        true
    }

    fn insert(&mut self, frn: u64, parent: u64, name: &str, is_dir: bool, when: i64) {
        if self.missing.contains(&frn) {
            // Entries below it came first: their totals were never added here.
            self.stale = true;
        }
        let i = self.free.pop().unwrap_or(self.slots.len() as u32);
        let (at, len) = self.push_name(i, name);
        let slot = Slot {
            frn,
            parent,
            at,
            len,
            is_dir,
        };
        match self.slots.get_mut(i as usize) {
            Some(s) => *s = slot,
            None => self.slots.push(slot),
        }
        self.columns.set(i as usize, Info::default());
        if is_dir {
            self.folders.insert(i, Agg::default());
        }
        self.by_frn.insert(frn, i);
        let up = self.up(i);
        self.note_parent(up);
        let added = self.contrib(i);
        self.add_up(up, added);
        self.bump(up, when);
        self.bump_slot(i, when);
        self.touch(i);
    }

    /// Drops an entry; true when it existed.
    pub fn remove(&mut self, frn: u64) -> bool {
        self.remove_at(frn, 0)
    }

    /// Drops an entry removed at `when` (0 when unknown); true when it existed. Its
    /// folder's recently-changed date moves up to `when`.
    pub fn remove_at(&mut self, frn: u64, when: i64) -> bool {
        let Some(&i) = self.by_frn.get(&frn) else {
            return false;
        };
        let up = self.up(i);
        let gone = self.contrib(i);
        if gone.newest > when {
            self.stale = true;
        }
        self.add_up(up, gone.negated());
        let slot = self.slots[i as usize];
        // Entries still below a removed folder now hang off a missing parent.
        if slot.is_dir && (self.stale || gone.files != 0 || gone.folders != 1) {
            self.missing.insert(frn);
        }
        self.by_frn.remove(&frn);
        self.folders.remove(&i);
        self.garbage += slot.len as usize + 1;
        self.slots[i as usize].frn = DEAD;
        self.free.push(i);
        self.bump(up, when);
        self.touch(i);
        self.maybe_compact();
        true
    }

    /// Sets an entry's metadata (a folder's size and size on disk stay the totals
    /// below it, and the recently-changed date never goes back); true when anything
    /// changed.
    pub fn set_info(&mut self, frn: u64, mut info: Info) -> bool {
        let Some(&i) = self.by_frn.get(&frn) else {
            return false;
        };
        info.changed = info.changed.max(self.columns.changed[i as usize]);
        self.write_info(i, info)
    }

    /// A journal record for `frn` at `when` carrying the file's current `attributes`:
    /// its recently-changed date moves up to `when`. True when anything changed.
    pub fn note(&mut self, frn: u64, when: i64, attributes: u32) -> bool {
        let Some(&i) = self.by_frn.get(&frn) else {
            return false;
        };
        let mut info = self.columns.get(i as usize);
        info.changed = info.changed.max(when);
        info.attributes = attributes;
        self.write_info(i, info)
    }

    pub fn info(&self, frn: u64) -> Option<Info> {
        Some(self.columns.get(*self.by_frn.get(&frn)? as usize))
    }

    /// What is below a folder; None for a file or an unknown frn. Current as of the
    /// last [`Index::settle`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn folder(&self, frn: u64) -> Option<Folder> {
        let i = *self.by_frn.get(&frn)?;
        if !self.slots[i as usize].is_dir {
            return None;
        }
        let c = self.contrib(i);
        Some(Folder {
            size: c.size,
            allocated: c.allocated,
            files: c.files,
            folders: c.folders.wrapping_sub(1),
            newest: c.newest,
        })
    }

    /// Before adding many entries at once: per-entry total updates are skipped until
    /// the next [`Index::settle`] counts everything once.
    pub fn begin_bulk(&mut self) {
        self.stale = true;
    }

    /// The folder an entry's totals go to: none for the root or an entry that is its
    /// own parent.
    fn up(&self, i: u32) -> Option<u64> {
        let s = &self.slots[i as usize];
        (s.frn != self.root && s.parent != s.frn).then_some(s.parent)
    }

    fn note_parent(&mut self, up: Option<u64>) {
        if let Some(parent) = up {
            if !self.by_frn.contains_key(&parent) {
                self.missing.insert(parent);
            }
        }
    }

    /// What slot `i` adds to every folder above it.
    fn contrib(&self, i: u32) -> Contrib {
        let at = i as usize;
        let c = &self.columns;
        let touched = c.touched(at);
        if self.slots[at].is_dir {
            let agg = self.folders.get(&i).copied().unwrap_or_default();
            Contrib {
                size: c.size[at],
                allocated: c.allocated[at],
                files: agg.files,
                folders: agg.folders.wrapping_add(1),
                newest: agg.newest.max(touched),
            }
        } else {
            Contrib {
                size: c.size[at],
                allocated: c.allocated[at],
                files: 1,
                folders: 0,
                newest: touched,
            }
        }
    }

    /// Fills `above` with the folders above an entry whose parent is `parent`, nearest
    /// first, up to the root; false when the chain runs deeper than any real tree (a
    /// parent cycle).
    fn climb(&mut self, mut parent: u64) -> bool {
        self.above.clear();
        while let Some(&i) = self.by_frn.get(&parent) {
            let slot = self.slots[i as usize];
            if !slot.is_dir {
                break;
            }
            if self.above.len() > MAX_DEPTH {
                return false;
            }
            self.above.push(i);
            if slot.frn == self.root || slot.parent == slot.frn {
                break;
            }
            parent = slot.parent;
        }
        true
    }

    /// Adds `delta` to every folder above an entry whose parent is `up`. Skipped while
    /// a roll-up is due anyway.
    fn add_up(&mut self, up: Option<u64>, delta: Contrib) {
        let Some(parent) = up else {
            return;
        };
        if self.stale {
            return;
        }
        if !self.climb(parent) {
            self.stale = true;
            return;
        }
        let sized = delta.size != 0 || delta.allocated != 0;
        let above = std::mem::take(&mut self.above);
        for &i in &above {
            let at = i as usize;
            self.columns.size[at] = self.columns.size[at].wrapping_add(delta.size);
            self.columns.allocated[at] = self.columns.allocated[at].wrapping_add(delta.allocated);
            let agg = self.folders.entry(i).or_default();
            agg.files = agg.files.wrapping_add(delta.files);
            agg.folders = agg.folders.wrapping_add(delta.folders);
            agg.newest = agg.newest.max(delta.newest);
            if sized {
                self.touch(i);
            }
        }
        self.above = above;
    }

    /// Something in the folder `up` was added, removed or renamed at `when`.
    fn bump(&mut self, up: Option<u64>, when: i64) {
        if let Some(&i) = up.and_then(|parent| self.by_frn.get(&parent)) {
            self.bump_slot(i, when);
        }
    }

    /// Raises slot `i`'s recently-changed date to `when`.
    fn bump_slot(&mut self, i: u32, when: i64) {
        let mut info = self.columns.get(i as usize);
        if when > info.changed {
            info.changed = when;
            self.write_info(i, info);
        }
    }

    /// Replaces slot `i`'s row, keeping folder totals current; true when anything
    /// changed.
    fn write_info(&mut self, i: u32, mut info: Info) -> bool {
        let at = i as usize;
        let old = self.columns.get(at);
        if self.slots[at].is_dir {
            info.size = old.size;
            info.allocated = old.allocated;
        }
        if info == old {
            return false;
        }
        let before = self.contrib(i);
        self.columns.set(at, info);
        let after = self.contrib(i);
        // A newest date that went down: only a recount can lower the folders' dates.
        if after.newest < before.newest {
            self.stale = true;
        }
        let up = self.up(i);
        self.add_up(up, after.since(before));
        self.touch(i);
        true
    }

    fn touch(&mut self, i: u32) {
        if !self.orders.is_empty() {
            self.dirty.push(i);
        }
    }

    /// Keeps fast-sort orders for `columns` (none: no orders), built at the next
    /// [`Index::settle`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_fast_sort(&mut self, columns: &[Column]) {
        self.orders.clear();
        for &column in columns {
            if !self.orders.iter().any(|(c, _)| *c == column) {
                self.orders.push((column, Vec::new()));
            }
        }
        self.dirty.clear();
        self.reorder = true;
    }

    /// The live slots in `column` order, when fast sort is on for it and nothing
    /// changed since the last [`Index::settle`].
    #[cfg_attr(not(test), allow(dead_code))]
    fn order(&self, column: Column) -> Option<&[u32]> {
        if self.stale || self.reorder || !self.dirty.is_empty() {
            return None;
        }
        self.orders
            .iter()
            .find(|(c, _)| *c == column)
            .map(|(_, order)| order.as_slice())
    }

    /// Brings folder totals and fast-sort orders up to date after a batch of changes:
    /// a full recount when one is due; each order rebuilt when many entries changed,
    /// else the changed entries taken out and merged back in (O(entries + changes x
    /// log changes) per order).
    pub fn settle(&mut self) {
        if self.stale {
            self.rollup();
        }
        if self.orders.is_empty() {
            self.dirty.clear();
            self.reorder = false;
            return;
        }
        if self.reorder || self.dirty.len() * 8 > self.slots.len() {
            self.rebuild_orders();
        } else if !self.dirty.is_empty() {
            self.merge_orders();
        }
        self.dirty.clear();
        self.reorder = false;
    }

    fn rebuild_orders(&mut self) {
        let live: Vec<u32> = (0..self.slots.len() as u32)
            .filter(|&i| self.slots[i as usize].frn != DEAD)
            .collect();
        let mut orders = std::mem::take(&mut self.orders);
        let this = &*self;
        let sort = |column: Column, order: &mut Vec<u32>| {
            order.clone_from(&live);
            order.sort_unstable_by(|&a, &b| this.cmp_slots(column, a, b));
        };
        if live.len() < PARALLEL_SORT {
            for (column, order) in &mut orders {
                sort(*column, order);
            }
        } else {
            let sort = &sort;
            std::thread::scope(|s| {
                for (column, order) in &mut orders {
                    s.spawn(move || sort(*column, order));
                }
            });
        }
        self.orders = orders;
    }

    fn merge_orders(&mut self) {
        let mut changed = vec![0u64; self.slots.len().div_ceil(64)];
        for &i in &self.dirty {
            changed[i as usize / 64] |= 1u64 << (i % 64);
        }
        let mut fresh: Vec<u32> = self
            .dirty
            .iter()
            .copied()
            .filter(|&i| self.slots[i as usize].frn != DEAD)
            .collect();
        fresh.sort_unstable();
        fresh.dedup();
        let mut orders = std::mem::take(&mut self.orders);
        for (column, order) in &mut orders {
            order.retain(|&i| changed[i as usize / 64] & (1u64 << (i % 64)) == 0);
            let mut add = fresh.clone();
            add.sort_unstable_by(|&a, &b| self.cmp_slots(*column, a, b));
            *order = merge(order, &add, |a, b| self.cmp_slots(*column, a, b));
        }
        self.orders = orders;
    }

    /// Recounts every folder's totals: entries deepest first, each added to its folder
    /// (O(entries)). An entry in a parent cycle, or deeper than any real tree, counts
    /// nowhere.
    fn rollup(&mut self) {
        const NONE: u32 = u32::MAX;
        const UNKNOWN: u32 = u32::MAX;
        const BAD: u32 = u32::MAX - 1;
        const VISITING: u32 = u32::MAX - 2;
        let deepest = MAX_DEPTH + 1;
        let n = self.slots.len();
        // The folder each entry adds to.
        let par: Vec<u32> = (0..n as u32)
            .map(|i| {
                if self.slots[i as usize].frn == DEAD {
                    return NONE;
                }
                match self.up(i).and_then(|p| self.by_frn.get(&p)) {
                    Some(&p) if self.slots[p as usize].is_dir => p,
                    _ => NONE,
                }
            })
            .collect();
        // Depth below the top: walk up to a known depth (or the top), then number the
        // walked entries back down.
        let mut depth = vec![UNKNOWN; n];
        let mut stack: Vec<u32> = Vec::new();
        for start in 0..n as u32 {
            if self.slots[start as usize].frn == DEAD || depth[start as usize] != UNKNOWN {
                continue;
            }
            stack.clear();
            let mut cur = start;
            let above = loop {
                match depth[cur as usize] {
                    UNKNOWN => {}
                    VISITING => break Some(BAD), // a cycle
                    known => break Some(known),
                }
                depth[cur as usize] = VISITING;
                stack.push(cur);
                match par[cur as usize] {
                    NONE => break None,
                    p => cur = p,
                }
            };
            let mut next = match above {
                None => 0,
                Some(BAD) => BAD,
                Some(d) => d + 1,
            };
            for &s in stack.iter().rev() {
                if next != BAD && next as usize > deepest {
                    next = BAD;
                }
                depth[s as usize] = next;
                if next != BAD {
                    next += 1;
                }
            }
        }
        // Folders start empty.
        self.folders.clear();
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.frn != DEAD && slot.is_dir {
                self.columns.size[i] = 0;
                self.columns.allocated[i] = 0;
                self.folders.insert(i as u32, Agg::default());
            }
        }
        // Counting sort by depth, deepest first, so a folder is complete before it is
        // added to its own folder.
        let counted = |i: usize| par[i] != NONE && depth[i] as usize <= deepest;
        let mut starts = vec![0usize; deepest + 1];
        for i in (0..n).filter(|&i| counted(i)) {
            starts[depth[i] as usize] += 1;
        }
        let mut total = 0;
        for start in starts.iter_mut().rev() {
            let count = *start;
            *start = total;
            total += count;
        }
        let mut order = vec![0u32; total];
        for i in (0..n).filter(|&i| counted(i)) {
            let d = depth[i] as usize;
            order[starts[d]] = i as u32;
            starts[d] += 1;
        }
        for &i in &order {
            let c = self.contrib(i);
            let p = par[i as usize];
            let at = p as usize;
            self.columns.size[at] = self.columns.size[at].wrapping_add(c.size);
            self.columns.allocated[at] = self.columns.allocated[at].wrapping_add(c.allocated);
            let agg = self.folders.entry(p).or_default();
            agg.files = agg.files.wrapping_add(c.files);
            agg.folders = agg.folders.wrapping_add(c.folders);
            agg.newest = agg.newest.max(c.newest);
        }
        self.missing.clear();
        for i in 0..n as u32 {
            if self.slots[i as usize].frn == DEAD {
                continue;
            }
            if let Some(parent) = self.up(i) {
                if !self.by_frn.contains_key(&parent) {
                    self.missing.insert(parent);
                }
            }
        }
        self.stale = false;
        self.reorder = true;
    }

    /// Sort order of two slots by `column`, then name (case-insensitive), then frn: a
    /// total order, so an incrementally merged order equals a fresh sort.
    fn cmp_slots(&self, column: Column, a: u32, b: u32) -> Ordering {
        let (x, y) = (a as usize, b as usize);
        let c = &self.columns;
        let name = |i: usize| self.name_of(&self.slots[i]);
        let primary = match column {
            Column::Name => Ordering::Equal,
            Column::Extension => cmp_ci(extension(name(x)), extension(name(y))),
            Column::Size => c.size[x].cmp(&c.size[y]),
            Column::Allocated => c.allocated[x].cmp(&c.allocated[y]),
            Column::Created => c.created[x].cmp(&c.created[y]),
            Column::Modified => c.modified[x].cmp(&c.modified[y]),
            Column::Accessed => c.accessed[x].cmp(&c.accessed[y]),
            Column::Changed => c.changed[x].cmp(&c.changed[y]),
            Column::Attributes => c.attributes[x].cmp(&c.attributes[y]),
        };
        primary
            .then_with(|| cmp_ci(name(x), name(y)))
            .then_with(|| self.slots[x].frn.cmp(&self.slots[y].frn))
    }

    /// Orders `frns` by `column` (ties by name), ascending or descending; frns not in
    /// the index are dropped and duplicates kept once. With a fast-sort order for the
    /// column and a large set this is one pass over the precomputed order, not a sort.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn sort_frns(&self, column: Column, descending: bool, frns: &mut Vec<u64>) {
        let mut slots: Vec<u32> = frns
            .iter()
            .filter_map(|frn| self.by_frn.get(frn).copied())
            .collect();
        match self.order(column) {
            Some(order) if slots.len() * 16 > order.len() => {
                let mut wanted = vec![0u64; self.slots.len().div_ceil(64)];
                for &i in &slots {
                    wanted[i as usize / 64] |= 1u64 << (i % 64);
                }
                slots = order
                    .iter()
                    .copied()
                    .filter(|&i| wanted[i as usize / 64] & (1u64 << (i % 64)) != 0)
                    .collect();
            }
            _ => {
                slots.sort_unstable_by(|&a, &b| self.cmp_slots(column, a, b));
                slots.dedup();
            }
        }
        if descending {
            slots.reverse();
        }
        *frns = slots
            .into_iter()
            .map(|i| self.slots[i as usize].frn)
            .collect();
    }

    /// Rewrites `names` without garbage once more than half of it is garbage.
    fn maybe_compact(&mut self) {
        if self.garbage < 1 << 20 || self.garbage * 2 < self.names.len() {
            return;
        }
        let old = std::mem::take(&mut self.names);
        self.owners.clear();
        self.garbage = 0;
        for i in 0..self.slots.len() {
            let slot = self.slots[i];
            if slot.frn != DEAD {
                let name = &old[slot.at as usize..(slot.at + slot.len) as usize];
                let (at, _) = self.push_name(i as u32, name);
                self.slots[i].at = at;
            }
        }
    }

    /// Every live entry with its metadata.
    pub fn rows(&self) -> impl Iterator<Item = (u64, u64, &str, bool, Info)> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.frn != DEAD)
            .map(|(i, s)| {
                (
                    s.frn,
                    s.parent,
                    self.name_of(s),
                    s.is_dir,
                    self.columns.get(i),
                )
            })
    }

    /// Every live entry as (frn, parent, name, is_dir).
    #[cfg(test)]
    pub fn entries(&self) -> impl Iterator<Item = (u64, u64, &str, bool)> {
        self.slots
            .iter()
            .filter(|s| s.frn != DEAD)
            .map(|s| (s.frn, s.parent, self.name_of(s), s.is_dir))
    }

    pub fn get(&self, frn: u64) -> Option<(u64, &str, bool)> {
        let slot = &self.slots[*self.by_frn.get(&frn)? as usize];
        Some((slot.parent, self.name_of(slot), slot.is_dir))
    }

    /// Full path of an entry, None when its parent chain does not reach the root.
    pub fn path(&self, frn: u64) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = frn;
        while cur != self.root {
            let slot = &self.slots[*self.by_frn.get(&cur)? as usize];
            parts.push(self.name_of(slot));
            if parts.len() > MAX_DEPTH {
                return None;
            }
            cur = slot.parent;
        }
        let mut path = self.prefix.clone();
        for (n, part) in parts.iter().rev().enumerate() {
            if n > 0 || !path.is_empty() {
                path.push('\\');
            }
            path.push_str(part);
        }
        if path.ends_with(':') {
            path.push('\\');
        }
        Some(path)
    }

    /// The entry at `path` (case-insensitive), None when absent. `path` uses `\`.
    fn resolve(&self, path: &str) -> Option<u64> {
        let rest = if self.prefix.is_empty() {
            path
        } else {
            let head = path.get(..self.prefix.len())?;
            if !head.eq_ignore_ascii_case(&self.prefix) {
                return None;
            }
            &path[self.prefix.len()..]
        };
        let parts: Vec<&str> = rest.split('\\').filter(|p| !p.is_empty()).collect();
        let Some(last) = parts.last() else {
            return Some(self.root);
        };
        let exact = Regex::new(&format!("(?mi)^{}$", regex::escape(last))).ok()?;
        let found = self.par(|segs| {
            let mut found = Vec::new();
            self.scan(&exact, segs, |i, _| {
                let frn = self.slots[i as usize].frn;
                if self.chain_matches(frn, &parts) {
                    found.push(frn);
                    return false;
                }
                true
            });
            found
        });
        found.first().copied()
    }

    /// Whether `frn`'s ancestry spells `parts` (outermost first) up to the root.
    fn chain_matches(&self, mut frn: u64, parts: &[&str]) -> bool {
        for part in parts.iter().rev() {
            match self.get(frn) {
                Some((parent, name, _)) if name.eq_ignore_ascii_case(part) => frn = parent,
                _ => return false,
            }
        }
        frn == self.root
    }

    /// Runs `f` over all name segments: in parallel chunks for a large index, results
    /// concatenated in buffer order.
    fn par<T: Send>(&self, f: impl Fn(Range<usize>) -> Vec<T> + Sync) -> Vec<T> {
        let segs = self.owners.len();
        let threads = if self.names.len() < PARALLEL_BYTES {
            1
        } else {
            std::thread::available_parallelism().map_or(4, |n| n.get().min(8))
        };
        if threads == 1 {
            return f(0..segs);
        }
        let f = &f;
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..threads)
                .map(|t| s.spawn(move || f(segs * t / threads..segs * (t + 1) / threads)))
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
                .collect()
        })
    }

    /// Calls `each(slot, name)` for every live entry in segments `segs` whose name
    /// `re` (multi-line) matches, until it returns false.
    fn scan(&self, re: &Regex, segs: Range<usize>, mut each: impl FnMut(u32, &str) -> bool) {
        if segs.is_empty() {
            return;
        }
        let stop = self
            .owners
            .get(segs.end)
            .map_or(self.names.len(), |o| o.0 as usize);
        // The chunk ends after a '\n', so `$` and `^` see the same boundaries.
        let hay = &self.names[..stop];
        let mut pos = self.owners[segs.start].0 as usize;
        let mut seg = segs.start;
        while pos < stop {
            let Some(m) = re.find_at(hay, pos) else {
                return;
            };
            // Matches only move forward, so look only past the previous segment.
            seg +=
                self.owners[seg..segs.end].partition_point(|&(at, _)| at as usize <= m.start()) - 1;
            let (at, i) = self.owners[seg];
            let end = self
                .owners
                .get(seg + 1)
                .map_or(self.names.len(), |next| next.0 as usize)
                - 1;
            pos = end + 1;
            let slot = &self.slots[i as usize];
            if slot.frn == DEAD || slot.at != at {
                continue; // garbage left by a rename or delete
            }
            let name = &self.names[at as usize..end];
            // A match running past this name's end ('\s', '[^x]') proves nothing.
            if m.end() > end && !re.is_match(name) {
                continue;
            }
            if !each(i, name) {
                return;
            }
        }
    }

    /// Up to `want` matching slots in segments `segs` whose quality passes `keep`,
    /// in buffer order. Without `scan` every entry is a candidate.
    #[allow(clippy::too_many_arguments)]
    fn collect(
        &self,
        m: &Matcher,
        tree: &IndexTree<'_>,
        scan: Option<&Regex>,
        mut within: Option<Within>,
        segs: Range<usize>,
        want: usize,
        keep: &(dyn Fn(u8) -> bool + Sync),
    ) -> Vec<(u8, u32)> {
        let mut hits = Vec::new();
        if want == 0 {
            return hits;
        }
        let mut consider = |i: u32, name: &str| {
            let slot = &self.slots[i as usize];
            if slot.frn == self.root || (m.folders_only && !slot.is_dir) {
                return true;
            }
            let quality = quality(m, name);
            if keep(quality)
                && !within.as_mut().is_some_and(|w| !w.contains(self, slot))
                && m.matches_in(&EntryProps::new(self, i), Some(tree))
            {
                hits.push((quality, i));
            }
            hits.len() < want
        };
        match scan {
            Some(re) => self.scan(re, segs, consider),
            None => {
                for &(at, i) in &self.owners[segs] {
                    let slot = &self.slots[i as usize];
                    if slot.frn != DEAD && slot.at == at && !consider(i, self.name_of(slot)) {
                        break;
                    }
                }
            }
        }
        hits
    }

    /// The `max` best matches, ranked by [`rank`]. For a substring, names that equal
    /// or start with it come first (one pass over the `^` form finds all of those),
    /// then other matches in index order until `max`; any other query stops at the
    /// first `max` matches.
    #[cfg(test)]
    pub fn search(&self, m: &Matcher, max: usize) -> Vec<Found> {
        self.try_search(m, max).unwrap()
    }

    pub fn try_search(&self, m: &Matcher, max: usize) -> anyhow::Result<Vec<Found>> {
        let within = match &m.within {
            None => None,
            Some(w) => match self.within(w) {
                Some(w) => Some(w),
                None => return Ok(Vec::new()),
            },
        };
        let tree = IndexTree::new(self);
        let run = |scan: Option<&Regex>, want: usize, keep: &(dyn Fn(u8) -> bool + Sync)| {
            self.par(|segs| self.collect(m, &tree, scan, within.clone(), segs, want, keep))
        };
        let mut hits = if m.query.has_set_functions() {
            // Set functions (`dupe:`, `unique:`, `count:`) see every match.
            run(m.scan.as_ref(), usize::MAX, &|_| true)
        } else {
            match (&m.prefix, &m.scan) {
                (Some(prefix), Some(scan)) => {
                    let mut best = run(Some(prefix), usize::MAX, &|q| q < 2);
                    if best.len() < max {
                        best.extend(run(Some(scan), max - best.len(), &|q| q == 2));
                    }
                    best
                }
                (_, scan) => run(scan.as_ref(), max, &|_| true),
            }
        };
        hits.sort_by_key(|&(quality, _)| quality);
        if let Some(mut pass) = m.query.set_pass() {
            for &(_, slot) in &hits {
                if !pass.push(u64::from(slot), &EntryProps::new(self, slot), Some(&tree))? {
                    break;
                }
            }
            hits = (pass.finish().into_iter())
                .map(|slot| {
                    let slot = slot as u32;
                    (quality(m, self.name_of(&self.slots[slot as usize])), slot)
                })
                .collect();
            hits.sort_by_key(|&(quality, _)| quality);
        }
        hits.truncate(max);
        let mut found: Vec<Found> = hits
            .into_iter()
            .filter_map(|(quality, i)| {
                let slot = &self.slots[i as usize];
                let path = self.path(slot.frn)?;
                Some(Found {
                    quality,
                    path,
                    is_dir: slot.is_dir,
                })
            })
            .collect();
        found.sort_by(rank);
        Ok(found)
    }

    /// Whether the `in:` target `prefix` names a folder of this index.
    pub fn covers(&self, prefix: &str) -> bool {
        self.within(prefix).is_some()
    }

    fn within(&self, prefix: &str) -> Option<Within> {
        let prefix = prefix.replace('/', "\\");
        let (folder, partial) = prefix.rsplit_once('\\').unwrap_or(("", &prefix));
        // "C:" alone names the volume root; "C:\" + nothing is the same.
        let (folder, partial) = if folder.is_empty() && partial.ends_with(':') {
            (partial, "")
        } else {
            (folder, partial)
        };
        Some(Within {
            folder: self.resolve(folder)?,
            partial: partial.to_owned(),
            memo: FrnMap::default(),
            chain: Vec::new(),
        })
    }
}

#[derive(Clone)]
/// An `in:` filter that remembers the verdict for every folder it walked through.
struct Within {
    folder: u64,
    partial: String,
    memo: FrnMap<bool>,
    chain: Vec<u64>,
}

impl Within {
    /// Whether `slot` sits under the folder, below (or being) a child of it whose
    /// name starts with the partial component.
    fn contains(&mut self, index: &Index, slot: &Slot) -> bool {
        self.chain.clear();
        let mut name = index.name_of(slot);
        let mut parent = slot.parent;
        let verdict = loop {
            if parent == self.folder {
                break name
                    .get(..self.partial.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(&self.partial));
            }
            if parent == index.root || self.chain.len() > MAX_DEPTH {
                break false;
            }
            if let Some(&known) = self.memo.get(&parent) {
                break known;
            }
            // An entry gets the same verdict as its parent folder.
            self.chain.push(parent);
            let Some(&i) = index.by_frn.get(&parent) else {
                break false;
            };
            let up = &index.slots[i as usize];
            name = index.name_of(up);
            parent = up.parent;
        };
        for &dir in &self.chain {
            self.memo.insert(dir, verdict);
        }
        verdict
    }
}

/// One slot as the evaluator sees it: the metadata columns are plain reads, the path is
/// built only when a term reads it.
struct EntryProps<'a> {
    index: &'a Index,
    slot: u32,
    path: OnceCell<String>,
}

impl<'a> EntryProps<'a> {
    fn new(index: &'a Index, slot: u32) -> Self {
        Self {
            index,
            slot,
            path: OnceCell::new(),
        }
    }

    fn entry(&self) -> &Slot {
        &self.index.slots[self.slot as usize]
    }

    fn info(&self) -> Info {
        self.index.columns.get(self.slot as usize)
    }
}

impl Props for EntryProps<'_> {
    fn name(&self) -> &str {
        self.index.name_of(self.entry())
    }

    /// Empty when the parent chain does not reach the root.
    fn path(&self) -> &str {
        self.path
            .get_or_init(|| self.index.path(self.entry().frn).unwrap_or_default())
    }

    fn is_dir(&self) -> bool {
        self.entry().is_dir
    }

    /// A folder's is the total of the files below it.
    fn size(&self) -> Option<u64> {
        Some(self.info().size)
    }

    fn allocated(&self) -> Option<u64> {
        Some(self.info().allocated)
    }

    fn date(&self, which: DateProp) -> Option<Timestamp> {
        let info = self.info();
        crate::language::filetime(match which {
            DateProp::Modified => info.modified,
            DateProp::Created => info.created,
            DateProp::Accessed => info.accessed,
            DateProp::RecentlyChanged => info.changed,
            _ => return None,
        })
    }

    fn attributes(&self) -> Option<u32> {
        let dir = if self.entry().is_dir {
            keel_query::attr::DIRECTORY
        } else {
            0
        };
        Some(self.info().attributes | dir)
    }

    fn file_id(&self) -> Option<u64> {
        Some(self.entry().frn)
    }

    fn parent_file_id(&self) -> Option<u64> {
        Some(self.entry().parent)
    }

    fn hard_link_count(&self) -> Option<u64> {
        Some(u64::from(self.info().links.max(1)))
    }

    fn stream_count(&self) -> Option<u64> {
        Some(u64::from(self.info().streams))
    }

    fn child_counts(&self) -> Option<ChildCounts> {
        None
    }
}

/// The parent links of an [`Index`] for the tree functions. Folder totals answer the
/// descendant counts; the children of a folder come from a table of every entry's parent,
/// built the first time a query asks (one pass), so queries without tree functions pay
/// nothing.
struct IndexTree<'a> {
    index: &'a Index,
    kids: OnceLock<FrnMap<Vec<u32>>>,
}

impl<'a> IndexTree<'a> {
    fn new(index: &'a Index) -> Self {
        Self {
            index,
            kids: OnceLock::new(),
        }
    }

    fn slot(&self, frn: u64) -> Option<u32> {
        let slot = *self.index.by_frn.get(&frn)?;
        (self.index.slots[slot as usize].frn != DEAD).then_some(slot)
    }
}

impl Tree for IndexTree<'_> {
    fn node_of(&self, item: &dyn Props) -> Option<NodeId> {
        item.file_id()
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        let slot = &self.index.slots[self.slot(node)? as usize];
        (slot.frn != self.index.root && slot.parent != slot.frn && self.slot(slot.parent).is_some())
            .then_some(slot.parent)
    }

    fn children(&self, node: NodeId, f: &mut dyn FnMut(NodeId) -> bool) {
        let kids = self.kids.get_or_init(|| {
            let mut kids: FrnMap<Vec<u32>> = FrnMap::default();
            for (i, slot) in self.index.slots.iter().enumerate() {
                if slot.frn != DEAD && slot.frn != self.index.root && slot.parent != slot.frn {
                    kids.entry(slot.parent).or_default().push(i as u32);
                }
            }
            kids
        });
        for &kid in kids.get(&node).into_iter().flatten() {
            if !f(self.index.slots[kid as usize].frn) {
                break;
            }
        }
    }

    fn with_props(&self, node: NodeId, f: &mut dyn FnMut(&dyn Props)) {
        if let Some(slot) = self.slot(node) {
            f(&EntryProps::new(self.index, slot));
        }
    }

    fn find(&self, path: &str) -> Option<NodeId> {
        self.index.resolve(path)
    }

    fn descendant_counts(&self, node: NodeId) -> Option<ChildCounts> {
        let folder = self.index.folder(node)?;
        Some(ChildCounts {
            files: u64::from(folder.files),
            folders: u64::from(folder.folders),
        })
    }
}

/// What follows a name's last dot ("" when none).
fn extension(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// Merges two lists sorted by `cmp`.
fn merge(a: &[u32], b: &[u32], cmp: impl Fn(u32, u32) -> Ordering) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if cmp(b[j], a[i]) == Ordering::Less {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// 0 = whole name, 1 = name prefix, 2 = elsewhere (or a pattern term).
fn quality(m: &Matcher, name: &str) -> u8 {
    let Some(lit) = &m.literal else {
        return 0;
    };
    // ponytail: ASCII case folding only; a non-ASCII case difference ranks as 2.
    let eq = |a: &str, b: &str| {
        if m.match_case {
            a == b
        } else {
            a.eq_ignore_ascii_case(b)
        }
    };
    if name.len() == lit.len() && eq(name, lit) {
        0
    } else if name.get(..lit.len()).is_some_and(|head| eq(head, lit)) {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// C:\ (root 5) with Users\ann\{Doc.txt, docs\}, Windows\notepad.exe.
    pub(crate) fn sample() -> Index {
        let mut ix = Index::new("C:", 5);
        ix.upsert(5, 5, ".", true);
        ix.upsert(10, 5, "Users", true);
        ix.upsert(11, 10, "ann", true);
        ix.upsert(12, 11, "Doc.txt", false);
        ix.upsert(13, 11, "docs", true);
        ix.upsert(20, 5, "Windows", true);
        ix.upsert(21, 20, "notepad.exe", false);
        ix
    }

    #[test]
    fn paths_follow_parent_links() {
        let ix = sample();
        assert_eq!(ix.path(12).unwrap(), r"C:\Users\ann\Doc.txt");
        assert_eq!(ix.path(5).unwrap(), r"C:\");
        assert_eq!(ix.path(20).unwrap(), r"C:\Windows");
        assert_eq!(ix.len(), 7);
    }

    #[test]
    fn orphans_and_cycles_have_no_path() {
        let mut ix = sample();
        ix.upsert(30, 999, "orphan", false);
        ix.upsert(31, 32, "a", true);
        ix.upsert(32, 31, "b", true);
        assert!(ix.path(30).is_none());
        assert!(ix.path(31).is_none());
    }

    #[test]
    fn rename_move_and_delete_update_paths() {
        let mut ix = sample();
        assert!(ix.upsert(12, 20, "Moved.txt", false));
        assert!(!ix.upsert(12, 20, "Moved.txt", false));
        assert_eq!(ix.path(12).unwrap(), r"C:\Windows\Moved.txt");
        assert!(ix.remove(21));
        assert!(!ix.remove(21));
        assert!(ix.path(21).is_none());
        // A freed slot is reused without disturbing the others.
        ix.upsert(40, 5, "new", false);
        assert_eq!(ix.path(40).unwrap(), r"C:\new");
        assert_eq!(ix.path(13).unwrap(), r"C:\Users\ann\docs");
    }

    #[test]
    fn compaction_keeps_every_live_name() {
        let mut ix = Index::new("C:", 5);
        let long = "x".repeat(200);
        for i in 0..20_000u64 {
            ix.upsert(100 + i, 5, &format!("{long}{i}"), false);
        }
        for i in 0..15_000u64 {
            ix.remove(100 + i);
        }
        assert!(
            ix.names.len() <= 10_000 * 210,
            "compacted: {}",
            ix.names.len()
        );
        assert_eq!(ix.len(), 5_000);
        assert_eq!(ix.path(19_999 + 100).unwrap(), format!(r"C:\{long}19999"));
    }

    fn file_info(size: u64, modified: i64) -> Info {
        Info {
            size,
            allocated: size.div_ceil(4096) * 4096,
            modified,
            created: modified - 1,
            attributes: 0x20,
            links: 1,
            ..Info::default()
        }
    }

    /// Folder totals and fast-sort orders computed from scratch out of the index's
    /// rows, compared with what it keeps incrementally.
    fn check(ix: &Index) {
        let rows: HashMap<u64, (u64, bool, Info)> = ix
            .rows()
            .map(|(frn, parent, _, is_dir, info)| (frn, (parent, is_dir, info)))
            .collect();
        let touched = |info: &Info| info.modified.max(info.changed);
        let mut expect: HashMap<u64, Folder> = rows
            .iter()
            .filter(|(_, (_, is_dir, _))| *is_dir)
            .map(|(&frn, (_, _, info))| {
                let folder = Folder {
                    newest: touched(info).max(0),
                    ..Folder::default()
                };
                (frn, folder)
            })
            .collect();
        for (&frn, &(parent, is_dir, info)) in &rows {
            if frn == ix.root {
                continue;
            }
            let (size, allocated, files, folders) = if is_dir {
                (0, 0, 0, 1)
            } else {
                (info.size, info.allocated, 1, 0)
            };
            // The folders above, unless the chain loops (then it counts nowhere).
            let mut chain = Vec::new();
            let (mut cur, mut up) = (frn, parent);
            let counts = loop {
                if up == cur {
                    break true;
                }
                let Some(&(next, true, _)) = rows.get(&up) else {
                    break true;
                };
                if chain.contains(&up) || chain.len() > MAX_DEPTH {
                    break false;
                }
                chain.push(up);
                if up == ix.root {
                    break true;
                }
                (cur, up) = (up, next);
            };
            if !counts {
                continue;
            }
            for folder in chain {
                let f = expect.get_mut(&folder).unwrap();
                f.size = f.size.wrapping_add(size);
                f.allocated = f.allocated.wrapping_add(allocated);
                f.files += files;
                f.folders += folders;
                f.newest = f.newest.max(touched(&info));
            }
        }
        for (&frn, folder) in &expect {
            assert_eq!(ix.folder(frn).as_ref(), Some(folder), "folder {frn}");
        }
        let live: Vec<u32> = (0..ix.slots.len() as u32)
            .filter(|&i| ix.slots[i as usize].frn != DEAD)
            .collect();
        for (column, order) in &ix.orders {
            let mut sorted = live.clone();
            sorted.sort_by(|&a, &b| ix.cmp_slots(*column, a, b));
            assert_eq!(order, &sorted, "{column:?} order");
            assert!(ix.order(*column).is_some());
        }
    }

    #[test]
    fn folder_totals_follow_adds_moves_size_changes_and_deletes() {
        let mut ix = sample();
        ix.set_info(12, file_info(1000, 50));
        ix.set_info(21, file_info(200_000, 70));
        ix.upsert_at(14, 13, "a.bin", false, 80);
        ix.set_info(14, file_info(5, 60));
        assert!(!ix.stale);
        ix.settle();
        check(&ix);
        let root = ix.folder(5).unwrap();
        assert_eq!((root.files, root.folders), (3, 4));
        assert_eq!(root.size, 201_005);
        assert_eq!(root.allocated, 4096 + 200_704 + 4096);
        assert_eq!(ix.folder(11).unwrap().size, 1005);
        assert_eq!(
            ix.folder(13).unwrap().newest,
            80,
            "docs changed when a.bin came"
        );
        assert_eq!(ix.info(13).unwrap().changed, 80);
        assert!(ix.folder(12).is_none(), "a file has no totals");

        // Moving docs (with a.bin) under Windows moves its totals.
        ix.upsert_at(13, 20, "docs", true, 90);
        assert_eq!(ix.folder(11).unwrap().size, 1000);
        assert_eq!(ix.folder(20).unwrap().size, 200_005);
        assert_eq!(ix.folder(20).unwrap().folders, 1);
        assert_eq!(
            ix.folder(10).unwrap().newest,
            90,
            "ann changed when docs left"
        );
        // A size change reaches every folder above.
        ix.set_info(14, file_info(1_000_005, 95));
        assert_eq!(ix.folder(5).unwrap().size, 1_201_005);
        // Deleting a file takes it out and dates its folder.
        ix.remove_at(21, 100);
        assert_eq!(ix.folder(20).unwrap().files, 1);
        assert_eq!(ix.folder(5).unwrap().newest, 100);
        assert!(!ix.stale, "every change above was exact");
        ix.settle();
        check(&ix);
    }

    #[test]
    fn a_parent_that_arrives_late_is_recounted() {
        let mut ix = Index::new("C:", 5);
        ix.upsert(5, 5, ".", true);
        // Children first (an MFT read in record order can do this).
        ix.upsert(30, 20, "child.txt", false);
        ix.set_info(30, file_info(77, 1));
        ix.upsert(20, 5, "late", true);
        assert!(ix.stale);
        ix.settle();
        check(&ix);
        assert_eq!(ix.folder(5).unwrap().size, 77);
        // A folder removed with entries still in it, then re-added.
        ix.remove_at(20, 10);
        assert_eq!(ix.folder(5).unwrap().size, 0);
        ix.upsert(20, 5, "late", true);
        ix.settle();
        check(&ix);
        assert_eq!(ix.folder(5).unwrap().size, 77);
    }

    #[test]
    fn parent_cycles_count_nowhere_and_never_hang() {
        let mut ix = sample();
        ix.set_info(12, file_info(10, 1));
        // ann moved under its own child docs: ann -> docs -> ann.
        ix.upsert(11, 13, "ann", true);
        ix.settle();
        check(&ix);
        assert_eq!(ix.folder(10).unwrap().size, 0, "Users lost ann's subtree");
        assert_eq!(
            ix.folder(5).unwrap().files,
            1,
            "only notepad.exe is reachable"
        );
        // Changes inside the cycle stay bounded.
        ix.set_info(12, file_info(20, 2));
        ix.remove(12);
        ix.upsert(40, 11, "x", false);
        ix.settle();
        check(&ix);
        // Broken again.
        ix.upsert(11, 10, "ann", true);
        ix.settle();
        check(&ix);
        assert_eq!(ix.folder(5).unwrap().files, 2);
    }

    #[test]
    fn wrapping_and_hostile_values_stay_consistent() {
        let mut ix = sample();
        let huge = |modified| Info {
            size: u64::MAX,
            allocated: u64::MAX,
            modified,
            ..Info::default()
        };
        ix.set_info(12, huge(i64::MAX));
        ix.set_info(21, huge(i64::MIN));
        ix.settle();
        check(&ix);
        ix.remove(12);
        ix.settle();
        check(&ix);
    }

    #[test]
    fn fast_sort_orders_and_sorting_results() {
        let mut ix = sample();
        ix.set_fast_sort(&[Column::Size, Column::Name, Column::Extension, Column::Size]);
        assert_eq!(ix.orders.len(), 3);
        ix.set_info(12, file_info(300, 5));
        ix.set_info(21, file_info(100, 9));
        assert!(ix.order(Column::Size).is_none(), "not settled");
        ix.settle();
        check(&ix);
        let mut all: Vec<u64> = ix.entries().map(|(frn, ..)| frn).collect();
        ix.sort_frns(Column::Size, false, &mut all);
        // Folders sort by the size of what is below them.
        let sizes: Vec<u64> = all.iter().map(|&f| ix.info(f).unwrap().size).collect();
        assert!(sizes.windows(2).all(|w| w[0] <= w[1]), "{sizes:?}");
        assert_eq!(all.last(), Some(&5));
        // A small set takes the comparison path; same answer.
        let mut few = vec![21, 12, 999, 21];
        ix.sort_frns(Column::Size, true, &mut few);
        assert_eq!(few, [12, 21]);
        let mut names = vec![21, 12, 13];
        ix.sort_frns(Column::Name, false, &mut names);
        assert_eq!(names, [12, 13, 21], "Doc.txt, docs, notepad.exe");
        let mut by_ext = vec![21, 12, 13];
        ix.sort_frns(Column::Extension, false, &mut by_ext);
        assert_eq!(by_ext, [13, 21, 12], "'', exe, txt");
        // Turned off: nothing kept, sorting still works.
        ix.set_fast_sort(&[]);
        ix.settle();
        assert!(ix.orders.is_empty() && ix.dirty.is_empty());
        let mut few = vec![21, 12];
        ix.sort_frns(Column::Size, false, &mut few);
        assert_eq!(few, [21, 12]);
    }

    #[test]
    fn every_column_sorts_by_its_value_then_name() {
        let mut ix = sample();
        for (n, frn) in [12u64, 21, 13].into_iter().enumerate() {
            let n = n as i64;
            ix.set_info(
                frn,
                Info {
                    size: 30 - n as u64,
                    allocated: 4096 * (3 - n as u64),
                    created: 100 - n,
                    modified: 200 + n,
                    accessed: 300 - n,
                    changed: 400 + n,
                    attributes: 0x20 + n as u32,
                    ..Info::default()
                },
            );
        }
        ix.settle();
        let all = [
            Column::Name,
            Column::Extension,
            Column::Size,
            Column::Allocated,
            Column::Created,
            Column::Modified,
            Column::Accessed,
            Column::Changed,
            Column::Attributes,
        ];
        ix.set_fast_sort(&all);
        ix.settle();
        check(&ix);
        for column in all {
            let key = |frn: u64| {
                let i = ix.info(frn).unwrap();
                match column {
                    Column::Name | Column::Extension => 0,
                    Column::Size => i.size as i64,
                    Column::Allocated => i.allocated as i64,
                    Column::Created => i.created,
                    Column::Modified => i.modified,
                    Column::Accessed => i.accessed,
                    Column::Changed => i.changed,
                    Column::Attributes => i64::from(i.attributes),
                }
            };
            let mut frns: Vec<u64> = ix.entries().map(|(frn, ..)| frn).collect();
            ix.sort_frns(column, false, &mut frns);
            assert!(
                frns.windows(2).all(|w| key(w[0]) <= key(w[1])),
                "{column:?}: {frns:?}"
            );
        }
    }

    /// 10,000 random renames, moves, deletes, adds and metadata changes over a
    /// ~1,500-entry tree, settled in random batches: after every batch the folder
    /// totals equal a recount and every fast-sort order equals a fresh sort.
    #[test]
    fn ten_thousand_random_changes_keep_totals_and_orders_exact() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        const EXTS: [&str; 5] = ["txt", "rs", "PNG", "", "tar.gz"];
        let mut ix = Index::new("C:", 5);
        ix.set_fast_sort(&[
            Column::Name,
            Column::Extension,
            Column::Size,
            Column::Modified,
            Column::Changed,
        ]);
        ix.upsert(5, 5, ".", true);
        let mut live: Vec<u64> = vec![5];
        let mut next_frn = 100u64;
        let mut clock = 1_000i64;
        let mut stats = [0usize; 10];
        let mut batches = 0;
        let mut ops = 0;
        let mut pending = 0;
        let mut batch = 1 + rand(40);
        while ops < 10_000 {
            clock += 1 + rand(5) as i64;
            let pick = live[rand(live.len() as u64) as usize];
            let kind = if ops < 1_500 { 6 } else { rand(10) as usize };
            match kind {
                // Rename in place.
                0 | 1 if pick != 5 => {
                    let (parent, _, is_dir) = ix.get(pick).unwrap();
                    let ext = EXTS[rand(5) as usize];
                    let name = format!("n{}.{ext}", rand(500));
                    ix.upsert_at(pick, parent, &name, is_dir, clock);
                }
                // Move under a folder that is not below it.
                2 | 3 if pick != 5 => {
                    let target = live[rand(live.len() as u64) as usize];
                    let Some((_, name, is_dir)) = ix.get(pick) else {
                        continue;
                    };
                    let name = name.to_owned();
                    let mut up = target;
                    let mut below = false;
                    while let Some((parent, _, _)) = ix.get(up) {
                        if up == pick {
                            below = true;
                            break;
                        }
                        if up == 5 {
                            break;
                        }
                        up = parent;
                    }
                    if below || !ix.get(target).is_some_and(|t| t.2) {
                        continue;
                    }
                    ix.upsert_at(pick, target, &name, is_dir, clock);
                }
                // Delete (a folder may still hold entries: they become orphans).
                4 | 5 if pick != 5 => {
                    ix.remove_at(pick, clock);
                    live.retain(|&f| f != pick);
                }
                // Add a file or folder, now and then under a parent not added yet.
                6 => {
                    let frn = next_frn;
                    next_frn += 1;
                    let parent = if rand(50) == 0 {
                        next_frn + rand(5)
                    } else {
                        let dirs: Vec<u64> = live
                            .iter()
                            .copied()
                            .filter(|&f| ix.get(f).is_some_and(|e| e.2))
                            .collect();
                        dirs[rand(dirs.len() as u64) as usize]
                    };
                    let is_dir = rand(4) == 0;
                    let ext = EXTS[rand(5) as usize];
                    ix.upsert_at(frn, parent, &format!("f{}.{ext}", rand(300)), is_dir, clock);
                    if !is_dir {
                        ix.set_info(frn, file_info(rand(1 << 20), clock - rand(500) as i64));
                    }
                    live.push(frn);
                }
                // New size and date.
                7 => {
                    ix.set_info(pick, file_info(rand(1 << 30), clock - rand(900) as i64));
                }
                // A journal record: recently changed and attributes.
                8 => {
                    let attrs = if ix.get(pick).is_some_and(|e| e.2) {
                        0x10
                    } else {
                        0x20 | rand(2) as u32
                    };
                    ix.note(pick, clock, attrs);
                }
                _ => continue,
            }
            stats[kind] += 1;
            ops += 1;
            pending += 1;
            if pending >= batch {
                ix.settle();
                check(&ix);
                batches += 1;
                pending = 0;
                batch = 1 + rand(40);
            }
        }
        ix.settle();
        check(&ix);
        println!(
            "{} entries, {batches} batches, ops by kind {stats:?}",
            ix.len()
        );
        assert!(
            stats[0] + stats[1] > 1_000 && stats[2] + stats[3] > 200 && stats[4] + stats[5] > 1_000
        );
    }
}
