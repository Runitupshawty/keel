//! Icon (grid) view: tiles with image / PDF / video thumbnails. Task 66: icon size 16 to
//! 512, grid spacing, labels under or right of the icon (two lines, shortened in the
//! middle), and the arrangement: in the folder's sort order, or by hand (Snap to grid or
//! free) with Clean Up and Clean Up By; positions are remembered per folder
//! (`view_prefs`).

use crate::keys::Action;
use crate::pane::{context_menu, drag_and_drop, handle_click, DragPayload, ViewCx};
use crate::preview_panel::PreviewKey;
use crate::state::Msg;
use crate::tab::Tab;
use crate::view_prefs::{Arrange, IconOptions, LabelPos, MAX_ICON, MAX_SPACING, MIN_ICON};
use crate::worker::ThumbPool;
use crossbeam_channel::Sender;
use egui::{pos2, vec2, Key, Rect, TextureHandle, Vec2};
use keel_preview::Preview;
use keel_vfs::{Entry, Kind, Router, VPath};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

pub const THUMB_CACHE: usize = 2000;
/// Thumbnail textures kept at most, in bytes (a 512-point icon is a big texture).
pub const THUMB_BYTES: usize = 384 * 1024 * 1024;
/// Label font size.
const FONT: f32 = 12.0;
/// Width of a label right of its icon.
const RIGHT_LABEL: f32 = 150.0;
/// Folders with more items than this are always shown in their sort order: hand-placed
/// positions are stored per item.
pub const MAX_ARRANGED: usize = 5000;
/// Extensions whose preview is an image (keel-preview image, pdf and video renderers).
const THUMB_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "pdf", "mp4", "mkv", "mov", "avi",
    "webm", "m4v",
];

/// LRU of thumbnail textures fed by the 4-thread `ThumbPool`.
pub struct Thumbs {
    pool: ThumbPool,
    /// `None` = no image could be made; not retried until the file changes. With its last
    /// use and its size in bytes.
    cache: HashMap<PreviewKey, (Option<TextureHandle>, u64, usize)>,
    bytes: usize,
    /// `THUMB_BYTES` (smaller in tests).
    budget: usize,
    pending: HashSet<PreviewKey>,
    /// Keys asked for during the current pass, and that pass's number: a pending key not
    /// asked for in a whole pass belongs to a tile that left the view.
    seen: HashSet<PreviewKey>,
    pass: u64,
    ctx: egui::Context,
    clock: u64,
    /// Thumbnails for remote and cloud files (setting `remote_thumbnails`): each one is a
    /// download.
    pub remote: bool,
}

/// The thumbnail size asked for an icon box of `icon` points (`PreviewKey::width`): the
/// usual one (0) up to 96 points, else 192 or 512.
pub fn thumb_bucket(icon: f32) -> u32 {
    match icon {
        i if i <= crate::worker::THUMB_PX as f32 => 0,
        i if i <= 192.0 => 192,
        _ => 512,
    }
}

impl Thumbs {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, router: Arc<Router>) -> Self {
        Self {
            pool: ThumbPool::new(tx, ctx.clone(), router),
            cache: HashMap::new(),
            bytes: 0,
            budget: THUMB_BYTES,
            pending: HashSet::new(),
            seen: HashSet::new(),
            pass: 0,
            ctx,
            clock: 0,
            remote: false,
        }
    }

    /// The cached thumbnail, or None while it is (re)queued.
    pub fn get(&mut self, e: &Entry) -> Option<(egui::TextureId, egui::Vec2)> {
        self.get_sized(e, 0)
    }

    /// `get` for a thumbnail box of `bucket` points (`thumb_bucket`).
    pub fn get_sized(&mut self, e: &Entry, bucket: u32) -> Option<(egui::TextureId, egui::Vec2)> {
        if !THUMB_EXTS.contains(&e.ext.as_str())
            || e.encrypted
            || (!self.remote && crate::remotes::is_network(&e.path))
        {
            return None;
        }
        // Bounded like the preview: never materialise what the previewer would refuse.
        if e.size > keel_preview::MAX_PREVIEW_BYTES && e.path.split_archive().is_some() {
            return None;
        }
        let key = PreviewKey::of(e, 0, bucket);
        self.next_pass(self.ctx.cumulative_pass_nr());
        self.seen.insert(key.clone());
        self.clock += 1;
        if let Some((tex, used, _)) = self.cache.get_mut(&key) {
            *used = self.clock;
            return tex.as_ref().map(|t| (t.id(), t.size_vec2()));
        }
        if !self.pending.contains(&key) && self.pool.request(key.clone(), e.clone()) {
            self.pending.insert(key);
        }
        None
    }

    /// On the first request of a new pass: requests still queued for tiles that were not
    /// drawn in the previous pass are cancelled (scrolled away, folder left).
    fn next_pass(&mut self, pass: u64) {
        if pass == self.pass {
            return;
        }
        self.pass = pass;
        let gone: Vec<PreviewKey> = self
            .pending
            .iter()
            .filter(|k| !self.seen.contains(*k))
            .cloned()
            .collect();
        if !gone.is_empty() {
            let mut cancelled = self.pool.cancelled.lock();
            for key in gone {
                self.pending.remove(&key);
                cancelled.insert(key);
            }
        }
        self.seen.clear();
    }

    pub fn insert(&mut self, ctx: &egui::Context, key: PreviewKey, preview: Preview) {
        self.pending.remove(&key);
        let rgba = match preview {
            Preview::Image(r)
            | Preview::Pdf { image: r, .. }
            | Preview::Video { thumb: r, .. }
            | Preview::Audio { cover: Some(r), .. }
            | Preview::Font { sample: r, .. } => Some(r),
            _ => None,
        };
        let tex = rgba
            .filter(|r| r.w > 0 && r.h > 0 && r.data.len() == (r.w * r.h * 4) as usize)
            .map(|r| {
                ctx.load_texture(
                    key.path.display(),
                    egui::ColorImage::from_rgba_unmultiplied([r.w as usize, r.h as usize], &r.data),
                    egui::TextureOptions::LINEAR,
                )
            });
        let weight = tex.as_ref().map_or(0, |t| t.size()[0] * t.size()[1] * 4);
        self.clock += 1;
        if let Some((_, _, old)) = self.cache.insert(key, (tex, self.clock, weight)) {
            self.bytes -= old;
        }
        self.bytes += weight;
        // Past 2,000 entries or the byte budget; the newest always stays.
        while self.cache.len() > 1 && (self.cache.len() > THUMB_CACHE || self.bytes > self.budget) {
            // ponytail: O(n) eviction scan over 2,000 entries; a linked LRU if it ever shows.
            let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, (_, used, _))| *used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((_, _, w)) = self.cache.remove(&oldest) {
                self.bytes -= w;
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }
}

