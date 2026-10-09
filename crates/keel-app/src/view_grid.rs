//! Grid view: virtual rows of tiles with image / PDF / video thumbnails.

use crate::keys::Action;
use crate::pane::{context_menu, drag_and_drop, handle_click, ViewCx};
use crate::preview_panel::PreviewKey;
use crate::state::Msg;
use crate::tab::Tab;
use crate::worker::{ThumbPool, THUMB_PX};
use crossbeam_channel::Sender;
use egui::{pos2, vec2, Key, Rect, Sense, TextureHandle};
use keel_preview::Preview;
use keel_vfs::{Entry, Router};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const THUMB_CACHE: usize = 2000;
const TILE: egui::Vec2 = vec2(108.0, 132.0);
/// Extensions whose preview is an image (keel-preview image, pdf and video renderers).
const THUMB_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "pdf", "mp4", "mkv", "mov", "avi",
    "webm", "m4v",
];

/// LRU of thumbnail textures fed by the 4-thread `ThumbPool`.
pub struct Thumbs {
    pool: ThumbPool,
    /// `None` = no image could be made; not retried until the file changes.
    cache: HashMap<PreviewKey, (Option<TextureHandle>, u64)>,
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

impl Thumbs {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, router: Arc<Router>) -> Self {
        Self {
            pool: ThumbPool::new(tx, ctx.clone(), router),
            cache: HashMap::new(),
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
        let key = PreviewKey::of(e, 0, 0);
        self.next_pass(self.ctx.cumulative_pass_nr());
        self.seen.insert(key.clone());
        self.clock += 1;
        if let Some((tex, used)) = self.cache.get_mut(&key) {
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
            Preview::Image(r) | Preview::Pdf { image: r, .. } | Preview::Video { thumb: r, .. } => {
                Some(r)
            }
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
        self.clock += 1;
        self.cache.insert(key, (tex, self.clock));
        if self.cache.len() > THUMB_CACHE {
            // ponytail: O(n) eviction scan over 2,000 entries; a linked LRU if it ever shows.
            if let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.cache.remove(&oldest);
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }
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
    let cols = ((ui.available_width() / TILE.x) as usize).max(1);
    let rows = n.div_ceil(cols);
    let view_h = ui.available_height();
    tab.row_step = cols;
    tab.page_rows = cols * ((view_h / TILE.y) as usize).max(1);

    let mut area = egui::ScrollArea::vertical()
        .id_salt(("grid", id))
        .auto_shrink([false, true]);
    if let Some(pos) = tab.scroll_to.take() {
        let (offset, viewport) = tab.grid_scroll;
        let top = (pos / cols) as f32 * TILE.y;
        if top < offset {
            area = area.vertical_scroll_offset(top);
        } else if top + TILE.y > offset + viewport {
            area = area.vertical_scroll_offset(top + TILE.y - viewport);
        }
    }

    let muted = cx.theme.muted();
    let selection = ui.visuals().selection.bg_fill;
    let hover = ui.visuals().widgets.hovered.weak_bg_fill;
    let mut renaming = tab.renaming.take();
    let mut rename_done = false;
    let mut clicks: Vec<(egui::Response, Entry)> = Vec::new();

    let output = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            area.show_rows(ui, TILE.y, rows, |ui, range| {
                for r in range {
                    ui.horizontal(|ui| {
                        for c in 0..cols {
                            let Some(&i) = tab.visible_cached().get(r * cols + c) else {
                                break;
                            };
                            let e = &tab.entries()[i];
                            let (rect, resp) =
                                ui.allocate_exact_size(TILE, Sense::click_and_drag());
                            let tile = rect.shrink(3.0);
                            if tab.selected.contains(&e.name) {
                                ui.painter().rect_filled(tile, 4.0, selection);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(tile, 4.0, hover);
                            }
                            let img_box = Rect::from_min_size(
                                pos2(rect.center().x - THUMB_PX as f32 / 2.0, rect.top() + 4.0),
                                vec2(THUMB_PX as f32, THUMB_PX as f32),
                            );
                            match cx.thumbs.get(e) {
                                Some((tex, size)) => {
                                    // Rendered in physical pixels (HiDPI): show in points.
                                    let size = size / ui.ctx().pixels_per_point();
                                    let scale = (img_box.width() / size.x)
                                        .min(img_box.height() / size.y)
                                        .min(1.0);
                                    egui::Image::new((tex, size)).paint_at(
                                        ui,
                                        Rect::from_center_size(img_box.center(), size * scale),
                                    );
                                }
                                None => {
                                    // The size hint makes the SVG rasterize at tile size, not 16 px.
                                    let icon = vec2(56.0, 56.0);
                                    egui::Image::new(crate::icons::large(crate::icons::icon_for(
                                        e,
                                    )))
                                    .fit_to_exact_size(icon)
                                    .paint_at(ui, Rect::from_center_size(img_box.center(), icon));
                                }
                            }
                            let label_rect = Rect::from_min_max(
                                pos2(tile.left() + 2.0, img_box.bottom() + 2.0),
                                pos2(tile.right() - 2.0, tile.bottom()),
                            );
                            match &mut renaming {
                                Some((name, text)) if *name == e.name => {
                                    // A child Ui: the edit box never moves the layout
                                    // cursor, so the tiles after it stay in place.
                                    let mut child = ui.new_child(
                                        egui::UiBuilder::new()
                                            .max_rect(label_rect)
                                            .layout(egui::Layout::top_down(egui::Align::Min)),
                                    );
                                    let r = child.add(
                                        egui::TextEdit::singleline(text)
                                            .desired_width(label_rect.width()),
                                    );
                                    if r.lost_focus() {
                                        rename_done = true;
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
                                        r.request_focus();
                                    }
                                }
                                _ => {
                                    let color = if e.hidden {
                                        muted
                                    } else {
                                        ui.visuals().text_color()
                                    };
                                    let mut job = egui::text::LayoutJob::simple(
                                        e.name.clone(),
                                        egui::FontId::proportional(12.0),
                                        color,
                                        label_rect.width(),
                                    );
                                    job.wrap.max_rows = 2;
                                    job.wrap.break_anywhere = true;
                                    job.halign = egui::Align::Center;
                                    let galley = ui.fonts(|f| f.layout_job(job));
                                    ui.painter().galley(
                                        pos2(label_rect.center().x, label_rect.top()),
                                        galley,
                                        color,
                                    );
                                }
                            }
                            if e.encrypted {
                                let badge = Rect::from_min_size(
                                    img_box.right_top() - vec2(16.0, 0.0),
                                    vec2(16.0, 16.0),
                                );
                                egui::Image::new(crate::icons::lock()).paint_at(ui, badge);
                            }
                            let resp = resp.on_hover_text(&e.name);
                            resp.context_menu(|ui| context_menu(ui, tab, Some(e), out));
                            clicks.push((resp, e.clone()));
                        }
                    });
                }
            })
        })
        .inner;
    tab.grid_scroll = (output.state.offset.y, output.inner_rect.height());

