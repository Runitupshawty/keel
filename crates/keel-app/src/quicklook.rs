//! Quick Look (Phase 14 Task 67): Space opens a floating preview over the file list. It
//! shows what the docked preview panel (F3) shows, through the same renderers and cache
//! (`preview_panel`: text and code, Markdown, PDF, Office documents, tables, archives,
//! fonts, audio with playback), photos sharp at the window's size (`media_viewer`'s
//! full decode) and videos playing with sound (`video_player`, after Play). The arrow keys
//! move the list's cursor and the preview follows; Space or Esc closes. With several
//! items selected it opens on an index sheet (a grid of them): a click shows one, the
//! arrow keys then step through the selected items, "Index Sheet" goes back. The header
//! has Open With… and a full-screen button. Settings → General → "Space opens Quick
//! Look" off gives Space its old job (select the cursor row); Ctrl+Space always does that.

use crate::keys::Action;
use crate::media::{self, MediaType};
use crate::state::AppState;
use crate::video_player::{clock_text, Cmd, Transport, VideoPlayer};
use crossbeam_channel::Receiver;
use egui::text::{LayoutJob, TextWrapping};
use egui::{vec2, Align, Align2, Id, Layout, Rect, RichText, Sense, TextureHandle, Vec2};
use keel_vfs::{Entry, VPath};
use std::time::{Duration, Instant};

/// Index sheet tile size (points).
const TILE: Vec2 = vec2(136.0, 156.0);
/// A photo is decoded sharp once it has been shown this long (arrow keys held down over a
/// folder of photos start no decodes).
const SHARP_AFTER: Duration = Duration::from_millis(120);

/// A photo decoded at the window's size.
struct Sharp {
    path: VPath,
    since: Instant,
    rx: Option<Receiver<Result<egui::ColorImage, String>>>,
    started: bool,
    tex: Option<TextureHandle>,
}

#[derive(Default)]
pub struct QuickLook {
    pub open: bool,
    /// When the window opened (Task 68: the fade-in's start).
    opened_at: Option<f64>,
    /// Fills the window (and makes the window full screen).
    pub full: bool,
    /// Several items selected: the one shown alone (None: the index sheet).
    pub item: Option<usize>,
    /// Whether the window was full screen before Quick Look made it so.
    restore_fullscreen: Option<bool>,
    sharp: Option<Sharp>,
    /// The playing video and its file.
    player: Option<(VPath, VideoPlayer)>,
}

impl QuickLook {
    pub fn show(&mut self) {
        self.open = true;
        self.item = None;
    }

    pub fn close(&mut self, ctx: &egui::Context) {
        self.set_full(ctx, false);
        self.open = false;
        self.opened_at = None;
        self.item = None;
        self.sharp = None;
        self.player = None;
    }

    fn set_full(&mut self, ctx: &egui::Context, on: bool) {
        if on == self.full {
            return;
        }
        self.full = on;
        if on {
            let was = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            self.restore_fullscreen = Some(was);
            if !was {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
            }
        } else if self.restore_fullscreen.take() == Some(false) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }

    /// The sharp texture of photo `e` (local files), once decoded.
    fn sharp(&mut self, ctx: &egui::Context, e: &Entry) -> Option<TextureHandle> {
        if media::media_type(e) != Some(MediaType::Image) {
            self.sharp = None;
            return None;
        }
        let local = e.path.to_local_path()?;
        let now = Instant::now();
        if self.sharp.as_ref().map(|s| &s.path) != Some(&e.path) {
            self.sharp = Some(Sharp {
                path: e.path.clone(),
                since: now,
                rx: None,
                started: false,
                tex: None,
            });
        }
        let sh = self.sharp.as_mut()?;
        if !sh.started {
            let wait = SHARP_AFTER.saturating_sub(now.saturating_duration_since(sh.since));
            if !wait.is_zero() {
                ctx.request_repaint_after(wait);
                return None;
            }
            sh.started = true;
            let side = ctx.screen_rect().size().max_elem() * ctx.pixels_per_point();
            let side = (side as u32).clamp(512, 8192);
            let (tx, rx) = crossbeam_channel::bounded(1);
            let ctx2 = ctx.clone();
            crate::worker::spawn("keel-quicklook", move || {
                let img =
                    crate::media_viewer::decode_full(&local, side).map_err(|e| format!("{e:#}"));
                let _ = tx.send(img);
                ctx2.request_repaint();
            });
            sh.rx = Some(rx);
        }
        if let Some(Ok(img)) = sh.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            sh.tex = Some(ctx.load_texture("keel-quicklook", img, egui::TextureOptions::LINEAR));
            sh.rx = None;
        }
        sh.tex.clone()
    }
}

