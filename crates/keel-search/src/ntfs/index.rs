//! The in-memory name table: one slot per file reference number, all names in one
//! `\n`-separated buffer so a query is a single regex scan over contiguous memory.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Range;

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
}

type FrnMap<V> = HashMap<u64, V, BuildHasherDefault<FrnHasher>>;

const DEAD: u64 = u64::MAX;
const MAX_DEPTH: usize = 512;
/// Name buffers smaller than this are scanned on the calling thread only.
const PARALLEL_BYTES: usize = 1 << 20;

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
pub(crate) struct Matcher {
    /// Run over the whole name buffer to find candidates (multi-line mode).
    pub scan: Option<Regex>,
    /// Further terms every candidate name must match.
    pub rest: Vec<Regex>,
    /// The scan term when it is a plain substring: drives exact/prefix ranking.
    pub literal: Option<String>,
    /// `^literal`: finds every exact and prefix match in one cheap pass.
    pub prefix: Option<Regex>,
    pub match_case: bool,
    pub folders_only: bool,
    /// `in:` path prefix.
    pub within: Option<String>,
}

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
        if let Some(&i) = self.by_frn.get(&frn) {
            let slot = self.slots[i as usize];
            let same_name = self.name_of(&slot) == name;
            if same_name && slot.parent == parent && slot.is_dir == is_dir {
                return false;
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
            self.maybe_compact();
            return true;
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
        self.by_frn.insert(frn, i);
        true
    }

    /// Drops an entry; true when it existed.
    pub fn remove(&mut self, frn: u64) -> bool {
        let Some(i) = self.by_frn.remove(&frn) else {
            return false;
        };
        let slot = &mut self.slots[i as usize];
        self.garbage += slot.len as usize + 1;
        slot.frn = DEAD;
        self.free.push(i);
        self.maybe_compact();
        true
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

    /// Every live entry as (frn, parent, name, is_dir).
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
    fn collect(
        &self,
        m: &Matcher,
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
                && m.rest.iter().all(|re| re.is_match(name))
                && !within.as_mut().is_some_and(|w| !w.contains(self, slot))
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
    pub fn search(&self, m: &Matcher, max: usize) -> Vec<Found> {
        let within = match &m.within {
            None => None,
            Some(w) => match self.within(w) {
                Some(w) => Some(w),
                None => return Vec::new(),
            },
        };
        let run = |scan: Option<&Regex>, want: usize, keep: &(dyn Fn(u8) -> bool + Sync)| {
            self.par(|segs| self.collect(m, scan, within.clone(), segs, want, keep))
        };
        let mut hits = match (&m.prefix, &m.scan) {
            (Some(prefix), Some(scan)) => {
                let mut best = run(Some(prefix), usize::MAX, &|q| q < 2);
                if best.len() < max {
                    best.extend(run(Some(scan), max - best.len(), &|q| q == 2));
                }
                best
            }
            (_, scan) => run(scan.as_ref(), max, &|_| true),
        };
        hits.sort_by_key(|&(quality, _)| quality);
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
        found
    }

    /// `in:` target: the folder holding the prefix's last component, and that
    /// partial component (entries under the folder whose top name starts with it).
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
}
