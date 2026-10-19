//! Phase 17 Task 82: the **Disk Usage** window (Ctrl+Shift+U, the palette, a drive's
//! context menu "Analyze disk usage"). It scans drives and folders with
//! [`keel_vfs::du`] on a worker and shows the result as WinDirStat does: the directory
//! tree top-left, the file types top-right and the treemap below ([`chart`], Task 83's
//! widgets: treemap, sunburst or flame graph), all sharing one selection ([`Model`]).
//!
//! - **Select drives** ([`picker`]): all local drives, some drives (used / free bars,
//!   double-click scans one), or folders.
//! - **Tree list**: expandable, a Subtree % bar colored by depth, every column of
//!   [`Column`] (right-click a header to choose), sorting by any, multi-select with the
//!   selection's size in the status line, logical or on-disk sizes.
//! - **File types**: color, extension, the OS's description, bytes, %, files;
//!   unregistered extensions grouped on request; double-click lists only that type in
//!   the tree.
//!
//! - **File Watcher** ([`watcher`]): after a scan the folders are watched and what changes
//!   is applied to the tree live; the **Changes** tab lists it.
//! - **Owner** column: read for the rows shown only, names cached per account.
//! - **Relaunch elevated** ([`elevate`]): offered when folders refused to be listed.
//! - **Scan priority**: the CPU and I/O priority of the scan's threads (Options).
//! - **Recent scans** ([`history`]): the previous scans, to run again.
//! - A **Disk usage** view mode in a pane ([`view`]) shows the same tree list for the
//!   pane's folder.
//!
//! Scans run with the scanner's safe defaults (links not followed, hard links counted
//! once, cloud placeholders never read). A newer scan replaces an older one: the older
//! one is cancelled and its result, should it still arrive, is dropped with its channel.

pub mod chart;
pub mod elevate;
pub mod history;
pub mod model;
mod os;
pub mod picker;
pub mod steps;
#[cfg(test)]
mod tests;
pub mod ui;
pub mod view;
pub mod watcher;

pub use model::{Column, FileType, Model};

use keel_vfs::du::owner::Owners;
use keel_vfs::du::{self, NodeId, Priority, Root, ScanHandle, ScanOptions, Tree};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

/// The window's title (also its egui id, for Esc and the keyboard).
pub const TITLE: &str = "Disk Usage";

/// What the window is asked to do (`Action::DiskUsage`).
#[derive(Clone, Debug, PartialEq)]
pub enum DuCmd {
    /// Ctrl+Shift+U, the palette: shows the window, with Select drives when nothing has
    /// been scanned.
    Open,
    /// Shows the window and scans these (a drive's context menu, Select drives).
    Scan(Vec<Root>),
    /// The palette: scans the active pane's folder (local folders only).
    ScanCurrent,
    /// An analysis tab's operations, run one step at a time (`steps`).
    Plans(Vec<keel_core::Op>),
}

/// The views in the window's main area: the tree, or one of the analysis tabs
/// (`crate::du_tabs`, Task 84).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Tree,
    Largest,
    Duplicates,
    Search,
    Storage,
    Permissions,
    /// The File Watcher's change list.
    Changes,
}

impl Tab {
    pub const ALL: [Self; 7] = [
        Self::Tree,
        Self::Largest,
        Self::Duplicates,
        Self::Search,
        Self::Storage,
        Self::Permissions,
        Self::Changes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Tree => "Tree",
            Self::Largest => "Largest files",
            Self::Duplicates => "Duplicates",
            Self::Search => "Search",
            Self::Storage => "Storage analytics",
            Self::Permissions => "Permissions",
            Self::Changes => crate::i18n::tr("disk_usage.changes_tab"),
        }
    }
}

/// The analysis tabs' own states (kept across scans; each notices a new scan itself).
#[derive(Default)]
pub struct Tabs {
    pub largest: crate::du_tabs::largest::State,
    pub duplicates: crate::du_tabs::duplicates::State,
    pub search: crate::du_tabs::search::State,
    pub storage: crate::du_tabs::storage::State,
    pub permissions: crate::du_tabs::permissions::State,
}

