//! One tab of a pane: a directory, its listing, selection, sort, filter and history.
//! Pure state; `AppState` spawns the listing workers.

use keel_vfs::{Entry, Kind, VPath};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;
use std::time::Instant;

/// Task 66: Alt+click expands at most this many folders (a whole drive is not listed).
pub const MAX_EXPANDED: usize = 512;

/// Task 66: folders expanded inline in the list view. Their children follow the folder's
/// own entries in `Tab::entries` (`top..`), named by their path from the tab's folder
/// (`"src/main.rs"`), so selection, rename and every action work on them unchanged.
#[derive(Default)]
struct Tree {
    /// Expanded folders by row name.
    nodes: BTreeMap<String, Node>,
    /// The folder's own entries are `entries[..top]`.
    top: usize,
}

struct Node {
    dir: VPath,
    state: NodeState,
    /// Alt+click: folders inside are expanded too as they arrive.
    all: bool,
    /// Its listing, names already prefixed with the node's name.
    children: Option<Listing>,
    /// Where `children` sit in `Tab::entries`.
    range: Range<usize>,
}

/// Where an expanded folder's listing stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// To be listed (the state asks the listing worker).
    Wanted,
    Loading,
    Listed,
    Failed(String),
}

/// What `visible()` was last computed for.
#[derive(Clone, PartialEq)]
struct CacheKey {
    generation: u64,
    filter: String,
    mode: Mode,
    show_hidden: bool,
    totals: u64,
    /// The tree's generation while the triangles show.
    tree: Option<u64>,
    groups: u64,
}

impl CacheKey {
    /// Same inputs but the filter.
    fn same_rows(&self, o: &CacheKey) -> bool {
        (
            self.generation,
            self.mode,
            self.show_hidden,
            self.totals,
            self.tree,
            self.groups,
        ) == (
            o.generation,
            o.mode,
            o.show_hidden,
            o.totals,
            o.tree,
            o.groups,
        )
    }
}

/// A folder listing plus each name lowercased and the default (Name) order, both computed
/// on the listing worker, so the UI thread never case-folds or sorts 100k names on a
/// refresh.
#[derive(Clone)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub lower: Vec<String>,
    /// Indices of `entries`: folders first, then natural name order.
    pub order: Vec<usize>,
}

impl Listing {
    pub fn new(entries: Vec<Entry>) -> Self {
        let lower = entries.iter().map(|e| e.name.to_lowercase()).collect();
        Self::with_lower(entries, lower)
    }

