//! A persistent name index built by walking the file system (macOS and Linux, and
//! the last resort on Windows). Names, parents, sizes and dates live in memory for
//! queries and in SQLite (`<cache>/index/walk-<hash of roots>.db`) for restarts.
//! A `notify` watcher keeps it current in 500 ms batches; a full rebuild runs when
//! the db is older than a week or on demand. Same query syntax as the NTFS index
//! (substring, `*`/`?` glob, `regex:`, `folder:`, `in:`) and the same skip rules as
//! [`crate::walk`] (hidden and ignored entries are left out).

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

use crate::{Hit, Query, SearchState, Searcher};

const NONE: u32 = u32::MAX;
const NO_TIME: i64 = i64::MIN;
const BATCH: Duration = Duration::from_millis(500);
const MAX_AGE_SECS: u64 = 7 * 24 * 3600;
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct Meta {
    is_dir: bool,
    size: u64,
    /// Milliseconds since the epoch, `NO_TIME` when unknown.
    mtime: i64,
}

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
    },
};

/// A row as stored in the db: (parent, name, is_dir, size, mtime).
type Row = (i64, String, bool, i64, i64);

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
        }
    }

    fn new(roots: &[PathBuf]) -> Self {
        let mut t = Self::empty();
        let dir = Meta {
            is_dir: true,
            size: 0,
            mtime: NO_TIME,
        };
        for root in roots {
            let slot = t.add(NONE, &root.to_string_lossy(), dir);
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

    /// Adds `name` under `parent`, or updates it when present. Returns its slot.
    fn add(&mut self, parent: u32, name: &str, meta: Meta) -> u32 {
        let name = clean(name);
        let known = self
            .children
            .get(&parent)
            .and_then(|c| c.get(&*name))
            .copied();
        if let Some(slot) = known {
            let e = self.entries[slot as usize];
            if e.meta != meta {
                if e.meta.is_dir && !meta.is_dir {
                    self.remove_children(slot);
                }
                self.entries[slot as usize].meta = meta;
                self.dirty.insert(slot);
            }
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
        slot
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
            let name = c.as_os_str().to_string_lossy();
            cur = *self.children.get(&cur)?.get(&*clean(&name))?;
        }
        Some(cur)
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
        ))
    }

    /// Rows must come in ascending slot order.
    fn from_rows(roots: &[PathBuf], rows: Vec<(u32, Row)>) -> Option<Self> {
        let mut t = Self::empty();
        for (slot, (parent, name, is_dir, size, mtime)) in rows {
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
        if let Some(within) = &m.within {
            let path = self.path_of(slot)?.to_string_lossy().replace('\\', "/");
            if !path.to_lowercase().starts_with(within) {
                return None;
            }
        }
        let quality = match &m.literal {
            Some(lit) if eq_case(name, lit, m.match_case) => 0,
            Some(lit)
                if name
                    .get(..lit.len())
                    .is_some_and(|head| eq_case(head, lit, m.match_case)) =>
            {
                1
            }
            _ => 2,
        };
        Some((quality, slot))
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

fn eq_case(a: &str, b: &str, match_case: bool) -> bool {
    if match_case {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

// ---------------------------------------------------------------------------------
// Query text -> Matcher (mirrors ntfs/pattern.rs, which is Windows-only)

struct Matcher {
    /// Finds candidates over the whole name buffer (multi-line mode).
    scan: Option<Regex>,
    /// Further terms every candidate name must match.
    rest: Vec<Regex>,
    /// The scan term when it is a plain substring: drives exact/prefix ranking.
    literal: Option<String>,
    match_case: bool,
    folders_only: bool,
    /// `in:` path prefix, lowercased with `/` separators.
    within: Option<String>,
}

enum Term {
    Literal(String),
    Pattern(String),
}

fn split_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    terms.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    terms
}

/// Whole-name glob as a regex (a leading or trailing `*` drops that anchor).
fn glob(term: &str) -> String {
    let mut re = String::new();
    if !term.starts_with('*') {
        re.push('^');
    }
    for c in term.trim_matches('*').chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    if !term.ends_with('*') {
        re.push('$');
    }
    re
}

fn strip_ci<'a>(term: &'a str, prefix: &str) -> Option<&'a str> {
    term.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &term[prefix.len()..])
}