/// How the window is arranged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// WinDirStat's: the tree top-left, the file types top-right, the treemap below.
    #[default]
    Classic,
    /// The treemap on the left, the tree and the file types stacked on the right.
    TreemapLeft,
    /// The tree and the file types side by side, no treemap.
    ListsOnly,
    /// The tree list alone: no file types, no treemap.
    TreeOnly,
}

impl Layout {
    pub const ALL: [Self; 4] = [
        Self::Classic,
        Self::TreemapLeft,
        Self::ListsOnly,
        Self::TreeOnly,
    ];

    pub fn label(self) -> &'static str {
        crate::i18n::tr(match self {
            Self::Classic => "disk_usage.layout_classic",
            Self::TreemapLeft => "disk_usage.layout_treemap_left",
            Self::ListsOnly => "disk_usage.layout_lists",
            Self::TreeOnly => "disk_usage.layout_tree",
        })
    }

    /// Whether the layout shows the file types list.
    pub fn shows_types(self) -> bool {
        !matches!(self, Self::TreeOnly)
    }

    /// Whether the layout shows the treemap.
    pub fn shows_treemap(self) -> bool {
        matches!(self, Self::Classic | Self::TreemapLeft)
    }
}

/// `[disk_usage]` in the settings: the window's options and splitter positions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DuSettings {
    pub layout: Layout,
    /// The tree list's columns, in order (Name always first).
    pub columns: Vec<Column>,
    /// Sizes on disk instead of lengths.
    pub physical: bool,
    /// Percentage is the share of the parent folder instead of the whole scan.
    pub relative: bool,
    /// One "Unregistered" row for every extension the OS has no description for.
    pub group_unregistered: bool,
    /// Lines between the tree list's columns.
    pub grid: bool,
    /// Alternate row shading.
    pub stripes: bool,
    /// Times with seconds.
    pub seconds: bool,
    /// Splitters: the file types list's width, the treemap's height (Classic) or width
    /// (Treemap on the left), the tree's height when stacked.
    pub types_width: f32,
    pub treemap_size: f32,
    pub tree_height: f32,
    /// The chart in the treemap's place, and its options (style, layout, colors).
    pub chart: crate::treemap::widget::Chart,
    pub chart_options: crate::treemap::Options,
    /// Each drive's free space and unknown space as tiles of the chart.
    pub chart_free: bool,
    pub chart_unknown: bool,
    /// The CPU and I/O priority of scans.
    pub priority: Priority,
    /// Scan into junctions (Windows) and folder mount points; off like WinDirStat.
    pub follow_junctions: bool,
    pub follow_mount_points: bool,
    /// Leave hidden and system items out of scans.
    pub exclude_hidden: bool,
    pub exclude_system: bool,
    /// Watch the scanned folders and apply their changes (the File Watcher).
    pub watch: bool,
    /// The previous scans, newest first.
    pub history: Vec<history::Entry>,
}

impl Default for DuSettings {
    fn default() -> Self {
        DuSettings {
            layout: Layout::Classic,
            columns: Column::DEFAULT.to_vec(),
            physical: false,
            relative: false,
            group_unregistered: false,
            grid: false,
            stripes: true,
            seconds: false,
            types_width: 360.0,
            treemap_size: 260.0,
            tree_height: 320.0,
            chart: Default::default(),
            chart_options: Default::default(),
            chart_free: true,
            chart_unknown: true,
            priority: Priority::Normal,
            follow_junctions: false,
            follow_mount_points: false,
            exclude_hidden: false,
            exclude_system: false,
            watch: true,
            history: Vec::new(),
        }
    }
}

impl DuSettings {
    /// The columns to draw: Name first, each once, in the order chosen.
    pub fn shown_columns(&self) -> Vec<Column> {
        let mut out = vec![Column::Name];
        for c in &self.columns {
            if !out.contains(c) {
                out.push(*c);
            }
        }
        out
    }

    /// Shows or hides `c` (Name cannot be hidden); a shown column keeps its place in
    /// [`Column::ALL`]'s order.
    pub fn toggle_column(&mut self, c: Column) {
        if c == Column::Name {
            return;
        }
        if self.columns.contains(&c) {
            self.columns.retain(|x| *x != c);
        } else {
            self.columns.push(c);
            let rank = |c: &Column| Column::ALL.iter().position(|x| x == c);
            self.columns.sort_by_key(rank);
        }
    }
}