/// The item shown alone moved by `delta` through `len` selected items, wrapping around;
/// from the index sheet (None) forward shows the first, back the last.
pub fn step(item: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match item {
        None if delta >= 0 => 0,
        None => len - 1,
        Some(i) => (i as isize + delta).rem_euclid(len as isize) as usize,
    })
}

/// The selected items when several are (the index sheet's), else empty.
pub fn selection(s: &mut AppState) -> Vec<Entry> {
    let p = s.active;
    let show_hidden = s.show_hidden;
    s.tab_mut(p).visible(show_hidden);
    let tab = s.tab(p);
    if tab.selected.len() > 1 {
        tab.targets().into_iter().cloned().collect()
    } else {
        Vec::new()
    }
}

/// What the preview shows: while Quick Look shows one of several selected items, that
/// item; else the cursor entry (`AppState::preview_target`).
pub fn target(s: &mut AppState) -> Option<Entry> {
    if s.finder.quicklook.open {
        if let Some(i) = s.finder.quicklook.item {
            if let Some(e) = selection(s).get(i) {
                return Some(e.clone());
            }
        }
    }
    s.preview_target()
}

/// A header button; the click gives the keyboard straight back to the file list (Space
/// and the arrow keys stay Quick Look's).
fn button(ui: &mut egui::Ui, text: &str, tip: &str) -> bool {
    let r = ui.add(egui::Button::new(text)).on_hover_text(tip);
    if r.clicked() {
        r.surrender_focus();
        return true;
    }
    false
}

/// Draws texture `tex` (`size` in physical pixels) fitted and centered in `rect`, never
/// larger than its pixels.
fn fit(ui: &egui::Ui, rect: Rect, tex: egui::TextureId, size: Vec2) {
    let size = size / ui.ctx().pixels_per_point();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }
    let scale = (rect.width() / size.x).min(rect.height() / size.y).min(1.0);
    egui::Image::new((tex, size)).paint_at(ui, Rect::from_center_size(rect.center(), size * scale));
}