fn term_of(text: &str) -> Term {
    if text.contains(['*', '?']) {
        Term::Pattern(glob(text))
    } else {
        Term::Literal(text.to_owned())
    }
}

fn compile(query: &Query) -> anyhow::Result<Matcher> {
    let mut folders_only = query.folders_only;
    let mut within = None;
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
                    terms.push(term_of(rest));
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
                terms.push(term_of(&term));
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
    let build = |t: &Term| -> anyhow::Result<Regex> {
        let src = match t {
            Term::Literal(s) => regex::escape(s),
            Term::Pattern(p) => p.clone(),
        };
        Ok(RegexBuilder::new(&src)
            .multi_line(true)
            .case_insensitive(!query.match_case)
            .build()?)
    };
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
    Ok(Matcher {
        scan,
        rest,
        literal,
        match_case: query.match_case,
        folders_only,
        within,
    })
}

// ---------------------------------------------------------------------------------
// Walking

fn meta_of(md: &std::fs::Metadata, path: &Path) -> Meta {
    // A link counts as what it points at, like `walk` does.
    let followed;
    let md = if md.file_type().is_symlink() {
        followed = std::fs::metadata(path);
        followed.as_ref().unwrap_or(md)
    } else {
        md
    };
    Meta {
        is_dir: md.is_dir(),
        size: if md.is_dir() { 0 } else { md.len() },
        mtime: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(NO_TIME, |d| d.as_millis() as i64),
    }
}