/// A scan on its worker.
struct Scanning {
    roots: Vec<Root>,
    handle: ScanHandle,
    rx: Receiver<anyhow::Result<Scanned>>,
}

/// A finished scan and the OS's descriptions of its extensions.
struct Scanned {
    tree: Tree,
    descriptions: HashMap<String, String>,
}

/// Owner and times read for some rows, on a worker.
struct MetaFetch {
    /// The tree these ids belong to.
    generation: u64,
    rx: Receiver<Vec<(NodeId, model::Metadata)>>,
}

/// Rows read per metadata batch.
const META_BATCH: usize = 200;

/// Reads the owners and times of the rows shown (Owner, Last accessed, Created columns),
/// a batch at a time on a worker; owners are named through one cache.
#[derive(Default)]
pub struct MetaReader {
    /// The tree shown: a batch read for an older one is dropped.
    generation: u64,
    fetch: Option<MetaFetch>,
    /// Rows asked for (this generation).
    asked: HashSet<NodeId>,
    owners: Arc<Owners>,
}

impl MetaReader {
    /// A new tree is shown: what was asked is asked again.
    pub fn reset(&mut self, generation: u64) {
        self.generation = generation;
        self.asked.clear();
        self.fetch = None;
    }

    /// Reads the metadata of `rows` not read yet (one batch at a time).
    pub fn want(&mut self, model: &Model, rows: impl IntoIterator<Item = NodeId>) {
        if self.fetch.is_some() {
            return;
        }
        let ids: Vec<NodeId> = (rows.into_iter())
            .filter(|id| !self.asked.contains(id))
            .take(META_BATCH)
            .collect();
        if ids.is_empty() {
            return;
        }
        self.asked.extend(ids.iter().copied());
        let jobs: Vec<(NodeId, std::path::PathBuf)> = (ids.into_iter())
            .filter(|&id| !model.tree.on_device(id))
            .filter_map(|id| Some((id, model.tree.path(id)?)))
            .collect();
        let (tx, rx) = channel();
        let owners = self.owners.clone();
        crate::worker::spawn("keel-du-meta", move || {
            let batch = (jobs.into_iter())
                .map(|(id, path)| (id, os::metadata(&path, &owners)))
                .collect();
            let _ = tx.send(batch);
        });
        self.fetch = Some(MetaFetch {
            generation: self.generation,
            rx,
        });
    }