/// The Quick Look window (when open). Header buttons push their actions to `out`.
pub fn ui(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<(usize, Action)>) {
    if !s.finder.quicklook.open {
        return;
    }
    if s.finder.quicklook.opened_at.is_none() {
        s.finder.quicklook.opened_at = Some(ctx.input(|i| i.time));
    }
    let p = s.active;
    let sel = selection(s);
    {
        let ql = &mut s.finder.quicklook;
        if sel.is_empty() {
            ql.item = None;
        } else if let Some(i) = ql.item {
            ql.item = Some(i.min(sel.len() - 1));
        }
    }
    let shown = target(s).filter(|_| sel.is_empty() || s.finder.quicklook.item.is_some());
    let muted = s.theme.muted();
    let router = s.router.clone();
    let ql = &mut s.finder.quicklook;
    let preview = &mut s.preview;
    let thumbs = &mut s.thumbs;
    let library = &s.library;
    let screen = ctx.screen_rect();
    let full = ql.full;
    let mut window = egui::Window::new("Quick Look")
        .id(Id::new(if full { "keel-ql-full" } else { "keel-ql" }))
        .title_bar(false)
        .collapsible(false)
        // Task 68: the window fades in over `anim::QUICK_LOOK` (Finder; Reduce motion:
        // instant), not egui's shorter default.
        .fade_in(false);
    window = if full {
        window.fixed_rect(screen).resizable(false)
    } else {
        window
            .default_size(screen.size() * vec2(0.6, 0.7))
            .pivot(Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .resizable(true)
            .constrain(true)
    };
    let video = shown
        .as_ref()
        .is_some_and(|e| media::media_type(e) == Some(MediaType::Video));
    if ql
        .player
        .as_ref()
        .is_some_and(|(path, _)| Some(path) != shown.as_ref().map(|e| &e.path))
    {
        ql.player = None;
    }
    let (mut close, mut full_toggle, mut play) = (false, false, false);
    let resp = window.show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Close", "Close (Space or Esc)") {
                    close = true;
                }
                let (icon, tip) = if full {
                    ("Exit Full Screen", "Back to a floating window")
                } else {
                    ("Full Screen", "Fill the screen")
                };
                if button(ui, icon, tip) {
                    full_toggle = true;
                }
                if shown.is_some() && button(ui, "Open With…", "Open with another app") {
                    out.push((p, Action::OpenWith));
                }
                if video {
                    let playing = ql.player.as_ref().is_some_and(|(_, v)| v.transport.playing);
                    let label = if playing { "Pause" } else { "▶ Play" };
                    if button(ui, label, "Play or pause the video") {
                        play = true;
                    }
                }
                if !sel.is_empty() {
                    if ql.item.is_some() && button(ui, "Index Sheet", "All selected items") {
                        ql.item = None;
                    }
                    if button(ui, "⏵", "Next selected item (Right)") {
                        ql.item = step(ql.item, sel.len(), 1);
                    }
                    if button(ui, "⏴", "Previous selected item (Left)") {
                        ql.item = step(ql.item, sel.len(), -1);
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let title = match (&shown, ql.item) {
                        (Some(e), Some(i)) => format!("{}  ({} of {})", e.name, i + 1, sel.len()),
                        (Some(e), None) => e.name.clone(),
                        (None, _) if !sel.is_empty() => format!("{} items", sel.len()),
                        (None, _) => "Nothing selected".to_owned(),
                    };
                    ui.add(egui::Label::new(RichText::new(title).strong()).truncate());
                });
            });
        });
        ui.separator();
        let body = ui.available_rect_before_wrap();
        ui.allocate_ui(body.size(), |ui| {
            ui.set_min_size(body.size());
            let Some(e) = &shown else {
                if sel.is_empty() {
                    ui.centered_and_justified(|ui| ui.weak("Nothing selected"));
                } else if let Some(i) = sheet(ui, &sel, thumbs, library) {
                    ql.item = Some(i);
                }
                return;
            };
            let rect = ui.available_rect_before_wrap();
            if let Some(tex) = ql.sharp(ui.ctx(), e) {
                fit(ui, rect, tex.id(), tex.size_vec2());
                return;
            }
            if let Some((_, player)) = ql.player.as_mut().filter(|_| video) {
                let px = (rect.size() * ui.ctx().pixels_per_point()).into();
                let frame = player.frame(px);
                let dur = player.duration().unwrap_or(0.0);
                let pos = player.position();
                let picture = Rect::from_min_max(rect.min, rect.max - vec2(0.0, 24.0));
                match frame {
                    Some((tex, size)) => fit(ui, picture, tex, size),
                    None => {
                        ui.painter().text(
                            picture.center(),
                            Align2::CENTER_CENTER,
                            player.download_text().unwrap_or_else(|| "Starting…".into()),
                            egui::TextStyle::Body.resolve(ui.style()),
                            muted,
                        );
                    }
                }
                if ui
                    .interact(picture, ui.id().with("ql-video"), Sense::click())
                    .clicked()
                {
                    play = true;
                }
                let line = match &player.error {
                    Some(e) => format!("Playback failed: {e}"),
                    None => format!("{} / {}", clock_text(pos), clock_text(dur)),
                };
                ui.painter().text(
                    egui::pos2(rect.center().x, rect.bottom() - 12.0),
                    Align2::CENTER_CENTER,
                    line,
                    egui::TextStyle::Body.resolve(ui.style()),
                    muted,
                );
                return;
            }
            preview.width_px = (rect.width() * ui.ctx().pixels_per_point()).round() as u32;
            preview.ui(ui, muted);
        });
    });
    // Task 68: the window fades in (Finder; Reduce motion: instant).
    let dur = if s.settings.style == crate::design::Style::Finder {
        crate::anim::dur(ctx, crate::anim::QUICK_LOOK)
    } else {
        0.0
    };
    let since = ql.opened_at.unwrap_or_else(|| ctx.input(|i| i.time));
    let t = crate::anim::at(
        0.0,
        1.0,
        (ctx.input(|i| i.time) - since).max(0.0) as f32,
        dur,
    );
    if t < 1.0 {
        ctx.request_repaint();
        if let Some(inner) = &resp {
            // A veil in the window's own fill that fades out: the window fades in.
            let a = ((1.0 - t) * 255.0).round() as u8;
            let f = ctx.style().visuals.window_fill;
            ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("keel-quicklook-fade"),
            ))
            .rect_filled(
                inner.response.rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(f.r(), f.g(), f.b(), a),
            );
        }
    }
    if play {
        if let Some(e) = shown.as_ref().filter(|_| video) {
            match &mut ql.player {
                Some((_, player)) => player.command(Cmd::Toggle),
                None => {
                    let resolve = crate::video_player::resolve(Some(router), e.path.clone());
                    let px = (screen.size() * ctx.pixels_per_point()).into();
                    let mut player =
                        VideoPlayer::new(ctx.clone(), resolve, &Transport::default(), 0.0, px);
                    player.command(Cmd::Toggle);
                    ql.player = Some((e.path.clone(), player));
                }
            }
        }
    }
    if full_toggle {
        ql.set_full(ctx, !full);
    }
    if close {
        ql.close(ctx);
    }
}

