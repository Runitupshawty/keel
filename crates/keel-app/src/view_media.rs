//! Media view (Task 32): square photo / video tiles drawn from sidecar thumbnails, with
//! optional date headers. Virtualised: only rows in the viewport are laid out; the rows
//! within two screens of it are prefetched. Selection, keys, drag and the context menu are
//! the grid view's.

use crate::keys::Action;
use crate::media::{self, Media, MediaType, Req, Row, Tex, TileSize};
use crate::pane::{context_menu, drag_and_drop, handle_click, ViewCx};
use crate::tab::{SortKey, Tab};
use egui::{pos2, vec2, Color32, Key, Rect, Sense};
use keel_vfs::Entry;
use std::sync::Arc;
use std::time::{Duration, Instant};

const GAP: f32 = 4.0;
const HEADER_H: f32 = 30.0;
/// Rows within this many screens above and below the viewport are prefetched.
const PREFETCH_SCREENS: f32 = 2.0;
/// Prefetch priorities start here (visible tiles come first).
const PREFETCH_PRIO: u64 = 1 << 32;
/// One tile size step per Ctrl+wheel gesture step.
const ZOOM_STEP: Duration = Duration::from_millis(150);

/// The layout of one tab's listing, rebuilt when its inputs change.
struct Layout {
    sig: (u64, usize, u32, bool, usize),
    rows: Vec<Row>,
    tops: Vec<f32>,
}

/// The day of visible item `i` for date headers: from the date map, else the file's
/// modified day; None for folders and other files.
fn day_of(tab: &Tab, days: &std::collections::HashMap<String, i64>, i: usize) -> Option<i64> {
    let e = &tab.entries()[tab.visible_cached()[i]];
    media::media_type(e)?;
    Some(
        days.get(&e.name)
            .copied()
            .unwrap_or_else(|| media::modified_day(e.modified)),
    )
}

/// `e` as a worker request (`library://` paths resolved to their real path).
pub fn req_of(e: &Entry, sources: &[keel_core::SourceSummary]) -> Req {
    Req {
        entry: e.clone(),
        real: crate::library::real_of(sources, &e.path).unwrap_or_else(|| e.path.clone()),
    }
}

/// Size buttons, the date header toggle, and Ctrl+wheel over the view.
fn header(ui: &mut egui::Ui, tab: &mut Tab, m: &mut Media) {
    ui.horizontal(|ui| {
        for s in TileSize::ALL {
            if ui
                .selectable_label(m.tile == s, s.label())
                .on_hover_text("Tile size (Ctrl+wheel)")
                .clicked()
            {
                m.tile = s;
            }
        }
        ui.separator();
        if ui
            .selectable_label(m.dates, "Dates")
            .on_hover_text("Group photos by the day they were taken")
            .clicked()
        {
            m.dates = !m.dates;
            // Days only group well in date order.
            if m.dates && tab.sort.0 == SortKey::Name {
                tab.sort = (SortKey::Modified, false);
            }
        }
    });
}

fn ctrl_wheel(ui: &egui::Ui, m: &mut Media) {
    let zoom = ui.input(|i| i.zoom_delta());
    if zoom == 1.0 || !ui.rect_contains_pointer(ui.max_rect()) {
        return;
    }
    let id = egui::Id::new("keel-media-zoom");
    let last: Option<Instant> = ui.data(|d| d.get_temp(id));
    if last.is_some_and(|t| t.elapsed() < ZOOM_STEP) {
        return;
    }
    ui.data_mut(|d| d.insert_temp(id, Instant::now()));
    m.tile = m.tile.step(zoom > 1.0);
}