    /// Takes in a finished batch.
    pub fn poll(&mut self, ctx: &egui::Context, model: Option<&mut Model>) {
        let Some(m) = &self.fetch else { return };
        match m.rx.try_recv() {
            Ok(batch) => {
                let current = m.generation == self.generation;
                self.fetch = None;
                if let Some(model) = model.filter(|_| current) {
                    model.metadata.extend(batch);
                    if model.sort.0.metadata() {
                        model.rebuild();
                    }
                }
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            Err(TryRecvError::Disconnected) => self.fetch = None,
        }
    }

    /// Owner names looked up so far (each once).
    #[cfg(test)]
    pub fn owner_lookups(&self) -> u64 {
        self.owners.cache.lookups()
    }
}

#[derive(Default)]
pub struct DiskUsage {
    pub open: bool,
    /// The window has the keyboard (the last click was in it, or it was just opened).
    pub keys: bool,
    pub picker: Option<picker::Picker>,
    scan: Option<Scanning>,
    pub model: Option<Model>,
    /// The roots of the shown scan (Rescan).
    roots: Vec<Root>,
    pub error: Option<String>,
    descriptions: HashMap<String, String>,
    /// The file types list, built for `(group_unregistered, physical)`.
    types_key: Option<(bool, bool)>,
    types: Vec<FileType>,
    /// Each extension's row in `types` (its color rank).
    ranks: HashMap<String, usize>,
    /// Bumped per tree shown (a scan, or the File Watcher's changes): the chart is built
    /// again, metadata read for an older tree is dropped.
    generation: u64,
    /// Bumped per scan shown (the File Watcher's batches belong to one).
    scan_id: u64,
    pub meta: MetaReader,
    /// The File Watcher.
    pub watch: watcher::FileWatch,
    /// The scan priority and whether to watch, as last set (`ui::window` copies them from
    /// the settings each frame).
    pub priority: Priority,
    /// The scan switches, copied from the settings each frame like `priority`.
    pub follow_junctions: bool,
    pub follow_mount_points: bool,
    pub exclude_hidden: bool,
    pub exclude_system: bool,
    /// Scroll the tree to the cursor next frame.
    pub scroll_tree: bool,
    /// Scroll the file types list to its selected row next frame.
    pub scroll_types: bool,
    /// The window's rect last frame (where a click gives it the keys).
    pub rect: Option<egui::Rect>,
    /// Bring the window to the front next frame.
    pub raise: bool,
    /// The chart in the treemap's place.
    pub chart: chart::Host,
    /// The treemap's colors by size rank (the file types list's swatches follow them).
    pub palette: Vec<[u8; 3]>,
    pub tab: Tab,
    pub tabs: Tabs,
    /// Operations through the library's previews, one at a time.
    pub steps: steps::Steps,
    /// The next step to preview (sent by `AppState::disk_usage_tick`).
    pub next_plan: Option<keel_core::Op>,
    /// Operations confirmed and run by the window itself (no library).
    pub fallback: Option<steps::Fallback>,
    /// The last steps' outcome, shown until dismissed.
    pub note: Option<String>,
    /// The device storages' names the last Select drives dialog listed ("Scanning …").
    pub device_labels: Vec<(Root, String)>,
}

impl DiskUsage {
    pub fn command(&mut self, cmd: DuCmd, drives: &[crate::sidebar::Drive]) {
        match cmd {
            DuCmd::Open => {
                self.open = true;
                self.keys = true;
                self.raise = true;
                if self.model.is_none() && self.scan.is_none() {
                    self.picker = Some(picker::Picker::new(drives));
                }
            }
            DuCmd::Scan(roots) => self.start(roots),
            // Resolved by `AppState`.
            DuCmd::ScanCurrent | DuCmd::Plans(_) => {}
        }
    }

    /// Scans `roots` (cancelling a scan still running) and shows the window.
    pub fn start(&mut self, roots: Vec<Root>) {
        self.open = true;
        self.keys = true;
        self.raise = true;
        self.picker = None;
        if roots.is_empty() {
            return;
        }
        if let Some(old) = self.scan.take() {
            old.handle.cancel();
        }
        self.error = None;
        let handle = ScanHandle::new();
        let (tx, rx) = channel();
        let (h, r) = (handle.clone(), roots.clone());
        let options = self.scan_options();
        crate::worker::spawn("keel-du-scan", move || {
            let result = du::scan(&r, &options, &h).map(|tree| {
                let descriptions = os::descriptions(tree.exts().iter().map(|e| &**e));
                Scanned { tree, descriptions }
            });
            let _ = tx.send(result);
        });
        self.scan = Some(Scanning { roots, handle, rx });
    }

    pub fn rescan(&mut self) {
        if !self.roots.is_empty() {
            self.start(self.roots.clone());
        }
    }

    pub fn scanning(&self) -> Option<(&[Root], du::Progress)> {
        (self.scan.as_ref()).map(|s| (&s.roots[..], s.handle.progress()))
    }

    pub fn pause(&self, on: bool) {
        if let Some(s) = &self.scan {
            if on {
                s.handle.pause();
            } else {
                s.handle.resume();
            }
        }
    }

    /// Stops the scan; what it found so far is shown.
    pub fn cancel(&self) {
        if let Some(s) = &self.scan {
            s.handle.resume();
            s.handle.cancel();
        }
    }

    /// The options a scan from this window runs with.
    fn scan_options(&self) -> ScanOptions {
        let mut options = ScanOptions {
            priority: self.priority,
            follow_junctions: self.follow_junctions,
            follow_mount_points: self.follow_mount_points,
            ..ScanOptions::default()
        };
        options.filters.exclude_hidden = self.exclude_hidden;
        options.filters.exclude_system = self.exclude_system;
        options
    }

