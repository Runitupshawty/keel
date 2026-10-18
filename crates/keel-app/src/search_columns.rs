//! Task 76: the search results' columns and sort (Everything's Results options). The
//! column chooser (right-click a header) picks from Name, Path, Size, Size on disk,
//! Extension, Type, Date modified, created and accessed, Attributes, Folder size and Date
//! last opened; a header click sorts by that column and keeps the previous sort as the
//! secondary and tertiary one. Folders first: always, ascending, descending or never
//! (sort mix: folders and files mixed); sizes and dates can sort descending on the first
//! click. The options are `[views.search]` in the profile; the sort, a bookmark's columns
//! and the Folders sidebar's folder are the tab's (`Tab::results`).
//!
//! Dates created and accessed, attributes and size on disk are not in a search hit: a
//! worker reads them from the disk once per result list (local fixed disks only, at most
//! `MAX_META` results) when a shown column or the sort needs them.

use crate::list_view;
use crate::tab::{SortKey, Tab};
use keel_vfs::{Entry, Kind, VPath};
use parking_lot::Mutex;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::SystemTime;

/// Results whose dates, attributes and size on disk are read at most.
pub const MAX_META: usize = 100_000;
/// Sort keys kept: the primary, secondary and tertiary sort.
pub const SORT_KEYS: usize = 3;

/// A column of the search results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultCol {
    /// Always shown, first.
    Name,
    /// The folder the result is in.
    Path,
    Size,
    SizeOnDisk,
    Extension,
    /// Kind in words ("PDF document").
    Type,
    Modified,
    Created,
    Accessed,
    Attributes,
    /// A folder's total (the library's or a walked one); empty for files.
    FolderSize,
    /// When Keel last opened the file (the library's recents).
    Opened,
}

impl ResultCol {
    /// The columns of the chooser, in the order they are drawn.
    pub const CHOOSABLE: [ResultCol; 11] = [
        ResultCol::Path,
        ResultCol::Size,
        ResultCol::SizeOnDisk,
        ResultCol::Extension,
        ResultCol::Type,
        ResultCol::Modified,
        ResultCol::Created,
        ResultCol::Accessed,
        ResultCol::Attributes,
        ResultCol::FolderSize,
        ResultCol::Opened,
    ];

    pub fn label(self) -> String {
        crate::i18n::tr(match self {
            ResultCol::Name => "search_results.col_name",
            ResultCol::Path => "search_results.col_path",
            ResultCol::Size => "search_results.col_size",
            ResultCol::SizeOnDisk => "search_results.col_size_on_disk",
            ResultCol::Extension => "search_results.col_extension",
            ResultCol::Type => "search_results.col_type",
            ResultCol::Modified => "search_results.col_modified",
            ResultCol::Created => "search_results.col_created",
            ResultCol::Accessed => "search_results.col_accessed",
            ResultCol::Attributes => "search_results.col_attributes",
            ResultCol::FolderSize => "search_results.col_folder_size",
            ResultCol::Opened => "search_results.col_opened",
        })
        .to_owned()
    }