    fn with_lower(entries: Vec<Entry>, lower: Vec<String>) -> Self {
        let order = sorted(
            &entries,
            &lower,
            (SortKey::Name, true, false),
            &Default::default(),
        );
        Self {
            entries,
            lower,
            order,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Ext,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabKind {
    Dir,
    /// Search results (Everything on Windows). Entries are keyed by full path, so two
    /// hits with the same file name stay distinct.
    Search {
        query: String,
        /// Debounce: run the query at this instant.
        due: Option<Instant>,
        /// Request number of the query in flight or shown; older answers are dropped.
        req: u64,
        /// Match Case, Match Whole Words, Match Path, Match Diacritics, Regex.
        toggles: crate::search_tab::Toggles,
        /// The most results asked for; 0 until the first query runs, and again after the
        /// query or a toggle changed (the next query asks for the first page).
        limit: u32,
        /// Load more can ask for more than `limit` (it is below the Most results setting).
        more: bool,
        /// A pane's search box: only hits under this folder, and Esc, × or an empty box
        /// goes back to the folder tab it came from. None: Ctrl+F, everywhere.
        within: Option<VPath>,
    },
    /// Task 29: the library dashboard (`library_ui::overview`); never listed.
    Overview,
    /// Phase 15: the index journal (`journal_ui`); never listed.
    Journal,
}

/// The Journal tab's address (`keel://journal/`), kept in sessions like a folder.
pub fn journal_path() -> VPath {
    VPath {
        scheme: "keel".into(),
        authority: "journal".into(),
        path: "/".into(),
    }
}

/// Cursor movement inside the listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    /// Up/Down arrows: one row (a grid row in grid view).
    Prev,
    Next,
    /// Left/Right arrows: one item.
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

pub struct Tab {
    pub dir: VPath,
    entries: Vec<Entry>,
    lower: Vec<String>,
    /// The listing's Name order (see `Listing::order`).
    order: Vec<usize>,
    /// Every entry in the current sort order, for (generation, sort); filtering is a
    /// linear pass over it, so a keystroke never sorts.
    sorted: Vec<usize>,
    sorted_key: Option<(u64, u64, Mode)>,
    /// Entry names (names survive a refresh, indices do not).
    pub selected: BTreeSet<String>,
    pub cursor: Option<String>,
    /// Shift-range anchor.
    pub anchor: Option<String>,
    pub sort: (SortKey, bool),
    pub filter: String,
    /// The filter box is shown (opened by typing, closed by Esc).
    pub filter_open: bool,
    pub history: Vec<VPath>,
    pub future: Vec<VPath>,
    pub loading: bool,
    pub error: Option<String>,
    /// The folder `entries` were listed from (lags `dir` while a navigation loads).
    pub listed_dir: Option<VPath>,
    /// Request number of the listing shown; older answers are ignored.
    pub listed_req: u64,
    pub kind: TabKind,
    /// Inline rename: (original name, edited text).
    pub renaming: Option<(String, String)>,
    /// Visible position the view should scroll to (set by keyboard moves).
    pub scroll_to: Option<usize>,
    /// Items per page and per row, reported by the view for PageUp/PageDown and Up/Down.
    pub page_rows: usize,
    pub row_step: usize,
    /// Grid view scroll (offset, viewport height), for keeping the cursor in view.
    pub grid_scroll: (f32, f32),
    /// Scroll the cursor into view once the next listing arrives (Open location).
    pub reveal: bool,
    /// The search this tab left for a folder, restored if that folder cannot be listed.
    pub left_search: Option<TabKind>,
    // --- Task 23 ---
    /// Columns view: the columns right of this folder.
    pub columns: crate::view_columns::Columns,
    // --- end Task 23 ---
    /// Task 29: a search tab querying the library instead of the platform backend.
    pub library_search: bool,
    // --- Task 76 ---
    /// The search filter: a built-in or custom filter's name ("" = Everything).
    pub search_filter: String,
    /// Task 62: a library search's snippets of the text inside documents, by hit path.
    pub snippets: std::collections::HashMap<VPath, keel_search::Snippet>,
    /// A search tab's matched parts of names (and of folders with Match Path), from its
    /// last query.
    pub highlight: Option<keel_search::Highlighter>,
    /// Task 77: a file list tab: its queries search this list (`filelist_ui`).
    pub filelist: Option<std::sync::Arc<keel_search::FileListSearcher>>,
    // --- Task 46 ---
    /// Shown in the Sizes view: biggest first, folders mixed with files (set by the pane
    /// before drawing).
    pub sizes_view: bool,
    /// `sizes::sync_tab`'s inputs when `totals` was last filled.
    pub totals_key: Option<(u64, u64, u64)>,
    /// Known folder totals by entry name (sorting by size).
    totals: std::collections::HashMap<String, u64>,
    totals_gen: u64,
    generation: u64,
    cache_key: Option<CacheKey>,
    cache: Vec<usize>,
    /// Bumped whenever `cache` changes (Task 32: the media view's layout cache).
    cache_gen: u64,
    /// The Journal tab's filters and entries (unused elsewhere).
    pub journal: Box<crate::journal_ui::JournalView>,
    // --- Task 66 ---
    tree: Tree,
    tree_gen: u64,
    /// The list view shows disclosure triangles (set by the pane before drawing).
    pub disclosure: bool,
    /// Group by: each top-level entry's group (set by `list_view::prepare`).
    groups: Option<crate::list_view::Groups>,
    group_key: Option<u64>,
    group_gen: u64,
    /// (position in `visible_cached()`, group) where each shown group starts.
    group_starts: Vec<(usize, u32)>,
    /// Creation times read off the UI thread.
    pub meta: crate::list_view::Meta,
}

impl Tab {
    /// A search tab's match toggles as a bookmark or filter keeps them (none: a folder tab).
    pub fn match_toggles(&self) -> keel_core::MatchToggles {
        match &self.kind {
            TabKind::Search { toggles, .. } => crate::search_tab::match_toggles(*toggles),
            _ => Default::default(),
        }
    }

    /// Sets a search tab's match toggles from a bookmark's.
    pub fn set_match_toggles(&mut self, m: keel_core::MatchToggles) {
        if let TabKind::Search { toggles, .. } = &mut self.kind {
            *toggles = crate::search_tab::toggles_of(m);
        }
    }
}

impl Tab {
    pub fn new(dir: VPath) -> Self {
        let overview = dir == crate::library::overview_path();
        let journal = dir == journal_path();
        Self {
            dir,
            entries: Vec::new(),
            lower: Vec::new(),
            order: Vec::new(),
            sorted: Vec::new(),
            sorted_key: None,
            selected: BTreeSet::new(),
            cursor: None,
            anchor: None,
            sort: (SortKey::Name, true),
            filter: String::new(),
            filter_open: false,
            history: Vec::new(),
            future: Vec::new(),
            loading: !overview && !journal,
            error: None,
            listed_dir: None,
            listed_req: 0,
            kind: if overview {
                TabKind::Overview
            } else if journal {
                TabKind::Journal
            } else {
                TabKind::Dir
            },
            renaming: None,
            scroll_to: None,
            page_rows: 20,
            row_step: 1,
            grid_scroll: (0.0, 0.0),
            reveal: false,
            left_search: None,
            columns: Default::default(),
            library_search: false,
            search_filter: String::new(),
            snippets: Default::default(),
            highlight: None,
            filelist: None,
            sizes_view: false,
            totals_key: None,
            totals: Default::default(),
            totals_gen: 0,
            generation: 0,
            cache_key: None,
            cache: Vec::new(),
            cache_gen: 0,
            journal: Default::default(),
            tree: Tree::default(),
            tree_gen: 0,
            disclosure: false,
            groups: None,
            group_key: None,
            group_gen: 0,
            group_starts: Vec::new(),
            meta: Default::default(),
        }
    }

    /// A search tab started from `dir` (where it pastes nothing and lists nothing).
    pub fn search(dir: VPath) -> Self {
        Self {
            kind: TabKind::Search {
                query: String::new(),
                due: None,
                req: 0,
                toggles: Default::default(),
                limit: 0,
                more: false,
                within: None,
            },
            loading: false,
            listed_dir: Some(dir.clone()),
            ..Self::new(dir)
        }
    }

    pub fn is_search(&self) -> bool {
        matches!(self.kind, TabKind::Search { .. })
    }

    /// A search tab Ctrl+F, the toolbar's search and a bookmark run in: not a pane's
    /// search kept to a folder (they open a new tab instead).
    pub fn is_open_search(&self) -> bool {
        self.is_search() && self.search_within().is_none()
    }

    /// The folder a pane's search box searches under (None: not such a search tab).
    pub fn search_within(&self) -> Option<&VPath> {
        match &self.kind {
            TabKind::Search { within, .. } => within.as_ref(),
            _ => None,
        }
    }

    /// A tab showing the Recycle Bin / Trash (`trash://`): its rows are restored or
    /// deleted for good, never pasted into or edited.
    pub fn is_trash(&self) -> bool {
        self.kind == TabKind::Dir && self.dir.scheme == keel_vfs::trashbin::SCHEME
    }

    /// The name shown for `e`: search rows are keyed by full path but show the file name,
    /// rows inside an expanded folder by their path from this folder (Task 66).
    pub fn shown_name<'a>(&self, e: &'a Entry) -> &'a str {
        if self.is_search() || e.name.contains('/') {
            e.path.name()
        } else {
            &e.name
        }
    }

    pub fn title(&self) -> String {
        match &self.kind {
            TabKind::Search { query, .. } if self.library_search => match query.as_str() {
                crate::library::FAVORITES_QUERY => "Favorites".into(),
                crate::library::RECENTS_QUERY => "Recents".into(),
                "" => "Library search".into(),
                q => format!("Library: {q}"),
            },
            TabKind::Search { query, .. } if self.filelist.is_some() => {
                let list = self.filelist.as_ref().map_or("", |s| s.label());
                match query.trim() {
                    "" => list.to_owned(),
                    q => format!("{list}: {q}"),
                }
            }
            TabKind::Search { query, .. } if query.is_empty() => "Search".into(),
            TabKind::Search { query, .. } => format!("Search: {query}"),
            TabKind::Overview => "Overview".into(),
            TabKind::Journal => "Journal".into(),
            TabKind::Dir => match self.dir.name() {
                _ if self.dir.scheme == "node" => crate::devices::node_title(&self.dir)
                    .unwrap_or_else(|| match self.dir.name() {
                        "" => self.dir.display(),
                        name => name.to_owned(),
                    }),
                "" if self.dir.scheme == keel_vfs::library::SCHEME => {
                    crate::library::label_of(&self.dir.authority)
                        .unwrap_or_else(|| "Library".into())
                }
                "" if self.is_trash() => keel_vfs::trashbin::label().into(),
                "" => self.dir.display(),
                name => name.to_owned(),
            },
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[cfg(test)]
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        self.set_listing(Listing::new(entries));
    }

    /// Replaces the listing; selection names that no longer exist are dropped. Expanded
    /// folders that are still there keep their rows and are listed again.
    pub fn set_listing(&mut self, listing: Listing) {
        crate::devices::note_listing(&self.dir, &listing.entries);
        crate::finder_icons::note_listing(&self.dir, &listing.entries);
        let old = std::mem::replace(&mut self.entries, listing.entries);
        let old_lower = std::mem::replace(&mut self.lower, listing.lower);
        self.order = listing.order;
        self.generation += 1;
        self.tree.top = self.entries.len();
        self.groups = None;
        self.group_key = None;
        self.group_gen += 1;
        if !self.tree.nodes.is_empty() {
            let dirs: HashSet<&str> = (self.entries.iter())
                .filter(|e| e.kind == Kind::Dir)
                .map(|e| e.name.as_str())
                .collect();
            self.tree.nodes.retain(|name, _| {
                let first = name.split('/').next().unwrap_or(name);
                dirs.contains(first)
            });
            for node in self.tree.nodes.values_mut() {
                if node.state != NodeState::Loading {
                    node.state = NodeState::Wanted;
                }
            }
            self.rebuild_tail();
        }
        if old.len() > 10_000 {
            // Freeing 100k entries takes milliseconds: not on the UI thread.
            std::thread::spawn(move || drop((old, old_lower)));
        }
        // The usual selection is a few names: look them up instead of indexing 100k names.
        let kept = self.selected.len() + usize::from(self.cursor.is_some());
        let entries = &self.entries;
        let names: HashSet<&str> = if kept > 16 {
            entries.iter().map(|e| e.name.as_str()).collect()
        } else {
            HashSet::new()
        };
        let has = |n: &str| {
            if kept > 16 {
                names.contains(n)
            } else {
                entries.iter().any(|e| e.name == n)
            }
        };
        self.selected.retain(|n| has(n));
        if self.cursor.as_deref().is_some_and(|c| !has(c)) {
            self.cursor = None;
        }
    }

    /// Replaces the known folder totals (a size sort follows them).
    pub fn set_totals(&mut self, totals: std::collections::HashMap<String, u64>) {
        if totals != self.totals {
            self.totals = totals;
            self.totals_gen += 1;
        }
    }

    /// The order rows are shown in: the Sizes view's, else the chosen sort (folders first).
    fn mode(&self) -> Mode {
        if self.sizes_view {
            (SortKey::Size, false, true)
        } else {
            (self.sort.0, self.sort.1, false)
        }
    }

    /// Indices into `entries()` after hidden/filter/sort, dirs first; the children of
    /// expanded folders under them while the triangles show (Task 66), and with Group by
    /// the rows in group order, sorted within each group. Cached until the entries,
    /// filter, sort, tree, grouping or `show_hidden` change; a filter that only grew
    /// narrows the cached rows instead of sorting again. Selection is trimmed to the
    /// visible rows.
    pub fn visible(&mut self, show_hidden: bool) -> &[usize] {
        let mode = self.mode();
        let tree_on = self.tree_on();
        let key = CacheKey {
            generation: self.generation,
            filter: self.filter.clone(),
            mode,
            show_hidden,
            totals: self.totals_gen,
            tree: tree_on.then_some(self.tree_gen),
            groups: self.group_gen,
        };
        let top = self.tree.top;
        let ranks: Option<&[u32]> = (self.groups.as_ref())
            .map(|g| g.rank.as_slice())
            .filter(|r| r.len() == top);
        match &self.cache_key {
            Some(old) if *old == key => return &self.cache,
            // Children follow their folder: a narrower filter could keep orphans.
            Some(old)
                if !tree_on
                    && old.same_rows(&key)
                    && key
                        .filter
                        .to_lowercase()
                        .starts_with(&old.filter.to_lowercase()) =>
            {
                let needle = key.filter.to_lowercase();
                let lower = &self.lower;
                self.cache.retain(|&i| lower[i].contains(&needle));
                self.group_starts = match ranks {
                    Some(r) => starts(self.cache.iter().map(|&i| r[i])),
                    None => Vec::new(),
                };
            }
            _ => {
                let sort_key = (self.generation, self.totals_gen, mode);
                if self.sorted_key != Some(sort_key) {
                    let order = &self.order;
                    self.sorted = match mode {
                        (SortKey::Name, true, false) => order.clone(),
                        (SortKey::Name, false, false) => {
                            // Folders stay first; each group reversed.
                            let dirs = order
                                .iter()
                                .take_while(|&&i| self.entries[i].kind == Kind::Dir)
                                .count();
                            let (d, f) = order.split_at(dirs);
                            d.iter().rev().chain(f.iter().rev()).copied().collect()
                        }
                        mode => {
                            sorted(&self.entries[..top], &self.lower[..top], mode, &self.totals)
                        }
                    };
                    self.sorted_key = Some(sort_key);
                }
                let needle = self.filter.to_lowercase();
                let (entries, lower) = (&self.entries, &self.lower);
                let pass =
                    |i: usize| (show_hidden || !entries[i].hidden) && lower[i].contains(&needle);
                let cx = TreeCx {
                    tree: &self.tree,
                    entries,
                    lower,
                    mode,
                    totals: &self.totals,
                    pass: &pass,
                };
                let mut rows: Vec<(u32, usize)> = Vec::new();
                for &i in &self.sorted {
                    if !pass(i) {
                        continue;
                    }
                    let rank = ranks.map_or(0, |r| r[i]);
                    rows.push((rank, i));
                    if tree_on {
                        cx.children(&entries[i].name, rank, &mut rows);
                    }
                }
                if ranks.is_some() {
                    // Stable: the sort order holds inside each group.
                    rows.sort_by_key(|r| r.0);
                    self.group_starts = starts(rows.iter().map(|r| r.0));
                } else {
                    self.group_starts.clear();
                }
                self.cache = rows.into_iter().map(|r| r.1).collect();
            }
        }
        self.cache_key = Some(key);
        self.cache_gen += 1;
        if !self.selected.is_empty() {
            let shown: HashSet<&str> = self
                .cache
                .iter()
                .map(|&i| self.entries[i].name.as_str())
                .collect();
            self.selected.retain(|n| shown.contains(n.as_str()));
        }
        &self.cache
    }

    /// The cached result of the last `visible()` call (empty before the first call).
    pub fn visible_cached(&self) -> &[usize] {
        &self.cache
    }

    /// Changes whenever `visible_cached()` may have changed.
    pub fn visible_gen(&self) -> u64 {
        self.cache_gen
    }

    /// Changes with every new listing (and every change of the expanded folders' rows).
    pub fn generation(&self) -> u64 {
        self.generation + self.tree_gen
    }

    /// Changes directory: pushes history, clears selection/filter, marks loading.
    pub fn navigate(&mut self, to: VPath) {
        if to == self.dir && !self.is_search() {
            return;
        }
        let from = std::mem::replace(&mut self.dir, to);
        self.history.push(from);
        self.future.clear();
        self.reset_view();
    }

    pub fn back(&mut self) -> bool {
        let Some(prev) = self.history.pop() else {
            return false;
        };
        let from = std::mem::replace(&mut self.dir, prev);
        self.future.push(from);
        self.reset_view();
        true
    }

    pub fn forward(&mut self) -> bool {
        let Some(next) = self.future.pop() else {
            return false;
        };
        let from = std::mem::replace(&mut self.dir, next);
        self.history.push(from);
        self.reset_view();
        true
    }

    /// Goes to the parent and puts the cursor on the folder we came from.
    pub fn up(&mut self) -> bool {
        let Some(parent) = self.dir.parent() else {
            return false;
        };
        let child = self.dir.name().to_owned();
        self.navigate(parent);
        self.cursor = Some(child.clone());
        self.selected.insert(child);
        true
    }

    pub fn refresh(&mut self) {
        self.loading = true;
    }

    /// Leaving a search for a folder turns the tab into a folder tab.
    fn reset_view(&mut self) {
        let kind = std::mem::replace(&mut self.kind, TabKind::Dir);
        if let TabKind::Search {
            query,
            req,
            toggles,
            limit,
            more,
            within,
            ..
        } = kind
        {
            self.left_search = Some(TabKind::Search {
                query,
                due: None,
                req,
                toggles,
                limit,
                more,
                within,
            });
        }
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.filter.clear();
        self.filter_open = false;
        self.renaming = None;
        self.error = None;
        self.loading = true;
        self.scroll_to = Some(0);
        // --- Task 66 ---: a new folder starts collapsed.
        if !self.tree.nodes.is_empty() {
            self.tree.nodes.clear();
            self.rebuild_tail();
        }
        // --- Task 23 ---
        crate::view_columns::collapse(self);
    }

    /// Mouse click on `name`: plain = select only, ctrl = toggle, shift = range from anchor.
    pub fn click(&mut self, name: &str, ctrl: bool, shift: bool) {
        if shift {
            let anchor = self.anchor.clone().unwrap_or_else(|| name.to_owned());
            let range = self.range(&anchor, name);
            if !ctrl {
                self.selected.clear();
            }
            self.selected.extend(range);
            self.anchor = Some(anchor);
        } else {
            if ctrl {
                if !self.selected.remove(name) {
                    self.selected.insert(name.to_owned());
                }
            } else {
                self.selected.clear();
                self.selected.insert(name.to_owned());
            }
            self.anchor = Some(name.to_owned());
        }
        self.cursor = Some(name.to_owned());
    }

    /// Keyboard cursor move; `extend` selects from the anchor to the new cursor.
    pub fn move_cursor(&mut self, nav: Nav, extend: bool) {
        let len = self.cache.len();
        if len == 0 {
            return;
        }
        let cur = self.cursor_pos();
        // Task 66: Right expands the folder at the cursor, Left collapses it or goes to
        // the folder a row is in.
        if self.disclosure && !extend && matches!(nav, Nav::Left | Nav::Right) {
            if let Some(c) = cur {
                self.disclose(c, nav == Nav::Right);
            }
            return;
        }
        let page = self.page_rows.max(1);
        let step = self.row_step.max(1);
        let grouped = step > 1 && !self.group_starts.is_empty();
        let pos = match (nav, cur) {
            (Nav::Home, _) => 0,
            (Nav::End, _) => len - 1,
            (_, None) => 0,
            (Nav::Prev, Some(c)) if grouped => self.grouped_step(c, step, false),
            (Nav::Next, Some(c)) if grouped => self.grouped_step(c, step, true),
            (Nav::Prev, Some(c)) => c.saturating_sub(step),
            (Nav::Next, Some(c)) => (c + step).min(len - 1),
            (Nav::Left, Some(c)) => c.saturating_sub(1),
            (Nav::Right, Some(c)) => (c + 1).min(len - 1),
            (Nav::PageUp, Some(c)) => c.saturating_sub(page),
            (Nav::PageDown, Some(c)) => (c + page).min(len - 1),
        };
        let name = self.entries[self.cache[pos]].name.clone();
        if extend {
            let anchor = self
                .anchor
                .clone()
                .or_else(|| self.cursor.clone())
                .unwrap_or_else(|| name.clone());
            self.selected = self.range(&anchor, &name).into_iter().collect();
            self.anchor = Some(anchor);
        } else {
            self.selected.clear();
            self.selected.insert(name.clone());
            self.anchor = Some(name.clone());
        }
        self.cursor = Some(name);
        self.scroll_to = Some(pos);
    }

    /// Selects the row at visible position `pos` alone (cursor and anchor too).
    fn select_pos(&mut self, pos: usize) {
        let name = self.entries[self.cache[pos]].name.clone();
        self.selected = [name.clone()].into();
        self.anchor = Some(name.clone());
        self.cursor = Some(name);
        self.scroll_to = Some(pos);
    }

    /// Right (`open`) / Left on the row at visible position `pos` (Task 66).
    fn disclose(&mut self, pos: usize, open: bool) {
        let e = &self.entries[self.cache[pos]];
        let name = e.name.clone();
        let dir = e.kind == Kind::Dir;
        if open {
            if dir && !self.is_expanded(&name) {
                self.expand(&name, false);
            }
        } else if self.is_expanded(&name) {
            self.collapse(&name);
        } else if let Some((parent, _)) = name.rsplit_once('/') {
            let at = (self.cache.iter()).position(|&i| self.entries[i].name == parent);
            if let Some(at) = at {
                self.select_pos(at);
            }
        }
    }

    /// Up/Down in a grouped grid: rows restart at each group, so the column is kept
    /// within a group and carried into the next or previous one.
    fn grouped_step(&self, pos: usize, step: usize, down: bool) -> usize {
        let len = self.cache.len();
        let starts: Vec<usize> = self.group_starts.iter().map(|g| g.0).collect();
        let g = starts.partition_point(|&s| s <= pos).saturating_sub(1);
        let bounds = |g: usize| (starts[g], starts.get(g + 1).copied().unwrap_or(len));
        let (start, end) = bounds(g);
        let col = (pos - start) % step;
        let last_row = |(s, e): (usize, usize)| s + (e - 1 - s) / step * step;
        if down {
            if pos + step < end {
                pos + step
            } else if g + 1 < starts.len() {
                let (s, e) = bounds(g + 1);
                (s + col).min(e - 1)
            } else if pos < last_row((start, end)) {
                end - 1
            } else {
                pos
            }
        } else if pos >= start + step {
            pos - step
        } else if g > 0 {
            let (s, e) = bounds(g - 1);
            (last_row((s, e)) + col).min(e - 1)
        } else {
            pos
        }
    }

    // --- Task 66: disclosure triangles ---

    /// The triangles show and something is expanded.
    fn tree_on(&self) -> bool {
        self.disclosure && !self.tree.nodes.is_empty() && !self.is_search() && !self.sizes_view
    }

    /// How deep row `e` is inside expanded folders (0: this folder's own entries).
    pub fn depth(&self, e: &Entry) -> usize {
        if self.is_search() {
            0
        } else {
            e.name.matches('/').count()
        }
    }

    pub fn is_expanded(&self, name: &str) -> bool {
        self.tree.nodes.contains_key(name)
    }

    /// The listing state of expanded folder `name`.
    pub fn node_state(&self, name: &str) -> Option<&NodeState> {
        self.tree.nodes.get(name).map(|n| &n.state)
    }

    /// Expands folder row `name` (listed on the next tick); `all` (Alt+click) expands the
    /// folders inside it too, up to `MAX_EXPANDED`.
    pub fn expand(&mut self, name: &str, all: bool) {
        if let Some(node) = self.tree.nodes.get_mut(name) {
            if all && !node.all {
                node.all = true;
                let dirs: Vec<String> = (node.children.iter())
                    .flat_map(|l| l.entries.iter())
                    .filter(|e| e.kind == Kind::Dir && !e.is_link)
                    .map(|e| e.name.clone())
                    .collect();
                for d in dirs {
                    self.expand(&d, true);
                }
            }
            return;
        }
        if self.tree.nodes.len() >= MAX_EXPANDED || self.is_search() {
            return;
        }
        let Some(e) = self.entries.iter().find(|e| e.name == name) else {
            return;
        };
        if e.kind != Kind::Dir {
            return;
        }
        let node = Node {
            dir: e.path.clone(),
            state: NodeState::Wanted,
            all,
            children: None,
            range: 0..0,
        };
        self.tree.nodes.insert(name.to_owned(), node);
        self.tree_gen += 1;
    }

    /// Collapses folder row `name` and every folder inside it; a listing still running for
    /// them is ignored when it answers. A cursor inside moves to the folder.
    pub fn collapse(&mut self, name: &str) {
        let prefix = format!("{name}/");
        let before = self.tree.nodes.len();
        self.tree
            .nodes
            .retain(|n, _| n != name && !n.starts_with(&prefix));
        if self.tree.nodes.len() == before {
            return;
        }
        if self
            .cursor
            .as_deref()
            .is_some_and(|c| c.starts_with(&prefix))
        {
            self.cursor = Some(name.to_owned());
            self.selected.insert(name.to_owned());
        }
        if self
            .anchor
            .as_deref()
            .is_some_and(|c| c.starts_with(&prefix))
        {
            self.anchor = Some(name.to_owned());
        }
        self.selected.retain(|n| !n.starts_with(&prefix));
        self.rebuild_tail();
    }

    /// The triangle: expands or collapses `name`; with `all` (Alt+click) every folder below.
    pub fn toggle_expanded(&mut self, name: &str, all: bool) {
        if self.is_expanded(name) {
            self.collapse(name);
        } else {
            self.expand(name, all);
        }
    }

    /// Expanded folders waiting for a listing (marked as being listed). Empty while the
    /// triangles do not show.
    pub fn take_wanted(&mut self) -> Vec<VPath> {
        if !self.disclosure {
            return Vec::new();
        }
        (self.tree.nodes.values_mut())
            .filter(|n| n.state == NodeState::Wanted)
            .map(|n| {
                n.state = NodeState::Loading;
                n.dir.clone()
            })
            .collect()
    }

    /// A listing of `dir` arrived: the expanded folders on it take it. False when none
    /// waited for it (collapsed meanwhile: the answer is dropped).
    pub fn fill_child(&mut self, dir: &VPath, result: &anyhow::Result<Listing>) -> bool {
        let names: Vec<String> = (self.tree.nodes.iter())
            .filter(|(_, n)| n.dir == *dir && n.state != NodeState::Listed)
            .map(|(name, _)| name.clone())
            .collect();
        if names.is_empty() {
            return false;
        }
        let mut more = Vec::new();
        for name in names {
            let Some(node) = self.tree.nodes.get_mut(&name) else {
                continue;
            };
            match result {
                Ok(listing) => {
                    let mut listing = listing.clone();
                    for e in &mut listing.entries {
                        e.name = format!("{name}/{}", e.name);
                    }
                    if node.all {
                        more.extend(
                            (listing.entries.iter())
                                .filter(|e| e.kind == Kind::Dir && !e.is_link)
                                .map(|e| e.name.clone()),
                        );
                    }
                    node.children = Some(listing);
                    node.state = NodeState::Listed;
                }
                Err(e) => node.state = NodeState::Failed(format!("{e:#}")),
            }
        }
        self.rebuild_tail();
        for name in more {
            self.expand(&name, true);
        }
        true
    }

    /// Lays the expanded folders' children out after the folder's own entries again.
    fn rebuild_tail(&mut self) {
        let top = self.tree.top;
        self.entries.truncate(top);
        self.lower.truncate(top);
        for node in self.tree.nodes.values_mut() {
            let start = self.entries.len();
            if let Some(l) = &node.children {
                self.entries.extend(l.entries.iter().cloned());
                self.lower.extend(l.lower.iter().cloned());
            }
            node.range = start..self.entries.len();
        }
        self.tree_gen += 1;
        // Rows of the old tail may be gone: nothing reads the old rows until `visible()`.
        self.cache.clear();
        self.cache_key = None;
        self.cache_gen += 1;
        self.group_starts.clear();
    }

    // --- Task 66: Group by ---

    /// The grouping (`list_view::prepare`): `key` identifies its inputs.
    pub fn set_groups(&mut self, key: Option<u64>, groups: Option<crate::list_view::Groups>) {
        if key == self.group_key && groups.is_some() == self.groups.is_some() {
            return;
        }
        self.group_key = key;
        self.groups = groups;
        self.group_gen += 1;
    }

    pub fn group_key(&self) -> Option<u64> {
        self.group_key
    }

    /// Where each shown group starts in `visible_cached()`, and its group (empty when not
    /// grouped).
    pub fn group_starts(&self) -> &[(usize, u32)] {
        &self.group_starts
    }

    /// Items in shown group `g` (`group_starts()[g]`), rows inside expanded folders not
    /// counted.
    pub fn group_count(&self, g: usize) -> usize {
        let Some(&(start, _)) = self.group_starts.get(g) else {
            return 0;
        };
        let end = (self.group_starts.get(g + 1)).map_or(self.cache.len(), |s| s.0);
        let top = self.tree.top;
        self.cache[start..end].iter().filter(|&&i| i < top).count()
    }

    pub fn group_label(&self, group: u32) -> &str {
        (self.groups.as_ref())
            .and_then(|g| g.labels.get(group as usize))
            .map_or("", String::as_str)
    }

    /// The folder's own entries (`entries()[..top_len()]`; expanded folders' rows follow).
    pub fn top_len(&self) -> usize {
        self.tree.top
    }

    /// Space: toggle the cursor row.
    pub fn toggle_cursor(&mut self) {
        if let Some(c) = self.cursor.clone() {
            if !self.selected.remove(&c) {
                self.selected.insert(c);
            }
        }
    }

    /// Selects only visible rows.
    pub fn select_all(&mut self) {
        self.selected = self.visible_names().collect();
    }

    pub fn invert_selection(&mut self) {
        let next: BTreeSet<String> = self
            .visible_names()
            .filter(|n| !self.selected.contains(n))
            .collect();
        self.selected = next;
    }

    /// Scrolls to the cursor if `reveal` was asked for (call after `visible()`).
    pub fn reveal_cursor(&mut self) {
        if std::mem::take(&mut self.reveal) {
            self.scroll_to = self.cursor_pos();
        }
    }

    pub fn cursor_pos(&self) -> Option<usize> {
        let c = self.cursor.as_deref()?;
        self.cache.iter().position(|&i| self.entries[i].name == c)
    }

    fn visible_names(&self) -> impl Iterator<Item = String> + '_ {
        self.cache.iter().map(|&i| self.entries[i].name.clone())
    }

    fn range(&self, a: &str, b: &str) -> Vec<String> {
        let pos = |n: &str| self.cache.iter().position(|&i| self.entries[i].name == n);
        match (pos(a), pos(b)) {
            (Some(x), Some(y)) => self.cache[x.min(y)..=x.max(y)]
                .iter()
                .map(|&i| self.entries[i].name.clone())
                .collect(),
            _ => vec![b.to_owned()],
        }
    }

    /// Visible selected entries, or the cursor entry when nothing is selected. Rows
    /// hidden by the filter or hide-hidden are never targets. Empty until `visible()`
    /// has run for the current listing.
    pub fn targets(&self) -> Vec<&Entry> {
        if self
            .cache_key
            .as_ref()
            .is_none_or(|k| k.generation != self.generation)
        {
            return Vec::new();
        }
        let picked: Vec<&Entry> = self
            .cache
            .iter()
            .map(|&i| &self.entries[i])
            .filter(|e| self.selected.contains(&e.name))
            .collect();
        if picked.is_empty() {
            self.cursor_pos()
                .map(|pos| &self.entries[self.cache[pos]])
                .into_iter()
                .collect()
        } else {
            picked
        }
    }
}

/// Search hits as a listing: names are full paths (unique), `lower` is the lowercased
/// file name so Name sort and the filter work on what the row shows.
pub fn hits_listing(hits: Vec<keel_search::Hit>) -> Listing {
    let entries: Vec<Entry> = hits
        .into_iter()
        .map(|h| {
            let name = h.path.display();
            let file = h.path.name();
            let ext = match file.rsplit_once('.') {
                Some((stem, ext)) if !h.is_dir && !stem.is_empty() => ext.to_lowercase(),
                _ => String::new(),
            };
            Entry {
                kind: if h.is_dir { Kind::Dir } else { Kind::File },
                size: h.size,
                modified: h.modified,
                hidden: false,
                is_link: false,
                encrypted: false,
                ext,
                name,
                path: h.path,
            }
        })
        .collect();
    let lower = entries
        .iter()
        .map(|e| e.path.name().to_lowercase())
        .collect();
    Listing::with_lower(entries, lower)
}

/// Sort key, ascending, and folders mixed with files (the Sizes view).
type Mode = (SortKey, bool, bool);

/// (position, group) where the group changes along `ranks`.
fn starts(ranks: impl Iterator<Item = u32>) -> Vec<(usize, u32)> {
    let mut out: Vec<(usize, u32)> = Vec::new();
    for (pos, r) in ranks.enumerate() {
        if out.last().is_none_or(|l| l.1 != r) {
            out.push((pos, r));
        }
    }
    out
}

/// What laying out an expanded folder's rows reads.
struct TreeCx<'a> {
    tree: &'a Tree,
    entries: &'a [Entry],
    lower: &'a [String],
    mode: Mode,
    totals: &'a std::collections::HashMap<String, u64>,
    pass: &'a dyn Fn(usize) -> bool,
}

