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
    clock: u64,
    /// Thumbnails for remote files (setting `remote_thumbnails`): each one is a download.
    pub remote: bool,
}

impl Thumbs {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, router: Arc<Router>) -> Self {
        Self {
            pool: ThumbPool::new(tx, ctx, router),
            cache: HashMap::new(),
            pending: HashSet::new(),
            clock: 0,
            remote: false,
        }
    }

    /// The cached thumbnail, or None while it is (re)queued.
    pub fn get(&mut self, e: &Entry) -> Option<(egui::TextureId, egui::Vec2)> {
        if !THUMB_EXTS.contains(&e.ext.as_str()) || (!self.remote && e.path.scheme == "sftp") {
            return None;
        }
        let key = PreviewKey::of(e, 0);
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
                                    let r = ui.put(label_rect, egui::TextEdit::singleline(text));
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
                            let resp = resp.on_hover_text(&e.name);
                            resp.context_menu(|ui| context_menu(ui, true, false, out));
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
        };
        for i in 0..=THUMB_CACHE {
            thumbs.insert(&ctx, key(i), Preview::Unsupported);
        }
        assert_eq!(thumbs.len(), THUMB_CACHE);
        assert!(!thumbs.cache.contains_key(&key(0)), "oldest evicted");
    }
}
