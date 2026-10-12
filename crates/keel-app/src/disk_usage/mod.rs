//! Phase 17 Task 82: the **Disk Usage** window (Ctrl+Shift+U, the palette, a drive's
//! context menu "Analyze disk usage"). It scans drives and folders with
//! [`keel_vfs::du`] on a worker and shows the result as WinDirStat does: the directory
//! tree top-left, the file types top-right and the treemap below ([`ui::treemap_slot`],
//! filled by Task 83), all sharing one selection ([`Model`]).
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
//! Scans run with the scanner's safe defaults (links not followed, hard links counted
//! once, cloud placeholders never read). A newer scan replaces an older one: the older
//! one is cancelled and its result, should it still arrive, is dropped with its channel.

pub mod model;
mod os;
pub mod picker;
#[cfg(test)]
mod tests;
pub mod ui;

pub use model::{Column, FileType, Model};

use keel_vfs::du::{self, NodeId, Root, ScanHandle, ScanOptions, Tree};
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
}

impl Layout {
    pub const ALL: [Self; 3] = [Self::Classic, Self::TreemapLeft, Self::ListsOnly];

    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Tree and types above the treemap",
            Self::TreemapLeft => "Treemap on the left",
            Self::ListsOnly => "Lists only",
        }
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
    /// The scan these ids belong to.
    generation: u64,
    rx: Receiver<Vec<(NodeId, model::Metadata)>>,
}

/// Rows read per metadata batch.
const META_BATCH: usize = 200;

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
    /// Bumped per scan shown: metadata read for an older one is dropped.
    generation: u64,
    meta: Option<MetaFetch>,
    /// Rows whose metadata was asked for (this generation).
    meta_asked: HashSet<NodeId>,
    /// Scroll the tree to the cursor next frame.
    pub scroll_tree: bool,
    /// Scroll the file types list to its selected row next frame.
    pub scroll_types: bool,
    /// The window's rect last frame (where a click gives it the keys).
    pub rect: Option<egui::Rect>,
    /// Bring the window to the front next frame.
    pub raise: bool,
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
            // Resolved by `AppState` into `Scan`.
            DuCmd::ScanCurrent => {}
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
        crate::worker::spawn("keel-du-scan", move || {
            let result = du::scan(&r, &ScanOptions::default(), &h).map(|tree| {
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

    /// Takes in a finished scan and finished metadata reads. Called every frame the
    /// window is open; repaints while a scan runs.
    pub fn poll(&mut self, ctx: &egui::Context, settings: &DuSettings) {
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
        if let Some(m) = &self.meta {
            match m.rx.try_recv() {
                Ok(batch) => {
                    let current = m.generation == self.generation;
                    self.meta = None;
                    if let Some(model) = self.model.as_mut().filter(|_| current) {
                        model.metadata.extend(batch);
                        if model.sort.0.metadata() {
                            model.rebuild();
                        }
                    }
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => self.meta = None,
            }
        }
    }

    fn show(&mut self, scanned: Scanned, roots: Vec<Root>, settings: &DuSettings) {
        let mut model = Model::new(Arc::new(scanned.tree));
        model.physical = settings.physical;
        model.relative = settings.relative;
        model.rebuild();
        if let Some(&(first, _)) = model.rows.first() {
            model.select(first, false, false);
        }
        self.model = Some(model);
        self.descriptions = scanned.descriptions;
        self.roots = roots;
        self.types_key = None;
        self.generation += 1;
        self.meta_asked.clear();
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
    pub fn ranks(&self) -> &HashMap<String, usize> {
        &self.ranks
    }

    /// Reads the owner and times of `rows` that have not been read (one batch at a time,
    /// on a worker).
    pub fn want_metadata(&mut self, rows: impl Iterator<Item = NodeId>) {
        if self.meta.is_some() {
            return;
        }
        let Some(model) = &self.model else { return };
        let ids: Vec<NodeId> = rows
            .filter(|id| !self.meta_asked.contains(id))
            .take(META_BATCH)
            .collect();
        if ids.is_empty() {
            return;
        }
        self.meta_asked.extend(ids.iter().copied());
        let jobs: Vec<(NodeId, std::path::PathBuf)> = (ids.into_iter())
            .filter_map(|id| Some((id, model.tree.path(id)?)))
            .collect();
        let (tx, rx) = channel();
        crate::worker::spawn("keel-du-meta", move || {
            let batch = (jobs.into_iter())
                .map(|(id, path)| (id, os::metadata(&path)))
                .collect();
            let _ = tx.send(batch);
        });
        self.meta = Some(MetaFetch {
            generation: self.generation,
            rx,
        });
    }
}
