//! A persistent name index built by walking the file system: the home folder on
//! macOS and Linux (and the last resort on Windows), and every folder and non-NTFS
//! volume of Settings → Search ([`crate::folders`]). Names, parents, sizes, dates
//! (modified and created) and attributes live in memory for queries and in SQLite
//! (`<cache>/index/walk-<hash of roots>.db`) for restarts.
//!
//! A watcher keeps it current in 500 ms batches (ReadDirectoryChangesW with the
//! chosen buffer on Windows, FSEvents / inotify through `notify` elsewhere); an
//! overflowed buffer rescans. Rescans also run on a schedule (every N hours or daily
//! at a time), and a fast rescan lists only the folders whose modified time moved. A
//! root that is missing (an unplugged drive, a share that is down) is offline: its
//! last scan stays searchable, nothing is deleted, and it is rescanned when it is back.
//!
//! The home index keeps [`crate::walk`]'s skip rules (hidden and git-ignored entries are
//! left out); configured roots use the Exclude tab's [`ExcludeRules`] instead. Same
//! query syntax as the NTFS index (substring, `*`/`?` glob, `regex:`, `folder:`, `in:`, and the
//! search toggles of `terms.rs`).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use regex::{Regex, RegexBuilder};
use rusqlite::{params, Connection};

use crate::exclude::{attr, ExcludeRules};
use crate::index_settings::ScanOptions;
use crate::journal::{self, Change, Journal, Stat};
use crate::schedule::{Clock, SystemClock};
use crate::terms::{self, split_terms, strip_ci};
use crate::{Hit, Query, SearchState, Searcher};

const NONE: u32 = u32::MAX;
const NO_TIME: i64 = i64::MIN;
const BATCH: Duration = Duration::from_millis(500);
/// The home index is walked again when its db is older than this.
const MAX_AGE_SECS: i64 = 7 * 24 * 3600;
/// How often an offline root is checked for coming back.
const OFFLINE_POLL: Duration = Duration::from_secs(10);
/// Longest sleep of the worker between checks (schedule due, roots still online).
const IDLE: Duration = Duration::from_secs(60);
/// The db layout: version 2 added `created` and `attrs`.
const VERSION: &str = "2";
const MAX_DEPTH: usize = 512;
/// Name buffers smaller than this are scanned on the calling thread only.
const PARALLEL_BYTES: usize = 1 << 20;

/// Where index dbs live: `KEEL_INDEX_DIR`, `$KEEL_CONFIG_DIR/index`, else the OS
/// cache folder (`Keel/index`). Test binaries get a temp folder.
pub fn index_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return crate::ntfs::index_dir();
    #[cfg(not(windows))]
    {
        let var = |name| std::env::var_os(name).filter(|d| !d.is_empty());
        if let Some(dir) = var("KEEL_INDEX_DIR") {
            return Some(dir.into());
        }
        if let Some(dir) = var("KEEL_CONFIG_DIR") {
            return Some(PathBuf::from(dir).join("index"));
        }
        let in_tests = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent()?.file_name().map(|d| d == "deps"))
            .unwrap_or(false);
        if in_tests {
            return Some(std::env::temp_dir().join("keel-test-index"));
        }
        Some(
            directories::BaseDirs::new()?
                .cache_dir()
                .join("Keel")
                .join("index"),
        )
    }
}

/// The user's home folder, the default root.
pub fn home_root() -> Option<PathBuf> {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .filter(|h| h.is_dir())
}

// ---------------------------------------------------------------------------------
// The in-memory table

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Meta {
    is_dir: bool,
    size: u64,
    /// Milliseconds since the epoch, `NO_TIME` when unknown.
    mtime: i64,
    /// Creation time, milliseconds since the epoch, `NO_TIME` when unknown.
    created: i64,
    /// [`attr`] bits.
    attrs: u32,
}

const DIR_META: Meta = Meta {
    is_dir: true,
    size: 0,
    mtime: NO_TIME,
    created: NO_TIME,
    attrs: attr::DIRECTORY,
};

#[derive(Clone, Copy)]
struct Entry {
    parent: u32,
    /// Byte offset and length of the name in `Table::names`.
    at: u32,
    len: u32,
    live: bool,
    meta: Meta,
}

const DEAD: Entry = Entry {
    parent: NONE,
    at: 0,
    len: 0,
    live: false,
    meta: Meta {
        is_dir: false,
        size: 0,
        mtime: NO_TIME,
        created: NO_TIME,
        attrs: 0,
    },
};

/// A row as stored in the db: (parent, name, is_dir, size, mtime, created, attrs).
type Row = (i64, String, bool, i64, i64, i64, u32);

fn clean(name: &str) -> Cow<'_, str> {
    if name.contains('\n') {
        Cow::Owned(name.replace('\n', " "))
    } else {
        Cow::Borrowed(name)
    }
}

/// Every name under the roots. Slots are stable ids (the db row id); all names sit
/// in one `\n`-separated buffer so a query is one regex scan over contiguous memory.
struct Table {
    /// (root path, slot of its entry); a root's entry is named by the whole path.
    roots: Vec<(PathBuf, u32)>,
    entries: Vec<Entry>,
    free: Vec<u32>,
    names: String,
    /// (offset of a name in `names`, slot), ascending by offset.
    owners: Vec<(u32, u32)>,
    garbage: usize,
    /// parent slot -> name -> slot.
    // ponytail: a map per folder costs ~100 B per entry; a sorted Vec per folder if
    // memory matters.
    children: HashMap<u32, HashMap<String, u32>>,
    /// Slots changed since the last db write.
    dirty: HashSet<u32>,
    live: usize,
    /// What changed, for the journal: on (Some) once the table serves queries, so a
    /// walk building it records nothing.
    log: Option<Vec<Change>>,
}

fn stat(m: Meta) -> Stat {
    let known = |t: i64| (t != NO_TIME).then_some(t);
    Stat {
        is_dir: m.is_dir,
        size: m.size,
        mtime: known(m.mtime),
        created: known(m.created),
        attrs: m.attrs,
    }
}

impl Table {
    fn empty() -> Self {
        Self {
            roots: Vec::new(),
            entries: Vec::new(),
            free: Vec::new(),
            names: String::new(),
            owners: Vec::new(),
            garbage: 0,
            children: HashMap::new(),
            dirty: HashSet::new(),
            live: 0,
            log: None,
        }
    }

    fn new(roots: &[PathBuf]) -> Self {
        let mut t = Self::empty();
        for root in roots {
            let slot = t.add(NONE, &root.to_string_lossy(), DIR_META);
            t.roots.push((root.clone(), slot));
        }
        t
    }

    fn name_of(&self, e: &Entry) -> &str {
        &self.names[e.at as usize..(e.at + e.len) as usize]
    }

    fn push_name(&mut self, slot: u32, name: &str) -> (u32, u32) {
        let at = self.names.len() as u32;
        self.names.push_str(name);
        let len = self.names.len() as u32 - at;
        self.names.push('\n');
        self.owners.push((at, slot));
        (at, len)
    }

    /// The slot of `name` directly under `parent`.
    fn child(&self, parent: u32, name: &str) -> Option<u32> {
        self.children.get(&parent)?.get(&*clean(name)).copied()
    }

