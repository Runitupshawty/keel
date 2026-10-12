//! Task 66: Gallery view. A big preview of the cursor item (the preview panel's renderers:
//! images, PDF pages, video frames, audio, text, documents) over a strip of thumbnails,
//! with the item's details and Quick Actions on the right. Folders show a big folder icon.
//! The arrow keys move through the items (the tab steps one item per row).

use crate::keys::Action;
use crate::pane::{context_menu, drag_and_drop, handle_click, Pane, ViewCx, ViewMode};
use crate::tab::{Nav, Tab, TabKind};
use egui::{pos2, vec2, Rect, Sense, UiBuilder};
use keel_preview::Preview;
use keel_vfs::{Entry, Kind};

/// The thumbnail strip's height, a thumbnail's box and the gap between them.
const STRIP_H: f32 = 88.0;
const THUMB: f32 = 64.0;
const GAP: f32 = 8.0;
/// The details and Quick Actions column.
const SIDEBAR_W: f32 = 240.0;

/// The active pane shows the gallery of a folder: the preview follows its cursor.
pub fn wants_preview(pane: &Pane) -> bool {
    let tab = &pane.tabs[pane.active];
    pane.view == ViewMode::Gallery && tab.kind == TabKind::Dir
}

/// The gallery always shows an item: the first one gets the cursor when none has it.
/// True when it moved the cursor.
pub fn ensure_cursor(tab: &mut Tab) -> bool {
    let none = tab.cursor_pos().is_none() && !tab.visible_cached().is_empty();
    if none {
        tab.move_cursor(Nav::Home, false);
    }
    none
}

/// The strip's scroll offset that keeps thumbnail `pos` in view of `width` points.
pub fn strip_offset(pos: usize, offset: f32, width: f32) -> f32 {
    let left = pos as f32 * (THUMB + GAP);
    let right = left + THUMB + GAP;
    if left < offset {
        left
    } else if right > offset + width {
        right - width
    } else {
        offset
    }
}

/// How many thumbnails the strip shows at `width` points (PageUp / PageDown).
pub fn strip_page(width: f32) -> usize {
    ((width / (THUMB + GAP)) as usize).max(1)
}