    /// Takes in a finished scan, finished metadata reads and the File Watcher's changes.
    /// Called every frame; repaints while a scan runs.
    pub fn poll(&mut self, ctx: &egui::Context, settings: &mut DuSettings) {
        self.priority = settings.priority;
        self.follow_junctions = settings.follow_junctions;
        self.follow_mount_points = settings.follow_mount_points;
        self.exclude_hidden = settings.exclude_hidden;
        self.exclude_system = settings.exclude_system;
        if let Some(s) = &self.scan {
            match s.rx.try_recv() {
                Ok(result) => {
                    let roots = self.scan.take().map(|s| s.roots).unwrap_or_default();
                    match result {
                        Ok(scanned) => self.show(scanned, roots, settings),
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => {
                    self.scan = None;
                    self.error = Some("The scan stopped unexpectedly".into());
                }
            }
        }
        self.meta.poll(ctx, self.model.as_mut());
        if !settings.watch && (self.watch.watching() || self.watch.starting()) {
            self.watch.stop();
        } else if settings.watch && !self.watch.watching() && !self.watch.starting() {
            if let Some(m) = &self.model {
                // Turned on after the scan: watch it from now.
                if self.watch.error.is_none() {
                    self.watch.start(watch_roots(&m.tree), self.scan_id);
                }
            }
        }
        let tree = self.model.as_ref().map(|m| m.tree.clone());
        if let Some(u) = self.watch.poll(ctx, tree.as_ref(), self.scan_id) {
            if let Some(m) = &mut self.model {
                m.replace_tree(Arc::new(u.tree), u.applied.remap.as_deref());
                self.generation += 1;
                self.meta.reset(self.generation);
                self.types_key = None;
            }
        }
    }

    fn show(&mut self, scanned: Scanned, roots: Vec<Root>, settings: &mut DuSettings) {
        let mut model = Model::new(Arc::new(scanned.tree));
        model.physical = settings.physical;
        model.relative = settings.relative;
        model.rebuild();
        if let Some(&(first, _)) = model.rows.first() {
            model.select(first, false, false);
        }
        self.model = Some(model);
        self.descriptions = scanned.descriptions;
        self.types_key = None;
        self.generation += 1;
        self.scan_id += 1;
        self.meta.reset(self.generation);
        if let Some(m) = &self.model {
            if let Some(paths) = history::folder_roots(&roots) {
                let top = m.tree.node(du::TOP);
                let entry = history::Entry {
                    roots: paths,
                    scanned: chrono::Utc::now().timestamp(),
                    bytes: top.size,
                    files: top.files,
                };
                history::remember(&mut settings.history, entry);
            }
            self.watch.stop();
            if settings.watch {
                self.watch.start(watch_roots(&m.tree), self.scan_id);
            }
        }
        self.roots = roots;
    }

    /// The folders of the scan shown an elevated scan may read more of (some refused to be
    /// listed), when this Keel can relaunch elevated.
    pub fn denied_roots(&self) -> Vec<std::path::PathBuf> {
        match &self.model {
            Some(m) if elevate::available() => m.tree.denied_roots(),
            _ => Vec::new(),
        }
    }

    /// The file types list (built again when the grouping or the size kind changes).
    pub fn types(&mut self, group: bool) -> &[FileType] {
        let Some(model) = &self.model else {
            return &[];
        };
        let key = (group, model.physical);
        if self.types_key != Some(key) {
            let d = &self.descriptions;
            self.types = model.file_types(group, &|ext: &str| d.get(ext).cloned());
            self.ranks = (self.types.iter().enumerate())
                .flat_map(|(rank, t)| t.extensions.iter().map(move |e| (e.clone(), rank)))
                .collect();
            self.types_key = Some(key);
        }
        &self.types
    }

    /// The file types as last built by [`types`](Self::types).
    pub fn types_cached(&self) -> &[FileType] {
        &self.types
    }

    /// Each extension's color rank, as last built by [`types`](Self::types).
    #[cfg(test)]
    pub fn ranks(&self) -> &HashMap<String, usize> {
        &self.ranks
    }
}

/// The folders of `tree` the File Watcher watches (not portable devices).
fn watch_roots(tree: &Tree) -> Vec<std::path::PathBuf> {
    (tree.roots().iter())
        .filter(|r| r.device.is_none())
        .map(|r| r.path.clone())
        .collect()
}
