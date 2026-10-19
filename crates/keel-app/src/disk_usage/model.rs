//! The Disk Usage window's presentation model over a finished scan ([`Tree`], shared and
//! never changed): the visible rows of the tree list (expanded folders, sorted siblings),
//! the selection and its byte sum, and the file types totalled by extension.
//!
//! Rows are rebuilt only when the expansion, the sort, the size kind or the extension
//! filter changes, never per frame. The file-type totals are built once per scan.

use keel_vfs::du::{flag, NodeId, NodeKind, Tree, NONE, TOP};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// The tree list's columns (Name is always shown, first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Name,
    /// The share of the parent folder, as a bar colored by depth.
    Subtree,
    /// The share of the parent or of the whole scan (`Model::relative`).
    Percentage,
    Size,
    Allocated,
    Items,
    Files,
    Folders,
    /// The newest change in the subtree.
    Changed,
    Attributes,
    Owner,
    Accessed,
    Created,
}

impl Column {
    pub const ALL: [Self; 13] = [
        Self::Name,
        Self::Subtree,
        Self::Percentage,
        Self::Size,
        Self::Allocated,
        Self::Items,
        Self::Files,
        Self::Folders,
        Self::Changed,
        Self::Attributes,
        Self::Owner,
        Self::Accessed,
        Self::Created,
    ];

    /// Shown until the user picks others.
    pub const DEFAULT: [Self; 9] = [
        Self::Name,
        Self::Subtree,
        Self::Percentage,
        Self::Size,
        Self::Allocated,
        Self::Items,
        Self::Files,
        Self::Folders,
        Self::Changed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Subtree => "Subtree %",
            Self::Percentage => "Percentage",
            Self::Size => "Size",
            Self::Allocated => "Size on disk",
            Self::Items => "Items",
            Self::Files => "Files",
            Self::Folders => "Folders",
            Self::Changed => "Last change",
            Self::Attributes => "Attributes",
            Self::Owner => "Owner",
            Self::Accessed => "Last accessed",
            Self::Created => "Created",
        }
    }

    /// Read from the file system per visible row (not part of the scan).
    pub fn metadata(self) -> bool {
        matches!(self, Self::Owner | Self::Accessed | Self::Created)
    }

    /// Numbers: right-aligned, biggest first on the first click.
    pub fn numeric(self) -> bool {
        !matches!(self, Self::Name | Self::Attributes | Self::Owner)
    }
}

/// What the scan does not keep, read per row when its column is shown.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Metadata {
    pub owner: Option<String>,
    /// Unix ns.
    pub accessed: Option<i64>,
    /// Unix ns.
    pub created: Option<i64>,
}

/// A row of the file types list: one extension, or every extension the OS has no
/// description for when those are grouped.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct FileType {
    /// ".txt", "(no extension)" or "Unregistered".
    pub label: String,
    pub description: String,
    /// Lowercase, without the dot ("" for no extension).
    pub extensions: BTreeSet<String>,
    pub logical: u64,
    pub physical: u64,
    pub files: u64,
}

impl FileType {
    pub fn bytes(&self, physical: bool) -> u64 {
        if physical {
            self.physical
        } else {
            self.logical
        }
    }
}

pub const NO_EXTENSION: &str = "(no extension)";
pub const UNREGISTERED: &str = "Unregistered";

pub struct Model {
    pub tree: Arc<Tree>,
    /// The item the rows start below: the top item (every root), or one folder (a pane's
    /// Disk usage view). Shares "of the whole scan" are of this item.
    pub base: NodeId,
    pub expanded: HashSet<NodeId>,
    /// The visible rows: `(item, depth)`, roots at depth 0.
    pub rows: Vec<(NodeId, usize)>,
    /// `(column, ascending)`.
    pub sort: (Column, bool),
    /// Sizes on disk instead of lengths (sorting, shares, the selection sum, file types).
    pub physical: bool,
    /// The Percentage column is the share of the parent instead of the whole scan.
    pub relative: bool,
    pub selected: BTreeSet<NodeId>,
    pub cursor: Option<NodeId>,
    anchor: Option<NodeId>,
    /// The file types selected: the extensions of the selected files, or the type picked in
    /// the file types list. The treemap outlines every file of these.
    pub highlight_extensions: BTreeSet<String>,
    /// Only files of these extensions (and the folders holding them) are listed.
    pub filter: Option<BTreeSet<String>>,
    /// Owner and times read so far (by the window, on a worker).
    pub metadata: HashMap<NodeId, Metadata>,
    extensions: BTreeMap<String, FileType>,
}