    /// The word a bookmark's sort and columns use (`size:desc,name`).
    pub fn word(self) -> &'static str {
        match self {
            ResultCol::Name => "name",
            ResultCol::Path => "path",
            ResultCol::Size => "size",
            ResultCol::SizeOnDisk => "size_on_disk",
            ResultCol::Extension => "extension",
            ResultCol::Type => "type",
            ResultCol::Modified => "modified",
            ResultCol::Created => "created",
            ResultCol::Accessed => "accessed",
            ResultCol::Attributes => "attributes",
            ResultCol::FolderSize => "folder_size",
            ResultCol::Opened => "opened",
        }
    }

    /// A bookmark's word, Everything's names included (`date_modified`, `ext`, ...).
    pub fn parse(word: &str) -> Option<ResultCol> {
        Some(match word.trim().to_ascii_lowercase().as_str() {
            "name" => ResultCol::Name,
            "path" | "full_path" | "folder" => ResultCol::Path,
            "size" => ResultCol::Size,
            "size_on_disk" | "allocated" | "allocated_size" => ResultCol::SizeOnDisk,
            "extension" | "ext" => ResultCol::Extension,
            "type" | "kind" => ResultCol::Type,
            "modified" | "date_modified" | "dm" => ResultCol::Modified,
            "created" | "date_created" | "dc" => ResultCol::Created,
            "accessed" | "date_accessed" | "da" => ResultCol::Accessed,
            "attributes" | "attrib" => ResultCol::Attributes,
            "folder_size" => ResultCol::FolderSize,
            "opened" | "date_run" | "date_opened" => ResultCol::Opened,
            _ => return None,
        })
    }

    /// The tab's own sort key for this column, when it has one.
    pub fn sort_key(self) -> Option<SortKey> {
        match self {
            ResultCol::Name => Some(SortKey::Name),
            ResultCol::Size => Some(SortKey::Size),
            ResultCol::Modified => Some(SortKey::Modified),
            ResultCol::Extension => Some(SortKey::Ext),
            _ => None,
        }
    }

    pub fn of_sort_key(key: SortKey) -> ResultCol {
        match key {
            SortKey::Name => ResultCol::Name,
            SortKey::Size => ResultCol::Size,
            SortKey::Modified => ResultCol::Modified,
            SortKey::Ext => ResultCol::Extension,
        }
    }

    /// A size or a date: sorted biggest or newest first on the first click when
    /// [`ResultsOptions::descending_first`] is on.
    pub fn size_or_date(self) -> bool {
        matches!(
            self,
            ResultCol::Size
                | ResultCol::SizeOnDisk
                | ResultCol::FolderSize
                | ResultCol::Modified
                | ResultCol::Created
                | ResultCol::Accessed
                | ResultCol::Opened
        )
    }

    /// Read from the disk by the meta worker ([`MetaCache`]).
    pub fn needs_meta(self) -> bool {
        matches!(
            self,
            ResultCol::SizeOnDisk
                | ResultCol::Created
                | ResultCol::Accessed
                | ResultCol::Attributes
        )
    }
}

/// Where folders go in the results (Everything's Folders first).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum FoldersFirst {
    /// Before the files whatever the sort direction.
    #[default]
    Always,
    /// Before the files in an ascending sort, after them in a descending one.
    Ascending,
    /// Before the files in a descending sort, after them in an ascending one.
    Descending,
    /// Sort mix: folders and files mixed, in the sort order alone.
    Never,
}

impl FoldersFirst {
    pub const ALL: [FoldersFirst; 4] = [
        FoldersFirst::Always,
        FoldersFirst::Ascending,
        FoldersFirst::Descending,
        FoldersFirst::Never,
    ];

    pub fn label(self) -> String {
        crate::i18n::tr(match self {
            FoldersFirst::Always => "search_results.folders_always",
            FoldersFirst::Ascending => "search_results.folders_ascending",
            FoldersFirst::Descending => "search_results.folders_descending",
            FoldersFirst::Never => "search_results.folders_never",
        })
        .to_owned()
    }

    /// Folders before files (`Some(true)`), after them (`Some(false)`) or mixed (`None`)
    /// when the primary sort is ascending (`asc`) or not.
    pub fn folders_before(self, asc: bool) -> Option<bool> {
        match self {
            FoldersFirst::Always => Some(true),
            FoldersFirst::Ascending => Some(asc),
            FoldersFirst::Descending => Some(!asc),
            FoldersFirst::Never => None,
        }
    }
}

/// `[views.search]` in the profile: the results' columns and sort options and the
/// sidebars.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ResultsOptions {
    /// Shown after Name, in this order (a bookmark with columns of its own overrides it).
    pub columns: Vec<ResultCol>,
    pub folders_first: FoldersFirst,
    /// Sizes and dates sort biggest or newest first on the first click.
    pub descending_first: bool,
    /// The Filters sidebar (Everything 1.5).
    pub filters_sidebar: bool,
    /// The Folders sidebar: narrows the results to the folder picked there.
    pub folders_sidebar: bool,
}