// --- Layout math (Task 66) ---

/// One grid cell in points; `line_h` is the label font's row height.
pub fn cell_size(o: &IconOptions, line_h: f32) -> Vec2 {
    let s = o.icon_size;
    match o.label {
        LabelPos::Bottom => vec2(
            s.max(64.0) + 12.0 + o.spacing,
            s + 8.0 + 2.0 * line_h + 4.0 + o.spacing,
        ),
        LabelPos::Right => vec2(
            s + 14.0 + RIGHT_LABEL + o.spacing,
            (s + 8.0).max(2.0 * line_h + 8.0) + o.spacing,
        ),
    }
}

/// Columns that fit `width`; at least one.
pub fn columns(width: f32, cell: Vec2) -> usize {
    ((width / cell.x) as usize).max(1)
}

/// The grid cell of the `i`th item in reading order, in cells (x = column, y = row).
pub fn slot(i: usize, cols: usize) -> [f32; 2] {
    [(i % cols) as f32, (i / cols) as f32]
}

fn cell_of(p: [f32; 2]) -> (i64, i64) {
    (p[0].round() as i64, p[1].round() as i64)
}

/// Where each of `names` sits when arranged by hand, in cells: its saved position, else
/// the next grid cell (in reading order) no saved icon covers.
pub fn free_layout(
    names: &[&str],
    saved: &BTreeMap<String, [f32; 2]>,
    cols: usize,
) -> Vec<[f32; 2]> {
    let mut taken: HashSet<(i64, i64)> = (names.iter())
        .filter_map(|n| saved.get(*n))
        .map(|p| cell_of(*p))
        .collect();
    let mut next = 0;
    names
        .iter()
        .map(|n| match saved.get(*n) {
            Some(p) => *p,
            None => loop {
                let s = slot(next, cols);
                next += 1;
                if taken.insert(cell_of(s)) {
                    break s;
                }
            },
        })
        .collect()
}

/// The free grid cell nearest to `want` (column inside `0..cols`, row from 0); ties go to
/// the upper, then the left one.
fn nearest_free(want: (i64, i64), cols: usize, taken: &HashSet<(i64, i64)>) -> (i64, i64) {
    let cols = cols as i64;
    let want = (want.0.clamp(0, cols - 1), want.1.max(0));
    for r in 0.. {
        let mut best: Option<(i64, i64, i64)> = None;
        for row in (want.1 - r).max(0)..=want.1 + r {
            for col in (want.0 - r).max(0)..=(want.0 + r).min(cols - 1) {
                // Only the ring at distance r: the inside was searched before.
                if (col - want.0).abs().max((row - want.1).abs()) != r
                    || taken.contains(&(col, row))
                {
                    continue;
                }
                let d = (col - want.0).pow(2) + (row - want.1).pow(2);
                if best.is_none_or(|(bd, br, bc)| (d, row, col) < (bd, br, bc)) {
                    best = Some((d, row, col));
                }
            }
        }
        if let Some((_, row, col)) = best {
            return (col, row);
        }
    }
    unreachable!("rows are unbounded")
}

/// Clean Up: every icon moves to the nearest free grid cell, taken in reading order of
/// where it is now, so the arrangement keeps its shape and loses its overlaps.
pub fn clean_up(at: &[[f32; 2]], cols: usize) -> Vec<[f32; 2]> {
    let mut order: Vec<usize> = (0..at.len()).collect();
    order.sort_by(|&a, &b| {
        let key = |i: usize| (at[i][1].round() as i64, at[i][0]);
        let (ka, kb) = (key(a), key(b));
        ka.0.cmp(&kb.0).then(ka.1.total_cmp(&kb.1)).then(a.cmp(&b))
    });
    let mut taken = HashSet::new();
    let mut out = vec![[0.0; 2]; at.len()];
    for i in order {
        let c = nearest_free(cell_of(at[i]), cols, &taken);
        taken.insert(c);
        out[i] = [c.0 as f32, c.1 as f32];
    }
    out
}

/// Clean Up By: the keys icons are put in order by (folders first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanBy {
    Name,
    Kind,
    Size,
    Modified,
}

impl CleanBy {
    pub const ALL: [CleanBy; 4] = [Self::Name, Self::Kind, Self::Size, Self::Modified];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Kind => "Kind",
            Self::Size => "Size",
            Self::Modified => "Date Modified",
        }
    }
}

/// Clean Up By: the icons in reading order of `by` (Name and Kind A to Z, Size largest
/// first, Date Modified newest first; folders first), as positions by name.
pub fn clean_up_by(entries: &[&Entry], by: CleanBy, cols: usize) -> BTreeMap<String, [f32; 2]> {
    let mut order: Vec<&Entry> = entries.to_vec();
    let lower: HashMap<&str, String> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.name.to_lowercase()))
        .collect();
    order.sort_by(|a, b| {
        let name = || natord::compare(&lower[a.name.as_str()], &lower[b.name.as_str()]);
        (b.kind == Kind::Dir)
            .cmp(&(a.kind == Kind::Dir))
            .then_with(|| match by {
                CleanBy::Name => name(),
                CleanBy::Kind => a.ext.cmp(&b.ext).then_with(name),
                CleanBy::Size => b.size.cmp(&a.size).then_with(name),
                CleanBy::Modified => b.modified.cmp(&a.modified).then_with(name),
            })
    });
    (order.iter().enumerate())
        .map(|(i, e)| (e.name.clone(), slot(i, cols)))
        .collect()
}