impl Model {
    pub fn new(tree: Arc<Tree>) -> Self {
        Self::below(tree, TOP)
    }

    /// The rows below `base` only (its children at depth 0), nothing opened.
    pub fn below(tree: Arc<Tree>, base: NodeId) -> Self {
        let base = if tree.get(base).is_some_and(|n| n.is_container()) {
            base
        } else {
            TOP
        };
        let expanded = match base {
            TOP => tree.roots().iter().map(|r| r.node).collect(),
            _ => HashSet::new(),
        };
        let extensions = Self::totals(&tree, base);
        let mut out = Self {
            tree,
            base,
            expanded,
            rows: Vec::new(),
            sort: (Column::Size, false),
            physical: false,
            relative: false,
            selected: BTreeSet::new(),
            cursor: None,
            anchor: None,
            highlight_extensions: BTreeSet::new(),
            filter: None,
            metadata: HashMap::new(),
            extensions,
        };
        out.rebuild();
        out
    }

    /// The files below `base` by extension.
    fn totals(tree: &Tree, base: NodeId) -> BTreeMap<String, FileType> {
        let mut extensions: BTreeMap<String, FileType> = BTreeMap::new();
        let below = |mut id: NodeId| {
            while id != NONE {
                if id == base {
                    return true;
                }
                id = tree.node(id).parent;
            }
            false
        };
        for (id, n) in tree.nodes().filter(|(_, n)| n.kind == NodeKind::File) {
            // The scanner's totals leave out a file's other hard-link names: so do these.
            if n.has(flag::HARDLINK_DUP) || (base != TOP && !below(id)) {
                continue;
            }
            let ext = tree.ext(id);
            let t = extensions.entry(ext.to_owned()).or_default();
            if t.extensions.is_empty() {
                t.extensions.insert(ext.to_owned());
            }
            t.logical = t.logical.saturating_add(n.size);
            t.physical = t.physical.saturating_add(n.allocated);
            t.files += 1;
        }
        extensions
    }

    /// Shows `tree`, a newer version of the scan shown (the File Watcher's changes),
    /// keeping what is open, selected and read: `remap` gives each old id's new one (None:
    /// ids are unchanged). Items that are gone drop out.
    pub fn replace_tree(&mut self, tree: Arc<Tree>, remap: Option<&[NodeId]>) {
        let live = |id: NodeId| tree.get(id).is_some_and(|n| n.kind != NodeKind::Dead);
        let map = |id: NodeId| -> Option<NodeId> {
            let new = match remap {
                Some(r) => *r.get(id as usize)?,
                None => id,
            };
            (new != NONE && live(new)).then_some(new)
        };
        self.base = match self.base {
            TOP => TOP,
            b => map(b).unwrap_or(TOP),
        };
        self.expanded = self.expanded.iter().filter_map(|&i| map(i)).collect();
        self.selected = self.selected.iter().filter_map(|&i| map(i)).collect();
        self.cursor = self.cursor.and_then(map);
        self.anchor = self.anchor.and_then(map);
        self.metadata = (std::mem::take(&mut self.metadata).into_iter())
            .filter_map(|(i, m)| Some((map(i)?, m)))
            .collect();
        self.extensions = Self::totals(&tree, self.base);
        self.tree = tree;
        self.rebuild();
    }

    /// The extensions found (lowercase, "" for none).
    #[cfg(test)]
    pub fn extensions(&self) -> impl Iterator<Item = &str> {
        self.extensions.keys().map(String::as_str)
    }