/// Walks every root into a fresh table (the same skip rules as [`crate::walk`]).
fn scan(roots: &[PathBuf], progress: &AtomicUsize, stop: &AtomicBool) -> Table {
    let mut t = Table::new(roots);
    for (root, root_slot) in t.roots.clone() {
        // stack[depth] = slot of the folder at that depth.
        let mut stack = vec![root_slot];
        for entry in ignore::WalkBuilder::new(&root).build().flatten() {
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

// ---------------------------------------------------------------------------------
// SQLite

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files(
    id INTEGER PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL,
    is_dir INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);";

fn save_full(path: &Path, t: &Table, roots: &[PathBuf], built: u64) -> anyhow::Result<()> {
    let tmp = sibling(path, ".tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut conn = Connection::open(&tmp)?;
        conn.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;")?;
        conn.execute_batch(SCHEMA)?;
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO files VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            for slot in 0..t.entries.len() as u32 {
                if let Some((parent, name, is_dir, size, mtime)) = t.row(slot) {
                    insert.execute(params![slot, parent, name, is_dir, size, mtime])?;
                }
            }
        }
        tx.execute(
            "INSERT INTO meta VALUES ('built', ?1), ('roots', ?2)",
            params![built.to_string(), roots_key(roots)],
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

fn save_changes(path: &Path, changes: Vec<(u32, Option<Row>)>) -> anyhow::Result<()> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let tx = conn.transaction()?;
    {
        let mut delete = tx.prepare("DELETE FROM files WHERE id = ?1")?;
        let mut upsert =
            tx.prepare("INSERT OR REPLACE INTO files VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
        for (slot, row) in changes {
            match row {
                Some((parent, name, is_dir, size, mtime)) => {
                    upsert.execute(params![slot, parent, name, is_dir, size, mtime])?;
                }
                None => {
                    delete.execute([slot])?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// The saved table and when it was built; None when absent, for other roots, or bad.
fn load(path: &Path, roots: &[PathBuf]) -> Option<(Table, u64)> {
    if !path.is_file() {
        return None;
    }
    let conn = Connection::open(path).ok()?;
    let meta = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
    };
    if meta("roots")? != roots_key(roots) {
        return None;
    }
    let built = meta("built")?.parse().ok()?;
    let mut stmt = conn
        .prepare("SELECT id, parent, name, is_dir, size, mtime FROM files ORDER BY id")
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?),
            ))
        })
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some((Table::from_rows(roots, rows)?, built))
}

// ---------------------------------------------------------------------------------
// The searcher and its worker

enum Cmd {
    Paths(Vec<PathBuf>),
    Rebuild,
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
    stop: AtomicBool,
}

/// A persistent, watched name index over one or more root folders.
pub struct WalkIndexSearcher {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Cmd>,
}

impl WalkIndexSearcher {
    /// Opens (or starts building) the index of `roots` with its db in `dir`. Returns
    /// at once; the saved db loads, or the walk runs, on a background thread.
    pub fn open(dir: PathBuf, roots: Vec<PathBuf>) -> anyhow::Result<Self> {
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
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            shared: shared.clone(),
            db,
            roots,
            built: 0,
        };
        let events = tx.clone();
        std::thread::Builder::new()
            .name("keel-walk-index".into())
            .spawn(move || worker.run(rx, events))?;
        Ok(Self { shared, tx })
    }

    /// Walks everything again in the background; queries keep using the old index.
    pub fn rebuild(&self) {
        let _ = self.tx.send(Cmd::Rebuild);
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
}

impl Drop for WalkIndexSearcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }
}

/// Absolute, resolved (macOS reports `/private/var/...` for `/var/...`), sorted, and
/// without roots that sit inside another root.
fn normalize_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|r| {
            let abs = std::path::absolute(&r).ok()?;
            #[cfg(windows)]
            return Some(abs);
            #[cfg(not(windows))]
            return Some(std::fs::canonicalize(&abs).unwrap_or(abs));
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
                    modified: (m.mtime != NO_TIME)
                        .then(|| UNIX_EPOCH + Duration::from_millis(m.mtime as u64)),
                }
            })
            .collect())
    }

    fn available(&self) -> bool {
        self.shared.ready.load(Relaxed)
    }

    fn status(&self) -> Option<String> {
        if self.shared.building.load(Relaxed) {
            Some(format!(
                "indexing {} files\u{2026}",
                self.shared.progress.load(Relaxed)
            ))
        } else if self.shared.unwatched.load(Relaxed) {
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
}

struct Worker {
    shared: Arc<Shared>,
    db: PathBuf,
    roots: Vec<PathBuf>,
    /// Unix seconds of the last full walk.
    built: u64,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Cmd>, events: mpsc::Sender<Cmd>) {
        let sh = self.shared.clone();
        if let Some((table, built)) = load(&self.db, &self.roots) {
            sh.progress.store(table.live, Relaxed);
            *sh.table.write() = table;
            self.built = built;
            sh.ready.store(true, Relaxed);
            sh.swapped.store(true, Relaxed);
        }
        // Started before the first walk so changes made meanwhile queue up.
        let _watcher = self.watch(events);
        if !sh.ready.load(Relaxed) || self.stale() {
            self.rebuild();
        } else {
            sh.building.store(false, Relaxed);
        }
        let mut pending: HashSet<PathBuf> = HashSet::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(3600)) {
                Ok(Cmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.stale() {
                        self.rebuild();
                    }
                }
                Ok(Cmd::Rebuild) => self.rebuild(),
                Ok(Cmd::Paths(paths)) => {
                    pending.extend(paths);
                    let (mut rescan, mut stop) = (false, false);
                    let deadline = Instant::now() + BATCH;
                    loop {
                        let left = deadline.saturating_duration_since(Instant::now());
                        match rx.recv_timeout(left) {
                            Ok(Cmd::Paths(p)) => pending.extend(p),
                            Ok(Cmd::Rebuild) => rescan = true,
                            Ok(Cmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                                stop = true;
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                        }
                    }
                    if stop {
                        break;
                    }
                    if rescan {
                        pending.clear();
                        self.rebuild();
                    } else {
                        self.apply(std::mem::take(&mut pending));
                    }
                }
            }
        }
    }

    fn stale(&self) -> bool {
        now_secs().saturating_sub(self.built) > MAX_AGE_SECS
    }

    fn watch(&self, tx: mpsc::Sender<Cmd>) -> Option<notify::RecommendedWatcher> {
        use notify::{EventKind, RecursiveMode, Watcher};
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(ev) if ev.need_rescan() => {
                    let _ = tx.send(Cmd::Rebuild);
                }
                Ok(ev) if !matches!(ev.kind, EventKind::Access(_)) => {
                    let _ = tx.send(Cmd::Paths(ev.paths));
                }
                Ok(_) => {}
                // Lost events: walk again.
                Err(_) => {
                    let _ = tx.send(Cmd::Rebuild);
                }
            })
            .ok();
        let watched = watcher.as_mut().is_some_and(|w| {
            self.roots
                .iter()
                .all(|r| w.watch(r, RecursiveMode::Recursive).is_ok())
        });
        self.shared.unwatched.store(!watched, Relaxed);
        watcher
    }

    fn rebuild(&mut self) {
        let sh = &self.shared;
        sh.building.store(true, Relaxed);
        sh.progress.store(0, Relaxed);
        let table = scan(&self.roots, &sh.progress, &sh.stop);
        if sh.stop.load(Relaxed) {
            return;
        }
        self.built = now_secs();
        // Best effort: without a db the index just does not survive a restart.
        let _ = save_full(&self.db, &table, &self.roots, self.built);
        *sh.table.write() = table;
        sh.ready.store(true, Relaxed);
        sh.swapped.store(true, Relaxed);
        sh.building.store(false, Relaxed);
    }

    /// Reconciles the table with the file system for each path an event named:
    /// present and admitted by the walk rules -> insert or refresh (a new folder
    /// with its whole subtree), else drop. File system reads happen without the
    /// lock; the table is then updated under one short write lock.
    fn apply(&self, paths: HashSet<PathBuf>) {
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
                    .or_insert_with_key(|p| list_admitted(p))
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
                subtree(&path)
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
                                t.add(parent, &name.to_string_lossy(), m);
                            }
                        }
                    }
                }
            }
            t.take_dirty()
        };
        if !changes.is_empty() {
            let _ = save_changes(&self.db, changes);
        }
    }
}