pub fn ui(
    ui: &mut egui::Ui,
    id: (usize, usize),
    tab: &mut Tab,
    cx: &mut ViewCx,
    out: &mut Vec<Action>,
) {
    tab.visible(cx.show_hidden);
    header(ui, tab, cx.media);
    ctrl_wheel(ui, cx.media);
    let ppp = ui.ctx().pixels_per_point();
    let tile = cx.media.tile.points();
    let cell = tile + GAP;
    let tile_px = (tile * ppp).round() as u32;
    let cols = ((ui.available_width() / cell) as usize).max(1);
    let n = tab.visible_cached().len();

    // Rows (and date headers), cached in egui memory per tab until the inputs change.
    let dates = cx.media.dates;
    let days_len;
    let layout = {
        let days = dates.then(|| {
            let sources = &cx.library.sources;
            let t: &Tab = tab;
            cx.media.days(&t.dir, t.generation(), || {
                t.entries()
                    .iter()
                    .filter(|e| media::media_type(e).is_some())
                    .map(|e| (e.name.clone(), req_of(e, sources)))
                    .collect()
            })
        });
        days_len = days.map_or(0, |d| d.len());
        let sig = (tab.visible_gen(), cols, tile as u32, dates, days_len);
        let lid = egui::Id::new(("keel-media-layout", id));
        let cached: Option<Arc<Layout>> = ui.data(|d| d.get_temp(lid));
        match cached.filter(|l| l.sig == sig) {
            Some(l) => l,
            None => {
                let rows = match days {
                    Some(days) => {
                        let f = |i: usize| day_of(tab, days, i);
                        media::layout(n, cols, Some(&f))
                    }
                    None => media::layout(n, cols, None),
                };
                let tops = media::row_tops(&rows, cell, HEADER_H);
                let l = Arc::new(Layout { sig, rows, tops });
                ui.data_mut(|d| d.insert_temp(lid, l.clone()));
                l
            }
        }
    };
    let (rows, tops) = (&layout.rows, &layout.tops);
    let total = *tops.last().unwrap_or(&0.0);
    let view_h = ui.available_height();
    tab.row_step = cols;
    tab.page_rows = cols * ((view_h / cell) as usize).max(1);

    let mut area = egui::ScrollArea::vertical()
        .id_salt(("media", id))
        .auto_shrink([false, false]);
    if let Some(pos) = tab.scroll_to.take() {
        let r = rows.partition_point(|row| match row {
            Row::Tiles(_, end) => *end <= pos,
            Row::Header(_) => true,
        });
        if r < rows.len() {
            // A header right above the row comes into view with it.
            let top = if r > 0 && matches!(rows[r - 1], Row::Header(_)) {
                tops[r - 1]
            } else {
                tops[r]
            };
            let (offset, viewport) = tab.grid_scroll;
            if top < offset {
                area = area.vertical_scroll_offset(top);
            } else if tops[r] + cell > offset + viewport {
                area = area.vertical_scroll_offset(tops[r] + cell - viewport);
            }
        }
    }

    let selection = ui.visuals().selection.bg_fill;
    let hover = ui.visuals().widgets.hovered.weak_bg_fill;
    let placeholder = ui.visuals().extreme_bg_color;
    let text = ui.visuals().text_color();
    let muted = cx.theme.muted();
    let fade = crate::anim::fade_in(
        ui.ctx(),
        egui::Id::new(("keel-fade-media", id)),
        tab.cursor.as_deref(),
    );
    let mut renaming = tab.renaming.take();
    let mut rename_done = false;
    let mut clicks: Vec<(egui::Response, Entry)> = Vec::new();
    let sources = cx.library.sources.clone();

    let output = area.show_viewport(ui, |ui, viewport| {
        ui.set_height(total);
        let origin = ui.max_rect().min;
        let row_h = |r: usize| tops[r + 1] - tops[r];
        let first = tops[..rows.len()]
            .partition_point(|&t| t <= viewport.min.y)
            .saturating_sub(1);
        let reach = viewport.height() * PREFETCH_SCREENS;
        let pre_first = tops[..rows.len()]
            .partition_point(|&t| t <= viewport.min.y - reach)
            .saturating_sub(1);
        let pre_last = tops[..rows.len()].partition_point(|&t| t < viewport.max.y + reach);

        // What the workers should load: visible tiles first (in reading order), then the
        // prefetch rows by distance. Rebuilt only when the window or layout moves.
        let sig = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (layout.sig, first, pre_first, pre_last, tile_px).hash(&mut h);
            h.finish() | 1
        };
        if cx.media.sig[id.0] != sig {
            cx.media.sig[id.0] = sig;
            let mut list = Vec::new();
            for r in pre_first..pre_last {
                let Row::Tiles(a, b) = rows[r] else { continue };
                let visible = tops[r] + row_h(r) > viewport.min.y && tops[r] < viewport.max.y;
                for pos in a..b {
                    let e = &tab.entries()[tab.visible_cached()[pos]];
                    let Some(kind) = media::media_type(e) else {
                        continue;
                    };
                    let video = kind == MediaType::Video;
                    let prio = if visible {
                        pos as u64
                    } else {
                        let rows_away = first.abs_diff(r);
                        PREFETCH_PRIO + rows_away as u64 * cols as u64 + (pos - a) as u64
                    };
                    // A video's thumbnail stands in until its strip is made (or for good
                    // when the strip cannot be).
                    let keys = [
                        Some(media::tile_key(e, video, tile_px)),
                        video.then(|| media::thumb_key(e, tile_px)),
                    ];
                    for key in keys.into_iter().flatten() {
                        if !cx.media.has(&key) {
                            list.push((key, prio, req_of(e, &sources)));
                        }
                    }
                }
            }
            cx.media.want(id.0, list);
        }

        for r in first..rows.len() {
            if tops[r] >= viewport.max.y {
                break;
            }
            let top = origin.y + tops[r];
            match rows[r] {
                Row::Header(day) => {
                    let label = match day {
                        Some(d) => media::day_label(d),
                        None => "Folders and other files".into(),
                    };
                    ui.painter().text(
                        pos2(origin.x + GAP, top + HEADER_H / 2.0 + 2.0),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(14.0),
                        text,
                    );
                }
                Row::Tiles(a, b) => {
                    for pos in a..b {
                        let i = tab.visible_cached()[pos];
                        let e = &tab.entries()[i];
                        let x = origin.x + (pos - a) as f32 * cell + GAP / 2.0;
                        let rect = Rect::from_min_size(pos2(x, top + GAP / 2.0), vec2(tile, tile));
                        let resp =
                            ui.interact(rect, ui.id().with(&e.name), Sense::click_and_drag());
                        let back = rect.expand(GAP / 2.0 - 0.5);
                        if tab.selected.contains(&e.name) {
                            let t = if tab.cursor.as_deref() == Some(&e.name) {
                                fade
                            } else {
                                1.0
                            };
                            ui.painter()
                                .rect_filled(back, 4.0, selection.gamma_multiply(t));
                        } else if resp.hovered() {
                            ui.painter().rect_filled(back, 4.0, hover);
                        }
                        let img = rect.shrink(2.0);
                        match media::media_type(e) {
                            Some(kind) => {
                                let video = kind == MediaType::Video;
                                let (tex, strip) = cx.media.tile_tex(e, video, tile_px);
                                match tex {
                                    Tex::Ready(tex, size) => {
                                        let uv = if strip {
                                            let frame = resp
                                                .hover_pos()
                                                .map(|p| {
                                                    media::strip_frame(
                                                        (p.x - img.left()) / img.width(),
                                                    )
                                                })
                                                .unwrap_or(0);
                                            media::strip_uv(frame)
                                        } else {
                                            media::FULL_UV
                                        };
                                        let uv = media::cover_uv(size, uv);
                                        ui.painter().image(tex, img, uv, Color32::WHITE);
                                        if video {
                                            video_badge(ui, img);
                                        }
                                    }
                                    Tex::Missing => {
                                        ui.painter().rect_filled(img, 3.0, placeholder);
                                    }
                                    Tex::Failed => icon_tile(ui, e, img, muted, false),
                                }
                            }
                            None => {
                                icon_tile(ui, e, img, if e.hidden { muted } else { text }, true)
                            }
                        }
                        if e.encrypted {
                            let badge = Rect::from_min_size(
                                img.right_top() - vec2(16.0, 0.0),
                                vec2(16.0, 16.0),
                            );
                            egui::Image::new(crate::icons::lock()).paint_at(ui, badge);
                        }
                        match &mut renaming {
                            Some((name, text)) if *name == e.name => {
                                let edit = Rect::from_min_max(
                                    pos2(img.left(), img.bottom() - 24.0),
                                    img.max,
                                );
                                let mut child = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(edit)
                                        .layout(egui::Layout::top_down(egui::Align::Min)),
                                );
                                let r = child.add(
                                    egui::TextEdit::singleline(text).desired_width(edit.width()),
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
                            _ => {}
                        }
                        let resp = resp.on_hover_text(&e.name);
                        resp.context_menu(|ui| context_menu(ui, tab, Some(e), out));
                        clicks.push((resp, e.clone()));
                    }
                }
            }
        }
    });
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

/// A folder or file without a thumbnail: its icon (and name for non-media files).
fn icon_tile(ui: &egui::Ui, e: &Entry, rect: Rect, color: Color32, named: bool) {
    let side = (rect.width() * 0.45).min(96.0);
    let icon_center = if named {
        pos2(rect.center().x, rect.top() + rect.height() * 0.42)
    } else {
        rect.center()
    };
    egui::Image::new(crate::icons::large(crate::icons::icon_for(e)))
        .fit_to_exact_size(vec2(side, side))
        .paint_at(ui, Rect::from_center_size(icon_center, vec2(side, side)));
    if named {
        let mut job = egui::text::LayoutJob::simple(
            e.name.clone(),
            egui::FontId::proportional(12.0),
            color,
            rect.width() - 4.0,
        );
        job.wrap.max_rows = 2;
        job.wrap.break_anywhere = true;
        job.halign = egui::Align::Center;
        let galley = ui.fonts(|f| f.layout_job(job));
        let y = icon_center.y + side / 2.0 + 4.0;
        ui.painter().galley(pos2(rect.center().x, y), galley, color);
    }
}

/// A play triangle in the bottom-left corner of a video tile.
fn video_badge(ui: &egui::Ui, img: Rect) {
    let c = img.left_bottom() + vec2(14.0, -14.0);
    ui.painter()
        .circle_filled(c, 10.0, Color32::from_black_alpha(140));
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            c + vec2(-3.0, -5.0),
            c + vec2(5.0, 0.0),
            c + vec2(-3.0, 5.0),
        ],
        Color32::WHITE,
        egui::Stroke::NONE,
    ));
}

#[cfg(test)]
#[path = "view_media_tests.rs"]
mod tests;
