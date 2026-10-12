//! The chart widget: one tree, one zoom and one selection shown as a treemap, a sunburst
//! or a flame graph. The treemap is laid out and shaded on a worker thread; the UI thread
//! only uploads the image (or the flat mesh) and hit-tests the layout.

use super::layout::{self, Dir, Layout, R};
use super::radial::{self, Partition};
use super::shade::{self, Options};
use super::tree::{NodeId, Tree, SPECIAL};
use egui::{
    Color32, ColorImage, EventFilter, Key, Mesh, Modifiers, PointerButton, Pos2, Rect, Response,
    Sense, Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chart {
    #[default]
    Treemap,
    Sunburst,
    Flame,
}

impl Chart {
    pub fn name(self) -> &'static str {
        match self {
            Chart::Treemap => "Treemap",
            Chart::Sunburst => "Sunburst",
            Chart::Flame => "Flame graph",
        }
    }
}

/// What a frame of the chart did.
pub struct ChartResponse {
    pub response: Response,
    /// The node a click or an arrow key selected this frame.
    pub picked: Option<NodeId>,
    /// The zoom changed this frame.
    pub zoomed: bool,
}

/// Everything a render depends on.
#[derive(Clone, Debug, PartialEq)]
struct RenderKey {
    tree: u64,
    root: u32,
    size: [usize; 2],
    ppp: u32,
    options: Options,
}

/// A finished render.
pub struct Rendered {
    key: RenderKey,
    pub layout: Layout,
    pub image: Option<ColorImage>,
    pub mesh: Option<Mesh>,
}

struct Job {
    key: RenderKey,
    rx: crossbeam_channel::Receiver<Rendered>,
    cancel: Arc<AtomicBool>,
}

struct Shown {
    key: RenderKey,
    layout: Layout,
    /// The flat mesh in points from (0, 0), and its copy placed at the last drawn origin.
    mesh: Option<(Arc<Mesh>, Pos2, Arc<Mesh>)>,
}

/// Lays out and shades one treemap (also the benchmark's entry point).
pub fn render(
    tree: &Tree,
    root: u32,
    size: [usize; 2],
    ppp: f32,
    o: &Options,
    cancel: Option<&AtomicBool>,
) -> Option<(Layout, Option<ColorImage>, Option<Mesh>)> {
    let bounds = R::new(0.0, 0.0, size[0] as f64, size[1] as f64);
    let lay = layout::layout(tree, root, bounds, &o.params(ppp), cancel)?;
    let colors = o.key_colors(tree, root);
    if o.cushion {
        let img = shade::render(tree, &lay, &colors, o, size, cancel)?;
        Some((lay, Some(img), None))
    } else {
        let mesh = shade::flat_mesh(tree, &lay, &colors, o, ppp);
        Some((lay, None, Some(mesh)))
    }
}

pub struct TreemapView {
    tree: Arc<Tree>,
    tree_gen: u64,
    /// The zoom: the node shown at the top.
    pub root: u32,
    pub selected: Option<u32>,
    job: Option<Job>,
    shown: Option<Shown>,
    texture: Option<TextureHandle>,
    partition: Option<((u64, u32, u16, u64), Partition)>,
    last_zoom: Option<Instant>,
}

impl Default for TreemapView {
    fn default() -> Self {
        Self::new()
    }
}

/// The id of the chart holding the keyboard (`keys_owned`).
fn keys_id() -> egui::Id {
    egui::Id::new("keel-chart-keys")
}

/// A chart has the keyboard focus: Backspace zooms out there instead of going up a folder.
pub fn keys_owned(ctx: &egui::Context) -> bool {
    let id = ctx.data(|d| d.get_temp::<egui::Id>(keys_id()));
    id.is_some() && ctx.memory(|m| m.focused()) == id
}