/// "Folder", "Link", "PDF file", "File".
fn kind_text(e: &Entry) -> String {
    match (&e.kind, e.is_link) {
        (_, true) => "Link".into(),
        (Kind::Dir, _) => "Folder".into(),
        _ if e.ext.is_empty() => "File".into(),
        _ => format!("{} file", e.ext.to_uppercase()),
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
    if ensure_cursor(tab) {
        // The preview follows the cursor at the start of a frame.
        ui.ctx().request_repaint();
    }
    let full = ui.available_rect_before_wrap();
    let n = tab.visible_cached().len();
    if n == 0 {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| ui.weak("This folder is empty"));
        crate::view_details::empty_area(ui, tab, out);
        return;
    }
    let side_w = SIDEBAR_W.min(full.width() * 0.4);
    let main = Rect::from_min_max(full.min, pos2(full.right() - side_w, full.bottom()));
    let strip = Rect::from_min_max(pos2(main.left(), main.bottom() - STRIP_H), main.max);
    let stage = Rect::from_min_max(main.min, pos2(main.right(), strip.top())).shrink(10.0);
    let side = Rect::from_min_max(pos2(main.right(), full.top()), full.max);
    // One item per step: Left/Right and Up/Down all move along the strip.
    tab.row_step = 1;
    tab.page_rows = strip_page(strip.width());
    ui.allocate_rect(full, Sense::hover());

    let current = tab
        .cursor_pos()
        .map(|pos| tab.entries()[tab.visible_cached()[pos]].clone());
    if let Some(e) = &current {
        let mut stage_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(stage)
                .id_salt(("gallery-stage", id))
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        stage_ui.set_clip_rect(stage);
        big_preview(&mut stage_ui, id, e, cx, out);
    }
    ui.painter().hline(
        main.x_range(),
        strip.top(),
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    let mut strip_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(strip)
            .id_salt(("gallery-strip", id)),
    );
    strip_ui.set_clip_rect(strip);
    thumbnails(&mut strip_ui, id, tab, cx, out);
    ui.painter().vline(
        side.left(),
        side.y_range(),
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    let mut side_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(side.shrink2(vec2(10.0, 6.0)))
            .id_salt(("gallery-side", id))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    if let Some(e) = &current {
        sidebar(&mut side_ui, tab, e, cx, out);
    }
}

/// The cursor item, as large as the stage: a fitted picture for images and videos, the
/// preview panel's view for the rest, a big folder icon for folders.
fn big_preview(
    ui: &mut egui::Ui,
    id: (usize, usize),
    e: &Entry,
    cx: &mut ViewCx,
    out: &mut Vec<Action>,
) {
    let stage = ui.max_rect();
    let open = |ui: &mut egui::Ui, out: &mut Vec<Action>| {
        let r = ui.interact(
            stage,
            egui::Id::new(("keel-gallery-stage", id)),
            Sense::click(),
        );
        crate::a11y::not_a_stop(&r);
        if r.double_clicked() {
            out.push(Action::Enter);
        }
    };
    let icon = |ui: &mut egui::Ui, lib: &crate::library::LibraryUi, side: f32| {
        let at = Rect::from_center_size(stage.center() - vec2(0.0, 12.0), vec2(side, side));
        crate::folder_style::paint_entry(ui, at, e, lib.style_of(&e.path), true);
        ui.painter().text(
            pos2(stage.center().x, at.bottom() + 10.0),
            egui::Align2::CENTER_TOP,
            &e.name,
            egui::FontId::proportional(15.0),
            ui.visuals().text_color(),
        );
    };
    let side = (stage.width().min(stage.height()) * 0.55).clamp(32.0, 256.0);
    if e.kind == Kind::Dir {
        open(ui, out);
        icon(ui, cx.library, side);
        return;
    }
    let muted = cx.theme.muted();
    match cx.preview.as_deref_mut() {
        Some(panel) if panel.key.as_ref().is_some_and(|k| k.path == e.path) => {
            if !panel.open {
                // Rendered for the stage (a PDF page sharp at this width).
                panel.width_px = (stage.width() * ui.ctx().pixels_per_point()).round() as u32;
            }
            let picture = matches!(
                panel.current,
                Some(Preview::Image(_) | Preview::Video { .. })
            );
            match (&panel.tex, picture) {
                (Some(tex), true) => {
                    open(ui, out);
                    let size = tex.size_vec2() / ui.ctx().pixels_per_point();
                    let scale = (stage.width() / size.x)
                        .min(stage.height() / size.y)
                        .min(1.0);
                    egui::Image::new((tex.id(), size))
                        .paint_at(ui, Rect::from_center_size(stage.center(), size * scale));
                    if let Some(Preview::Video { duration_s, .. }) = &panel.current {
                        let s = duration_s.max(0.0) as u64;
                        ui.painter().text(
                            stage.center_bottom(),
                            egui::Align2::CENTER_BOTTOM,
                            format!("▶ {}:{:02}", s / 60, s % 60),
                            egui::FontId::proportional(13.0),
                            ui.visuals().text_color(),
                        );
                    }
                }
                _ => panel.ui(ui, muted),
            }
        }
        // Not this pane's preview (the other pane has the keys): the thumbnail, or the icon.
        _ => match cx
            .thumbs
            .get_sized(e, crate::view_grid::thumb_bucket(side * 2.0))
        {
            Some((tex, size)) => {
                open(ui, out);
                let size = size / ui.ctx().pixels_per_point();
                let scale = (stage.width() / size.x).min(stage.height() / size.y);
                egui::Image::new((tex, size))
                    .paint_at(ui, Rect::from_center_size(stage.center(), size * scale));
            }
            None => {
                open(ui, out);
                icon(ui, cx.library, side);
            }
        },
    }
}

/// Every item as a small thumbnail in one row; the cursor item highlighted and kept in
/// view.
fn thumbnails(
    ui: &mut egui::Ui,
    id: (usize, usize),
    tab: &mut Tab,
    cx: &mut ViewCx,
    out: &mut Vec<Action>,
) {
    let n = tab.visible_cached().len();
    let width = ui.max_rect().width();
    let scroll_id = egui::Id::new(("keel-gallery-strip-scroll", id));
    let mut area = egui::ScrollArea::horizontal()
        .id_salt(("gallery-thumbs", id))
        .auto_shrink([false, false]);
    if let Some(pos) = tab.scroll_to.take() {
        let offset = ui.data(|d| d.get_temp::<f32>(scroll_id)).unwrap_or(0.0);
        area = area.horizontal_scroll_offset(strip_offset(pos, offset, width));
    }
    let selection = ui.visuals().selection.bg_fill;
    let hover = ui.visuals().widgets.hovered.weak_bg_fill;
    let cursor = tab.cursor.clone();
    let mut clicks: Vec<(egui::Response, Entry)> = Vec::new();
    let output = area.show_viewport(ui, |ui, viewport| {
        let origin = ui.max_rect().min;
        let step = THUMB + GAP;
        ui.set_min_size(vec2(n as f32 * step + GAP, STRIP_H));
        let first = (viewport.left() / step).floor().max(0.0) as usize;
        let last = ((viewport.right() / step).ceil() as usize + 1).min(n);
        for i in first..last {
            let e = tab.entries()[tab.visible_cached()[i]].clone();
            let rect = Rect::from_min_size(
                origin + vec2(GAP + i as f32 * step, (STRIP_H - THUMB) / 2.0),
                vec2(THUMB, THUMB),
            );
            let r = ui.interact(
                rect,
                egui::Id::new(("keel-gallery-thumb", id, &e.name)),
                crate::a11y::NO_TAB,
            );
            let at_cursor = cursor.as_deref() == Some(e.name.as_str());
            if at_cursor || tab.selected.contains(&e.name) {
                let fill = if at_cursor {
                    selection
                } else {
                    selection.gamma_multiply(0.5)
                };
                ui.painter().rect_filled(rect.expand(3.0), 6.0, fill);
            } else if r.hovered() {
                ui.painter().rect_filled(rect.expand(3.0), 6.0, hover);
            }
            let inner = rect.shrink(3.0);
            match cx.thumbs.get(&e) {
                Some((tex, size)) => {
                    let size = size / ui.ctx().pixels_per_point();
                    let scale = (inner.width() / size.x).min(inner.height() / size.y);
                    egui::Image::new((tex, size))
                        .paint_at(ui, Rect::from_center_size(inner.center(), size * scale));
                }
                None => {
                    let at = Rect::from_center_size(inner.center(), vec2(40.0, 40.0));
                    let style = cx.library.style_of(&e.path);
                    crate::folder_style::paint_entry(ui, at, &e, style, true);
                }
            }
            let r = r.on_hover_text(&e.name);
            r.context_menu(|ui| context_menu(ui, tab, Some(&e), out));
            clicks.push((r, e));
        }
    });
    ui.data_mut(|d| d.insert_temp(scroll_id, output.state.offset.x));
    for (r, e) in clicks {
        crate::pane::a11y_item(ui, &r, tab, &e, cx.keys);
        handle_click(&r, tab, &e, out);
        drag_and_drop(&r, id.0, tab, &e, out);
    }
}

/// Name, kind, size, dates and where; then the Quick Actions Keel has for the item.
fn sidebar(ui: &mut egui::Ui, tab: &Tab, e: &Entry, cx: &mut ViewCx, out: &mut Vec<Action>) {
    egui::ScrollArea::vertical()
        .id_salt("gallery-details")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(&e.name).strong().size(14.0)).wrap());
            ui.weak(kind_text(e));
            ui.add_space(6.0);
            let size = crate::sizes::size_text(cx.library, cx.sizes, e);
            let row = |ui: &mut egui::Ui, label: &str, value: &str| {
                if !value.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(label);
                        ui.label(value);
                    });
                }
            };
            row(ui, "Size", &size);
            row(ui, "Modified", &crate::view_details::date_text(e));
            let wherein = e.path.parent().map(|p| p.display()).unwrap_or_default();
            row(ui, "Where", &wherein);
            // What the preview found out (video and audio details, lines of text).
            if let Some(panel) = cx.preview.as_deref() {
                if panel.key.as_ref().is_some_and(|k| k.path == e.path) {
                    match &panel.current {
                        Some(Preview::Video { meta, .. } | Preview::Audio { meta, .. }) => {
                            row(ui, "Media", meta);
                        }
                        Some(Preview::Pdf { pages, .. }) => {
                            row(ui, "Pages", &pages.to_string());
                        }
                        Some(Preview::Text {
                            language, lines, ..
                        }) => {
                            row(ui, "Text", &format!("{language}, {} lines", lines.len()));
                        }
                        _ => {}
                    }
                }
            }
            ui.add_space(8.0);
            ui.separator();
            ui.strong("Quick Actions");
            ui.add_space(2.0);
            let width = ui.available_width();
            let mut action = |ui: &mut egui::Ui, text: &str, tip: &str, a: Action| {
                if ui
                    .add_sized([width, 22.0], egui::Button::new(text))
                    .on_hover_text(tip)
                    .clicked()
                {
                    out.push(a);
                }
            };
            action(ui, "Open", "Open (Enter)", Action::Enter);
            if e.kind != Kind::Dir {
                action(
                    ui,
                    "Open with…",
                    "Choose the app to open it with (Ctrl+Shift+O)",
                    Action::OpenWith,
                );
            }
            if crate::jobs::is_archive_file(e) {
                action(
                    ui,
                    "Extract here",
                    "Extract next to the archive",
                    Action::ExtractHere,
                );
            }
            // Zips are made in local folders only (not inside an archive or on a remote).
            if tab.dir.to_local_path().is_some() {
                action(
                    ui,
                    "Compress…",
                    "Compress the selection to a zip",
                    Action::CompressToZip,
                );
            }
            action(
                ui,
                "Show in system file manager",
                "Show it in the system's own file manager",
                Action::RevealInSystem,
            );
            action(ui, "Properties", "Properties", Action::Properties);
            ui.add_space(8.0);
            let mut always = cx.views.always_open(&tab.dir) == Some(ViewMode::Gallery);
            if ui
                .checkbox(&mut always, "Always open in Gallery view")
                .on_hover_text("This folder opens in the Gallery view")
                .changed()
            {
                cx.views
                    .set_always_open(&tab.dir, always.then_some(ViewMode::Gallery));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::test_entry;
    use std::sync::Arc;

    fn tab_of(names: &[(&str, Kind)]) -> Tab {
        let dir = keel_vfs::VPath::local(std::env::temp_dir());
        let mut t = Tab::new(dir.clone());
        t.set_entries(
            names
                .iter()
                .map(|(n, k)| test_entry(&dir, n, k.clone(), 1))
                .collect(),
        );
        t.loading = false;
        t.listed_dir = Some(dir);
        t
    }

    /// Draws the gallery twice and returns the texts painted and the tab.
    fn draw(tab: &mut Tab, views: &mut crate::view_prefs::ViewPrefs) -> Vec<String> {
        let ctx = egui::Context::default();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let router = Arc::new(keel_vfs::Router::new());
        let library = crate::library::LibraryUi::new(&router, tx.clone(), ctx.clone());
        let mut thumbs = crate::view_grid::Thumbs::new(tx, ctx.clone(), router.clone());
        let mut media = crate::media::Media::new(ctx.clone(), router);
        let theme = crate::theme::Theme::load("dark");
        let mut texts = Vec::new();
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
                    super::ui(p, (0, 0), tab, &mut cx, &mut Vec::new());
                });
            });
            texts = out
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => Some(t.galley.text().to_owned()),
                    _ => None,
                })
                .collect();
        }
        texts
    }

    #[test]
    fn the_gallery_selects_the_first_item_and_arrows_step_one_item() {
        let mut tab = tab_of(&[
            ("sub", Kind::Dir),
            ("a.png", Kind::File),
            ("b.png", Kind::File),
            ("c.png", Kind::File),
        ]);
        draw(&mut tab, &mut Default::default());
        assert_eq!(tab.cursor.as_deref(), Some("sub"), "first item shown");
        assert!(tab.selected.contains("sub"));
        assert_eq!(tab.row_step, 1);
        // Down and Right both go to the next item, Up and Left back.
        tab.move_cursor(Nav::Next, false);
        assert_eq!(tab.cursor.as_deref(), Some("a.png"));
        tab.move_cursor(Nav::Right, false);
        assert_eq!(tab.cursor.as_deref(), Some("b.png"));
        tab.move_cursor(Nav::Prev, false);
        assert_eq!(tab.cursor.as_deref(), Some("a.png"));
        tab.move_cursor(Nav::End, false);
        assert_eq!(tab.cursor.as_deref(), Some("c.png"));
        tab.move_cursor(Nav::Right, false);
        assert_eq!(tab.cursor.as_deref(), Some("c.png"), "stops at the end");
        // An existing cursor is kept.
        draw(&mut tab, &mut Default::default());
        assert_eq!(tab.cursor.as_deref(), Some("c.png"));
    }

    #[test]
    fn folders_show_big_and_files_get_their_quick_actions() {
        let mut tab = tab_of(&[("sub", Kind::Dir), ("pack.zip", Kind::File)]);
        let texts = draw(&mut tab, &mut Default::default());
        assert!(
            texts.iter().any(|t| t == "sub"),
            "the folder's name under its icon"
        );
        assert!(texts.iter().any(|t| t == "Folder"));
        assert!(!texts.iter().any(|t| t == "Open with…"), "not for folders");
        tab.move_cursor(Nav::Next, false);
        let texts = draw(&mut tab, &mut Default::default());
        for action in ["Quick Actions", "Open with…", "Extract here", "Compress…"] {
            assert!(texts.iter().any(|t| t == action), "{action} in {texts:?}");
        }
        assert!(texts.iter().any(|t| t == "ZIP file"));
    }

    #[test]
    fn always_open_in_gallery_is_remembered_for_the_folder() {
        let mut tab = tab_of(&[("a.png", Kind::File)]);
        let mut views = crate::view_prefs::ViewPrefs::default();
        views.set_always_open(&tab.dir, Some(ViewMode::Gallery));
        let texts = draw(&mut tab, &mut views);
        assert!(texts.iter().any(|t| t == "Always open in Gallery view"));
        assert_eq!(views.always_open(&tab.dir), Some(ViewMode::Gallery));
    }

    #[test]
    fn the_strip_keeps_the_cursor_thumbnail_in_view() {
        let step = THUMB + GAP;
        // In view: unchanged.
        assert_eq!(strip_offset(2, 0.0, 10.0 * step), 0.0);
        // Right of the view: scrolled so it is the last one shown.
        assert_eq!(strip_offset(12, 0.0, 10.0 * step), 3.0 * step);
        // Left of the view: scrolled to it.
        assert_eq!(strip_offset(1, 5.0 * step, 10.0 * step), step);
        assert_eq!(strip_page(10.0 * step), 10);
        assert_eq!(strip_page(1.0), 1, "never zero");
    }
}