impl TreeCx<'_> {
    /// Rows of expanded folder `name` (and of the folders expanded inside it), sorted like
    /// the folder's own rows, in group `rank`.
    fn children(&self, name: &str, rank: u32, rows: &mut Vec<(u32, usize)>) {
        let Some(node) = self.tree.nodes.get(name) else {
            return;
        };
        let r = node.range.clone();
        let order: Vec<usize> = match (self.mode, &node.children) {
            ((SortKey::Name, true, false), Some(l)) => l.order.clone(),
            _ => sorted(
                &self.entries[r.clone()],
                &self.lower[r.clone()],
                self.mode,
                self.totals,
            ),
        };
        for c in order.into_iter().map(|o| r.start + o) {
            if (self.pass)(c) {
                rows.push((rank, c));
                self.children(&self.entries[c].name, rank, rows);
            }
        }
    }
}

/// Every index of `entries` in `(key, asc)` order, folders first unless `mixed`. By size a
/// folder counts its total from `totals` (by name); folders without one come last.
fn sorted(
    entries: &[Entry],
    lower: &[String],
    (key, asc, mixed): Mode,
    totals: &std::collections::HashMap<String, u64>,
) -> Vec<usize> {
    let mut keyed: Vec<(usize, &str)> = lower.iter().map(String::as_str).enumerate().collect();
    let size = |e: &Entry| match e.kind {
        Kind::Dir => totals.get(&e.name).copied(),
        _ => Some(e.size),
    };
    keyed.sort_by(|(ai, an), (bi, bn)| {
        let (a, b) = (&entries[*ai], &entries[*bi]);
        let by_name = || natord::compare(an, bn);
        let ord = match key {
            SortKey::Name => by_name(),
            SortKey::Size => size(a).cmp(&size(b)).then_with(by_name),
            SortKey::Modified => a.modified.cmp(&b.modified).then_with(by_name),
            SortKey::Ext => a.ext.cmp(&b.ext).then_with(by_name),
        };
        let ord = if asc { ord } else { ord.reverse() };
        let unknown = |e: &Entry| key == SortKey::Size && size(e).is_none();
        let dirs_first = if mixed {
            std::cmp::Ordering::Equal
        } else {
            (a.kind != Kind::Dir).cmp(&(b.kind != Kind::Dir))
        };
        dirs_first.then(unknown(a).cmp(&unknown(b))).then(ord)
    });
    keyed.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
pub(crate) fn test_entry(dir: &VPath, name: &str, kind: Kind, size: u64) -> Entry {
    Entry {
        path: dir.join(name),
        name: name.into(),
        kind,
        size,
        modified: None,
        hidden: false,
        is_link: false,
        encrypted: false,
        ext: name
            .rsplit_once('.')
            .map(|(_, e)| e.into())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab() -> Tab {
        let dir = VPath::parse("mem://t/").unwrap();
        let mut tab = Tab::new(dir.clone());
        tab.set_entries(vec![
            test_entry(&dir, "alpha.txt", Kind::File, 10),
            test_entry(&dir, "beta.rs", Kind::File, 300),
            test_entry(&dir, "data", Kind::Dir, 0),
            test_entry(&dir, "gamma.md", Kind::File, 5),
        ]);
        tab
    }

    fn names(tab: &mut Tab) -> Vec<String> {
        let vis = tab.visible(false).to_vec();
        vis.iter().map(|&i| tab.entries()[i].name.clone()).collect()
    }

    #[test]
    fn filter_and_sort_by_size_desc_keeps_dirs_first() {
        let mut tab = tab();
        tab.filter = "A".into();
        tab.sort = (SortKey::Size, false);
        assert_eq!(
            names(&mut tab),
            ["data", "beta.rs", "alpha.txt", "gamma.md"]
        );
        tab.filter = "al".into();
        assert_eq!(names(&mut tab), ["alpha.txt"]);
    }

    /// Name order comes from the listing worker; descending keeps folders first.
    #[test]
    fn name_order_from_the_listing_matches_a_full_sort() {
        let mut tab = tab();
        tab.set_entries(vec![
            test_entry(&tab.dir, "b10.txt", Kind::File, 1),
            test_entry(&tab.dir, "zeta", Kind::Dir, 0),
            test_entry(&tab.dir, "b9.txt", Kind::File, 2),
            test_entry(&tab.dir, "alpha", Kind::Dir, 0),
        ]);
        assert_eq!(names(&mut tab), ["alpha", "zeta", "b9.txt", "b10.txt"]);
        tab.sort = (SortKey::Name, false);
        assert_eq!(names(&mut tab), ["zeta", "alpha", "b10.txt", "b9.txt"]);
        tab.filter = "B".into();
        assert_eq!(names(&mut tab), ["b10.txt", "b9.txt"]);
        tab.filter.clear();
        tab.sort = (SortKey::Size, true);
        assert_eq!(names(&mut tab), ["alpha", "zeta", "b10.txt", "b9.txt"]);
    }

    /// Task 46: by size a folder counts its total, unknown ones last (either direction);
    /// the Sizes view mixes folders with files, biggest first.
    #[test]
    fn size_order_uses_folder_totals_and_the_sizes_view_mixes_folders() {
        let mut tab = tab();
        tab.set_entries(vec![
            test_entry(&tab.dir, "small.txt", Kind::File, 10),
            test_entry(&tab.dir, "big.bin", Kind::File, 300),
            test_entry(&tab.dir, "data", Kind::Dir, 0),
            test_entry(&tab.dir, "more", Kind::Dir, 0),
        ]);
        tab.set_totals([("data".to_string(), 100)].into());
        tab.sort = (SortKey::Size, true);
        assert_eq!(names(&mut tab), ["data", "more", "small.txt", "big.bin"]);
        tab.sort = (SortKey::Size, false);
        assert_eq!(names(&mut tab), ["data", "more", "big.bin", "small.txt"]);
        tab.sizes_view = true;
        assert_eq!(names(&mut tab), ["big.bin", "data", "small.txt", "more"]);
        tab.set_totals([("data".to_string(), 1000), ("more".to_string(), 1)].into());
        assert_eq!(names(&mut tab), ["data", "big.bin", "small.txt", "more"]);
        tab.sizes_view = false;
        tab.sort = (SortKey::Name, true);
        assert_eq!(names(&mut tab), ["data", "more", "big.bin", "small.txt"]);
    }

    #[test]
    fn visible_is_cached_until_inputs_change() {
        let mut tab = tab();
        let first = tab.visible(false).as_ptr();
        assert_eq!(tab.visible(false).as_ptr(), first);
        tab.sort = (SortKey::Ext, true);
        assert_eq!(names(&mut tab)[0], "data");
    }

    #[test]
    fn selection_by_name_survives_refresh_and_ranges_work() {
        let mut tab = tab();
        tab.visible(false);
        tab.click("alpha.txt", false, false);
        tab.click("gamma.md", false, true);
        assert_eq!(tab.selected.len(), 3);
        let entries = tab.entries().to_vec();
        tab.set_entries(entries[..2].to_vec());
        assert_eq!(
            tab.selected.iter().collect::<Vec<_>>(),
            ["alpha.txt", "beta.rs"]
        );
        tab.visible(false);
        tab.invert_selection();
        assert!(tab.selected.is_empty());
        tab.move_cursor(Nav::End, false);
        assert_eq!(tab.cursor.as_deref(), Some("beta.rs"));
    }

    #[test]
    fn filtered_out_rows_are_never_targets() {
        let mut tab = tab();
        tab.visible(false);
        tab.select_all();
        tab.filter = "a".into();
        tab.visible(false);
        tab.filter = "al".into();
        // Narrowed from the "a" rows, still in sort order.
        assert_eq!(names(&mut tab), ["alpha.txt"]);
        let targets: Vec<&str> = tab.targets().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(targets, ["alpha.txt"]);
        tab.filter.clear();
        tab.visible(false);
        assert_eq!(tab.selected.len(), 1, "selection was trimmed by the filter");
        tab.cursor = Some("gamma.md".into());
        tab.selected.clear();
        tab.filter = "beta".into();
        tab.visible(false);
        assert!(tab.targets().is_empty(), "invisible cursor is not a target");
    }

    /// Polish backlog: per-keystroke and per-refresh cost on a 100,000-entry folder.
    /// Release only (the < 16 ms budget is checked there):
    /// `cargo test -p keel-app --release -- --ignored perf_100k --nocapture`.
    #[test]
    #[ignore]
    fn perf_100k() {
        use std::time::{Duration, Instant};
        let dir = VPath::parse("mem://t/").unwrap();
        let entries: Vec<Entry> = (0..100_000)
            .map(|i| {
                let kind = if i % 10 == 0 { Kind::Dir } else { Kind::File };
                test_entry(&dir, &format!("File {i} report-{}.txt", i % 977), kind, i)
            })
            .collect();
        let median = |mut runs: Vec<Duration>| {
            runs.sort();
            runs[runs.len() / 2]
        };
        let mut tab = Tab::new(dir.clone());
        let t = Instant::now();
        let listing = Listing::new(entries.clone());
        let lower = t.elapsed();
        // Refresh: a new listing arrives (watcher), sorted and filtered again.
        let mut refresh = Vec::new();
        for _ in 0..5 {
            let listing = listing.clone();
            let t = Instant::now();
            tab.set_listing(listing);
            tab.visible(false);
            refresh.push(t.elapsed());
        }
        // Keystrokes: typing narrows the cached rows; Backspace recomputes.
        let (mut narrow, mut widen) = (Vec::new(), Vec::new());
        for _ in 0..5 {
            tab.filter.clear();
            tab.visible(false);
            for c in "report-9".chars() {
                tab.filter.push(c);
                let t = Instant::now();
                tab.visible(false);
                narrow.push(t.elapsed());
            }
            while tab.filter.pop().is_some() {
                let t = Instant::now();
                tab.visible(false);
                widen.push(t.elapsed());
            }
        }
        let (refresh, narrow, widen) = (median(refresh), median(narrow), median(widen));
        eprintln!(
            "100k entries: lowercase (worker) {lower:?}, refresh {refresh:?}, \
             keystroke typing {narrow:?}, keystroke backspace {widen:?}"
        );
        if !cfg!(debug_assertions) {
            let budget = Duration::from_millis(16);
            assert!(refresh < budget && narrow < budget && widen < budget);
        }
    }

    fn cursor(tab: &Tab) -> &str {
        tab.cursor.as_deref().unwrap_or("")
    }

    /// Task 66: Right expands the folder at the cursor, its listing arrives through the
    /// listing worker, Left goes to the folder a row is in and then collapses it.
    #[test]
    fn disclosure_expands_and_collapses_with_the_keyboard() {
        let mut tab = tab();
        tab.disclosure = true;
        tab.visible(false);
        tab.click("data", false, false);
        tab.move_cursor(Nav::Right, false);
        assert!(tab.is_expanded("data"));
        let sub = tab.dir.join("data");
        assert_eq!(tab.take_wanted(), std::slice::from_ref(&sub));
        assert!(tab.take_wanted().is_empty(), "listed once");
        assert_eq!(tab.node_state("data"), Some(&NodeState::Loading));
        let listing = Listing::new(vec![
            test_entry(&sub, "z.txt", Kind::File, 1),
            test_entry(&sub, "inner", Kind::Dir, 0),
            test_entry(&sub, "a.txt", Kind::File, 2),
        ]);
        assert!(tab.fill_child(&sub, &Ok(listing)));
        assert_eq!(
            names(&mut tab),
            [
                "data",
                "data/inner",
                "data/a.txt",
                "data/z.txt",
                "alpha.txt",
                "beta.rs",
                "gamma.md"
            ]
        );
        let child = tab
            .entries()
            .iter()
            .find(|e| e.name == "data/a.txt")
            .unwrap();
        assert_eq!((tab.shown_name(child), tab.depth(child)), ("a.txt", 1));
        assert_eq!(child.path, sub.join("a.txt"));
        // Sorted like the folder's own rows.
        tab.sort = (SortKey::Size, false);
        assert_eq!(
            names(&mut tab)[..4],
            ["data", "data/inner", "data/a.txt", "data/z.txt"]
        );
        tab.sort = (SortKey::Name, false);
        assert_eq!(
            names(&mut tab)[..4],
            ["data", "data/inner", "data/z.txt", "data/a.txt"]
        );
        tab.sort = (SortKey::Name, true);
        tab.visible(false);
        // Down into the folder; Left goes to it, Left again collapses it.
        tab.move_cursor(Nav::Next, false);
        tab.move_cursor(Nav::Next, false);
        assert_eq!(cursor(&tab), "data/a.txt");
        let targets: Vec<VPath> = tab.targets().iter().map(|e| e.path.clone()).collect();
        assert_eq!(targets, [sub.join("a.txt")]);
        tab.move_cursor(Nav::Left, false);
        assert_eq!(cursor(&tab), "data");
        tab.move_cursor(Nav::Left, false);
        assert!(!tab.is_expanded("data"));
        assert_eq!(
            names(&mut tab),
            ["data", "alpha.txt", "beta.rs", "gamma.md"]
        );
        // Right on a file does nothing; the triangles off, Left/Right move as before.
        tab.move_cursor(Nav::End, false);
        tab.move_cursor(Nav::Right, false);
        assert_eq!(cursor(&tab), "gamma.md");
        tab.disclosure = false;
        tab.move_cursor(Nav::Left, false);
        assert_eq!(cursor(&tab), "beta.rs");
    }

    /// Task 66: a listing that answers after its folder collapsed is dropped; a failed
    /// one says why.
    #[test]
    fn collapsing_cancels_and_failures_are_kept() {
        let mut tab = tab();
        tab.disclosure = true;
        let sub = tab.dir.join("data");
        tab.expand("data", false);
        assert_eq!(tab.take_wanted(), std::slice::from_ref(&sub));
        tab.collapse("data");
        let late = Listing::new(vec![test_entry(&sub, "x", Kind::File, 1)]);
        assert!(!tab.fill_child(&sub, &Ok(late)));
        assert_eq!(names(&mut tab).len(), 4);
        tab.expand("data", false);
        tab.take_wanted();
        assert!(tab.fill_child(&sub, &Err(anyhow::anyhow!("denied"))));
        assert!(matches!(tab.node_state("data"), Some(NodeState::Failed(w)) if w == "denied"));
        // Files and unknown rows never expand.
        tab.expand("alpha.txt", false);
        tab.expand("nope", false);
        assert!(!tab.is_expanded("alpha.txt") && !tab.is_expanded("nope"));
    }

    /// Task 66: Alt+click expands every folder below as it is listed; collapsing the top
    /// collapses them all and brings a cursor inside back to it. A refresh keeps the
    /// expanded rows and lists them again; a folder that went away is dropped.
    #[test]
    fn expand_all_refresh_and_collapse() {
        let mut tab = tab();
        tab.disclosure = true;
        let sub = tab.dir.join("data");
        let inner = sub.join("inner");
        tab.expand("data", true);
        tab.take_wanted();
        let listing = Listing::new(vec![test_entry(&sub, "inner", Kind::Dir, 0)]);
        tab.fill_child(&sub, &Ok(listing));
        assert!(tab.is_expanded("data/inner"), "expanded as it arrived");
        assert_eq!(tab.take_wanted(), std::slice::from_ref(&inner));
        let listing = Listing::new(vec![test_entry(&inner, "deep.txt", Kind::File, 1)]);
        tab.fill_child(&inner, &Ok(listing));
        assert_eq!(
            names(&mut tab)[..3],
            ["data", "data/inner", "data/inner/deep.txt"]
        );
        let deep = tab
            .entries()
            .iter()
            .find(|e| e.name.ends_with("deep.txt"))
            .unwrap();
        assert_eq!(tab.depth(deep), 2);
        // Refresh: the rows stay while the folders are listed again.
        let top: Vec<Entry> = tab.entries()[..tab.top_len()].to_vec();
        tab.set_entries(top.clone());
        assert_eq!(names(&mut tab).len(), 6);
        let mut again = tab.take_wanted();
        again.sort_by_key(|d| d.display());
        assert_eq!(again, [sub.clone(), inner.clone()]);
        // Collapse with the cursor deep inside.
        tab.visible(false);
        tab.click("data/inner/deep.txt", false, false);
        tab.collapse("data");
        assert_eq!(cursor(&tab), "data");
        assert!(!tab.is_expanded("data/inner"));
        assert_eq!(names(&mut tab).len(), 4);
        // The folder went away: its node goes too.
        tab.expand("data", false);
        tab.set_entries(top.into_iter().filter(|e| e.name != "data").collect());
        assert!(!tab.is_expanded("data"));
        // Navigating starts collapsed.
        tab.navigate(VPath::parse("mem://t/elsewhere").unwrap());
        assert!(tab.take_wanted().is_empty());
    }

    fn grouped_tab() -> Tab {
        let dir = VPath::parse("mem://g/").unwrap();
        let mut tab = Tab::new(dir.clone());
        tab.set_entries(vec![
            test_entry(&dir, "a.txt", Kind::File, 5),
            test_entry(&dir, "b.md", Kind::File, 1),
            test_entry(&dir, "c.txt", Kind::File, 9),
            test_entry(&dir, "d", Kind::Dir, 0),
            test_entry(&dir, "e.md", Kind::File, 7),
        ]);
        group_by_kind(&mut tab);
        tab
    }

    fn group_by_kind(tab: &mut Tab) {
        use crate::list_view::{groups_of, GroupBy, GroupInputs};
        let created = Default::default();
        let inp = GroupInputs {
            today: chrono::NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
            created: &created,
            tags: &|_| None,
            bytes: &|_| None,
        };
        let groups = groups_of(GroupBy::Kind, &tab.entries()[..tab.top_len()], &inp);
        tab.set_groups(Some(tab.generation()), Some(groups));
    }

    fn labels(tab: &Tab) -> Vec<(usize, &str)> {
        (tab.group_starts().iter())
            .map(|&(pos, g)| (pos, tab.group_label(g)))
            .collect()
    }

    /// Task 66: Group by orders the groups, the sort orders the rows inside each group.
    #[test]
    fn group_by_is_independent_of_the_sort() {
        let mut tab = grouped_tab();
        assert_eq!(names(&mut tab), ["d", "b.md", "e.md", "a.txt", "c.txt"]);
        assert_eq!(
            labels(&tab),
            [(0, "Folder"), (1, "Markdown document"), (3, "Plain text")]
        );
        tab.sort = (SortKey::Size, false);
        assert_eq!(names(&mut tab), ["d", "e.md", "b.md", "c.txt", "a.txt"]);
        assert_eq!(labels(&tab).len(), 3);
        // A filter narrows the groups too; empty groups have no header.
        tab.filter = "t".into();
        assert_eq!(names(&mut tab), ["c.txt", "a.txt"]);
        assert_eq!(labels(&tab), [(0, "Plain text")]);
        tab.filter = "tx".into();
        assert_eq!(names(&mut tab), ["c.txt", "a.txt"]);
        tab.filter.clear();
        // Ungrouped: back to folders first, then the sort.
        tab.set_groups(None, None);
        assert_eq!(names(&mut tab), ["d", "c.txt", "e.md", "a.txt", "b.md"]);
        assert!(tab.group_starts().is_empty());
        // A new listing drops groups computed for the old one.
        group_by_kind(&mut tab);
        let entries = tab.entries().to_vec();
        tab.set_entries(entries);
        assert!(tab.group_key().is_none());
        tab.visible(false);
        assert!(tab.group_starts().is_empty());
    }

    /// Task 66: rows of an expanded folder stay under it, in its group.
    #[test]
    fn expanded_rows_stay_in_their_folders_group() {
        let mut tab = grouped_tab();
        tab.disclosure = true;
        let sub = tab.dir.join("d");
        tab.expand("d", false);
        tab.take_wanted();
        let listing = Listing::new(vec![test_entry(&sub, "x.pdf", Kind::File, 1)]);
        tab.fill_child(&sub, &Ok(listing));
        assert_eq!(
            names(&mut tab),
            ["d", "d/x.pdf", "b.md", "e.md", "a.txt", "c.txt"]
        );
        assert_eq!(
            labels(&tab),
            [(0, "Folder"), (2, "Markdown document"), (4, "Plain text")]
        );
        assert_eq!(
            tab.group_count(0),
            1,
            "rows inside the folder are not counted"
        );
        assert_eq!(tab.group_count(1), 2);
    }

    /// Task 66: Up/Down in a grouped grid keep the column inside a group and carry it
    /// into the next or previous group, whose rows start under its header.
    #[test]
    fn grouped_grid_rows_move_by_group() {
        let mut tab = grouped_tab();
        tab.visible(false);
        // Two tiles a row: [d] / [b.md e.md] / [a.txt c.txt].
        tab.row_step = 2;
        tab.click("d", false, false);
        tab.move_cursor(Nav::Next, false);
        assert_eq!(cursor(&tab), "b.md");
        tab.click("e.md", false, false);
        tab.move_cursor(Nav::Next, false);
        assert_eq!(cursor(&tab), "c.txt");
        tab.move_cursor(Nav::Next, false);
        assert_eq!(cursor(&tab), "c.txt", "last row stays");
        tab.move_cursor(Nav::Prev, false);
        assert_eq!(cursor(&tab), "e.md");
        tab.move_cursor(Nav::Prev, false);
        assert_eq!(cursor(&tab), "d");
        tab.move_cursor(Nav::Prev, false);
        assert_eq!(cursor(&tab), "d");
    }

    /// Task 66: a 100,000-entry folder with Group by: grouping once per listing, and the
    /// usual keystroke cost when nothing is expanded. Release only, like `perf_100k`.
    #[test]
    #[ignore]
    fn perf_100k_grouped() {
        use std::time::{Duration, Instant};
        let dir = VPath::parse("mem://t/").unwrap();
        let entries: Vec<Entry> = (0..100_000)
            .map(|i| {
                let kind = if i % 10 == 0 { Kind::Dir } else { Kind::File };
                let ext = ["txt", "pdf", "png", "rs"][i % 4];
                test_entry(
                    &dir,
                    &format!("File {i} report-{}.{ext}", i % 977),
                    kind,
                    i as u64,
                )
            })
            .collect();
        let mut tab = Tab::new(dir);
        tab.disclosure = true;
        tab.set_listing(Listing::new(entries));
        let t = Instant::now();
        group_by_kind(&mut tab);
        tab.visible(false);
        let group = t.elapsed();
        let t = Instant::now();
        tab.filter = "report-9".into();
        tab.visible(false);
        let keystroke = t.elapsed();
        eprintln!("100k entries grouped by kind: group {group:?}, keystroke {keystroke:?}");
        if !cfg!(debug_assertions) {
            assert!(keystroke < Duration::from_millis(16));
        }
    }

    #[test]
    fn history_back_forward_up() {
        let mut tab = Tab::new(VPath::parse("mem://t/a/b").unwrap());
        assert!(tab.up());
        assert_eq!(tab.dir.path, "/a");
        assert_eq!(tab.cursor.as_deref(), Some("b"));
        assert!(tab.back());
        assert_eq!(tab.dir.path, "/a/b");
        assert!(tab.forward());
        assert_eq!(tab.dir.path, "/a");
    }
}