impl TreemapView {
    pub fn new() -> Self {
        Self {
            tree: Arc::new(Tree::new(0, "")),
            tree_gen: 0,
            root: 0,
            selected: None,
            job: None,
            shown: None,
            texture: None,
            partition: None,
            last_zoom: None,
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Shows a new tree, keeping the zoom and the selection where their ids still exist.
    pub fn set_tree(&mut self, tree: Tree) {
        let root = self.tree.nodes.get(self.root as usize).map(|n| n.id);
        let sel = self.selected_id();
        self.root = root.and_then(|id| tree.find(id)).unwrap_or(0);
        self.selected = sel.and_then(|id| tree.find(id));
        self.tree = Arc::new(tree);
        self.tree_gen += 1;
    }

    pub fn selected_id(&self) -> Option<NodeId> {
        let n = self.selected?;
        self.tree.nodes.get(n as usize).map(|n| n.id)
    }

    /// Selects the node with `id`; false when there is none.
    pub fn select(&mut self, id: NodeId) -> bool {
        self.selected = self.tree.find(id);
        self.selected.is_some()
    }

    /// Zooms into `node` (a folder).
    pub fn zoom_to(&mut self, node: u32) -> bool {
        if (node as usize) >= self.tree.len() || self.tree.is_leaf(node) || node == self.root {
            return false;
        }
        self.root = node;
        true
    }

    pub fn zoom_out(&mut self) -> bool {
        match self.tree.node(self.root).parent {
            Some(p) => {
                self.root = p;
                true
            }
            None => false,
        }
    }

    /// One level in toward `node`: the zoom root's child holding it, if a folder.
    fn zoom_toward(&mut self, node: u32) -> bool {
        match self.tree.child_toward(self.root, node) {
            Some(c) => self.zoom_to(c),
            None => false,
        }
    }

    /// The shown layout, if it is of the current tree.
    fn layout(&self) -> Option<&Layout> {
        let s = self.shown.as_ref()?;
        (s.key.tree == self.tree_gen).then_some(&s.layout)
    }

    /// The tree node under a point of the shown treemap (layout pixels).
    #[cfg(test)]
    pub fn node_at(&self, x: f64, y: f64) -> Option<u32> {
        let lay = self.layout()?;
        lay.hit(x, y).map(|t| lay.tiles[t as usize].node)
    }

    /// The tile showing the selection (its nearest laid-out ancestor when it is too small).
    fn selected_tile(&self, lay: &Layout) -> Option<u32> {
        let mut at = self.selected?;
        if !self.tree.contains(lay.root, at) {
            return None;
        }
        loop {
            if let Some(t) = lay.tile_of(at) {
                return Some(t);
            }
            at = self.tree.node(at).parent?;
        }
    }

    /// Moves the treemap selection to the neighbor in `dir`; true when it moved.
    pub fn step(&mut self, dir: Dir) -> bool {
        let Some(lay) = self.layout() else {
            return false;
        };
        let next = match self.selected_tile(lay) {
            Some(t) => lay.neighbor(t, dir),
            None => lay
                .tiles
                .first()
                .map(|r| r.first_child)
                .filter(|&c| c != layout::NONE),
        };
        let node = next.map(|t| lay.tiles[t as usize].node);
        if node.is_some() {
            self.selected = node;
        }
        node.is_some()
    }

    /// Lays out and shades now, on this thread (tests; the widget uses a worker).
    #[cfg(test)]
    pub fn render_now(&mut self, size: [usize; 2], o: &Options) {
        let key = self.key(size, 1.0, o);
        if let Some((layout, _, _)) = render(&self.tree, self.root, size, 1.0, o, None) {
            self.shown = Some(Shown {
                key,
                layout,
                mesh: None,
            });
        }
    }

    fn key(&self, size: [usize; 2], ppp: f32, o: &Options) -> RenderKey {
        RenderKey {
            tree: self.tree_gen,
            root: self.root,
            size,
            ppp: ppp.to_bits(),
            options: o.clone(),
        }
    }

    /// Takes a finished render and starts the one `key` needs.
    fn update(&mut self, ctx: &egui::Context, key: RenderKey) {
        if let Some(job) = &self.job {
            if let Ok(r) = job.rx.try_recv() {
                self.job = None;
                if let Some(img) = r.image {
                    match &mut self.texture {
                        Some(t) => t.set(img, TextureOptions::NEAREST),
                        None => {
                            self.texture =
                                Some(ctx.load_texture("keel-treemap", img, TextureOptions::NEAREST))
                        }
                    }
                }
                let mesh = r.mesh.map(|m| {
                    let m = Arc::new(m);
                    (m.clone(), Pos2::ZERO, m)
                });
                self.shown = Some(Shown {
                    key: r.key,
                    layout: r.layout,
                    mesh,
                });
            }
        }
        if self.shown.as_ref().is_some_and(|s| s.key == key) {
            if let Some(job) = self.job.take() {
                job.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        if self.job.as_ref().is_some_and(|j| j.key == key) {
            return;
        }
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let (tree, k, stop, ctx) = (self.tree.clone(), key.clone(), cancel.clone(), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("keel-treemap".into())
            .spawn(move || {
                let ppp = f32::from_bits(k.ppp);
                if let Some((layout, image, mesh)) =
                    render(&tree, k.root, k.size, ppp, &k.options, Some(&stop))
                {
                    let _ = tx.send(Rendered {
                        key: k,
                        layout,
                        image,
                        mesh,
                    });
                    ctx.request_repaint();
                }
            });
        if spawned.is_ok() {
            self.job = Some(Job { key, rx, cancel });
        }
    }

    /// Shows the chart in the space left in `ui`.
    pub fn ui(&mut self, ui: &mut Ui, chart: Chart, o: &Options, name: &str) -> ChartResponse {
        let size = ui.available_size().max(egui::vec2(64.0, 64.0));
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        let response = crate::a11y::Named::named(response, name);
        let mut out = ChartResponse {
            response,
            picked: None,
            zoomed: false,
        };
        if self.tree.is_empty() || self.tree.node(self.root).total == 0 {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Nothing to show: no sizes known here yet",
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            return out;
        }
        let hovered = match chart {
            Chart::Treemap => self.treemap(ui, rect, o),
            Chart::Sunburst | Chart::Flame => self.radial(ui, rect, chart, o),
        };
        self.interact(ui, chart, hovered, &mut out);
        if let Some(n) = hovered {
            out.response = out
                .response
                .clone()
                .on_hover_ui_at_pointer(|ui| self.tip(ui, n));
        }
        out
    }

    fn tip(&self, ui: &mut Ui, n: u32) {
        let t = &self.tree;
        ui.strong(t.path(n));
        let bytes = t.node(n).total;
        let root = t.node(self.root);
        let pct = |of: u64| {
            if of == 0 {
                0.0
            } else {
                100.0 * bytes as f64 / of as f64
            }
        };
        ui.label(format!(
            "{}, {:.1} % of {}",
            crate::view_details::size_text_of(bytes),
            pct(root.total),
            if root.label.is_empty() {
                "the shown folder"
            } else {
                &root.label
            }
        ));
        if self.root != 0 {
            ui.label(format!("{:.1} % of everything shown", pct(t.node(0).total)));
        }
        if t.node(n).synthetic {
            ui.weak("What the folder holds besides the items shown");
        }
        let key = t.key_of(n);
        if t.is_leaf(n) && !key.is_empty() && !key.starts_with(SPECIAL) {
            ui.label(format!("Type: .{key}"));
        }
    }

    /// Paints the treemap; the node under the pointer.
    fn treemap(&mut self, ui: &mut Ui, rect: Rect, o: &Options) -> Option<u32> {
        let ppp = ui.ctx().pixels_per_point();
        let px = [
            (rect.width() * ppp).round().max(1.0) as usize,
            (rect.height() * ppp).round().max(1.0) as usize,
        ];
        let key = self.key(px, ppp, o);
        self.update(ui.ctx(), key);
        let painter = ui.painter_at(rect);
        let Some(shown) = &mut self.shown else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Laying out…",
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            return None;
        };
        let [sw, sh] = shown.key.size;
        // Layout pixels to points (a stale render is stretched until the new one is in).
        let (kx, ky) = (rect.width() / sw as f32, rect.height() / sh as f32);
        if shown.key.options.cushion {
            if let Some(t) = &self.texture {
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                painter.image(t.id(), rect, uv, Color32::WHITE);
            }
        } else if let Some((local, at, placed)) = &mut shown.mesh {
            if *at != rect.min {
                let mut m = (**local).clone();
                m.translate(rect.min.to_vec2());
                *placed = Arc::new(m);
                *at = rect.min;
            }
            painter.add(Shape::Mesh(placed.clone()));
        }
        if shown.key.tree != self.tree_gen {
            // Laid out from the previous tree: shown until the new one is in, not hit.
            return None;
        }
        let lay = &shown.layout;
        let to_screen = |r: &R| {
            Rect::from_min_max(
                rect.min + egui::vec2(r.x0 as f32 * kx, r.y0 as f32 * ky),
                rect.min + egui::vec2(r.x1 as f32 * kx, r.y1 as f32 * ky),
            )
        };
        // The selected file's type, every tile of it outlined (at most this many).
        const EXT_OUTLINES: usize = 4000;
        let sel_tile = {
            let mut at = self.selected.filter(|&s| self.tree.contains(lay.root, s));
            let mut t = None;
            while let Some(n) = at {
                if let Some(x) = lay.tile_of(n) {
                    t = Some(x);
                    break;
                }
                at = self.tree.node(n).parent;
            }
            t
        };
        if let (true, Some(sel)) = (o.highlight_ext, self.selected) {
            let key = self.tree.node(sel).key;
            let k = self.tree.key_of(sel);
            if self.tree.is_leaf(sel) && !k.is_empty() && !k.starts_with(SPECIAL) {
                let stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(200));
                for t in lay
                    .leaves()
                    .filter(|t| self.tree.node(t.node).key == key)
                    .take(EXT_OUTLINES)
                {
                    painter.rect_stroke(to_screen(&t.rect), 0.0, stroke, StrokeKind::Inside);
                }
            }
        }
        if let Some(t) = sel_tile {
            let r = to_screen(&lay.tiles[t as usize].rect);
            painter.rect_stroke(
                r,
                0.0,
                Stroke::new(3.0_f32, Color32::BLACK),
                StrokeKind::Inside,
            );
            painter.rect_stroke(
                r,
                0.0,
                Stroke::new(1.5_f32, Color32::WHITE),
                StrokeKind::Inside,
            );
        }
        let p = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| rect.contains(*p))?;
        let (x, y) = (
            f64::from((p.x - rect.min.x) / kx),
            f64::from((p.y - rect.min.y) / ky),
        );
        lay.hit(x, y).map(|t| lay.tiles[t as usize].node)
    }

    fn partition(&mut self, levels: u16, min_frac: f64) -> &Partition {
        let key = (self.tree_gen, self.root, levels, min_frac.to_bits());
        if self.partition.as_ref().is_none_or(|(k, _)| *k != key) {
            let p = radial::partition(&self.tree, self.root, levels, min_frac);
            self.partition = Some((key, p));
        }
        &self.partition.as_ref().expect("set above").1
    }

    /// Paints the sunburst or the flame graph; the node under the pointer.
    fn radial(&mut self, ui: &mut Ui, rect: Rect, chart: Chart, o: &Options) -> Option<u32> {
        let colors = o.key_colors(&self.tree, self.root);
        let tree = self.tree.clone();
        let selected = self.selected;
        let dark = ui.visuals().dark_mode;
        let bg = ui.visuals().extreme_bg_color;
        let text = ui.visuals().text_color();
        let color = |n: u32, depth: u16| {
            let node = tree.node(n);
            if node.children.is_empty() {
                o.flat_color(colors[node.key as usize])
            } else {
                let g = if dark { 70 } else { 200 };
                let step = (depth % 4) as u8 * 12;
                Color32::from_gray(if dark { g + step } else { g - step })
            }
        };
        let painter = ui.painter_at(rect);
        let hover = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| rect.contains(*p));
        match chart {
            Chart::Sunburst => {
                let p = self.partition(10, 0.002).clone();
                let (center, ring) = radial::sunburst_geometry(rect, p.levels);
                for s in radial::sunburst_shapes(&p, center, ring, |s| color(s.node, s.depth), bg) {
                    painter.add(s);
                }
                if let Some(i) = selected.and_then(|n| p.seg_of(n)) {
                    let pts = radial::sector_outline(center, ring, &p.segs[i]);
                    painter.add(Shape::closed_line(pts, Stroke::new(2.0_f32, text)));
                }
                let root = tree.node(self.root);
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    format!(
                        "{}\n{}",
                        root.label,
                        crate::view_details::size_text_of(root.total)
                    ),
                    egui::FontId::proportional(12.0),
                    text,
                );
                let (depth, f) = radial::sunburst_at(center, ring, hover?);
                p.hit(depth, f).map(|i| p.segs[i].node)
            }
            _ => {
                let levels = ((rect.height() / radial::ROW) as u16).max(1);
                let min = 1.0 / f64::from(rect.width().max(1.0));
                let p = self.partition(levels, min).clone();
                let mut mesh = Mesh::default();
                let mut labels = Vec::new();
                for s in &p.segs {
                    let r = radial::flame_rect(rect, s);
                    let c = color(s.node, s.depth);
                    mesh.add_colored_rect(r, c);
                    if r.width() > 40.0 {
                        let ink = if c.r() as u32 + c.g() as u32 + c.b() as u32 > 380 {
                            Color32::BLACK
                        } else {
                            Color32::WHITE
                        };
                        labels.push((r, tree.node(s.node).label.clone(), ink));
                    }
                }
                painter.add(Shape::mesh(mesh));
                for (r, label, ink) in labels {
                    painter.with_clip_rect(r.intersect(rect)).text(
                        r.left_center() + egui::vec2(4.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(12.0),
                        ink,
                    );
                }
                if let Some(i) = selected.and_then(|n| p.seg_of(n)) {
                    let r = radial::flame_rect(rect, &p.segs[i]);
                    painter.rect_stroke(r, 0.0, Stroke::new(2.0_f32, text), StrokeKind::Inside);
                }
                let (depth, f) = radial::flame_at(rect, hover?);
                p.hit(depth, f).map(|i| p.segs[i].node)
            }
        }
    }

    /// Clicks, double-clicks, Ctrl+wheel, the back button and the keys.
    fn interact(&mut self, ui: &Ui, chart: Chart, hovered: Option<u32>, out: &mut ChartResponse) {
        let r = &out.response;
        if r.clicked() || r.double_clicked() || r.secondary_clicked() {
            r.request_focus();
        }
        if let Some(n) = hovered {
            if r.clicked() || r.secondary_clicked() {
                self.selected = Some(n);
                out.picked = Some(self.tree.node(n).id);
            }
            if r.double_clicked() {
                out.zoomed |= self.zoom_toward(n);
            }
        }
        if r.hovered() {
            let (zoom, back) = ui.input(|i| {
                (
                    i.zoom_delta(),
                    i.pointer.button_pressed(PointerButton::Extra1),
                )
            });
            let calm = self
                .last_zoom
                .is_none_or(|t| t.elapsed() > Duration::from_millis(150));
            if calm && (zoom - 1.0).abs() > 1e-3 {
                let zoomed = if zoom > 1.0 {
                    hovered.is_some_and(|n| self.zoom_toward(n))
                } else {
                    self.zoom_out()
                };
                if zoomed {
                    self.last_zoom = Some(Instant::now());
                    out.zoomed = true;
                }
            }
            if back {
                out.zoomed |= self.zoom_out();
            }
        }
        if !r.has_focus() {
            return;
        }
        let id = r.id;
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            )
        });
        ui.ctx().data_mut(|d| d.insert_temp(keys_id(), id));
        let (back, enter, keys) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_key(Modifiers::NONE, Key::Enter),
                [
                    (Key::ArrowLeft, Dir::Left),
                    (Key::ArrowRight, Dir::Right),
                    (Key::ArrowUp, Dir::Up),
                    (Key::ArrowDown, Dir::Down),
                ]
                .map(|(k, d)| (i.count_and_consume_key(Modifiers::NONE, k), d)),
            )
        });
        if back {
            out.zoomed |= self.zoom_out();
        }
        if enter {
            if let Some(n) = self.selected {
                out.zoomed |= self.zoom_to(n);
            }
        }
        for (count, dir) in keys {
            for _ in 0..count {
                let moved = match chart {
                    Chart::Treemap => self.step(dir),
                    _ => self.step_radial(dir),
                };
                if moved {
                    out.picked = self.selected_id();
                }
            }
        }
    }

    /// Arrow keys in the sunburst and flame graph: siblings left and right, the parent up,
    /// the biggest child down.
    fn step_radial(&mut self, dir: Dir) -> bool {
        let Some((_, p)) = &self.partition else {
            return false;
        };
        let cur = self.selected.and_then(|n| p.seg_of(n));
        let next = match (cur, dir) {
            (None, _) => (p.segs.len() > 1).then_some(1),
            (Some(i), Dir::Left) => p.sibling(i, -1),
            (Some(i), Dir::Right) => p.sibling(i, 1),
            (Some(i), Dir::Up) => {
                let parent = self.tree.node(p.segs[i].node).parent;
                parent.and_then(|n| p.seg_of(n))
            }
            (Some(i), Dir::Down) => p.first_child(i),
        };
        match next {
            Some(i) => {
                self.selected = Some(p.segs[i].node);
                true
            }
            None => false,
        }
    }
}