/// Icons dropped by hand: `dragged` (names in `layout`) move by how far `primary` moved to
/// `at` (in cells). Free arrangement keeps the spot (never left of or above the origin);
/// Snap to grid takes the nearest free cell. Returns every icon's position.
pub fn drop_icons(
    layout: &[(&str, [f32; 2])],
    dragged: &[&str],
    primary: &str,
    at: [f32; 2],
    arrange: Arrange,
    cols: usize,
) -> BTreeMap<String, [f32; 2]> {
    let mut out: BTreeMap<String, [f32; 2]> =
        layout.iter().map(|(n, p)| ((*n).to_owned(), *p)).collect();
    let Some(from) = out.get(primary).copied() else {
        return out;
    };
    let delta = [at[0] - from[0], at[1] - from[1]];
    let moving: HashSet<&str> = dragged.iter().copied().collect();
    let mut taken: HashSet<(i64, i64)> = (layout.iter())
        .filter(|(n, _)| !moving.contains(n))
        .map(|(_, p)| cell_of(*p))
        .collect();
    // The primary icon first, so it gets the cell under the pointer.
    let mut order: Vec<&str> = vec![primary];
    order.extend(dragged.iter().copied().filter(|n| *n != primary));
    for name in order {
        let Some(p) = out.get_mut(name) else { continue };
        let moved = [(p[0] + delta[0]).max(0.0), (p[1] + delta[1]).max(0.0)];
        *p = match arrange {
            Arrange::SnapToGrid => {
                let c = nearest_free(cell_of(moved), cols, &taken);
                taken.insert(c);
                [c.0 as f32, c.1 as f32]
            }
            _ => moved,
        };
    }
    out
}