impl Default for ResultsOptions {
    fn default() -> Self {
        Self {
            columns: vec![ResultCol::Path, ResultCol::Size, ResultCol::Modified],
            folders_first: FoldersFirst::Always,
            descending_first: false,
            filters_sidebar: false,
            folders_sidebar: false,
        }
    }
}

impl ResultsOptions {
    pub fn shows(&self, c: ResultCol) -> bool {
        self.columns.contains(&c)
    }

    /// Shows or hides `c`; a column shown again goes where the chooser lists it.
    pub fn set(&mut self, c: ResultCol, on: bool) {
        self.columns.retain(|x| *x != c && *x != ResultCol::Name);
        if on {
            let rank = |c: &ResultCol| ResultCol::CHOOSABLE.iter().position(|x| x == c);
            let at = (self.columns.iter())
                .position(|x| rank(x) > rank(&c))
                .unwrap_or(self.columns.len());
            self.columns.insert(at, c);
        }
    }
}

/// A sort: (column, ascending), the primary first; at most [`SORT_KEYS`].
pub type Sort = Vec<(ResultCol, bool)>;

/// A header click on `col`: the primary column again turns its direction; another column
/// becomes the primary sort (sizes and dates descending first when `descending_first`)
/// and the earlier keys follow it as the secondary and tertiary sort.
pub fn click(sort: &[(ResultCol, bool)], col: ResultCol, descending_first: bool) -> Sort {
    match sort.first() {
        Some(&(c, asc)) if c == col => {
            let mut out = sort.to_vec();
            out[0] = (c, !asc);
            out
        }
        _ => {
            let asc = !(descending_first && col.size_or_date());
            let mut out = vec![(col, asc)];
            out.extend(sort.iter().copied().filter(|(c, _)| *c != col));
            out.truncate(SORT_KEYS);
            out
        }
    }
}