    /// Sets an entry's meta (a changed type drops what was below a folder).
    fn set_meta(&mut self, slot: u32, meta: Meta) {
        let e = self.entries[slot as usize];
        if e.meta != meta {
            if e.meta.is_dir && !meta.is_dir {
                self.remove_children(slot);
            }
            self.entries[slot as usize].meta = meta;
            self.dirty.insert(slot);
            if self.log.is_some() && e.parent != NONE {
                if let Some(path) = self.path_of(slot) {
                    let change = Change::Changed(path, stat(e.meta), stat(meta));
                    self.log.get_or_insert_with(Vec::new).push(change);
                }
            }
        }
    }

    /// Adds `name` under `parent`, or updates it when present. Returns its slot.
    fn add(&mut self, parent: u32, name: &str, meta: Meta) -> u32 {
        let name = clean(name);
        if let Some(slot) = self.child(parent, &name) {
            self.set_meta(slot, meta);
            return slot;
        }
        let slot = self.free.pop().unwrap_or(self.entries.len() as u32);
        let (at, len) = self.push_name(slot, &name);
        let e = Entry {
            parent,
            at,
            len,
            live: true,
            meta,
        };
        match self.entries.get_mut(slot as usize) {
            Some(s) => *s = e,
            None => self.entries.push(e),
        }
        self.children
            .entry(parent)
            .or_default()
            .insert(name.into_owned(), slot);
        self.dirty.insert(slot);
        self.live += 1;
        if self.log.is_some() && parent != NONE {
            if let Some(path) = self.path_of(slot) {
                let change = Change::Created(path, stat(meta));
                self.log.get_or_insert_with(Vec::new).push(change);
            }
        }
        slot
    }

    /// `slot` (at `path`) and everything below it, parents first.
    fn subtree_of(&self, slot: u32, path: PathBuf) -> Vec<(PathBuf, Meta)> {
        let mut out = vec![(path, self.entries[slot as usize].meta)];
        let mut stack = vec![(slot, 0usize)];
        while let Some((s, at)) = stack.pop() {
            let Some(kids) = self.children.get(&s) else {
                continue;
            };
            for (name, &k) in kids {
                let p = out[at].0.join(name);
                stack.push((k, out.len()));
                out.push((p, self.entries[k as usize].meta));
            }
        }
        out
    }

    /// [`add`](Self::add) from a change notice or a parent's listing: a known folder
    /// keeps its stored modified time, which records when its own entries were last
    /// listed (a fast rescan relies on it).
    fn refresh(&mut self, parent: u32, name: &str, mut meta: Meta) -> u32 {
        if let Some(slot) = self.child(parent, name) {
            let old = self.entries[slot as usize].meta;
            if old.is_dir && meta.is_dir {
                meta.mtime = old.mtime;
            }
        }
        self.add(parent, name, meta)
    }

    fn remove_children(&mut self, slot: u32) {
        let kids: Vec<u32> = self
            .children
            .get(&slot)
            .map(|c| c.values().copied().collect())
            .unwrap_or_default();
        for kid in kids {
            self.remove(kid);
        }
    }

    /// Drops an entry and everything below it (roots stay).
    fn remove(&mut self, slot: u32) {
        let e = self.entries[slot as usize];
        if !e.live || e.parent == NONE {
            return;
        }
        if self.log.is_some() {
            if let Some(path) = self.path_of(slot) {
                let gone: Vec<Change> = self
                    .subtree_of(slot, path)
                    .into_iter()
                    .map(|(p, m)| Change::Deleted(p, stat(m)))
                    .collect();
                self.log.get_or_insert_with(Vec::new).extend(gone);
            }
        }
        let name = self.name_of(&e).to_owned();
        if let Some(c) = self.children.get_mut(&e.parent) {
            c.remove(&name);
        }
        let mut stack = vec![slot];
        while let Some(s) = stack.pop() {
            if let Some(kids) = self.children.remove(&s) {
                stack.extend(kids.into_values());
            }
            let e = &mut self.entries[s as usize];
            e.live = false;
            self.garbage += e.len as usize + 1;
            self.free.push(s);
            self.dirty.insert(s);
            self.live -= 1;
        }
        if self.garbage >= 1 << 20 && self.garbage * 2 >= self.names.len() {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let old = std::mem::take(&mut self.names);
        self.owners.clear();
        self.garbage = 0;
        for i in 0..self.entries.len() {
            let e = self.entries[i];
            if e.live {
                let at = self.names.len() as u32;
                self.names
                    .push_str(&old[e.at as usize..(e.at + e.len) as usize]);
                self.names.push('\n');
                self.owners.push((at, i as u32));
                self.entries[i].at = at;
            }
        }
    }

    fn path_of(&self, mut slot: u32) -> Option<PathBuf> {
        let mut parts = Vec::new();
        loop {
            let e = self.entries.get(slot as usize).filter(|e| e.live)?;
            parts.push(self.name_of(e));
            if e.parent == NONE {
                break;
            }
            if parts.len() > MAX_DEPTH {
                return None;
            }
            slot = e.parent;
        }
        let mut path = PathBuf::from(parts.pop()?);
        path.extend(parts.iter().rev());
        Some(path)
    }

    /// The root `path` sits under (or is) and the part below it.
    fn root_of<'a>(&self, path: &'a Path) -> Option<(u32, &'a Path)> {
        self.roots
            .iter()
            .find_map(|(root, slot)| Some((*slot, path.strip_prefix(root).ok()?)))
    }

    /// The slot of `path`, None when it is not indexed.
    fn lookup(&self, path: &Path) -> Option<u32> {
        let (mut cur, rel) = self.root_of(path)?;
        for c in rel.components() {
            cur = self.child(cur, &c.as_os_str().to_string_lossy())?;
        }
        Some(cur)
    }

