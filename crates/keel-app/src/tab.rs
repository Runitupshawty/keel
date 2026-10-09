//! One tab of a pane: a directory, its listing, selection, sort, filter and history.
//! Pure state; `AppState` spawns the listing workers.

use keel_vfs::{Entry, Kind, VPath};
use std::collections::{BTreeSet, HashSet};
use std::time::Instant;

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
        let order = sorted(&entries, &lower, (SortKey::Name, true));
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
    },
    /// Task 29: the library dashboard (`library_ui::overview`); never listed.
    Overview,
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
    sorted_key: Option<(u64, (SortKey, bool))>,
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
    generation: u64,
    cache_key: Option<(u64, String, (SortKey, bool), bool)>,
    cache: Vec<usize>,
    /// Bumped whenever `cache` changes (Task 32: the media view's layout cache).
    cache_gen: u64,
}

impl Tab {
    pub fn new(dir: VPath) -> Self {
        let overview = dir == crate::library::overview_path();
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
            loading: !overview,
            error: None,
            listed_dir: None,
            listed_req: 0,
            kind: if overview {
                TabKind::Overview
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
            generation: 0,
            cache_key: None,
            cache: Vec::new(),
            cache_gen: 0,
        }
    }

    /// A search tab started from `dir` (where it pastes nothing and lists nothing).
    pub fn search(dir: VPath) -> Self {
        Self {
            kind: TabKind::Search {
                query: String::new(),
                due: None,
                req: 0,
            },
            loading: false,
            listed_dir: Some(dir.clone()),
            ..Self::new(dir)
        }
    }

    pub fn is_search(&self) -> bool {
        matches!(self.kind, TabKind::Search { .. })
    }

    /// The name shown for `e`: search rows are keyed by full path but show the file name.
    pub fn shown_name<'a>(&self, e: &'a Entry) -> &'a str {
        if self.is_search() {
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
            TabKind::Search { query, .. } if query.is_empty() => "Search".into(),
            TabKind::Search { query, .. } => format!("Search: {query}"),
            TabKind::Overview => "Overview".into(),
            TabKind::Dir => match self.dir.name() {
                "" if self.dir.scheme == keel_vfs::library::SCHEME => {
                    crate::library::label_of(&self.dir.authority)
                        .unwrap_or_else(|| "Library".into())
                }
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

    /// Replaces the listing; selection names that no longer exist are dropped.
    pub fn set_listing(&mut self, listing: Listing) {
        let old = std::mem::replace(&mut self.entries, listing.entries);
        let old_lower = std::mem::replace(&mut self.lower, listing.lower);
        self.order = listing.order;
        self.generation += 1;
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

    /// Indices into `entries()` after hidden/filter/sort, dirs first. Cached until the
    /// entries, filter, sort or `show_hidden` change; a filter that only grew narrows the
    /// cached rows instead of sorting again. Selection is trimmed to the visible rows.
    pub fn visible(&mut self, show_hidden: bool) -> &[usize] {
        let key = (self.generation, self.filter.clone(), self.sort, show_hidden);
        match &self.cache_key {
            Some(old) if *old == key => return &self.cache,
            Some((g, f, s, h))
                if (*g, *s, *h) == (key.0, key.2, key.3)
                    && key.1.to_lowercase().starts_with(&f.to_lowercase()) =>
            {
                let needle = key.1.to_lowercase();
                let lower = &self.lower;
                self.cache.retain(|&i| lower[i].contains(&needle));
            }
            _ => {
                let sort_key = (self.generation, self.sort);
                if self.sorted_key != Some(sort_key) {
                    self.sorted = match self.sort {
                        (SortKey::Name, true) => self.order.clone(),
                        (SortKey::Name, false) => {
                            // Folders stay first; each group reversed.
                            let dirs = self
                                .order
                                .iter()
                                .take_while(|&&i| self.entries[i].kind == Kind::Dir)
                                .count();
                            let (d, f) = self.order.split_at(dirs);
                            d.iter().rev().chain(f.iter().rev()).copied().collect()
                        }
                        sort => sorted(&self.entries, &self.lower, sort),
                    };
                    self.sorted_key = Some(sort_key);
                }
                let needle = self.filter.to_lowercase();
                let (entries, lower) = (&self.entries, &self.lower);
                self.cache = self
                    .sorted
                    .iter()
                    .copied()
                    .filter(|&i| (show_hidden || !entries[i].hidden) && lower[i].contains(&needle))
                    .collect();
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

    /// Changes with every new listing.
    pub fn generation(&self) -> u64 {
        self.generation
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
        if let TabKind::Search { query, req, .. } = kind {
            self.left_search = Some(TabKind::Search {
                query,
                due: None,
                req,
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
        let page = self.page_rows.max(1);
        let step = self.row_step.max(1);
        let pos = match (nav, cur) {
            (Nav::Home, _) => 0,
            (Nav::End, _) => len - 1,
            (_, None) => 0,
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
            .is_none_or(|k| k.0 != self.generation)
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

/// Every index of `entries` in `(key, asc)` order, folders first.
fn sorted(entries: &[Entry], lower: &[String], (key, asc): (SortKey, bool)) -> Vec<usize> {
    let mut keyed: Vec<(usize, &str)> = lower.iter().map(String::as_str).enumerate().collect();
    keyed.sort_by(|(ai, an), (bi, bn)| {
        let (a, b) = (&entries[*ai], &entries[*bi]);
        let by_name = || natord::compare(an, bn);
        let ord = match key {
            SortKey::Name => by_name(),
            SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
            SortKey::Modified => a.modified.cmp(&b.modified).then_with(by_name),
            SortKey::Ext => a.ext.cmp(&b.ext).then_with(by_name),
        };
        let ord = if asc { ord } else { ord.reverse() };
        (a.kind != Kind::Dir).cmp(&(b.kind != Kind::Dir)).then(ord)
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