    /// An item's bytes in the current size kind (0 for a hard link's other names).
    pub fn bytes(&self, id: NodeId) -> u64 {
        let n = self.tree.node(id);
        if n.has(flag::HARDLINK_DUP) {
            0
        } else if self.physical {
            n.allocated
        } else {
            n.size
        }
    }

    /// An item's share (0..=1) of its parent (`relative`) or of the whole scan. Free and
    /// unknown space are shares of their drive.
    pub fn fraction(&self, id: NodeId, relative: bool) -> f64 {
        let n = self.tree.node(id);
        let total = if n.is_pseudo() {
            (self.tree.root_of(id))
                .and_then(|r| r.space)
                .map_or(0, |s| s.total)
        } else {
            let of = if relative { n.parent } else { self.base };
            if of == NONE {
                0
            } else {
                self.bytes(of)
            }
        };
        if total == 0 {
            0.0
        } else {
            (self.bytes(id) as f64 / total as f64).min(1.0)
        }
    }

    pub fn toggle(&mut self, id: NodeId) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.rebuild();
    }

    /// Whether `id` has children to show.
    pub fn expandable(&self, id: NodeId) -> bool {
        self.tree.node(id).is_container() && self.tree.children(id).next().is_some()
    }

    pub fn set_sort(&mut self, column: Column) {
        self.sort = if self.sort.0 == column {
            (column, !self.sort.1)
        } else {
            (column, !column.numeric())
        };
        self.rebuild();
    }

    pub fn set_physical(&mut self, physical: bool) {
        if self.physical != physical {
            self.physical = physical;
            self.rebuild();
        }
    }

    pub fn set_relative(&mut self, relative: bool) {
        if self.relative != relative {
            self.relative = relative;
            self.rebuild();
        }
    }

    /// Lists the rows again (after the expansion, the sort or the filter changed).
    pub fn rebuild(&mut self) {
        let mut allowed = HashSet::new();
        if let Some(exts) = &self.filter {
            for (id, n) in self.tree.nodes() {
                if n.kind == NodeKind::File && exts.contains(self.tree.ext(id)) {
                    let mut at = id;
                    while at != NONE && allowed.insert(at) {
                        at = self.tree.node(at).parent;
                    }
                }
            }
        }
        let mut stack = vec![(self.base, 0)];
        let mut rows = Vec::new();
        while let Some((id, depth)) = stack.pop() {
            if id != self.base {
                rows.push((id, depth - 1));
            }
            // A filter shows every folder holding a match, open.
            if id == self.base || self.expanded.contains(&id) || self.filter.is_some() {
                let mut children: Vec<NodeId> = (self.tree.children(id))
                    .filter(|c| self.filter.is_none() || allowed.contains(c))
                    .collect();
                children.sort_by(|&a, &b| self.compare(a, b));
                stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
            }
        }
        self.rows = rows;
    }

    /// Sibling order: the sort column, then the name, then the id (total and stable);
    /// free and unknown space always last.
    fn compare(&self, a: NodeId, b: NodeId) -> Ordering {
        let (na, nb) = (self.tree.node(a), self.tree.node(b));
        let (ma, mb) = (self.metadata.get(&a), self.metadata.get(&b));
        let order = match self.sort.0 {
            Column::Name => natord::compare_ignore_case(&na.name, &nb.name),
            Column::Subtree => self.fraction(a, true).total_cmp(&self.fraction(b, true)),
            Column::Percentage | Column::Size => self.bytes(a).cmp(&self.bytes(b)),
            Column::Allocated => na.allocated.cmp(&nb.allocated),
            Column::Items => na.items.cmp(&nb.items),
            Column::Files => na.files.cmp(&nb.files),
            Column::Folders => na.folders.cmp(&nb.folders),
            Column::Changed => na.newest.cmp(&nb.newest),
            Column::Attributes => attributes(na.attributes).cmp(&attributes(nb.attributes)),
            Column::Owner => {
                (ma.and_then(|m| m.owner.as_deref())).cmp(&mb.and_then(|m| m.owner.as_deref()))
            }
            Column::Accessed => (ma.and_then(|m| m.accessed)).cmp(&mb.and_then(|m| m.accessed)),
            Column::Created => (ma.and_then(|m| m.created)).cmp(&mb.and_then(|m| m.created)),
        };
        let order = if self.sort.1 { order } else { order.reverse() };
        (na.is_pseudo().cmp(&nb.is_pseudo()))
            .then(order)
            .then_with(|| na.name.cmp(&nb.name))
            .then(a.cmp(&b))
    }

    /// The row of `id`, when visible.
    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.rows.iter().position(|r| r.0 == id)
    }

    /// Click (`toggle`: Ctrl, `range`: Shift) on row `id`.
    pub fn select(&mut self, id: NodeId, toggle: bool, range: bool) {
        let anchor = self.anchor.and_then(|a| self.row_of(a));
        match (range, anchor, self.row_of(id)) {
            (true, Some(a), Some(b)) => {
                if !toggle {
                    self.selected.clear();
                }
                let rows = &self.rows[a.min(b)..=a.max(b)];
                self.selected.extend(rows.iter().map(|r| r.0));
            }
            _ => {
                if !toggle {
                    self.selected.clear();
                }
                if !toggle || !self.selected.remove(&id) {
                    self.selected.insert(id);
                }
                self.anchor = Some(id);
            }
        }
        self.cursor = Some(id);
        self.sync_extensions();
    }

    /// Ctrl+A: every visible row.
    pub fn select_all(&mut self) {
        self.selected = self.rows.iter().map(|r| r.0).collect();
        self.sync_extensions();
    }

    /// The selected rows (or `fallback` when nothing is selected) as tab-separated text
    /// with a header line, in the order they are listed; for pasting into a spreadsheet.
    pub fn selected_tsv(&self, fallback: NodeId) -> String {
        let mut out = String::from("Path\tSize\tSize on disk\tItems\tFiles\tFolders\n");
        let ids: Vec<NodeId> = if self.selected.is_empty() {
            vec![fallback]
        } else {
            self.rows
                .iter()
                .map(|r| r.0)
                .filter(|id| self.selected.contains(id))
                .collect()
        };
        for id in ids {
            let Some(path) = self.tree.path(id) else {
                continue;
            };
            let n = self.tree.node(id);
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                path.display(),
                n.size,
                n.allocated,
                n.items,
                n.files,
                n.folders
            ));
        }
        out
    }

    /// The selected files' extensions become the selected file types.
    fn sync_extensions(&mut self) {
        self.highlight_extensions = (self.selected.iter())
            .filter(|&&i| self.tree.node(i).kind == NodeKind::File)
            .map(|&i| self.tree.ext(i).to_owned())
            .collect();
    }

    /// Shows `id` (opening the folders above it) and selects it alone: a pick in the
    /// treemap or another chart.
    pub fn reveal(&mut self, id: NodeId) {
        if self.tree.get(id).is_none_or(|n| n.kind == NodeKind::Dead) {
            return;
        }
        let mut at = self.tree.node(id).parent;
        while at != NONE && at != TOP && at != self.base {
            self.expanded.insert(at);
            at = self.tree.node(at).parent;
        }
        if self.filter.is_some() && self.row_of(id).is_none() {
            self.filter = None;
        }
        self.rebuild();
        self.select(id, false, false);
    }

    /// Arrow keys, Page Up/Down, Home and End: moves the cursor `delta` rows (clamped);
    /// `extend` (Shift) selects the range from the anchor.
    pub fn move_cursor(&mut self, delta: isize, extend: bool) {
        if self.rows.is_empty() {
            return;
        }
        let at = self.cursor.and_then(|c| self.row_of(c));
        let to = match at {
            None => 0,
            Some(at) => (at as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize,
        };
        let id = self.rows[to].0;
        if extend {
            if self.anchor.is_none() {
                self.anchor = self.cursor.or(Some(id));
            }
            self.select(id, false, true);
        } else {
            self.select(id, false, false);
        }
    }

    /// Left: closes the cursor's folder, or goes to its parent.
    pub fn left(&mut self) {
        let Some(id) = self.cursor else { return };
        if self.expanded.contains(&id) && self.filter.is_none() {
            self.toggle(id);
        } else {
            let parent = self.tree.node(id).parent;
            if parent != TOP && parent != NONE && parent != self.base {
                self.select(parent, false, false);
            }
        }
    }

    /// Right: opens the cursor's folder, or goes to its first child.
    pub fn right(&mut self) {
        let Some(id) = self.cursor else { return };
        if !self.expandable(id) {
            return;
        }
        if !self.expanded.contains(&id) && self.filter.is_none() {
            self.toggle(id);
        } else if let Some(&(child, _)) = self.row_of(id).and_then(|r| self.rows.get(r + 1)) {
            if self.tree.node(child).parent == id {
                self.select(child, false, false);
            }
        }
    }

    /// The selection's bytes; an item inside a selected folder is not counted twice, and
    /// free and unknown space are not counted.
    pub fn selection_bytes(&self) -> u64 {
        (self.selected.iter())
            .filter(|&&id| {
                if self.tree.node(id).is_pseudo() {
                    return false;
                }
                let mut p = self.tree.node(id).parent;
                while p != NONE {
                    if self.selected.contains(&p) {
                        return false;
                    }
                    p = self.tree.node(p).parent;
                }
                true
            })
            .fold(0u64, |sum, &id| sum.saturating_add(self.bytes(id)))
    }

    /// Only files with these extensions (None: everything).
    pub fn filter_extensions(&mut self, exts: Option<BTreeSet<String>>) {
        self.filter = exts;
        self.rebuild();
    }

    /// The file types, biggest first. `describe` gives the OS's description of an
    /// extension (None: unregistered); `group` puts every unregistered extension in one
    /// "Unregistered" row. Files without an extension are always their own row.
    pub fn file_types(
        &self,
        group: bool,
        describe: &impl Fn(&str) -> Option<String>,
    ) -> Vec<FileType> {
        let mut types = Vec::new();
        let mut unknown = FileType {
            label: UNREGISTERED.into(),
            description: "Extensions with no type description on this system".into(),
            ..Default::default()
        };
        for (ext, totals) in &self.extensions {
            let description = if ext.is_empty() { None } else { describe(ext) };
            if group && !ext.is_empty() && description.is_none() {
                unknown.extensions.extend(totals.extensions.iter().cloned());
                unknown.logical = unknown.logical.saturating_add(totals.logical);
                unknown.physical = unknown.physical.saturating_add(totals.physical);
                unknown.files += totals.files;
            } else {
                let mut t = totals.clone();
                t.label = if ext.is_empty() {
                    NO_EXTENSION.into()
                } else {
                    format!(".{ext}")
                };
                t.description = description.unwrap_or_else(|| {
                    if ext.is_empty() {
                        "Files without an extension".into()
                    } else {
                        format!("{} file", ext.to_uppercase())
                    }
                });
                types.push(t);
            }
        }
        if !unknown.extensions.is_empty() {
            types.push(unknown);
        }
        types.sort_by(|a, b| {
            (b.bytes(self.physical).cmp(&a.bytes(self.physical)))
                .then_with(|| a.label.cmp(&b.label))
        });
        types
    }
}

/// WinDirStat's attribute letters: R read-only, H hidden, S system, A archive,
/// C compressed, P sparse, L reparse point (link), O offline (cloud placeholder).
pub fn attributes(bits: u32) -> String {
    use keel_vfs::du::attr;
    [
        (attr::READONLY, 'R'),
        (attr::HIDDEN, 'H'),
        (attr::SYSTEM, 'S'),
        (attr::ARCHIVE, 'A'),
        (attr::COMPRESSED, 'C'),
        (attr::SPARSE, 'P'),
        (attr::REPARSE_POINT, 'L'),
        (attr::OFFLINE, 'O'),
    ]
    .into_iter()
    .filter(|(bit, _)| bits & bit != 0)
    .map(|(_, c)| c)
    .collect()
}