    /// The changes logged since the last call.
    fn take_log(&mut self) -> Vec<Change> {
        self.log.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// What a full walk (`new`) found changed against the table it replaces (same
    /// roots): entries gone, entries new, and entries whose columns moved.
    fn diff(old: &Table, new: &Table) -> Vec<Change> {
        let mut out = Vec::new();
        let mut stack: Vec<(u32, u32, PathBuf)> = old
            .roots
            .iter()
            .zip(&new.roots)
            .filter(|(a, b)| a.0 == b.0)
            .map(|((path, o), (_, n))| (*o, *n, path.clone()))
            .collect();
        let none = HashMap::new();
        while let Some((o, n, path)) = stack.pop() {
            let (oe, ne) = (old.entries[o as usize], new.entries[n as usize]);
            if oe.parent != NONE && oe.meta != ne.meta {
                out.push(Change::Changed(path.clone(), stat(oe.meta), stat(ne.meta)));
            }
            let ok = old.children.get(&o).unwrap_or(&none);
            let nk = new.children.get(&n).unwrap_or(&none);
            for (name, &s) in ok {
                match nk.get(name) {
                    Some(&t) => stack.push((s, t, path.join(name))),
                    None => out.extend(
                        old.subtree_of(s, path.join(name))
                            .into_iter()
                            .map(|(p, m)| Change::Deleted(p, stat(m))),
                    ),
                }
            }
            for (name, &t) in nk {
                if !ok.contains_key(name) {
                    out.extend(
                        new.subtree_of(t, path.join(name))
                            .into_iter()
                            .map(|(p, m)| Change::Created(p, stat(m))),
                    );
                }
            }
        }
        out
    }

    fn take_dirty(&mut self) -> Vec<(u32, Option<Row>)> {
        let dirty: Vec<u32> = self.dirty.drain().collect();
        dirty.into_iter().map(|s| (s, self.row(s))).collect()
    }

    fn row(&self, slot: u32) -> Option<Row> {
        let e = self.entries.get(slot as usize).filter(|e| e.live)?;
        let m = e.meta;
        Some((
            if e.parent == NONE {
                -1
            } else {
                e.parent as i64
            },
            self.name_of(e).to_owned(),
            m.is_dir,
            m.size as i64,
            m.mtime,
            m.created,
            m.attrs,
        ))
    }

    /// Rows must come in ascending slot order.
    fn from_rows(roots: &[PathBuf], rows: Vec<(u32, Row)>) -> Option<Self> {
        let mut t = Self::empty();
        for (slot, (parent, name, is_dir, size, mtime, created, attrs)) in rows {
            while t.entries.len() < slot as usize {
                t.free.push(t.entries.len() as u32);
                t.entries.push(DEAD);
            }
            let (at, len) = t.push_name(slot, &name);
            let parent = if parent < 0 { NONE } else { parent as u32 };
            t.entries.push(Entry {
                parent,
                at,
                len,
                live: true,
                meta: Meta {
                    is_dir,
                    size: size as u64,
                    mtime,
                    created,
                    attrs,
                },
            });
            t.children.entry(parent).or_default().insert(name, slot);
            t.live += 1;
        }
        for root in roots {
            let slot = *t.children.get(&NONE)?.get(&*root.to_string_lossy())?;
            t.roots.push((root.clone(), slot));
        }
        (t.children.get(&NONE).map_or(0, HashMap::len) == roots.len()).then_some(t)
    }

    /// Runs `f` over all name segments: in parallel chunks for a large table,
    /// results concatenated in buffer order.
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

    /// Checks one candidate (the owner at `owners[i]`, whose name is `name`) against
    /// every term; the rank is 0 exact, 1 prefix, 2 anything else.
    fn candidate(&self, i: usize, name: &str, m: &Matcher) -> Option<(u8, u32)> {
        let (at, slot) = self.owners[i];
        let e = self.entries[slot as usize];
        if !e.live || e.at != at || e.parent == NONE || (m.folders_only && !e.meta.is_dir) {
            return None;
        }
        if let Some(scan) = &m.scan {
            if !scan.is_match(name) {
                return None;
            }
        }
        if !m.rest.iter().all(|re| re.is_match(name)) {
            return None;
        }
        if !m.path.is_empty() {
            let path = self.path_of(slot)?;
            let path = path.to_string_lossy();
            if !m.path.iter().all(|re| re.is_match(&path)) {
                return None;
            }
        }
        if let Some(within) = &m.within {
            let path = self.path_of(slot)?.to_string_lossy().replace('\\', "/");
            if !path.to_lowercase().starts_with(within) {
                return None;
            }
        }
        Some((m.quality(name), slot))
    }

    fn scan_range(&self, m: &Matcher, range: Range<usize>) -> Vec<(u8, u32)> {
        let mut out = Vec::new();
        if range.is_empty() {
            return out;
        }
        let Some(scan) = &m.scan else {
            for i in range {
                let e = self.entries[self.owners[i].1 as usize];
                if e.live && e.at == self.owners[i].0 {
                    out.extend(self.candidate(i, self.name_of(&e), m));
                }
            }
            return out;
        };
        let stop = self
            .owners
            .get(range.end)
            .map_or(self.names.len(), |o| o.0 as usize);
        // The chunk ends after a '\n', so `^` and `$` see the same boundaries.
        let hay = &self.names[..stop];
        let mut pos = self.owners[range.start].0 as usize;
        while pos < stop {
            let Some(found) = scan.find_at(hay, pos) else {
                break;
            };
            let at = found.start();
            let line = hay[..at].rfind('\n').map_or(0, |i| i + 1);
            let end = hay[at..].find('\n').map_or(stop, |i| at + i);
            pos = end + 1;
            if let Ok(i) = self.owners.binary_search_by_key(&(line as u32), |o| o.0) {
                out.extend(self.candidate(i, &hay[line..end], m));
            }
        }
        out
    }

    /// The best `max` matches, ranked by quality then path.
    fn search(&self, m: &Matcher, max: usize) -> Vec<(u32, PathBuf)> {
        let mut found = self.par(|range| self.scan_range(m, range));
        if found.len() > max {
            if max == 0 {
                return Vec::new();
            }
            let name = |slot: u32| self.name_of(&self.entries[slot as usize]).bytes();
            found.select_nth_unstable_by(max - 1, |a, b| {
                (a.0.cmp(&b.0))
                    .then_with(|| {
                        let lower = |c: u8| c.to_ascii_lowercase();
                        name(a.1).map(lower).cmp(name(b.1).map(lower))
                    })
                    .then(a.1.cmp(&b.1))
            });
            found.truncate(max);
        }
        let mut out: Vec<(u8, String, u32, PathBuf)> = found
            .into_iter()
            .filter_map(|(q, slot)| {
                let path = self.path_of(slot)?;
                Some((q, path.to_string_lossy().to_ascii_lowercase(), slot, path))
            })
            .collect();
        out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        out.into_iter()
            .map(|(_, _, slot, path)| (slot, path))
            .collect()
    }
}

pub(crate) fn eq_case(a: &str, b: &str, match_case: bool) -> bool {
    if match_case {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

// ---------------------------------------------------------------------------------
// Query text -> Matcher (mirrors ntfs/pattern.rs, which is Windows-only)

pub(crate) struct Matcher {
    /// Finds candidates over the whole name buffer (multi-line mode).
    pub(crate) scan: Option<Regex>,
    /// Further terms every candidate name must match.
    pub(crate) rest: Vec<Regex>,
    /// The scan term when it is a plain substring: drives exact/prefix ranking.
    pub(crate) literal: Option<String>,
    pub(crate) match_case: bool,
    pub(crate) folders_only: bool,
    /// `in:` path prefix, lowercased with `/` separators.
    pub(crate) within: Option<String>,
    /// Match Path: every term, matched against the candidate's full path instead.
    pub(crate) path: Vec<Regex>,
}

impl Matcher {
    /// 0 for an exact name, 1 for a prefix, 2 otherwise.
    fn quality(&self, name: &str) -> u8 {
        match &self.literal {
            Some(lit) if eq_case(name, lit, self.match_case) => 0,
            Some(lit)
                if name
                    .get(..lit.len())
                    .is_some_and(|head| eq_case(head, lit, self.match_case)) =>
            {
                1
            }
            _ => 2,
        }
    }
}

enum Term {
    Literal(String),
    Pattern(String),
}

fn term_of(text: &str, query: &Query) -> Term {
    if text.contains(['*', '?']) {
        Term::Pattern(terms::glob(text, query))
    } else {
        Term::Literal(text.to_owned())
    }
}

pub(crate) fn compile(query: &Query) -> anyhow::Result<Matcher> {
    let mut folders_only = query.folders_only;
    // A pane's folder, resolved like a typed `in:` (below) and ending in a separator (whole
    // folder names only); a typed `in:` takes its place.
    let mut within = query.within.as_ref().map(|dir| {
        #[cfg(not(windows))]
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        let dir = dir.display().to_string().replace('\\', "/").to_lowercase();
        format!("{}/", dir.trim_end_matches('/'))
    });
    let mut terms = Vec::new();
    if query.regex {
        if !query.text.is_empty() {
            terms.push(Term::Pattern(query.text.clone()));
        }
    } else {
        for term in split_terms(&query.text) {
            if let Some(rest) = strip_ci(&term, "folder:") {
                folders_only = true;
                if !rest.is_empty() {
                    terms.push(term_of(rest, query));
                }
            } else if let Some(rest) = strip_ci(&term, "in:") {
                // The index stores resolved roots (macOS: /private/var for /var), so a typed
                // folder is resolved the same way when it exists.
                #[cfg(not(windows))]
                let rest: String = std::fs::canonicalize(rest)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| rest.to_owned());
                within = Some(rest.replace('\\', "/").to_lowercase());
            } else if let Some(rest) = strip_ci(&term, "regex:") {
                terms.push(Term::Pattern(rest.to_owned()));
            } else {
                terms.push(term_of(&term, query));
            }
        }
    }
    // Scan with the longest substring (most selective), else the first pattern.
    let lead = terms
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match t {
            Term::Literal(s) => Some((s.len(), i)),
            Term::Pattern(_) => None,
        })
        .max()
        .map(|(_, i)| i)
        .or((!terms.is_empty()).then_some(0));
    let regex = |src: &str| -> anyhow::Result<Regex> {
        Ok(RegexBuilder::new(src)
            .multi_line(true)
            .case_insensitive(!query.match_case)
            .build()?)
    };
    let build = |t: &Term| match t {
        Term::Literal(s) => regex(&terms::literal(s, query)),
        Term::Pattern(p) => regex(p),
    };
    // Match Whole Words and Match Path (shared with the NTFS index, `terms.rs`).
    let literals: Vec<&str> = (terms.iter())
        .filter_map(|t| match t {
            Term::Literal(s) => Some(s.as_str()),
            Term::Pattern(_) => None,
        })
        .collect();
    let words = terms::word_terms(&literals, query, &regex)?;
    if query.match_path {
        let mut path = terms
            .iter()
            .map(build)
            .collect::<anyhow::Result<Vec<_>>>()?;
        path.extend(words);
        return Ok(Matcher {
            scan: None,
            rest: Vec::new(),
            literal: None,
            match_case: query.match_case,
            folders_only,
            within,
            path,
        });
    }
    let (mut scan, mut literal, mut rest) = (None, None, Vec::new());
    for (i, t) in terms.iter().enumerate() {
        if Some(i) == lead {
            scan = Some(build(t)?);
            if let Term::Literal(s) = t {
                literal = Some(s.clone());
            }
        } else {
            rest.push(build(t)?);
        }
    }
    rest.extend(words);
    Ok(Matcher {
        scan,
        rest,
        literal,
        match_case: query.match_case,
        folders_only,
        within,
        path: Vec::new(),
    })
}

/// Merges hits of several backends answering `query`: one copy of each path, ranked
/// like one index would (exact name, then prefix, then the rest, each by path), at
/// most `query.max`.
pub(crate) fn merge_hits(query: &Query, hits: Vec<Hit>) -> Vec<Hit> {
    let quality = compile(query).ok();
    let key = |h: &Hit| {
        let path = h.path.display();
        // Windows paths ignore case; elsewhere `A` and `a` are two files.
        let same = if cfg!(windows) {
            path.to_lowercase()
        } else {
            path.clone()
        };
        let rank = quality.as_ref().map_or(2, |m| m.quality(h.path.name()));
        (rank, path.to_lowercase(), same)
    };
    let mut keyed: Vec<((u8, String, String), Hit)> =
        hits.into_iter().map(|h| (key(&h), h)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut seen = HashSet::new();
    keyed
        .into_iter()
        .filter(|((_, _, same), _)| seen.insert(same.clone()))
        .map(|(_, h)| h)
        .take(query.max as usize)
        .collect()
}

// ---------------------------------------------------------------------------------
// Walking

fn millis(t: std::io::Result<SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(NO_TIME, |d| d.as_millis() as i64)
}

#[cfg(windows)]
fn attrs_of(md: &std::fs::Metadata, _path: &Path) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

#[cfg(not(windows))]
fn attrs_of(md: &std::fs::Metadata, path: &Path) -> u32 {
    let mut a = 0;
    if md.is_dir() {
        a |= attr::DIRECTORY;
    }
    if md.permissions().readonly() {
        a |= attr::READONLY;
    }
    if path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
    {
        a |= attr::HIDDEN;
    }
    a
}

fn meta_of(md: &std::fs::Metadata, path: &Path) -> Meta {
    // A link counts as what it points at, like `walk` does.
    let link = md.file_type().is_symlink();
    let followed;
    let md = if link {
        followed = std::fs::metadata(path);
        followed.as_ref().unwrap_or(md)
    } else {
        md
    };
    // A folder's time as listed in its parent (Windows: the parent's copy, updated
    // lazily) can lag its own; a fast rescan compares the folder's own, so store that.
    #[cfg(windows)]
    let own = (md.is_dir() && !link)
        .then(|| std::fs::metadata(path).ok())
        .flatten();
    #[cfg(windows)]
    let mtime = millis(own.as_ref().unwrap_or(md).modified());
    #[cfg(not(windows))]
    let mtime = millis(md.modified());
    Meta {
        is_dir: md.is_dir(),
        size: if md.is_dir() { 0 } else { md.len() },
        mtime,
        created: millis(md.created()),
        attrs: attrs_of(md, path) | if link { attr::REPARSE_POINT } else { 0 },
    }
}

/// The rules an index walks by: `None` is the home index's (skip hidden and
/// git-ignored entries), `Some` the Exclude tab's.
type Rules = Option<Arc<ExcludeRules>>;

fn rules_key(rules: &Rules) -> String {
    rules
        .as_ref()
        .map_or_else(|| "standard".into(), |r| r.key())
}

/// A walk of `root` (to `depth` levels) by `rules`.
fn walker(root: &Path, rules: &Rules, depth: Option<usize>) -> ignore::Walk {
    let mut b = ignore::WalkBuilder::new(root);
    b.max_depth(depth);
    if let Some(rules) = rules {
        let rules = rules.clone();
        b.standard_filters(false).filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            let attrs = if rules.needs_attrs() {
                e.metadata().map_or(0, |m| attrs_of(&m, e.path()))
            } else {
                0
            };
            rules.admit(e.path(), is_dir, attrs)
        });
    }
    b.build()
}

