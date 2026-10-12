//! The Disk Usage window's chart (Task 83's treemap, sunburst and flame graph) in the
//! treemap's place: the scan is turned into a [`treemap::tree::Tree`](Tree) through
//! [`WeightedTree`] on a worker, and the widget shows it with the selection shared with the
//! tree list and the file types list.
//!
//! The chart's tree is a pruned copy: in each folder only the children that are at least
//! 1/[`SHARE_DIV`] of it (at most [`MAX_KIDS`], biggest first) are kept, and the rest shows
//! as the folder's "Other" tile, so a scan of millions of files stays a few thousand
//! tiles at every zoom level.

use super::{DuSettings, Model};
use crate::treemap::panel::options_ui;
use crate::treemap::tree::{NodeId as ChartId, Tree, WeightedTree};
use crate::treemap::widget::{Chart, TreemapView};
use egui::Ui;
use keel_vfs::du::{flag, Node, NodeId, NodeKind, Tree as Scan, NONE, TOP};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

/// A folder keeps the children that are at least this fraction of it (1/n).
pub const SHARE_DIV: u64 = 2000;
/// A folder keeps at most this many children.
pub const MAX_KIDS: usize = 1000;

/// Color keys of the items that are not files (`/` marks them as neutral).
const FOLDER: &str = "/folder";
const LINK: &str = "/link";
const FREE: &str = "/free";
const UNKNOWN: &str = "/unknown";

/// An item's bytes in the chosen size kind (0 for a hard link's other names).
fn bytes_of(n: &Node, physical: bool) -> u64 {
    if n.has(flag::HARDLINK_DUP) {
        0
    } else if physical {
        n.allocated
    } else {
        n.size
    }
}

/// A scan seen as a weighted tree: only the items worth a tile, indexed in the order found.
pub struct Source<'a> {
    scan: &'a Scan,
    ids: Vec<NodeId>,
    parents: Vec<Option<usize>>,
    weights: Vec<u64>,
}

impl<'a> Source<'a> {
    /// `physical`: sizes on disk; `free` / `unknown`: keep each drive's free space and
    /// unknown (used but not found) space as tiles.
    pub fn new(scan: &'a Scan, physical: bool, free: bool, unknown: bool) -> Self {
        let mut out = Self {
            scan,
            ids: Vec::new(),
            parents: Vec::new(),
            weights: Vec::new(),
        };
        if scan.get(TOP).is_none() {
            return out;
        }
        out.push(TOP, None, bytes_of(scan.node(TOP), physical));
        // (index in the source, scan id): folders to take the kept children of.
        let mut stack = vec![(0usize, TOP)];
        while let Some((at, id)) = stack.pop() {
            let own = out.weights[at];
            let mut kids: Vec<(NodeId, u64)> = (scan.children(id))
                .filter_map(|c| {
                    let n = scan.node(c);
                    let keep = match n.kind {
                        NodeKind::Free => free,
                        NodeKind::Unknown => unknown,
                        _ => true,
                    };
                    let w = bytes_of(n, physical);
                    (keep && w > 0).then_some((c, w))
                })
                .collect();
            kids.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            kids.truncate(MAX_KIDS);
            for (c, w) in kids {
                // Free and unknown space are shown whatever their size.
                if w.saturating_mul(SHARE_DIV) < own && !scan.node(c).is_pseudo() {
                    continue;
                }
                let i = out.push(c, Some(at), w);
                if scan.node(c).is_container() {
                    stack.push((i, c));
                }
            }
        }
        out
    }

    fn push(&mut self, id: NodeId, parent: Option<usize>, weight: u64) -> usize {
        self.ids.push(id);
        self.parents.push(parent);
        self.weights.push(weight);
        self.ids.len() - 1
    }
}

impl WeightedTree for Source<'_> {
    fn len(&self) -> usize {
        self.ids.len()
    }

    fn id(&self, i: usize) -> ChartId {
        ChartId::from(self.ids[i])
    }

    fn parent(&self, i: usize) -> Option<usize> {
        self.parents[i]
    }

    fn weight(&self, i: usize) -> u64 {
        self.weights[i]
    }

    fn color_key(&self, i: usize) -> &str {
        let id = self.ids[i];
        match self.scan.node(id).kind {
            NodeKind::File => self.scan.ext(id),
            NodeKind::Link => LINK,
            NodeKind::Free => FREE,
            NodeKind::Unknown => UNKNOWN,
            _ => FOLDER,
        }
    }

    fn label(&self, i: usize) -> &str {
        &self.scan.node(self.ids[i]).name
    }
}

/// The chart's tree of a scan. `ranks` gives each extension its row in the file types list,
/// which is also its color's rank.
pub fn build(
    scan: &Scan,
    physical: bool,
    free: bool,
    unknown: bool,
    ranks: &HashMap<String, usize>,
) -> Tree {
    let src = Source::new(scan, physical, free, unknown);
    let mut tree = Tree::from_source(&src, "");
    tree.set_key_ranks(|key| ranks.get(key).map(|&r| r as u32));
    tree
}

/// What a chart tree was built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    /// The scan shown (`DiskUsage::generation`).
    scan: u64,
    physical: bool,
    group: bool,
    free: bool,
    unknown: bool,
}