/// `name` shortened in the middle to fit `max_w` ("My Long Fi…me.pdf"), whole graphemes
/// only; `width` measures a piece of text.
pub fn middle_truncate(name: &str, max_w: f32, width: impl Fn(&str) -> f32) -> String {
    if width(name) <= max_w {
        return name.to_owned();
    }
    let g: Vec<&str> = name.graphemes(true).collect();
    let compose = |k: usize| {
        let tail = k / 2;
        format!(
            "{}…{}",
            g[..k - tail].concat(),
            g[g.len() - tail..].concat()
        )
    };
    // The most graphemes kept that still fit (widths grow with k).
    let (mut lo, mut hi) = (0, g.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&compose(mid)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    compose(lo)
}

/// An icon's label in at most two lines: the first breaks after a space, '-', '_' or '.'
/// when one is in its second half (else where it is full), the rest is shortened in the
/// middle when it does not fit the second line.
pub fn label_lines(name: &str, max_w: f32, width: impl Fn(&str) -> f32) -> Vec<String> {
    if width(name) <= max_w {
        return vec![name.to_owned()];
    }
    let g: Vec<&str> = name.graphemes(true).collect();
    // The longest first line that fits (at least one grapheme).
    let (mut lo, mut hi) = (1, g.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&g[..mid].concat()) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut n = lo;
    if let Some(b) = (n / 2 + 1..=n)
        .rev()
        .find(|&i| matches!(g[i - 1], " " | "-" | "_" | "."))
    {
        n = b;
    }
    let first = g[..n].concat().trim_end().to_owned();
    let rest = g[n..].concat();
    let rest = rest.trim_start();
    let mut lines = vec![first];
    if !rest.is_empty() {
        lines.push(middle_truncate(rest, max_w, width));
    }
    lines
}

// --- Drawing ---

/// What every tile of one frame shares.
struct Look {
    opts: IconOptions,
    line_h: f32,
    font: egui::FontId,
    muted: egui::Color32,
    selection: egui::Color32,
    hover: egui::Color32,
    fade: f32,
    bucket: u32,
}

/// The view's own items in the background context menu: arrangement and Clean Up.
fn arrange_menu(ui: &mut egui::Ui, opts: &mut IconOptions, act: &mut Option<Arrangement>) {
    for a in Arrange::ALL {
        if ui.radio(opts.arrange == a, a.label()).clicked() {
            opts.arrange = a;
            ui.close_menu();
        }
    }
    ui.separator();
    let clean = ui.add_enabled(opts.arrange.by_hand(), egui::Button::new("Clean Up"));
    if clean.clicked() {
        *act = Some(Arrangement::CleanUp);
        ui.close_menu();
    }
    ui.menu_button("Clean Up By", |ui| {
        for by in CleanBy::ALL {
            if ui.button(by.label()).clicked() {
                *act = Some(Arrangement::CleanUpBy(by));
                ui.close_menu();
            }
        }
    });
}

#[derive(Clone, Copy)]
enum Arrangement {
    CleanUp,
    CleanUpBy(CleanBy),
}

/// The options strip above the icons: arrangement, icon size, and the rest in a menu.
fn options_bar(
    ui: &mut egui::Ui,
    dir: &VPath,
    opts: &mut IconOptions,
    cx: &mut ViewCx,
    too_many: bool,
    act: &mut Option<Arrangement>,
) {
    use crate::a11y::Named;
    ui.horizontal(|ui| {
        ui.menu_button("Arrange", |ui| arrange_menu(ui, opts, act))
            .response
            .tip("Arrange icons: in the sort order, snapped to a grid or freely; Clean Up");
        ui.label("Size");
        ui.add(
            egui::Slider::new(&mut opts.icon_size, MIN_ICON..=MAX_ICON)
                .logarithmic(true)
                .show_value(false),
        )
        .named("Icon size")
        .on_hover_text(format!("Icon size: {} points", opts.icon_size.round()));
        ui.menu_button("View options", |ui| {
            ui.horizontal(|ui| {
                ui.label("Grid spacing");
                ui.add(egui::Slider::new(&mut opts.spacing, 0.0..=MAX_SPACING).integer())
                    .named("Grid spacing");
            });
            ui.horizontal(|ui| {
                ui.label("Label position");
                ui.radio_value(&mut opts.label, LabelPos::Bottom, "Bottom");
                ui.radio_value(&mut opts.label, LabelPos::Right, "Right");
            });
            let mut always = cx.views.always_open(dir) == Some(crate::pane::ViewMode::Grid);
            if ui
                .checkbox(&mut always, "Always open in Grid view")
                .on_hover_text("This folder opens in the Grid view")
                .changed()
            {
                let view = always.then_some(crate::pane::ViewMode::Grid);
                cx.views.set_always_open(dir, view);
            }
            if ui
                .button("Use as defaults")
                .on_hover_text("Folders without their own options use these")
                .clicked()
            {
                cx.views.set_icons(dir, *opts);
                cx.views.use_as_defaults(dir);
                ui.close_menu();
            }
        })
        .response
        .tip("Grid spacing, label position, Always open in Grid view");
        if too_many && opts.arrange.by_hand() {
            ui.weak(format!(
                "Shown in sort order: arranging by hand needs {MAX_ARRANGED} items or fewer"
            ));
        }
    });
}

pub fn ui(
    ui: &mut egui::Ui,
    id: (usize, usize),
    tab: &mut Tab,
    cx: &mut ViewCx,
    out: &mut Vec<Action>,
) {
    tab.visible(cx.show_hidden);
    let n = tab.visible_cached().len();
    let dir = tab.dir.clone();
    let mut opts = cx.views.icons_for(&dir);
    let too_many = n > MAX_ARRANGED;
    let mut act = None;
    options_bar(ui, &dir, &mut opts, cx, too_many, &mut act);

    let font = egui::FontId::proportional(FONT);
    let line_h = ui.fonts(|f| f.row_height(&font));
    let cell = cell_size(&opts, line_h);
    let cols = columns(ui.available_width(), cell);
    let view_h = ui.available_height();
    tab.row_step = cols;
    tab.page_rows = cols * ((view_h / cell.y) as usize).max(1);
    let by_hand = opts.arrange.by_hand() && !too_many;

    // Where each visible item sits, in cells (by hand only; else its slot).
    let layout: Vec<[f32; 2]> = if by_hand {
        let names: Vec<&str> = (tab.visible_cached().iter())
            .map(|&i| tab.entries()[i].name.as_str())
            .collect();
        let empty = BTreeMap::new();
        let saved = cx.views.positions(&dir).unwrap_or(&empty);
        free_layout(&names, saved, cols)
    } else {
        Vec::new()
    };
    let at = |i: usize| if by_hand { layout[i] } else { slot(i, cols) };

    // Content size in points.
    let extent = if by_hand {
        layout.iter().fold([0.0f32; 2], |m, p| {
            [m[0].max(p[0] + 1.0), m[1].max(p[1] + 1.0)]
        })
    } else {
        [cols.min(n.max(1)) as f32, n.div_ceil(cols) as f32]
    };
    let content = vec2(extent[0] * cell.x, extent[1] * cell.y);

    let mut area = if by_hand {
        egui::ScrollArea::both()
    } else {
        egui::ScrollArea::vertical()
    }
    .id_salt(("grid", id))
    .auto_shrink([false, false]);
    if let Some(pos) = tab.scroll_to.take().filter(|&p| p < n) {
        let (offset, viewport) = tab.grid_scroll;
        let top = at(pos)[1] * cell.y;
        if top < offset {
            area = area.vertical_scroll_offset(top);
        } else if top + cell.y > offset + viewport {
            area = area.vertical_scroll_offset(top + cell.y - viewport);
        }
    }

    let look = Look {
        opts,
        line_h,
        font,
        muted: cx.theme.muted(),
        selection: ui.visuals().selection.bg_fill,
        hover: ui.visuals().widgets.hovered.weak_bg_fill,
        // The tile that just got the cursor fades its highlight in (Task 23).
        fade: crate::anim::fade_in(
            ui.ctx(),
            egui::Id::new(("keel-fade-grid", id)),
            tab.cursor.as_deref(),
        ),
        bucket: thumb_bucket(opts.icon_size),
    };
    let mut renaming = tab.renaming.take();
    let mut rename_done = false;
    let mut clicks: Vec<(egui::Response, Entry)> = Vec::new();
    let mut over_target = false;
    let mut menu_act = None;
    let mut menu_opts = opts;

    let output = area.show_viewport(ui, |ui, viewport| {
        let origin = ui.max_rect().min;
        ui.set_min_size(content.max(viewport.size()));
        // The background: clears the selection, the folder's context menu, Clean Up.
        let shown = Rect::from_min_size(origin + viewport.min.to_vec2(), viewport.size());
        let bg = ui.interact(shown, ui.id().with("grid-bg"), egui::Sense::click());
        crate::a11y::not_a_stop(&bg);
        if bg.clicked() || bg.secondary_clicked() {
            tab.selected.clear();
        }
        bg.context_menu(|ui| {
            ui.menu_button("Arrange", |ui| {
                arrange_menu(ui, &mut menu_opts, &mut menu_act)
            });
            ui.separator();
            context_menu(ui, tab, None, out);
        });

        let rect_of =
            |p: [f32; 2]| Rect::from_min_size(origin + vec2(p[0] * cell.x, p[1] * cell.y), cell);
        let shown_items: Vec<usize> = if by_hand {
            (0..n)
                .filter(|&i| rect_of(layout[i]).intersects(shown))
                .collect()
        } else {
            let first = (viewport.top() / cell.y).floor().max(0.0) as usize;
            let last = (viewport.bottom() / cell.y).ceil() as usize;
            (first * cols..((last + 1) * cols).min(n)).collect()
        };
        for i in shown_items {
            let e = tab.entries()[tab.visible_cached()[i]].clone();
            let rect = rect_of(at(i));
            let r = tile(
                ui,
                (id, rect),
                &e,
                tab,
                cx,
                &look,
                &mut renaming,
                &mut rename_done,
                out,
            );
            let target = e.kind == Kind::Dir || crate::jobs::is_addable_archive(&e);
            if target && r.contains_pointer() && !tab.selected.contains(&e.name) {
                over_target = true;
            }
            r.context_menu(|ui| context_menu(ui, tab, Some(&e), out));
            clicks.push((r, e));
        }
        (origin, shown)
    });
    let (origin, shown) = output.inner;
    opts = menu_opts;
    let act = act.or(menu_act);
    tab.grid_scroll = (output.state.offset.y, output.inner_rect.height());

    // Icons dragged within the folder and dropped off any folder: moved, when arranged by
    // hand (onto a folder they move into it, as everywhere).
    let released = ui.input(|i| i.pointer.any_released());
    if by_hand && released && !over_target {
        let pos = ui
            .ctx()
            .pointer_interact_pos()
            .filter(|p| shown.contains(*p));
        let payload = egui::DragAndDrop::payload::<DragPayload>(ui.ctx())
            .filter(|p| p.pane == id.0 && p.dir == dir);
        if let (Some(pos), Some(p)) = (pos, payload) {
            egui::DragAndDrop::clear_payload(ui.ctx());
            let entries = tab.entries();
            let names: Vec<&str> = (tab.visible_cached().iter())
                .map(|&i| entries[i].name.as_str())
                .collect();
            let dragged: Vec<&str> = (tab.visible_cached().iter())
                .map(|&i| &entries[i])
                .filter(|e| p.paths.contains(&e.path))
                .map(|e| e.name.as_str())
                .collect();
            let primary = tab.cursor.as_deref().filter(|c| dragged.contains(c));
            if let Some(primary) = primary.or(dragged.first().copied()) {
                let at = (pos - origin) / cell - vec2(0.5, 0.5);
                let pairs: Vec<(&str, [f32; 2])> =
                    names.iter().copied().zip(layout.iter().copied()).collect();
                let moved = drop_icons(
                    &pairs,
                    &dragged,
                    primary,
                    [at.x, at.y],
                    look.opts.arrange,
                    cols,
                );
                cx.views.set_positions(&dir, moved);
            }
        }
    }
    // Clean Up / Clean Up By (options bar or background menu), then the options, saved
    // for the folder; the new positions show next frame.
    if let Some(act) = act {
        let names: Vec<&str> = (tab.visible_cached().iter())
            .map(|&i| tab.entries()[i].name.as_str())
            .collect();
        let positions = match act {
            Arrangement::CleanUp => {
                let all: Vec<[f32; 2]> = (0..n).map(at).collect();
                let tidy = clean_up(&all, cols);
                names.iter().map(|s| (*s).to_owned()).zip(tidy).collect()
            }
            Arrangement::CleanUpBy(by) => {
                let entries: Vec<&Entry> = (tab.visible_cached().iter())
                    .map(|&i| &tab.entries()[i])
                    .collect();
                if !opts.arrange.by_hand() {
                    opts.arrange = Arrange::SnapToGrid;
                }
                clean_up_by(&entries, by, cols)
            }
        };
        if !too_many {
            cx.views.set_positions(&dir, positions);
        }
        ui.ctx().request_repaint();
    }
    cx.views.set_icons(&dir, opts);

    for (r, e) in clicks {
        crate::pane::a11y_item(ui, &r, tab, &e, cx.keys);
        handle_click(&r, tab, &e, out);
        drag_and_drop(&r, id.0, tab, &e, out);
    }
    if !rename_done {
        tab.renaming = renaming;
    }
}

/// One icon with its label in the grid cell `rect`.
#[allow(clippy::too_many_arguments)]
fn tile(
    ui: &mut egui::Ui,
    (id, rect): ((usize, usize), Rect),
    e: &Entry,
    tab: &Tab,
    cx: &mut ViewCx,
    look: &Look,
    renaming: &mut Option<(String, String)>,
    rename_done: &mut bool,
    out: &mut Vec<Action>,
) -> egui::Response {
    let o = &look.opts;
    let inner = rect.shrink(o.spacing / 2.0 + 2.0);
    let resp = ui.interact(
        inner,
        egui::Id::new(("keel-grid-tile", id, &e.name)),
        crate::a11y::NO_TAB,
    );
    let s = o.icon_size;
    let (img_box, label_rect) = match o.label {
        LabelPos::Bottom => {
            let img = Rect::from_min_size(
                pos2(inner.center().x - s / 2.0, inner.top() + 4.0),
                vec2(s, s),
            );
            let label = Rect::from_min_max(
                pos2(inner.left() + 2.0, img.bottom() + 4.0),
                pos2(inner.right() - 2.0, inner.bottom()),
            );
            (img, label)
        }
        LabelPos::Right => {
            let img = Rect::from_min_size(
                pos2(inner.left() + 4.0, inner.center().y - s / 2.0),
                vec2(s, s),
            );
            let h = 2.0 * look.line_h;
            let label = Rect::from_min_max(
                pos2(img.right() + 6.0, inner.center().y - h / 2.0),
                pos2(inner.right() - 2.0, inner.center().y + h / 2.0),
            );
            (img, label)
        }
    };
    let selected = tab.selected.contains(&e.name);
    let strength = if tab.cursor.as_deref() == Some(&e.name) {
        look.fade
    } else {
        1.0
    };
    // Finder: a soft box behind the icon, the label on the selection color.
    if selected {
        let soft = look.selection.gamma_multiply(0.35 * strength);
        ui.painter().rect_filled(img_box.expand(3.0), 5.0, soft);
    } else if resp.hovered() {
        ui.painter().rect_filled(inner, 5.0, look.hover);
    }
    match cx.thumbs.get_sized(e, look.bucket) {
        Some((tex, size)) => {
            // Rendered in physical pixels (HiDPI): show in points.
            let size = size / ui.ctx().pixels_per_point();
            let scale = (img_box.width() / size.x)
                .min(img_box.height() / size.y)
                .min(1.0);
            egui::Image::new((tex, size))
                .paint_at(ui, Rect::from_center_size(img_box.center(), size * scale));
        }
        None => {
            // File-type icons a bit smaller than thumbnails, as before (56 of 96).
            let icon = if s <= 48.0 { s } else { (s * 0.6).max(48.0) };
            let at = Rect::from_center_size(img_box.center(), vec2(icon, icon));
            let style = cx.library.style_of(&e.path);
            crate::folder_style::paint_entry(ui, at, e, style, s > 24.0);
        }
    }
    match renaming {
        Some((name, text)) if *name == e.name => {
            // A child Ui: the edit box never moves the layout cursor, so the tiles after
            // it stay in place.
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(label_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            let r = child.add(egui::TextEdit::singleline(text).desired_width(label_rect.width()));
            if r.lost_focus() {
                *rename_done = true;
                if ui.input(|i| i.key_pressed(Key::Enter))
                    && !text.trim().is_empty()
                    && text != name
                {
                    out.push(Action::RenameTo {
                        from: e.path.clone(),
                        to: text.trim().to_owned(),
                    });
                }
            } else {
                crate::pane::rename_focus(&r, text, e.kind == Kind::Dir);
            }
        }
        _ => {
            let color = if selected {
                ui.visuals().selection.stroke.color
            } else if e.hidden {
                look.muted
            } else {
                ui.visuals().text_color()
            };
            let max_w = label_rect.width() - 6.0;
            let lines = ui.fonts(|f| {
                label_lines(&e.name, max_w, |t| {
                    t.chars().map(|c| f.glyph_width(&look.font, c)).sum()
                })
            });
            for (row, line) in lines.into_iter().enumerate() {
                let galley = ui.fonts(|f| f.layout_no_wrap(line, look.font.clone(), color));
                let w = galley.size().x;
                let top = label_rect.top() + row as f32 * look.line_h;
                let x = match o.label {
                    LabelPos::Bottom => label_rect.center().x - w / 2.0,
                    LabelPos::Right => label_rect.left() + 3.0,
                };
                if selected {
                    let pill = Rect::from_min_size(pos2(x - 3.0, top), vec2(w + 6.0, look.line_h));
                    ui.painter()
                        .rect_filled(pill, 4.0, look.selection.gamma_multiply(strength));
                }
                ui.painter().galley(pos2(x, top), galley, color);
            }
        }
    }
    if e.encrypted {
        let badge = Rect::from_min_size(img_box.right_top() - vec2(16.0, 0.0), vec2(16.0, 16.0));
        egui::Image::new(crate::icons::lock()).paint_at(ui, badge);
    }
    resp.on_hover_text(&e.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_cache_is_bounded_lru() {
        let ctx = egui::Context::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut thumbs = Thumbs::new(tx, ctx.clone(), Arc::new(Router::new()));
        let dir = keel_vfs::VPath::parse("mem://t/").unwrap();
        let key = |i: usize| PreviewKey {
            path: dir.join(&format!("{i}.png")),
            mtime: 0,
            size: 0,
            page: 0,
            width: 0,
        };
        for i in 0..=THUMB_CACHE {
            thumbs.insert(&ctx, key(i), Preview::Unsupported);
        }
        assert_eq!(thumbs.len(), THUMB_CACHE);
        assert!(!thumbs.cache.contains_key(&key(0)), "oldest evicted");
    }

    /// Task 66: big thumbnails are bounded by bytes, not only by count.
    #[test]
    fn thumbnail_cache_keeps_under_its_byte_budget() {
        let ctx = egui::Context::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut thumbs = Thumbs::new(tx, ctx.clone(), Arc::new(Router::new()));
        let dir = keel_vfs::VPath::parse("mem://t/").unwrap();
        // 256² RGBA = 256 KiB each; a budget of three.
        let side = 256u32;
        thumbs.budget = 3 * (side * side * 4) as usize;
        let image = || {
            Preview::Image(keel_preview::Rgba {
                w: side,
                h: side,
                data: vec![0; (side * side * 4) as usize],
            })
        };
        for i in 0..6 {
            let key = PreviewKey {
                path: dir.join(&format!("{i}.png")),
                mtime: 0,
                size: 0,
                page: 0,
                width: 512,
            };
            thumbs.insert(&ctx, key, image());
        }
        assert!(thumbs.bytes <= thumbs.budget);
        assert_eq!(thumbs.len(), 3);
    }

    #[test]
    fn thumbnail_buckets_follow_the_icon_size() {
        assert_eq!(thumb_bucket(16.0), 0);
        assert_eq!(thumb_bucket(96.0), 0);
        assert_eq!(thumb_bucket(97.0), 192);
        assert_eq!(thumb_bucket(192.0), 192);
        assert_eq!(thumb_bucket(512.0), 512);
    }

    /// Polish backlog: queued thumbnails for tiles that scrolled away are dropped.
    #[test]
    fn queued_thumbnails_of_hidden_tiles_are_cancelled() {
        let ctx = egui::Context::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut thumbs = Thumbs::new(tx, ctx.clone(), Arc::new(Router::new()));
        let dir = keel_vfs::VPath::parse("mem://t/").unwrap();
        let e = |n: &str| crate::tab::test_entry(&dir, n, keel_vfs::Kind::File, 1);
        let (a, b) = (e("a.png"), e("b.png"));
        let key = |e: &Entry| PreviewKey::of(e, 0, 0);
        // Pass 1 draws both tiles; pass 2 only `a` (b scrolled away); pass 3 sweeps.
        for (pass, tiles) in [(1, vec![&a, &b]), (2, vec![&a]), (3, vec![&a])] {
            thumbs.next_pass(pass);
            for t in tiles {
                thumbs.seen.insert(key(t));
                thumbs.pending.insert(key(t));
            }
        }
        assert!(thumbs.pending.contains(&key(&a)));
        assert!(!thumbs.pending.contains(&key(&b)));
        assert!(thumbs.pool.cancelled.lock().contains(&key(&b)));
        // Back in view: asked again, no longer cancelled.
        assert!(thumbs.pool.request(key(&b), b.clone()));
        assert!(!thumbs.pool.cancelled.lock().contains(&key(&b)));
    }

    /// Draws the grid view of `names` twice (layout settles) and returns the frame's
    /// shapes and the tab.
    fn draw(
        names: &[&str],
        views: &mut crate::view_prefs::ViewPrefs,
        setup: impl Fn(&mut Tab),
    ) -> (Vec<egui::epaint::ClippedShape>, Tab) {
        let dir = keel_vfs::VPath::local(std::env::temp_dir());
        let ctx = egui::Context::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let router = Arc::new(Router::new());
        let library = crate::library::LibraryUi::new(&router, tx.clone(), ctx.clone());
        let mut thumbs = Thumbs::new(tx, ctx.clone(), router.clone());
        let mut media = crate::media::Media::new(ctx.clone(), router);
        let theme = crate::theme::Theme::load("dark");
        let mut tab = Tab::new(dir.clone());
        tab.set_entries(
            names
                .iter()
                .map(|n| crate::tab::test_entry(&dir, n, keel_vfs::Kind::File, 1))
                .collect(),
        );
        setup(&mut tab);
        let mut shapes = Vec::new();
        for _ in 0..2 {
            let out = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |p| {
                    let mut cx = ViewCx {
                        theme: &theme,
                        show_hidden: false,
                        thumbs: &mut thumbs,
                        active: false,
                        banner: None,
                        preview: None,
                        column_widths: &mut Vec::new(),
                        library: &library,
                        drives: &[],
                        searcher: None,
                        tags_column: false,
                        media: &mut media,
                        sizes: &Default::default(),
                        keys: true,
                        views: &mut *views,
                    };
                    super::ui(p, (0, 0), &mut tab, &mut cx, &mut Vec::new());
                });
            });
            shapes = out.shapes;
        }
        (shapes, tab)
    }

    fn text_pos(shapes: &[egui::epaint::ClippedShape], text: &str) -> Option<egui::Pos2> {
        shapes.iter().find_map(|s| match &s.shape {
            egui::Shape::Text(t) if t.galley.text() == text => Some(t.pos),
            _ => None,
        })
    }

    /// Polish backlog: the inline rename box does not shift the tiles after it.
    #[test]
    fn grid_rename_keeps_later_tiles_in_place() {
        let x_of = |renaming: bool| {
            let (shapes, _) = draw(
                &["a.txt", "b.txt", "c.txt"],
                &mut Default::default(),
                |tab| {
                    if renaming {
                        tab.renaming = Some(("a.txt".into(), "a.txt".into()));
                    }
                },
            );
            text_pos(&shapes, "c.txt").expect("c.txt tile drawn").x
        };
        assert_eq!(x_of(true), x_of(false));
    }

    /// Task 66: a folder arranged by hand draws its icons where they were left, and the
    /// positions live in the profile's settings by folder.
    #[test]
    fn hand_arranged_icons_draw_at_their_saved_cells() {
        let dir = keel_vfs::VPath::local(std::env::temp_dir());
        let mut views = crate::view_prefs::ViewPrefs::default();
        views.set_icons(
            &dir,
            IconOptions {
                arrange: Arrange::Free,
                ..Default::default()
            },
        );
        let (sorted, _) = draw(&["a.txt", "b.txt"], &mut views, |_| {});
        let (a0, b0) = (
            text_pos(&sorted, "a.txt").unwrap(),
            text_pos(&sorted, "b.txt").unwrap(),
        );
        // Nothing saved yet: reading order, b right of a.
        assert!(b0.x > a0.x && (b0.y - a0.y).abs() < 0.5);
        views.set_positions(&dir, [("b.txt".to_owned(), [0.0, 2.0])].into());
        let (moved, _) = draw(&["a.txt", "b.txt"], &mut views, |_| {});
        let (a1, b1) = (
            text_pos(&moved, "a.txt").unwrap(),
            text_pos(&moved, "b.txt").unwrap(),
        );
        assert_eq!(a1, a0, "a keeps the first free cell");
        assert!(
            (b1.x - a1.x).abs() < 0.5 && b1.y > a1.y + 1.0,
            "b two rows down"
        );
    }

    /// Task 66: long labels wrap to two lines, the second shortened in the middle.
    #[test]
    fn long_labels_take_two_lines_and_lose_their_middle() {
        let long = "a very long file name that will never fit under one icon.pdf";
        let (shapes, _) = draw(&[long], &mut Default::default(), |_| {});
        let lines: Vec<&str> = shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().starts_with("a very") => {
                    Some(t.galley.text())
                }
                egui::Shape::Text(t) if t.galley.text().contains('…') => Some(t.galley.text()),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].contains('…') && lines[1].ends_with(".pdf"),
            "{lines:?}"
        );
    }

    fn width(t: &str) -> f32 {
        t.chars().count() as f32
    }

    #[test]
    fn middle_truncation_keeps_both_ends_and_whole_graphemes() {
        assert_eq!(middle_truncate("short.txt", 20.0, width), "short.txt");
        let t = middle_truncate("My Long File Name.pdf", 11.0, width);
        assert_eq!(t, "My Lo…e.pdf");
        assert!(width(&t) <= 11.0);
        // Combining marks and emoji sequences are never split.
        let accented = "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}.txt";
        let t = middle_truncate(accented, 8.0, width);
        assert!(
            !t.contains("…\u{301}") && !t.starts_with('\u{301}'),
            "{t:?}"
        );
        let family = "👨‍👩‍👧👨‍👩‍👧👨‍👩‍👧👨‍👩‍👧.png";
        let t = middle_truncate(family, 12.0, width);
        for piece in t.split('…') {
            assert!(!piece.starts_with('\u{200d}') && !piece.ends_with('\u{200d}'));
        }
        assert_eq!(middle_truncate("abcdef", 0.5, width), "…", "nothing fits");
    }

    #[test]
    fn labels_break_at_a_separator_then_truncate_in_the_middle() {
        assert_eq!(label_lines("fits", 10.0, width), vec!["fits"]);
        assert_eq!(
            label_lines("holiday photos 2024.jpg", 16.0, width),
            vec!["holiday photos", "2024.jpg"]
        );
        let lines = label_lines("holiday photos from the long summer.jpg", 16.0, width);
        assert_eq!(lines[0], "holiday photos");
        assert!(lines[1].contains('…') && lines[1].ends_with(".jpg"));
        assert!(width(&lines[1]) <= 16.0);
        // No separator in reach: broken where the line is full.
        assert_eq!(
            label_lines("abcdefghijklmnop", 10.0, width),
            vec!["abcdefghij", "klmnop"]
        );
    }

    #[test]
    fn grid_cells_follow_icon_size_spacing_and_label_position() {
        let o = IconOptions::default();
        let bottom = cell_size(&o, 15.0);
        let right = cell_size(
            &IconOptions {
                label: LabelPos::Right,
                ..o
            },
            15.0,
        );
        assert!(right.x > bottom.x && right.y < bottom.y);
        let spaced = cell_size(&IconOptions { spacing: 30.0, ..o }, 15.0);
        assert_eq!(spaced - bottom, vec2(24.0, 24.0));
        assert_eq!(columns(1000.0, bottom), (1000.0 / bottom.x) as usize);
        assert_eq!(columns(10.0, bottom), 1, "never zero columns");
        assert_eq!(slot(7, 3), [1.0, 2.0]);
    }

    #[test]
    fn free_layout_fills_the_cells_saved_icons_leave() {
        let saved: BTreeMap<String, [f32; 2]> = [
            ("b".to_owned(), [0.0, 0.0]),
            ("gone".to_owned(), [1.0, 0.0]), // no longer in the folder: ignored
            ("d".to_owned(), [5.5, 3.25]),
        ]
        .into();
        let at = free_layout(&["a", "b", "c", "d"], &saved, 3);
        assert_eq!(at, vec![[1.0, 0.0], [0.0, 0.0], [2.0, 0.0], [5.5, 3.25]]);
    }

    #[test]
    fn clean_up_aligns_to_the_nearest_free_cells_in_reading_order() {
        // Two icons fighting over cell (1,0), one far right of the 3 columns.
        let at = [[0.9, 0.1], [1.2, -0.2], [7.0, 0.0], [0.0, 2.4]];
        let tidy = clean_up(&at, 3);
        assert_eq!(tidy[0], [1.0, 0.0], "first in reading order wins the cell");
        assert_eq!(tidy[1], [0.0, 0.0], "the other takes the nearest free one");
        assert_eq!(tidy[2], [2.0, 0.0], "pulled inside the columns");
        assert_eq!(tidy[3], [0.0, 2.0]);
        // Already tidy: unchanged.
        assert_eq!(clean_up(&tidy, 3), tidy);
    }

    #[test]
    fn clean_up_by_orders_by_the_key_with_folders_first() {
        let dir = keel_vfs::VPath::parse("mem://t/").unwrap();
        let e = |n: &str, k: Kind, size: u64| crate::tab::test_entry(&dir, n, k, size);
        let items = [
            e("b.txt", Kind::File, 10),
            e("a.png", Kind::File, 30),
            e("z", Kind::Dir, 0),
            e("c.png", Kind::File, 20),
        ];
        let refs: Vec<&Entry> = items.iter().collect();
        let names_in_order = |by| {
            let p = clean_up_by(&refs, by, 2);
            let mut v: Vec<(String, [f32; 2])> = p.into_iter().collect();
            v.sort_by(|a, b| (a.1[1], a.1[0]).partial_cmp(&(b.1[1], b.1[0])).unwrap());
            v.into_iter().map(|(n, _)| n).collect::<Vec<_>>()
        };
        assert_eq!(
            names_in_order(CleanBy::Name),
            ["z", "a.png", "b.txt", "c.png"]
        );
        assert_eq!(
            names_in_order(CleanBy::Kind),
            ["z", "a.png", "c.png", "b.txt"]
        );
        assert_eq!(
            names_in_order(CleanBy::Size),
            ["z", "a.png", "c.png", "b.txt"]
        );
        let p = clean_up_by(&refs, CleanBy::Name, 2);
        assert_eq!(p["b.txt"], [0.0, 1.0], "third item: second row");
    }

    #[test]
    fn dropped_icons_move_together_and_snap_to_free_cells() {
        let layout = [
            ("a", [0.0, 0.0]),
            ("b", [1.0, 0.0]),
            ("c", [2.0, 0.0]),
            ("d", [0.0, 1.0]),
        ];
        // Free: a and b move by the same amount, fractions kept.
        let free = drop_icons(&layout, &["a", "b"], "a", [0.5, 2.25], Arrange::Free, 3);
        assert_eq!(free["a"], [0.5, 2.25]);
        assert_eq!(free["b"], [1.5, 2.25]);
        assert_eq!(free["c"], [2.0, 0.0], "others stay");
        // Never above or left of the origin.
        let edge = drop_icons(&layout, &["c"], "c", [-4.0, -1.0], Arrange::Free, 3);
        assert_eq!(edge["c"], [0.0, 0.0]);
        // Snap: a dropped on d's cell takes the nearest free one instead.
        let snap = drop_icons(&layout, &["a"], "a", [0.1, 0.9], Arrange::SnapToGrid, 3);
        assert_eq!(snap["d"], [0.0, 1.0]);
        assert_eq!(
            snap["a"],
            [0.0, 0.0],
            "its own old cell is the nearest free one"
        );
        let snap = drop_icons(&layout, &["a"], "a", [1.2, 1.4], Arrange::SnapToGrid, 3);
        assert_eq!(snap["a"], [1.0, 1.0]);
    }
}