/// Walks every root into a fresh table.
fn scan(roots: &[PathBuf], rules: &Rules, progress: &AtomicUsize, stop: &AtomicBool) -> Table {
    let mut t = Table::new(roots);
    for (root, root_slot) in t.roots.clone() {
        if let Ok(md) = std::fs::metadata(&root) {
            t.set_meta(root_slot, meta_of(&md, &root));
        }
        // stack[depth] = slot of the folder at that depth.
        let mut stack = vec![root_slot];
        for entry in walker(&root, rules, None).flatten() {
            let depth = entry.depth();
            if depth == 0 {
                continue;
            }
            if stop.load(Relaxed) {
                return t;
            }
            stack.truncate(depth);
            let (true, Some(&parent)) = (stack.len() == depth, stack.last()) else {
                continue;
            };
            let Ok(md) = entry.metadata() else { continue };
            let meta = meta_of(&md, entry.path());
            let slot = t.add(parent, &entry.file_name().to_string_lossy(), meta);
            if meta.is_dir {
                stack.push(slot);
            }
            progress.store(t.live, Relaxed);
        }
    }
    t.dirty.clear();
    t
}

/// The entries directly in `dir` that the rules admit: (name, path, meta).
fn listing(dir: &Path, rules: &Rules) -> Vec<(String, PathBuf, Meta)> {
    walker(dir, rules, Some(1))
        .flatten()
        .filter(|e| e.depth() == 1)
        .filter_map(|e| {
            let meta = meta_of(&e.metadata().ok()?, e.path());
            Some((
                e.file_name().to_string_lossy().into_owned(),
                e.path().to_path_buf(),
                meta,
            ))
        })
        .collect()
}