/// The index sheet: a tile per selected item. Some: the item clicked.
fn sheet(
    ui: &mut egui::Ui,
    sel: &[Entry],
    thumbs: &mut crate::view_grid::Thumbs,
    library: &crate::library::LibraryUi,
) -> Option<usize> {
    let mut picked = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (i, e) in sel.iter().enumerate() {
                    let (rect, r) = ui.allocate_exact_size(TILE, Sense::click());
                    let r = crate::a11y::Named::named(r, &e.name);
                    let visuals = ui.visuals();
                    if r.hovered() {
                        let fill = visuals.widgets.hovered.weak_bg_fill;
                        ui.painter().rect_filled(rect.shrink(2.0), 6.0, fill);
                    }
                    let side = TILE.x - 16.0;
                    let img_box = Rect::from_min_size(rect.min + vec2(8.0, 6.0), vec2(side, side));
                    match thumbs.get(e) {
                        Some((tex, size)) => fit(ui, img_box, tex, size),
                        None => {
                            let at = Rect::from_center_size(img_box.center(), vec2(64.0, 64.0));
                            let style = library.style_of(&e.path);
                            crate::folder_style::paint_entry(ui, at, e, style, true);
                        }
                    }
                    let font = egui::TextStyle::Small.resolve(ui.style());
                    let color = ui.visuals().text_color();
                    let mut job = LayoutJob::simple_singleline(e.name.clone(), font, color);
                    job.wrap = TextWrapping {
                        max_width: TILE.x - 8.0,
                        max_rows: 1,
                        break_anywhere: true,
                        overflow_character: Some('…'),
                    };
                    let galley = ui.painter().layout_job(job);
                    let at = egui::pos2(
                        rect.center().x - galley.size().x / 2.0,
                        img_box.bottom() + 4.0,
                    );
                    ui.painter().galley(at, galley, color);
                    if r.clicked() {
                        picked = Some(i);
                    }
                }
            });
        });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_through_selected_items_wraps() {
        assert_eq!(step(None, 3, 1), Some(0));
        assert_eq!(step(None, 3, -1), Some(2));
        assert_eq!(step(Some(2), 3, 1), Some(0));
        assert_eq!(step(Some(0), 3, -1), Some(2));
        assert_eq!(step(Some(1), 3, 1), Some(2));
        assert_eq!(step(Some(0), 0, 1), None);
    }
}