/// A sort as a bookmark stores it (`size:desc,name`); "" for Name ascending alone.
pub fn sort_text(sort: &[(ResultCol, bool)]) -> String {
    if sort == [(ResultCol::Name, true)] {
        return String::new();
    }
    (sort.iter())
        .map(|&(c, asc)| match asc {
            true => c.word().to_owned(),
            false => format!("{}:desc", c.word()),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A bookmark's sort: every key Keel knows, at most [`SORT_KEYS`], each column once.
pub fn parse_sort(text: &str) -> Sort {
    let mut out: Sort = Vec::new();
    for part in text.split(',') {
        let (col, dir) = part.split_once(':').unwrap_or((part, "asc"));
        let Some(col) = ResultCol::parse(col) else {
            continue;
        };
        if out.iter().all(|(c, _)| *c != col) && out.len() < SORT_KEYS {
            out.push((col, dir.trim() != "desc"));
        }
    }
    out
}

/// A bookmark's columns (Name left out: it is always first); None when it names none
/// Keel knows (the profile's columns then).
pub fn parse_columns(words: &[String]) -> Option<Vec<ResultCol>> {
    let mut out = Vec::new();
    for c in words.iter().filter_map(|w| ResultCol::parse(w)) {
        if c != ResultCol::Name && !out.contains(&c) {
            out.push(c);
        }
    }
    (!out.is_empty()).then_some(out)
}

pub fn column_words(cols: &[ResultCol]) -> Vec<String> {
    cols.iter().map(|c| c.word().to_owned()).collect()
}

// ---------------------------------------------------------------------------------------
// Per tab
// ---------------------------------------------------------------------------------------

/// What one hit has on disk beyond the search's size and date.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResultMeta {
    pub created: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    /// Windows: the attribute bits; elsewhere the mode bits.
    pub attributes: Option<u32>,
    pub on_disk: Option<u64>,
}

type MetaSlot = Arc<Mutex<Option<HashMap<String, ResultMeta>>>>;

/// [`ResultMeta`] of a search tab's results by entry name, read by a worker once per
/// result list.
#[derive(Default)]
pub struct MetaCache {
    pub by_name: HashMap<String, ResultMeta>,
    /// Bumped when `by_name` changes.
    pub gen: u64,
    done: Option<u64>,
    job: Option<(u64, MetaSlot)>,
}

/// A search current results (`search_view`): what the tab searched before.
pub struct Refine {
    /// The tab's file list before (a file list tab refined), else None.
    pub base: Option<Arc<keel_search::FileListSearcher>>,
    pub query: String,
}

/// `Tab::results`: a search tab's sort, columns, Folders sidebar folder and metadata.
#[derive(Default)]
pub struct TabResults {
    /// Empty until the first sort (the tab's own `sort` then).
    pub sort: Sort,
    /// The tab's `sort` when `sort` last took it in.
    synced: Option<(SortKey, bool)>,
    /// A bookmark's own columns.
    pub columns: Option<Vec<ResultCol>>,
    /// The Folders sidebar: only results under this folder.
    pub folder: Option<VPath>,
    /// Folders of the Folders sidebar opened or closed by hand (the top ones start open,
    /// the others closed).
    pub toggled: HashSet<VPath>,
    /// Search current results is on.
    pub refine: Option<Refine>,
    pub meta: MetaCache,
    /// The Folders sidebar's tree and the result list it was built for.
    pub tree: Option<(u64, Arc<crate::search_view::FolderTree>)>,
    order_key: Option<u64>,
}

impl TabResults {
    /// The tab's sort: its `sort` with the earlier keys after it.
    pub fn sort(&self, tab_sort: (SortKey, bool)) -> Sort {
        let primary = (ResultCol::of_sort_key(tab_sort.0), tab_sort.1);
        if self.synced == Some(tab_sort) && !self.sort.is_empty() {
            return self.sort.clone();
        }
        let mut out = vec![primary];
        out.extend(self.sort.iter().copied().filter(|(c, _)| *c != primary.0));
        out.truncate(SORT_KEYS);
        out
    }

    /// Takes `sort` (a header click or a bookmark); `tab_sort` is the tab's sort after it.
    pub fn set_sort(&mut self, sort: Sort, tab_sort: (SortKey, bool)) {
        self.sort = sort;
        self.synced = Some(tab_sort);
    }
}

/// The columns a search tab shows after Name.
pub fn columns_of<'a>(tab: &'a Tab, opts: &'a ResultsOptions) -> &'a [ResultCol] {
    tab.results.columns.as_deref().unwrap_or(&opts.columns)
}

/// A header click in a search tab.
pub fn header_click(tab: &mut Tab, col: ResultCol, opts: &ResultsOptions) {
    let sort = click(&tab.results.sort(tab.sort), col, opts.descending_first);
    if let Some(key) = sort[0].0.sort_key() {
        tab.sort = (key, sort[0].1);
    }
    let tab_sort = tab.sort;
    tab.results.set_sort(sort, tab_sort);
}

/// Windows attribute letters in the order shown (`RHSDA`, then the rarer ones).
const ATTR_LETTERS: [(u32, char); 12] = [
    (0x1, 'R'),
    (0x2, 'H'),
    (0x4, 'S'),
    (0x10, 'D'),
    (0x20, 'A'),
    (0x800, 'C'),
    (0x4000, 'E'),
    (0x400, 'L'),
    (0x1000, 'O'),
    (0x200, 'P'),
    (0x100, 'T'),
    (0x2000, 'I'),
];

/// Attributes as text: Windows' letters (`RHA`); elsewhere `rwxr-xr-x` (with `d` first for
/// a folder).
pub fn attributes_text(bits: u32, dir: bool) -> String {
    if cfg!(windows) {
        return (ATTR_LETTERS.iter())
            .filter(|(b, _)| bits & b != 0)
            .map(|(_, c)| *c)
            .collect();
    }
    let mut s = String::with_capacity(10);
    s.push(if dir { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        let m = (bits >> shift) & 7;
        s.push(if m & 4 != 0 { 'r' } else { '-' });
        s.push(if m & 2 != 0 { 'w' } else { '-' });
        s.push(if m & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// What a cell of a disk-read column shows ("" while unknown).
pub fn meta_text(col: ResultCol, e: &Entry, meta: &MetaCache, relative: bool) -> String {
    let m = meta.by_name.get(&e.name);
    let now = chrono::Local::now();
    match col {
        ResultCol::Created => list_view::date_label(m.and_then(|m| m.created), relative, now),
        ResultCol::Accessed => list_view::date_label(m.and_then(|m| m.accessed), relative, now),
        ResultCol::Attributes => (m.and_then(|m| m.attributes))
            .map(|a| attributes_text(a, e.kind == Kind::Dir))
            .unwrap_or_default(),
        ResultCol::SizeOnDisk => (m.and_then(|m| m.on_disk))
            .filter(|_| e.kind != Kind::Dir)
            .map(crate::view_details::size_text_of)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn read_meta(path: &std::path::Path) -> Option<ResultMeta> {
    let md = std::fs::metadata(path).ok()?;
    #[cfg(windows)]
    let (attributes, on_disk) = {
        use std::os::windows::fs::MetadataExt;
        let on_disk = (!md.is_dir())
            .then(|| keel_vfs::compress::on_disk(path).ok())
            .flatten();
        (Some(md.file_attributes()), on_disk)
    };
    #[cfg(unix)]
    let (attributes, on_disk) = {
        use std::os::unix::fs::MetadataExt;
        (
            Some(md.mode() & 0o7777),
            Some(md.blocks().saturating_mul(512)),
        )
    };
    #[cfg(not(any(windows, unix)))]
    let (attributes, on_disk) = (None, None);
    Some(ResultMeta {
        created: md.created().ok(),
        accessed: md.accessed().ok(),
        attributes,
        on_disk,
    })
}

/// Fills `tab.results.meta` for the current results (starts or polls the worker).
pub fn want_meta(tab: &mut Tab, ctx: &egui::Context) {
    let key = tab.generation();
    let meta = &mut tab.results.meta;
    if meta.done == Some(key) {
        return;
    }
    if let Some((k, slot)) = &meta.job {
        if *k == key {
            let found = slot.lock().take();
            if let Some(found) = found {
                meta.by_name = found;
                meta.gen += 1;
                meta.done = Some(key);
                meta.job = None;
            }
            return;
        }
    }
    meta.job = None;
    let paths: Vec<(String, std::path::PathBuf)> = (tab.entries().iter())
        .take(MAX_META)
        .filter_map(|e| Some((e.name.clone(), e.path.to_local_path()?)))
        .collect();
    let meta = &mut tab.results.meta;
    if paths.is_empty() {
        meta.by_name.clear();
        meta.gen += 1;
        meta.done = Some(key);
        return;
    }
    let slot: MetaSlot = Default::default();
    let out = slot.clone();
    let ctx = ctx.clone();
    let started = crate::worker::spawn("keel-result-meta", move || {
        // Network and removable drives are left alone: each file there is a round trip.
        let mut fixed: HashMap<std::path::PathBuf, bool> = HashMap::new();
        let mut found = HashMap::new();
        for (name, p) in paths {
            // Windows asks the drive once per letter; elsewhere the check is by path.
            let ok = match cfg!(windows) {
                true => {
                    let root: std::path::PathBuf = p.components().take(1).collect();
                    *(fixed.entry(root.clone())).or_insert_with(|| keel_vfs::is_fixed_disk(&root))
                }
                false => keel_vfs::is_fixed_disk(&p),
            };
            if !ok {
                continue;
            }
            if let Some(m) = read_meta(&p) {
                found.insert(name, m);
            }
        }
        *out.lock() = Some(found);
        ctx.request_repaint();
    });
    if started {
        meta.job = Some((key, slot));
    } else {
        meta.done = Some(key);
    }
}

// ---------------------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------------------

/// One sort value of one result.
#[derive(Clone, Debug, PartialEq)]
pub enum SortValue {
    Text(String),
    Num(Option<u64>),
    Time(Option<SystemTime>),
}

impl SortValue {
    fn unknown(&self) -> bool {
        matches!(self, SortValue::Num(None) | SortValue::Time(None))
    }

    fn cmp(&self, o: &SortValue) -> Ordering {
        match (self, o) {
            (SortValue::Text(a), SortValue::Text(b)) => natord::compare(a, b),
            (SortValue::Num(a), SortValue::Num(b)) => a.cmp(b),
            (SortValue::Time(a), SortValue::Time(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

/// The value `col` sorts `e` by. `bytes` is a folder's known total, `opened` when Keel
/// last opened it.
pub fn value(
    col: ResultCol,
    e: &Entry,
    meta: &MetaCache,
    bytes: &dyn Fn(&Entry) -> Option<u64>,
    opened: &dyn Fn(&Entry) -> Option<SystemTime>,
) -> SortValue {
    let m = meta.by_name.get(&e.name);
    let dir = e.kind == Kind::Dir;
    match col {
        ResultCol::Name => SortValue::Text(e.path.name().to_lowercase()),
        ResultCol::Path => SortValue::Text(e.path.display().to_lowercase()),
        ResultCol::Extension => SortValue::Text(e.ext.clone()),
        ResultCol::Type => SortValue::Text(list_view::kind_text(e).to_lowercase()),
        ResultCol::Attributes => SortValue::Text(
            (m.and_then(|m| m.attributes))
                .map(|a| attributes_text(a, dir))
                .unwrap_or_default(),
        ),
        ResultCol::Size => SortValue::Num(bytes(e)),
        ResultCol::FolderSize => SortValue::Num(bytes(e).filter(|_| dir)),
        ResultCol::SizeOnDisk => SortValue::Num(m.and_then(|m| m.on_disk).filter(|_| !dir)),
        ResultCol::Modified => SortValue::Time(e.modified),
        ResultCol::Created => SortValue::Time(m.and_then(|m| m.created)),
        ResultCol::Accessed => SortValue::Time(m.and_then(|m| m.accessed)),
        ResultCol::Opened => SortValue::Time(opened(e)),
    }
}

/// The order of `entries` (indices) for `sort`, folders placed by `folders`, keeping only
/// the entries `keep` passes. Unknown sizes and dates go last in either direction; ties
/// fall back to the name, then the backend's order.
pub fn order(
    entries: &[Entry],
    sort: &[(ResultCol, bool)],
    folders: FoldersFirst,
    value: &dyn Fn(ResultCol, &Entry) -> SortValue,
    keep: &dyn Fn(&Entry) -> bool,
) -> Vec<usize> {
    let mut rows: Vec<(usize, Vec<SortValue>)> = (entries.iter().enumerate())
        .filter(|(_, e)| keep(e))
        .map(|(i, e)| {
            let mut vals: Vec<SortValue> = sort.iter().map(|&(c, _)| value(c, e)).collect();
            vals.push(SortValue::Text(e.path.name().to_lowercase()));
            (i, vals)
        })
        .collect();
    let before = folders.folders_before(sort.first().is_none_or(|s| s.1));
    rows.sort_by(|(ai, av), (bi, bv)| {
        let (a, b) = (&entries[*ai], &entries[*bi]);
        let place = match before {
            Some(first) => {
                let (ad, bd) = (a.kind == Kind::Dir, b.kind == Kind::Dir);
                match first {
                    true => bd.cmp(&ad),
                    false => ad.cmp(&bd),
                }
            }
            None => Ordering::Equal,
        };
        let mut ord = place;
        for (k, (x, y)) in av.iter().zip(bv).enumerate() {
            if ord != Ordering::Equal {
                break;
            }
            let asc = sort.get(k).is_none_or(|s| s.1);
            ord = x.unknown().cmp(&y.unknown()).then_with(|| match asc {
                true => x.cmp(y),
                false => x.cmp(y).reverse(),
            });
        }
        ord.then(ai.cmp(bi))
    });
    rows.into_iter().map(|(i, _)| i).collect()
}

/// Whether `path` is `dir` or inside it, by whole names (any case on Windows).
pub fn is_within(path: &VPath, dir: &VPath) -> bool {
    if path.scheme != dir.scheme || path.authority != dir.authority {
        return false;
    }
    let (p, d) = (
        path.path.trim_end_matches('/'),
        dir.path.trim_end_matches('/'),
    );
    let eq = |a: &str, b: &str| match cfg!(windows) {
        true => a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase(),
        false => a == b,
    };
    if d.is_empty() {
        return true;
    }
    match p.get(..d.len()) {
        Some(head) => eq(head, d) && (p.len() == d.len() || p[d.len()..].starts_with('/')),
        None => false,
    }
}

/// Before a search tab draws: reads the disk-read columns when shown or sorted by,
/// and gives the tab its row order for the sort, Folders first and the Folders sidebar.
pub fn prepare(ctx: &egui::Context, tab: &mut Tab, cx: &crate::pane::ViewCx) {
    let opts = &cx.views.search;
    let sort = tab.results.sort(tab.sort);
    let cols = columns_of(tab, opts).to_vec();
    let meta_wanted = (cols.iter().chain(sort.iter().map(|(c, _)| c))).any(|c| c.needs_meta());
    if meta_wanted {
        want_meta(tab, ctx);
    }
    let opened_wanted =
        cols.contains(&ResultCol::Opened) || sort.iter().any(|(c, _)| *c == ResultCol::Opened);
    let opened = match opened_wanted {
        true => list_view::with_opened(ctx, |o| {
            o.want(cx.library, ctx);
            o.by_path.clone()
        }),
        false => Default::default(),
    };
    let (lib, walked) = (cx.library, cx.sizes);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        tab.generation(),
        &sort,
        opts.folders_first,
        &tab.results.folder,
    )
        .hash(&mut h);
    if meta_wanted {
        tab.results.meta.gen.hash(&mut h);
    }
    if sort
        .iter()
        .any(|(c, _)| matches!(c, ResultCol::Size | ResultCol::FolderSize))
    {
        (lib.sizes_gen, walked.gen).hash(&mut h);
    }
    if opened_wanted {
        (Arc::as_ptr(&opened) as usize, opened.len()).hash(&mut h);
    }
    let key = h.finish();
    if tab.results.order_key == Some(key) {
        return;
    }
    let bytes = |e: &Entry| crate::sizes::bytes_of(lib, walked, e);
    let when = |e: &Entry| list_view::opened_of(&opened, lib, e);
    let meta = &tab.results.meta;
    let get = |c: ResultCol, e: &Entry| value(c, e, meta, &bytes, &when);
    let folder = tab.results.folder.clone();
    let keep = |e: &Entry| folder.as_ref().is_none_or(|d| is_within(&e.path, d));
    let rows = order(tab.entries(), &sort, opts.folders_first, &get, &keep);
    tab.set_custom_order(Some(rows));
    tab.results.order_key = Some(key);
}

// ---------------------------------------------------------------------------------------
// The column chooser and the settings rows
// ---------------------------------------------------------------------------------------

/// What the chooser asks the tab for after drawing.
#[derive(Default)]
pub struct ChooserOut {
    /// Back to the primary sort alone.
    pub clear_then_by: bool,
    /// Drop a bookmark's own columns (the profile's then).
    pub own_columns_off: bool,
}

/// The chooser's body: columns, the secondary and tertiary sort, Folders first and
/// descending first.
pub fn chooser_ui(
    ui: &mut egui::Ui,
    opts: &mut ResultsOptions,
    sort: &[(ResultCol, bool)],
    own_columns: bool,
    out: &mut ChooserOut,
) {
    ui.label(egui::RichText::new(crate::i18n::tr("search_results.columns")).strong());
    let mut name = true;
    ui.add_enabled(
        false,
        egui::Checkbox::new(&mut name, ResultCol::Name.label()),
    );
    ui.add_enabled_ui(!own_columns, |ui| {
        for c in ResultCol::CHOOSABLE {
            let mut on = opts.shows(c);
            if ui.checkbox(&mut on, c.label()).changed() {
                opts.set(c, on);
            }
        }
    });
    if own_columns
        && ui
            .button(crate::i18n::tr("search_results.bookmark_columns_off"))
            .clicked()
    {
        out.own_columns_off = true;
    }
    ui.separator();
    ui.label(egui::RichText::new(crate::i18n::tr("search_results.sorted_by")).strong());
    for (i, &(c, asc)) in sort.iter().enumerate() {
        let dir = match asc {
            true => crate::i18n::tr("search_results.ascending"),
            false => crate::i18n::tr("search_results.descending"),
        };
        let rank = match i {
            0 => crate::i18n::tr("search_results.primary"),
            1 => crate::i18n::tr("search_results.secondary"),
            _ => crate::i18n::tr("search_results.tertiary"),
        };
        ui.weak(format!("{rank}: {} ({dir})", c.label()));
    }
    if sort.len() > 1
        && ui
            .button(crate::i18n::tr("search_results.clear_then_by"))
            .clicked()
    {
        out.clear_then_by = true;
    }
    ui.separator();
    folders_first_ui(ui, opts);
}

fn folders_first_ui(ui: &mut egui::Ui, opts: &mut ResultsOptions) {
    ui.label(crate::i18n::tr("search_results.folders_first"));
    for f in FoldersFirst::ALL {
        ui.radio_value(&mut opts.folders_first, f, f.label());
    }
    ui.checkbox(
        &mut opts.descending_first,
        crate::i18n::tr("search_results.descending_first"),
    )
    .on_hover_text(crate::i18n::tr("search_results.descending_first_tip"));
}

/// Right-click on a search tab's column header opens the chooser below it.
pub fn header_chooser(
    ui: &egui::Ui,
    r: &egui::Response,
    opts: &mut ResultsOptions,
    sort: &[(ResultCol, bool)],
    own_columns: bool,
    out: &mut ChooserOut,
) {
    let id = r.id.with("keel-results-chooser");
    if r.secondary_clicked() {
        ui.memory_mut(|m| m.open_popup(id));
    }
    egui::popup_below_widget(
        ui,
        id,
        r,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(220.0);
            chooser_ui(ui, opts, sort, own_columns, out)
        },
    );
}

/// Settings → General: Folders first and descending first.
pub fn settings_rows(ui: &mut egui::Ui, opts: &mut ResultsOptions) {
    ui.label(crate::i18n::tr("search_results.results"));
    ui.horizontal_wrapped(|ui| {
        ui.label(crate::i18n::tr("search_results.folders_first"));
        let combo = egui::ComboBox::from_id_salt("keel-folders-first")
            .selected_text(opts.folders_first.label())
            .show_ui(ui, |ui| {
                for f in FoldersFirst::ALL {
                    ui.selectable_value(&mut opts.folders_first, f, f.label());
                }
            });
        let _ = crate::a11y::Named::named(
            combo.response,
            crate::i18n::tr("search_results.folders_first"),
        );
        ui.checkbox(
            &mut opts.descending_first,
            crate::i18n::tr("search_results.descending_first"),
        )
        .on_hover_text(crate::i18n::tr("search_results.descending_first_tip"));
    });
    ui.end_row();
}

#[cfg(test)]
#[path = "search_columns_tests.rs"]
mod tests;