/// Everything below `dir` that the rules admit, parents before children.
fn subtree(dir: &Path, rules: &Rules) -> Vec<(PathBuf, Meta)> {
    walker(dir, rules, None)
        .flatten()
        .filter(|e| e.depth() > 0)
        .filter_map(|e| {
            Some((
                e.path().to_path_buf(),
                meta_of(&e.metadata().ok()?, e.path()),
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------------
// SQLite

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn roots_key(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `walk-<hash of the roots>.db` in `dir`.
fn db_path(dir: &Path, roots: &[PathBuf]) -> PathBuf {
    let mut h = 0xcbf2_9ce4_8422_2325_u64; // FNV-1a
    for b in roots_key(roots).bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    dir.join(format!("walk-{h:016x}.db"))
}

/// Deletes the saved index of `root` (an offline volume removed from Settings).
pub(crate) fn delete_db(dir: &Path, root: &Path) {
    let db = db_path(dir, &normalize_roots(vec![root.to_path_buf()]));
    // Its worker may still be closing the db (Windows refuses to delete an open file).
    for _ in 0..20 {
        for suffix in ["", "-wal", "-shm", ".tmp"] {
            let _ = std::fs::remove_file(sibling(&db, suffix));
        }
        if !db.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files(
    id INTEGER PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL,
    is_dir INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
    created INTEGER NOT NULL, attrs INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);";

/// What a db says about itself besides its rows.
struct Stamp {
    /// Unix seconds of the last full walk and of the last scan of any kind.
    built: i64,
    scanned: i64,
    rules: String,
}

fn save_full(path: &Path, t: &Table, roots: &[PathBuf], stamp: &Stamp) -> anyhow::Result<()> {
    let tmp = sibling(path, ".tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut conn = Connection::open(&tmp)?;
        conn.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;")?;
        conn.execute_batch(SCHEMA)?;
        let tx = conn.transaction()?;
        {
            let mut insert =
                tx.prepare("INSERT INTO files VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
            for slot in 0..t.entries.len() as u32 {
                if let Some((parent, name, is_dir, size, mtime, created, attrs)) = t.row(slot) {
                    insert.execute(params![
                        slot, parent, name, is_dir, size, mtime, created, attrs
                    ])?;
                }
            }
        }
        tx.execute(
            "INSERT INTO meta VALUES ('version', ?1), ('built', ?2), ('scanned', ?3), \
             ('roots', ?4), ('rules', ?5)",
            params![
                VERSION,
                stamp.built.to_string(),
                stamp.scanned.to_string(),
                roots_key(roots),
                stamp.rules
            ],
        )?;
        tx.commit()?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    }
    for stale in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(sibling(path, stale));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Writes changed rows, and the time of a rescan when `scanned` is given.
fn save_changes(
    path: &Path,
    changes: Vec<(u32, Option<Row>)>,
    scanned: Option<i64>,
) -> anyhow::Result<()> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let tx = conn.transaction()?;
    {
        let mut delete = tx.prepare("DELETE FROM files WHERE id = ?1")?;
        let mut upsert =
            tx.prepare("INSERT OR REPLACE INTO files VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
        for (slot, row) in changes {
            match row {
                Some((parent, name, is_dir, size, mtime, created, attrs)) => {
                    upsert.execute(params![
                        slot, parent, name, is_dir, size, mtime, created, attrs
                    ])?;
                }
                None => {
                    delete.execute([slot])?;
                }
            }
        }
    }
    if let Some(scanned) = scanned {
        tx.execute(
            "INSERT OR REPLACE INTO meta VALUES ('scanned', ?1)",
            [scanned.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// The saved table and its stamp; None when absent, for other roots, of an older
/// layout, or bad.
fn load(path: &Path, roots: &[PathBuf]) -> Option<(Table, Stamp)> {
    if !path.is_file() {
        return None;
    }
    let conn = Connection::open(path).ok()?;
    let meta = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
    };
    if meta("version")? != VERSION || meta("roots")? != roots_key(roots) {
        return None;
    }
    let built: i64 = meta("built")?.parse().ok()?;
    let stamp = Stamp {
        built,
        scanned: meta("scanned")
            .and_then(|s| s.parse().ok())
            .unwrap_or(built),
        rules: meta("rules").unwrap_or_default(),
    };
    let mut stmt = conn
        .prepare(
            "SELECT id, parent, name, is_dir, size, mtime, created, attrs FROM files ORDER BY id",
        )
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                (
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ),
            ))
        })
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some((Table::from_rows(roots, rows)?, stamp))
}

// ---------------------------------------------------------------------------------
// The searcher and its worker

enum Cmd {
    Paths(Vec<PathBuf>),
    /// Renames the watcher reported as (old, new) pairs; both paths changed.
    Renamed(Vec<(PathBuf, PathBuf)>),
    /// A full walk (Force rebuild).
    Rebuild,
    /// A rescan as scheduled: fast when Fast rescan is on.
    Rescan,
    /// The change buffer overflowed: changes were lost.
    Overflow,
    /// The watch ended (the root or its volume went away, or the watcher failed).
    Lost,
    /// Check the schedule and whether the roots are online now; the sender, when
    /// given, hears when that is done (tests).
    Tick(Option<mpsc::Sender<()>>),
    Stop,
}

struct Shared {
    table: RwLock<Table>,
    /// A table (loaded or built) is serving queries.
    ready: AtomicBool,
    building: AtomicBool,
    progress: AtomicUsize,
    swapped: AtomicBool,
    unwatched: AtomicBool,
    /// A root is missing: the last scan is served, nothing is rescanned.
    offline: AtomicBool,
    /// Changes were lost (an overflow with Rescan on overflow off).
    missed: AtomicBool,
    /// Rescans and rebuilds run so far (tests).
    scans: AtomicUsize,
    stop: AtomicBool,
}

/// How a walk index is kept.
#[derive(Clone)]
pub struct WalkOptions {
    /// `None`: the home index's rules (hidden and git-ignored entries are skipped).
    /// `Some`: the Exclude tab's rules, and nothing else is skipped.
    pub rules: Option<ExcludeRules>,
    pub scan: ScanOptions,
    /// A full walk when the last one is older than this many seconds.
    pub max_age_secs: Option<i64>,
    pub clock: Arc<dyn Clock>,
    /// Where the changes the index applies are journaled; None: the process's journal
    /// ([`journal::record_all`], off when Settings → Search turns it off).
    pub journal: Option<Arc<Journal>>,
}

impl Default for WalkOptions {
    /// The home index: skip rules, watched, rebuilt weekly.
    fn default() -> Self {
        Self {
            rules: None,
            scan: ScanOptions::default(),
            max_age_secs: Some(MAX_AGE_SECS),
            clock: Arc::new(SystemClock),
            journal: None,
        }
    }
}

/// An indexed entry's stored columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// [`crate::attr`] bits.
    pub attributes: u32,
}

fn time_of(ms: i64) -> Option<SystemTime> {
    (ms != NO_TIME).then(|| {
        if ms >= 0 {
            UNIX_EPOCH + Duration::from_millis(ms as u64)
        } else {
            UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
        }
    })
}

/// A persistent, watched name index over one or more root folders.
pub struct WalkIndexSearcher {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Cmd>,
    roots: Vec<PathBuf>,
}

impl WalkIndexSearcher {
    /// Opens (or starts building) the home-style index of `roots` with its db in
    /// `dir`. Returns at once; the saved db loads, or the walk runs, on a background
    /// thread.
    pub fn open(dir: PathBuf, roots: Vec<PathBuf>) -> anyhow::Result<Self> {
        Self::open_with(dir, roots, WalkOptions::default())
    }

    /// [`open`](Self::open) with explicit rules, watching, schedule and clock.
    pub fn open_with(dir: PathBuf, roots: Vec<PathBuf>, opts: WalkOptions) -> anyhow::Result<Self> {
        let roots = normalize_roots(roots);
        anyhow::ensure!(!roots.is_empty(), "no folders to index");
        std::fs::create_dir_all(&dir)?;
        let db = db_path(&dir, &roots);
        let shared = Arc::new(Shared {
            table: RwLock::new(Table::new(&roots)),
            ready: AtomicBool::new(false),
            building: AtomicBool::new(true),
            progress: AtomicUsize::new(0),
            swapped: AtomicBool::new(false),
            unwatched: AtomicBool::new(false),
            offline: AtomicBool::new(false),
            missed: AtomicBool::new(false),
            scans: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            shared: shared.clone(),
            db,
            roots: roots.clone(),
            rules: opts.rules.clone().map(Arc::new),
            opts,
            built: 0,
            scanned: 0,
            rules_ok: false,
            events: tx.clone(),
            watch: None,
        };
        std::thread::Builder::new()
            .name("keel-walk-index".into())
            .spawn(move || worker.run(rx))?;
        Ok(Self { shared, tx, roots })
    }

    /// Walks everything again in the background; queries keep using the old index.
    pub fn rebuild(&self) {
        let _ = self.tx.send(Cmd::Rebuild);
    }

    /// Rescans in the background as a schedule would (fast when Fast rescan is on).
    pub fn rescan(&self) {
        let _ = self.tx.send(Cmd::Rescan);
    }

    /// Blocks until an index serves queries (tests, tools).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while !self.shared.ready.load(Relaxed) {
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// The stored columns of `path`, None when it is not indexed.
    pub fn info(&self, path: &Path) -> Option<EntryInfo> {
        let t = self.shared.table.read();
        let m = t.entries[t.lookup(path)? as usize].meta;
        Some(EntryInfo {
            is_dir: m.is_dir,
            size: m.size,
            modified: time_of(m.mtime),
            created: time_of(m.created),
            attributes: m.attrs,
        })
    }

    /// The indexed roots.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Makes the worker check its schedule and roots now and waits until it has
    /// (tests drive a fake clock with it).
    #[cfg(test)]
    fn tick(&self) {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Cmd::Tick(Some(tx)));
        let _ = rx.recv_timeout(Duration::from_secs(60));
    }
}

impl Drop for WalkIndexSearcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }
}

/// Absolute, resolved (macOS reports `/private/var/...` for `/var/...`), sorted, and
/// without roots that sit inside another root.
pub(crate) fn normalize_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|r| {
            let abs = std::path::absolute(&r).ok()?;
            #[cfg(windows)]
            return Some(abs);
            // A missing root (an offline volume) resolves through its parent, so it
            // keeps the name its saved index has.
            #[cfg(not(windows))]
            return Some(
                std::fs::canonicalize(&abs)
                    .ok()
                    .or_else(|| {
                        Some(
                            std::fs::canonicalize(abs.parent()?)
                                .ok()?
                                .join(abs.file_name()?),
                        )
                    })
                    .unwrap_or(abs),
            );
        })
        .collect();
    roots.sort();
    roots.dedup();
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o)));
    roots
}

impl Searcher for WalkIndexSearcher {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        // A pane's folder outside the roots (another drive, a share): walk it.
        if let Some(dir) = crate::pane_folder(q).filter(|d| !self.covers(d)) {
            return Ok(crate::walk(dir, q));
        }
        anyhow::ensure!(
            self.shared.ready.load(Relaxed),
            "{}",
            self.status().unwrap_or_else(|| "index not ready".into())
        );
        let matcher = compile(q)?;
        let table = self.shared.table.read();
        Ok(table
            .search(&matcher, q.max as usize)
            .into_iter()
            .map(|(slot, path)| {
                let m = table.entries[slot as usize].meta;
                Hit {
                    path: keel_vfs::VPath::local(path),
                    is_dir: m.is_dir,
                    size: m.size,
                    modified: time_of(m.mtime),
                    snippet: None,
                }
            })
            .collect())
    }

    fn available(&self) -> bool {
        self.shared.ready.load(Relaxed)
    }

    /// A folder under a root, or above one (its hits are the root's part of it).
    fn covers(&self, dir: &Path) -> bool {
        #[cfg(not(windows))]
        let dir = &std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
        (self.roots.iter()).any(|r| crate::is_under(dir, r) || crate::is_under(r, dir))
    }

    fn status(&self) -> Option<String> {
        let sh = &self.shared;
        if sh.offline.load(Relaxed) {
            let roots: Vec<String> = self.roots.iter().map(|r| r.display().to_string()).collect();
            Some(format!(
                "{} is offline; showing its files from the last scan",
                roots.join(", ")
            ))
        } else if sh.building.load(Relaxed) {
            Some(format!(
                "indexing {} files\u{2026}",
                sh.progress.load(Relaxed)
            ))
        } else if sh.missed.load(Relaxed) {
            Some("some changes were missed (the change buffer overflowed); they show after the next rescan".into())
        } else if sh.unwatched.load(Relaxed) {
            Some("changes are not tracked live (watch failed); rebuilds weekly".into())
        } else {
            None
        }
    }

    fn name(&self) -> &'static str {
        "Keel index"
    }

    fn state(&self) -> SearchState {
        if self.shared.ready.load(Relaxed) {
            SearchState::Ready
        } else if self.shared.building.load(Relaxed) {
            SearchState::Indexing {
                done: self.shared.progress.load(Relaxed),
            }
        } else {
            SearchState::Unavailable
        }
    }

    fn take_ready(&self) -> bool {
        self.shared.swapped.swap(false, Relaxed)
    }

    fn offline_roots(&self) -> Vec<PathBuf> {
        if self.shared.offline.load(Relaxed) {
            self.roots.clone()
        } else {
            Vec::new()
        }
    }
}