/// The chart, its tree being built, and what was last synced with the tree list.
#[derive(Default)]
pub struct Host {
    view: TreemapView,
    built: Option<Key>,
    job: Option<(Key, Receiver<Tree>)>,
    /// The tree list's cursor last put in the chart.
    synced: Option<NodeId>,
}

impl Host {
    /// Takes in a finished tree and starts the one `key` asks for when it differs.
    fn poll(
        &mut self,
        ctx: &egui::Context,
        key: Key,
        model: &Model,
        ranks: &HashMap<String, usize>,
    ) {
        if let Some((k, rx)) = &self.job {
            match rx.try_recv() {
                Ok(tree) => {
                    let k = *k;
                    self.job = None;
                    if self.built.is_none_or(|b| b.scan != k.scan) {
                        self.view.root = 0;
                        self.view.selected = None;
                    }
                    self.view.set_tree(tree);
                    self.built = Some(k);
                    self.synced = None;
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => self.job = None,
            }
        }
        let wanted = self.job.as_ref().map(|j| j.0).or(self.built);
        if wanted == Some(key) {
            return;
        }
        let (tx, rx) = channel();
        let (scan, ranks) = (Arc::clone(&model.tree), ranks.clone());
        let started = crate::worker::spawn("keel-du-chart", move || {
            let _ = tx.send(build(&scan, key.physical, key.free, key.unknown, &ranks));
        });
        // A job still running for an older key is dropped: its tree has no receiver.
        self.job = started.then_some((key, rx));
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// Shows the scan item `id` in the chart (or the nearest folder above it that is
    /// drawn), zoomed out as far as it takes to see it.
    fn show_item(&mut self, scan: &Scan, id: NodeId) {
        let mut at = id;
        loop {
            if self.view.select(ChartId::from(at)) {
                break;
            }
            match scan.get(at).map(|n| n.parent) {
                Some(p) if p != NONE => at = p,
                _ => {
                    self.view.selected = None;
                    break;
                }
            }
        }
        while let Some(s) = self.view.selected {
            if self.view.tree().contains(self.view.root, s) || !self.view.zoom_out() {
                break;
            }
        }
    }
}

/// The chart in `ui` with its toolbar; an item picked in the chart is returned (the caller
/// shows it in the tree list). `generation` is the scan's number.
pub fn show(
    ui: &mut Ui,
    host: &mut Host,
    model: &Model,
    generation: u64,
    ranks: &HashMap<String, usize>,
    settings: &mut DuSettings,
) -> Option<NodeId> {
    let key = Key {
        scan: generation,
        physical: model.physical,
        group: settings.group_unregistered,
        free: settings.chart_free,
        unknown: settings.chart_unknown,
    };
    host.poll(ui.ctx(), key, model, ranks);
    ui.horizontal_wrapped(|ui| {
        for c in [Chart::Treemap, Chart::Sunburst, Chart::Flame] {
            ui.selectable_value(&mut settings.chart, c, c.name())
                .on_hover_text(match c {
                    Chart::Treemap => {
                        "Each file a rectangle by its size, inside its folder's (double-click zooms in, Backspace out)"
                    }
                    Chart::Sunburst => "Folders as rings around the scan",
                    Chart::Flame => "Folders as rows below the scan",
                });
        }
        ui.separator();
        ui.menu_button("Chart options", |ui| {
            ui.checkbox(&mut settings.chart_free, "Show free space")
                .on_hover_text("Each drive's free space as a tile");
            ui.checkbox(&mut settings.chart_unknown, "Show unknown space")
                .on_hover_text("Space in use that the scan did not find, as a tile");
            ui.separator();
            options_ui(ui, &mut settings.chart_options);
        });
        if host.view.root != 0 {
            if ui
                .button("⏶ Zoom out")
                .on_hover_text("Backspace or the mouse's back button")
                .clicked()
            {
                host.view.zoom_out();
            }
            let root = host.view.root;
            ui.weak(host.view.tree().path(root));
        }
    });
    if host.built.is_none() {
        ui.centered_and_justified(|ui| ui.weak("Laying out the chart…"));
        return None;
    }
    // The tree list's cursor and the file types list's selection show in the chart.
    if host.synced != model.cursor {
        host.synced = model.cursor;
        match model.cursor {
            Some(c) => host.show_item(&model.tree, c),
            None => host.view.selected = None,
        }
    }
    let types: Vec<String> = model.highlight_extensions.iter().cloned().collect();
    if host.view.highlight != types {
        host.view.highlight = types;
    }
    let name = format!("{} of the scan", settings.chart.name());
    let r = host
        .view
        .ui(ui, settings.chart, &settings.chart_options, &name);
    let picked = r.picked.and_then(|id| NodeId::try_from(id).ok())?;
    host.synced = Some(picked);
    Some(picked)
}

#[cfg(test)]
impl Host {
    /// The scan item selected in the chart (None: nothing).
    pub fn selected_item(&self) -> Option<NodeId> {
        self.view
            .selected_id()
            .and_then(|id| NodeId::try_from(id).ok())
    }

    /// The chart's tree, once built.
    pub fn tree(&self) -> Option<&Tree> {
        self.built.map(|_| self.view.tree())
    }

    /// The extensions whose files are outlined.
    pub fn highlighted(&self) -> &[String] {
        &self.view.highlight
    }
}