    for (r, e) in clicks {
        handle_click(&r, tab, &e, out);
        drag_and_drop(&r, id.0, tab, &e, out);
    }
    if !rename_done {
        tab.renaming = renaming;
    }
    crate::view_details::empty_area(ui, tab, out);
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

    /// Polish backlog: the inline rename box does not shift the tiles after it.
    #[test]
    fn grid_rename_keeps_later_tiles_in_place() {
        let dir = keel_vfs::VPath::local(std::env::temp_dir());
        let x_of = |renaming: bool| {
            let ctx = egui::Context::default();
            let (tx, _rx) = crossbeam_channel::unbounded();
            let mut thumbs = Thumbs::new(tx, ctx.clone(), Arc::new(Router::new()));
            let theme = crate::theme::Theme::load("dark");
            let mut tab = Tab::new(dir.clone());
            tab.set_entries(
                ["a.txt", "b.txt", "c.txt"]
                    .map(|n| crate::tab::test_entry(&dir, n, keel_vfs::Kind::File, 1))
                    .into(),
            );
            if renaming {
                tab.renaming = Some(("a.txt".into(), "a.txt".into()));
            }
            let mut x = None;
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
                        };
                        super::ui(p, (0, 0), &mut tab, &mut cx, &mut Vec::new());
                    });
                });
                x = out.shapes.iter().find_map(|s| match &s.shape {
                    egui::Shape::Text(t) if t.galley.text() == "c.txt" => Some(t.pos.x),
                    _ => None,
                });
            }
            x.expect("c.txt tile drawn")
        };
        assert_eq!(x_of(true), x_of(false));
    }
}