/// Names in `dir` that the walk rules admit (hidden and ignored ones are not).
fn list_admitted(dir: &Path) -> HashSet<OsString> {
    ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .build()
        .flatten()
        .filter(|e| e.depth() == 1)
        .map(|e| e.file_name().to_owned())
        .collect()
}

/// Everything below `dir` that the walk rules admit, parents before children.
fn subtree(dir: &Path) -> Vec<(PathBuf, Meta)> {
    ignore::WalkBuilder::new(dir)
        .build()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder_index;
    use std::sync::atomic::AtomicU32;

    struct Dirs {
        root: PathBuf,
        idx: PathBuf,
    }

    impl Dirs {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let base = std::env::temp_dir().join(format!(
                "keel-walkidx-{}-{}",
                std::process::id(),
                N.fetch_add(1, Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&base);
            let (root, idx) = (base.join("root"), base.join("idx"));
            std::fs::create_dir_all(&root).unwrap();
            Self { root, idx }
        }

        fn file(&self, rel: &str, body: &[u8]) {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }

        fn open(&self) -> WalkIndexSearcher {
            let s = WalkIndexSearcher::open(self.idx.clone(), vec![self.root.clone()]).unwrap();
            assert!(s.wait_ready(Duration::from_secs(60)));
            s
        }

        fn sample() -> Self {
            let d = Self::new();
            d.file("docs/Report.PDF", b"12345");
            d.file("docs/report notes.txt", b"x");
            d.file("docs/old/report.pdf", b"x");
            d.file("src/main.rs", b"fn main(){}");
            d.file(".hidden/report.secret", b"x");
            d
        }
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
        }
    }

    fn q(text: &str) -> Query {
        Query {
            text: text.into(),
            ..Query::default()
        }
    }

    fn names(s: &WalkIndexSearcher, query: &Query) -> Vec<String> {
        s.query(query)
            .unwrap()
            .iter()
            .map(|h| h.path.name().to_owned())
            .collect()
    }

    /// Polls until the query's names equal `want` (the watcher batches for 500 ms).
    fn settle(s: &WalkIndexSearcher, query: &Query, want: &[&str]) {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let got = s.query(query).map(|_| names(s, query)).unwrap_or_default();
            if got == want && s.available() {
                return;
            }
            assert!(Instant::now() < end, "wanted {want:?}, still {got:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn substring_glob_regex_and_filters() {
        let d = Dirs::sample();
        let s = d.open();
        // Exact name first, then prefix, then the rest; hidden entries are skipped.
        assert_eq!(
            names(&s, &q("report")),
            ["report.pdf", "report notes.txt", "Report.PDF"]
        );
        assert_eq!(names(&s, &q("*.pdf")), ["report.pdf", "Report.PDF"]);
        assert_eq!(names(&s, &q("report* pdf")), ["report.pdf", "Report.PDF"]);
        let re = Query {
            regex: true,
            ..q(r"^report\s")
        };
        assert_eq!(names(&s, &re), ["report notes.txt"]);
        let bad = Query {
            regex: true,
            ..q("(")
        };
        assert!(s.query(&bad).is_err());
        let case = Query {
            match_case: true,
            ..q("Report")
        };
        assert_eq!(names(&s, &case), ["Report.PDF"]);
        assert_eq!(names(&s, &q(r"regex:main\.rs$")), ["main.rs"]);
        assert_eq!(names(&s, &q("folder:o")), ["old", "docs"]);
        let within = format!(
            "report in:\"{}\"",
            d.root.join("docs").join("old").display()
        );
        assert_eq!(names(&s, &q(&within)), ["report.pdf"]);
        let capped = Query {
            max: 2,
            ..q("report")
        };
        assert_eq!(names(&s, &capped).len(), 2);
        // Size and date come from the index, no file system call.
        let hit = &s.query(&case).unwrap()[0];
        assert_eq!((hit.size, hit.is_dir), (5, false));
        assert!(hit.modified.is_some());
    }

    #[test]
    fn rename_delete_and_create_show_up_after_the_watcher_batch() {
        let d = Dirs::sample();
        let s = d.open();
        assert!(!s.shared.unwatched.load(Relaxed), "watcher failed to start");
        let docs = d.root.join("docs");
        std::fs::rename(docs.join("report notes.txt"), docs.join("minutes.txt")).unwrap();
        std::fs::remove_file(docs.join("old/report.pdf")).unwrap();
        d.file("fresh/deep/report-new.md", b"x");
        settle(&s, &q("report"), &["Report.PDF", "report-new.md"]);
        settle(&s, &q("minutes"), &["minutes.txt"]);
        // A renamed folder takes its children along.
        std::fs::rename(d.root.join("fresh"), d.root.join("renamed")).unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let hits = s.query(&q("report-new")).unwrap();
            if hits.len() == 1 && hits[0].path.display().contains("renamed") {
                break;
            }
            assert!(Instant::now() < end, "still {:?}", hits.len());
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::remove_dir_all(d.root.join("renamed")).unwrap();
        settle(&s, &q("report-new"), &[]);
        // Hidden files created later stay out.
        d.file(".hidden/more.txt", b"x");
        d.file("late.txt", b"x");
        settle(&s, &q("txt"), &["minutes.txt", "late.txt"]);
    }

    #[test]
    fn restart_loads_the_db_and_rebuild_picks_up_offline_changes() {
        let d = Dirs::sample();
        {
            let s = d.open();
            d.file("watched.txt", b"x");
            settle(&s, &q("watched"), &["watched.txt"]);
            // Let the batch reach the db before closing.
            std::thread::sleep(Duration::from_millis(500));
        }
        std::fs::remove_file(d.root.join("src/main.rs")).unwrap();
        d.file("added-offline.txt", b"x");
        let s = d.open();
        // A fresh db is served as saved; the watcher only sees later changes.
        assert_eq!(names(&s, &q("watched")), ["watched.txt"]);
        assert_eq!(names(&s, &q("main.rs")), ["main.rs"]);
        s.rebuild();
        settle(&s, &q("main.rs"), &[]);
        settle(&s, &q("added-offline"), &["added-offline.txt"]);
        assert!(s.take_ready());
    }

    #[test]
    fn a_db_older_than_a_week_rebuilds_on_open() {
        let d = Dirs::sample();
        drop(d.open());
        let db = db_path(&d.idx, &normalize_roots(vec![d.root.clone()]));
        Connection::open(&db)
            .unwrap()
            .execute("UPDATE meta SET value = '1' WHERE key = 'built'", [])
            .unwrap();
        d.file("after.txt", b"x");
        let s = WalkIndexSearcher::open(d.idx.clone(), vec![d.root.clone()]).unwrap();
        settle(&s, &q("after.txt"), &["after.txt"]);
    }

    #[test]
    fn folder_index_lists_folders_only() {
        let d = Dirs::sample();
        let s = d.open();
        let mut rel: Vec<String> = folder_index(&s)
            .unwrap()
            .iter()
            .map(|f| {
                f.replace('\\', "/")
                    .rsplit("/root/")
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        rel.sort();
        // No files, no hidden folder, not the root itself.
        assert_eq!(rel, ["docs", "docs/old", "src"]);
    }

    #[test]
    fn status_says_indexing_while_building() {
        let d = Dirs::sample();
        let s = d.open();
        assert_eq!(s.state(), SearchState::Ready);
        assert_eq!(s.status(), None);
        s.shared.building.store(true, Relaxed);
        s.shared.progress.store(1234, Relaxed);
        assert_eq!(s.status().as_deref(), Some("indexing 1234 files\u{2026}"));
        s.shared.ready.store(false, Relaxed);
        assert!(s.query(&q("a")).is_err());
        assert_eq!(s.state(), SearchState::Indexing { done: 1234 });
        s.shared.ready.store(true, Relaxed);
        s.shared.building.store(false, Relaxed);
    }

    #[test]
    fn table_remove_reuses_slots_and_round_trips_rows() {
        let mut t = Table::new(&[PathBuf::from("/r")]);
        let dir = Meta {
            is_dir: true,
            size: 0,
            mtime: NO_TIME,
        };
        let file = Meta {
            is_dir: false,
            size: 3,
            mtime: 7,
        };
        let root = t.roots[0].1;
        let a = t.add(root, "a", dir);
        let f = t.add(a, "f.txt", file);
        assert_eq!(
            t.path_of(f).unwrap(),
            PathBuf::from("/r").join("a").join("f.txt")
        );
        t.remove(a);
        assert_eq!((t.live, t.lookup(Path::new("/r/a/f.txt"))), (1, None));
        let b = t.add(root, "b\nb", file);
        assert!(b == a || b == f, "slot reused");
        assert_eq!(t.lookup(Path::new("/r/b b")), Some(b));
        t.compact();
        assert_eq!(t.search(&compile(&q("b")).unwrap(), 10).len(), 1);
        let rows: Vec<_> = (0..t.entries.len() as u32)
            .filter_map(|s| Some((s, t.row(s)?)))
            .collect();
        let back = Table::from_rows(&[PathBuf::from("/r")], rows).unwrap();
        assert_eq!(back.live, t.live);
        assert_eq!(back.lookup(Path::new("/r/b b")), Some(b));
    }

    /// `cargo test -p keel-search --release bench -- --ignored --nocapture`
    /// (`KEEL_BENCH_FILES`, default 100000).
    #[test]
    #[ignore]
    fn bench_synthetic_tree() {
        let n: usize = std::env::var("KEEL_BENCH_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000);
        let d = Dirs::new();
        let start = Instant::now();
        for i in 0..n {
            let dir = d.root.join(format!("d{}/s{}", i / 1000, (i / 50) % 20));
            if i % 50 == 0 {
                std::fs::create_dir_all(&dir).unwrap();
            }
            std::fs::write(dir.join(format!("file{i}-{}.dat", i % 97)), b"x").unwrap();
        }
        println!("created {n} files in {:?}", start.elapsed());
        let start = Instant::now();
        let s = d.open();
        println!(
            "indexed in {:?} ({} entries)",
            start.elapsed(),
            s.shared.table.read().live
        );
        for text in [
            "file4242",
            "*-33.dat",
            "regex:^file9\\d+-1\\.dat$",
            "zzzzz",
            "d7 file1",
            "a",
        ] {
            let mut best = Duration::MAX;
            let mut count = 0;
            for _ in 0..20 {
                let t = Instant::now();
                count = s.query(&q(text)).unwrap().len();
                best = best.min(t.elapsed());
            }
            println!("query {text:?}: {count} hits, best of 20 {best:?}");
        }
        let t = Instant::now();
        let folders = folder_index(&s).unwrap().len();
        println!("folder_index: {folders} folders in {:?}", t.elapsed());
    }
}