/// The live watch of the roots.
enum Watch {
    Notify(#[allow(dead_code)] notify::RecommendedWatcher),
    #[cfg(windows)]
    Rdcw(#[allow(dead_code)] Vec<crate::rdcw::Watch>),
}

struct Worker {
    shared: Arc<Shared>,
    db: PathBuf,
    roots: Vec<PathBuf>,
    rules: Rules,
    opts: WalkOptions,
    /// Unix seconds (by the clock) of the last full walk and of the last scan.
    built: i64,
    scanned: i64,
    /// The table serving queries was walked with the current rules: a full walk's
    /// difference from it is journaled (not after the rules changed).
    rules_ok: bool,
    events: mpsc::Sender<Cmd>,
    watch: Option<Watch>,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        let sh = self.shared.clone();
        let mut rules_ok = false;
        if let Some((mut table, stamp)) = load(&self.db, &self.roots) {
            sh.progress.store(table.live, Relaxed);
            table.log = Some(Vec::new());
            *sh.table.write() = table;
            self.built = stamp.built;
            self.scanned = stamp.scanned;
            rules_ok = stamp.rules == rules_key(&self.rules);
            self.rules_ok = rules_ok;
            sh.ready.store(true, Relaxed);
            sh.swapped.store(true, Relaxed);
        }
        if !self.online() {
            // Serve the last scan (if any); never walk a missing root.
            sh.offline.store(true, Relaxed);
            sh.building.store(false, Relaxed);
        } else {
            // Started before the first walk so changes made meanwhile queue up.
            self.start_watch();
            if !sh.ready.load(Relaxed) || self.stale() || !rules_ok {
                self.rebuild();
            } else if self.opts.scan.fast_rescan {
                // Catch up with what changed while Keel was closed.
                self.rescan();
            }
            sh.building.store(false, Relaxed);
        }
        loop {
            let cmd = match rx.recv_timeout(self.wait()) {
                Ok(cmd) => cmd,
                Err(mpsc::RecvTimeoutError::Timeout) => Cmd::Tick(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if !self.handle(cmd, &rx) {
                break;
            }
        }
    }

    /// Runs one command; false when the worker should stop.
    fn handle(&mut self, cmd: Cmd, rx: &mpsc::Receiver<Cmd>) -> bool {
        match cmd {
            Cmd::Stop => return false,
            Cmd::Tick(ack) => {
                self.periodic();
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            Cmd::Rebuild => {
                if self.online() {
                    self.rebuild();
                }
            }
            Cmd::Rescan => self.rescan(),
            Cmd::Overflow => {
                if self.opts.scan.rescan_on_overflow {
                    self.rescan();
                } else {
                    self.shared.missed.store(true, Relaxed);
                }
            }
            Cmd::Lost => {
                self.watch = None;
                if self.online() {
                    // The watcher failed: watch again and catch up.
                    self.start_watch();
                    self.rescan();
                } else {
                    self.shared.offline.store(true, Relaxed);
                }
            }
            first @ (Cmd::Paths(_) | Cmd::Renamed(_)) => {
                let mut pending: HashSet<PathBuf> = HashSet::new();
                let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
                let mut take = |cmd: Cmd| match cmd {
                    Cmd::Paths(p) => pending.extend(p),
                    Cmd::Renamed(r) => {
                        pending.extend(r.iter().flat_map(|(a, b)| [a.clone(), b.clone()]));
                        renames.extend(r);
                    }
                    _ => {}
                };
                take(first);
                let mut later = Vec::new();
                let deadline = Instant::now() + BATCH;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(left) {
                        Ok(cmd @ (Cmd::Paths(_) | Cmd::Renamed(_))) => take(cmd),
                        Ok(Cmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return false,
                        Ok(other) => later.push(other),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                    }
                }
                self.apply(pending, &renames);
                for cmd in later {
                    if !self.handle(cmd, rx) {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn now(&self) -> i64 {
        self.opts.clock.now()
    }

    fn online(&self) -> bool {
        self.roots.iter().all(|r| r.is_dir())
    }

    fn stale(&self) -> bool {
        self.opts
            .max_age_secs
            .is_some_and(|age| self.now().saturating_sub(self.built) > age)
    }

    /// How long to sleep before the next check.
    fn wait(&self) -> Duration {
        if self.shared.offline.load(Relaxed) {
            return OFFLINE_POLL;
        }
        let due = self
            .opts
            .scan
            .schedule
            .wait(self.scanned, self.now(), self.opts.clock.as_ref())
            .map_or(IDLE, Duration::from_secs);
        due.clamp(Duration::from_secs(1), IDLE)
    }

    /// The schedule, the weekly rebuild, and roots going offline or coming back.
    fn periodic(&mut self) {
        let sh = self.shared.clone();
        if !self.online() {
            sh.offline.store(true, Relaxed);
            self.watch = None;
            return;
        }
        if sh.offline.swap(false, Relaxed) {
            // Back: watch again and catch up with what changed meanwhile.
            self.start_watch();
            self.rescan();
            return;
        }
        if self.stale() {
            self.rebuild();
        } else if self
            .opts
            .scan
            .schedule
            .due(self.scanned, self.now(), self.opts.clock.as_ref())
        {
            self.rescan();
        }
    }

    fn start_watch(&mut self) {
        self.watch = None;
        if !self.opts.scan.monitor {
            self.shared.unwatched.store(false, Relaxed);
            return;
        }
        #[cfg(windows)]
        if self.opts.rules.is_some() {
            // Configured roots: our own ReadDirectoryChangesW, with the chosen buffer.
            let buffer = self.opts.scan.buffer_kib as usize * 1024;
            let watches: Option<Vec<_>> = self
                .roots
                .iter()
                .map(|root| {
                    let tx = self.events.clone();
                    crate::rdcw::watch(root, buffer, move |e| {
                        let _ = tx.send(match e {
                            crate::rdcw::Event::Paths(p) => Cmd::Paths(p),
                            crate::rdcw::Event::Renamed(r) => Cmd::Renamed(r),
                            crate::rdcw::Event::Overflow => Cmd::Overflow,
                            crate::rdcw::Event::Lost => Cmd::Lost,
                        });
                    })
                    .ok()
                })
                .collect();
            if let Some(watches) = watches {
                self.shared.unwatched.store(false, Relaxed);
                self.watch = Some(Watch::Rdcw(watches));
                return;
            }
        }
        use notify::event::{ModifyKind, RenameMode};
        use notify::{EventKind, RecursiveMode, Watcher};
        let tx = self.events.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(ev) if ev.need_rescan() => {
                    let _ = tx.send(Cmd::Overflow);
                }
                Ok(ev)
                    if matches!(
                        ev.kind,
                        EventKind::Modify(ModifyKind::Name(RenameMode::Both))
                    ) && ev.paths.len() == 2 =>
                {
                    let pair = (ev.paths[0].clone(), ev.paths[1].clone());
                    let _ = tx.send(Cmd::Renamed(vec![pair]));
                }
                Ok(ev) if !matches!(ev.kind, EventKind::Access(_)) => {
                    let _ = tx.send(Cmd::Paths(ev.paths));
                }
                Ok(_) => {}
                // Lost events: walk again (or note the root went offline).
                Err(_) => {
                    let _ = tx.send(Cmd::Lost);
                }
            })
            .ok();
        let watched = watcher.as_mut().is_some_and(|w| {
            self.roots
                .iter()
                .all(|r| w.watch(r, RecursiveMode::Recursive).is_ok())
        });
        self.shared.unwatched.store(!watched, Relaxed);
        self.watch = watcher.filter(|_| watched).map(Watch::Notify);
    }

    /// A rescan: fast (changed folders only) when on and an index is loaded, else a
    /// full walk. Skipped while a root is offline.
    fn rescan(&mut self) {
        if !self.online() {
            self.shared.offline.store(true, Relaxed);
            return;
        }
        if self.opts.scan.fast_rescan && self.shared.ready.load(Relaxed) {
            self.fast_rescan();
        } else {
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let sh = self.shared.clone();
        if !self.online() {
            sh.offline.store(true, Relaxed);
            return;
        }
        sh.building.store(true, Relaxed);
        sh.progress.store(0, Relaxed);
        let mut table = scan(&self.roots, &self.rules, &sh.progress, &sh.stop);
        if sh.stop.load(Relaxed) {
            return;
        }
        sh.building.store(false, Relaxed);
        if !self.online() {
            // The root went away during the walk: keep the last full scan.
            sh.offline.store(true, Relaxed);
            return;
        }
        self.built = self.now();
        self.scanned = self.built;
        let stamp = Stamp {
            built: self.built,
            scanned: self.scanned,
            rules: rules_key(&self.rules),
        };
        // Best effort: without a db the index just does not survive a restart.
        let _ = save_full(&self.db, &table, &self.roots, &stamp);
        // What changed since the table it replaces (not the first walk, nor one after
        // the rules changed: that is the index changing, not the files).
        let changes = if sh.ready.load(Relaxed) && self.rules_ok {
            Table::diff(&sh.table.read(), &table)
        } else {
            Vec::new()
        };
        table.log = Some(Vec::new());
        *sh.table.write() = table;
        self.rules_ok = true;
        self.journal(changes, &[]);
        sh.missed.store(false, Relaxed);
        sh.scans.fetch_add(1, Relaxed);
        sh.ready.store(true, Relaxed);
        sh.swapped.store(true, Relaxed);
    }

    /// Lists again only the folders whose modified time differs from the one stored
    /// when they were last listed (an entry added, removed or renamed in a folder
    /// moves its time); new folders are walked whole. A file that changed in place
    /// (same folder time) keeps its old size and date until a full rescan.
    fn fast_rescan(&mut self) {
        let sh = self.shared.clone();
        let folders: Vec<(u32, PathBuf, i64)> = {
            let t = sh.table.read();
            (0..t.entries.len() as u32)
                .filter(|&s| {
                    let e = t.entries[s as usize];
                    e.live && e.meta.is_dir
                })
                .filter_map(|s| Some((s, t.path_of(s)?, t.entries[s as usize].meta.mtime)))
                .collect()
        };
        struct Changed {
            slot: u32,
            meta: Meta,
            kids: Vec<(String, PathBuf, Meta)>,
        }
        let mut changed = Vec::new();
        for (slot, path, mtime) in folders {
            if sh.stop.load(Relaxed) {
                return;
            }
            // Gone: its parent's listing drops it.
            let Ok(md) = std::fs::metadata(&path) else {
                continue;
            };
            let meta = meta_of(&md, &path);
            if !meta.is_dir || (meta.mtime == mtime && mtime != NO_TIME) {
                continue;
            }
            changed.push(Changed {
                slot,
                meta,
                kids: listing(&path, &self.rules),
            });
        }
        let fresh: Vec<PathBuf> = {
            let t = sh.table.read();
            changed
                .iter()
                .flat_map(|c| {
                    c.kids
                        .iter()
                        .filter(|(name, _, m)| m.is_dir && t.child(c.slot, name).is_none())
                        .map(|(_, path, _)| path.clone())
                })
                .collect()
        };
        let below: Vec<Vec<(PathBuf, Meta)>> =
            fresh.iter().map(|d| subtree(d, &self.rules)).collect();
        if !self.online() {
            sh.offline.store(true, Relaxed);
            return;
        }
        let changes = {
            let mut t = sh.table.write();
            for c in changed {
                if !t.entries[c.slot as usize].live {
                    continue;
                }
                t.set_meta(c.slot, c.meta);
                let names: HashSet<String> = c
                    .kids
                    .iter()
                    .map(|(n, _, _)| clean(n).into_owned())
                    .collect();
                let gone: Vec<u32> = t
                    .children
                    .get(&c.slot)
                    .map(|k| {
                        k.iter()
                            .filter(|(n, _)| !names.contains(*n))
                            .map(|(_, s)| *s)
                            .collect()
                    })
                    .unwrap_or_default();
                for s in gone {
                    t.remove(s);
                }
                for (name, _, m) in c.kids {
                    t.refresh(c.slot, &name, m);
                }
            }
            for (p, m) in below.into_iter().flatten() {
                let parent = p.parent().and_then(|d| t.lookup(d));
                if let (Some(parent), Some(name)) = (parent, p.file_name()) {
                    t.add(parent, &name.to_string_lossy(), m);
                }
            }
            sh.progress.store(t.live, Relaxed);
            (t.take_dirty(), t.take_log())
        };
        let (changes, log) = changes;
        self.scanned = self.now();
        let _ = save_changes(&self.db, changes, Some(self.scanned));
        self.journal(log, &[]);
        sh.missed.store(false, Relaxed);
        sh.scans.fetch_add(1, Relaxed);
        sh.swapped.store(true, Relaxed);
    }

    /// Reconciles the table with the file system for each path an event named:
    /// present and admitted by the walk rules -> insert or refresh (a new folder
    /// with its whole subtree), else drop. File system reads happen without the
    /// lock; the table is then updated under one short write lock. While a root is
    /// missing nothing is dropped: an unplugged volume is offline, not emptied.
    fn apply(&self, paths: HashSet<PathBuf>, renames: &[(PathBuf, PathBuf)]) {
        if !self.online() {
            self.shared.offline.store(true, Relaxed);
            return;
        }
        enum Seen {
            Gone(PathBuf),
            Here(PathBuf, Meta, Vec<(PathBuf, Meta)>),
        }
        let mut paths: Vec<PathBuf> = paths.into_iter().collect();
        paths.sort_by_key(|p| p.components().count());
        let mut listings: HashMap<PathBuf, HashSet<OsString>> = HashMap::new();
        let mut seen = Vec::new();
        for path in paths {
            let known = {
                let t = self.shared.table.read();
                match t.root_of(&path) {
                    Some((_, rel)) if !rel.as_os_str().is_empty() => {}
                    _ => continue,
                }
                t.lookup(&path).is_some()
            };
            let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let admitted = path.file_name().is_some_and(|name| {
                listings
                    .entry(parent)
                    .or_insert_with_key(|p| list_admitted(p, &self.rules))
                    .contains(name)
            });
            let md = admitted
                .then(|| std::fs::symlink_metadata(&path).ok())
                .flatten();
            let Some(md) = md else {
                seen.push(Seen::Gone(path));
                continue;
            };
            let meta = meta_of(&md, &path);
            let sub = if meta.is_dir && !known {
                subtree(&path, &self.rules)
            } else {
                Vec::new()
            };
            seen.push(Seen::Here(path, meta, sub));
        }
        if seen.is_empty() {
            return;
        }
        let changes = {
            let mut t = self.shared.table.write();
            for s in seen {
                match s {
                    Seen::Gone(path) => {
                        if let Some(slot) = t.lookup(&path) {
                            t.remove(slot);
                        }
                    }
                    Seen::Here(path, meta, sub) => {
                        for (p, m) in std::iter::once((path, meta)).chain(sub) {
                            let parent = p.parent().and_then(|d| t.lookup(d));
                            if let (Some(parent), Some(name)) = (parent, p.file_name()) {
                                t.refresh(parent, &name.to_string_lossy(), m);
                            }
                        }
                    }
                }
            }
            (t.take_dirty(), t.take_log())
        };
        let (changes, log) = changes;
        if !changes.is_empty() {
            let _ = save_changes(&self.db, changes, None);
        }
        self.journal(log, renames);
    }

    /// Journals what the table logged (renames: as the watcher paired them).
    fn journal(&self, changes: Vec<Change>, renames: &[(PathBuf, PathBuf)]) {
        if changes.is_empty() {
            return;
        }
        let events = journal::events_from(changes, renames, journal::now_ms());
        // The property index follows the same changes (whether the journal is on or not).
        crate::propindex::note_changes(&events);
        match &self.opts.journal {
            Some(j) => {
                let _ = j.record_all(&events);
            }
            None => journal::record_all(events),
        }
    }
}

/// Names in `dir` that the walk rules admit.
fn list_admitted(dir: &Path, rules: &Rules) -> HashSet<OsString> {
    walker(dir, rules, Some(1))
        .flatten()
        .filter(|e| e.depth() == 1)
        .map(|e| e.file_name().to_owned())
        .collect()
}

#[cfg(test)]
#[path = "walkindex_tests.rs"]
mod tests;
