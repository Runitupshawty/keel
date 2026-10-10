//! Full-window media viewer (Task 32): Space or Enter on a photo or video in the media view.
//! Shows the 1024 px sidecar at once and the full-resolution decode (on a worker, EXIF
//! orientation applied, at most `MAX_PIXELS`) when it arrives. Left/Right step through the
//! listing's media files, `+`/`-`/wheel zoom, drag pans, `0` fits, `1` is 100 %, `I` shows
//! the metadata, `F` toggles Favorite (library), Esc (or Space on a photo) closes. Videos
//! play in the viewer with sound (`video_player`, ffmpeg): Space plays and pauses, a click
//! on the strip or the progress bar seeks, Up/Down set the volume, `M` mutes, `L` loops.
//! Without ffmpeg the strip is a still-frame scrubber and Play opens the system player;
//! Enter always does.

use crate::keys::Action;
use crate::media::{self, MediaType, Req, Tex, TexKey, VIEWER_SLOT};
use crate::pane::ViewMode;
use crate::state::AppState;
use crate::video_player::{clock_text, Cmd, Transport, VideoPlayer};
use crossbeam_channel::{Receiver, Sender};
use egui::{pos2, vec2, Color32, Key, Rect, Sense, TextureHandle, Vec2};
use keel_core::{MediaMeta, SidecarKind};

const STRIP_H: f32 = 72.0;
const INFO_W: f32 = 320.0;
const BAR_H: f32 = 36.0;
const CTRL_H: f32 = 30.0;
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.0;

struct FullReq {
    index: usize,
    req: Req,
    max_side: u32,
    video: bool,
}

enum FullMsg {
    Meta(usize, Option<MediaMeta>),
    Image(usize, Result<egui::ColorImage, String>),
}

pub struct Viewer {
    pub pane: usize,
    /// The tab (of `pane`) and listing the viewer walks.
    tab: usize,
    gen: u64,
    /// Indices into the tab's entries: its visible media files in view order.
    items: Vec<usize>,
    pub index: usize,
    /// The shown file's name (kept across relists).
    name: String,
    /// Scale (1.0 = one image pixel per screen pixel); None = fit.
    zoom: Option<f32>,
    pan: Vec2,
    pub info: bool,
    /// Video: the strip frame shown instead of the poster frame.
    seek: Option<u32>,
    full: Option<TextureHandle>,
    full_err: Option<String>,
    meta: Option<MediaMeta>,
    /// The index the sidecar requests were made for.
    wanted: Option<usize>,
    jobs: Sender<FullReq>,
    rx: Receiver<FullMsg>,
    /// In-viewer playback of the shown video (after Play or a seek).
    pub player: Option<VideoPlayer>,
    /// Volume, mute and loop, kept from video to video.
    prefs: Transport,
    /// Why the video plays elsewhere or not at all.
    pub note: Option<String>,
    /// ffmpeg was found when the viewer opened.
    ffmpeg: bool,
    /// The image area in physical pixels (last frame): the decode size.
    area_px: [f32; 2],
}

/// `index` moved by `delta`, kept inside `0..len` (no wrap).
pub fn step(index: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    index.saturating_add_signed(delta).min(len - 1)
}

/// Zooms `z` by `factor` about `at`, keeping the image point under it in place. Returns
/// the new zoom and pan (`center` = the image area's centre, `pan` its offset).
pub fn zoom_about(
    z: f32,
    factor: f32,
    pan: Vec2,
    center: egui::Pos2,
    at: egui::Pos2,
) -> (f32, Vec2) {
    let nz = (z * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    let img_center = center + pan;
    let u = (at - img_center) / z;
    let new_center = at - u * nz;
    (nz, new_center - center)
}

/// Decodes a photo at full resolution (EXIF orientation applied), scaled to fit
/// `max_side` (twice the screen, within the GPU texture limit). Blocking: viewer worker
/// only.
pub fn decode_full(path: &std::path::Path, max_side: u32) -> anyhow::Result<egui::ColorImage> {
    use image::{DynamicImage, ImageDecoder, ImageReader};
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let (w, h) = decoder.dimensions();
    anyhow::ensure!(
        u64::from(w) * u64::from(h) <= keel_core::MAX_PIXELS,
        "{w} x {h} is over the {} MP limit",
        keel_core::MAX_PIXELS / 1_000_000
    );
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(keel_core::MAX_PIXELS * 8);
    decoder.set_limits(limits)?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    if img.width().max(img.height()) > max_side {
        img = img.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    }
    Ok(media::color_image(img, 0))
}

impl Viewer {
    fn new(state: &AppState, pane: usize, name: &str) -> Option<Viewer> {
        let p = &state.panes[pane];
        let tab = &p.tabs[p.active];
        let items = media_items(tab);
        let index = items.iter().position(|&i| tab.entries()[i].name == name)?;
        let (jobs, rx_jobs) = crossbeam_channel::unbounded::<FullReq>();
        let (tx, rx) = crossbeam_channel::unbounded();
        let (sh, ctx) = (state.media.sh.clone(), state.ctx.clone());
        crate::worker::spawn("keel-viewer", move || {
            while let Ok(first) = rx_jobs.recv() {
                // Only the newest request matters (arrow keys held down).
                let job = rx_jobs.try_iter().last().unwrap_or(first);
                let meta = sh.meta(&job.req, true);
                if tx.send(FullMsg::Meta(job.index, meta)).is_err() {
                    return;
                }
                ctx.request_repaint();
                if job.video || !rx_jobs.is_empty() {
                    continue;
                }
                let local = job.req.real.to_local_path().map(Ok).unwrap_or_else(|| {
                    sh.router
                        .provider_for(&job.req.real)
                        .ok_or_else(|| anyhow::anyhow!("no provider"))
                        .and_then(|p| p.local_copy(&job.req.real))
                });
                let image = local
                    .and_then(|l| decode_full(&l, job.max_side))
                    .map_err(|e| format!("{e:#}"));
                if tx.send(FullMsg::Image(job.index, image)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        Some(Viewer {
            pane,
            tab: p.active,
            gen: tab.generation(),
            items,
            index,
            name: name.to_owned(),
            zoom: None,
            pan: Vec2::ZERO,
            info: false,
            seek: None,
            full: None,
            full_err: None,
            meta: None,
            wanted: None,
            jobs,
            rx,
            player: None,
            prefs: Transport::default(),
            note: None,
            ffmpeg: keel_core::ffmpeg_available(),
            area_px: [1280.0, 720.0],
        })
    }

    /// The full-resolution image arrived (tests).
    #[cfg(test)]
    pub fn has_full(&self) -> bool {
        self.full.is_some()
    }

    fn go(&mut self, tab: &crate::tab::Tab, delta: isize) {
        let next = step(self.index, self.items.len(), delta);
        if next != self.index {
            self.index = next;
            self.name = tab.entries()[self.items[next]].name.clone();
            self.zoom = None;
            self.pan = Vec2::ZERO;
            self.seek = None;
            self.full = None;
            self.full_err = None;
            self.meta = None;
            self.stop_video();
        }
    }

    /// Kills playback (its processes end and are waited for), keeping volume and loop.
    fn stop_video(&mut self) {
        if let Some(p) = self.player.take() {
            self.prefs = p.transport.clone();
        }
        self.note = None;
    }

    /// The video's length in seconds (the player's probe, else the metadata).
    fn duration(&self) -> f64 {
        (self.player.as_ref().and_then(VideoPlayer::duration))
            .or_else(|| Some(self.meta.as_ref()?.duration_ms? as f64 / 1000.0))
            .unwrap_or(0.0)
    }
}

/// The time strip frame `f` shows in a video of `duration` seconds (its slot's middle).
pub fn strip_time(f: u32, duration: f64) -> f64 {
    (f64::from(f) + 0.5) / f64::from(media::STRIP_FRAMES) * duration
}

/// The visible media files of `tab`, as indices into its entries.
fn media_items(tab: &crate::tab::Tab) -> Vec<usize> {
    (tab.visible_cached().iter().copied())
        .filter(|&i| media::media_type(&tab.entries()[i]).is_some())
        .collect()
}

/// Opens the viewer on `name` of pane `p`'s tab.
pub fn open(state: &mut AppState, p: usize, name: &str) {
    if let Some(v) = Viewer::new(state, p, name) {
        state.viewer = Some(v);
    }
}

/// Space and Enter on a photo or video in the media view open the viewer.
pub fn intercept(state: &mut AppState, p: usize, action: Action) -> Option<Action> {
    let pane = &state.panes[p];
    if pane.view != ViewMode::Media || pane.tab().is_search() {
        return Some(action);
    }
    let tab = pane.tab();
    let name = match &action {
        Action::ToggleSelect => tab.cursor.clone(),
        Action::Enter => tab.targets().first().map(|e| e.name.clone()),
        _ => return Some(action),
    };
    let media = name.as_ref().is_some_and(|n| {
        tab.entries()
            .iter()
            .any(|e| &e.name == n && media::media_type(e).is_some())
    });
    match name {
        Some(name) if media => {
            open(state, p, &name);
            None
        }
        _ => Some(action),
    }
}

impl AppState {
    /// The viewer overlay (when open): input, worker answers, drawing.
    pub fn viewer_ui(&mut self, ctx: &egui::Context) {
        let Some(mut v) = self.viewer.take() else {
            return;
        };
        let keep = self.viewer_frame(ctx, &mut v);
        if keep {
            self.viewer = Some(v);
        } else {
            self.close_viewer(v);
        }
    }

    fn close_viewer(&mut self, v: Viewer) {
        self.media.want(VIEWER_SLOT, Vec::new());
        // The grid's cursor follows to the file shown last.
        let pane = &mut self.panes[v.pane];
        if pane.active == v.tab {
            let tab = &mut pane.tabs[v.tab];
            if tab.generation() == v.gen {
                if let Some(&i) = v.items.get(v.index) {
                    let name = tab.entries()[i].name.clone();
                    tab.selected.clear();
                    tab.selected.insert(name.clone());
                    tab.anchor = Some(name.clone());
                    tab.cursor = Some(name);
                    tab.scroll_to = tab.cursor_pos();
                }
            }
        }
    }

    /// One frame of the viewer; false closes it.
    fn viewer_frame(&mut self, ctx: &egui::Context, v: &mut Viewer) -> bool {
        let pane = &self.panes[v.pane];
        if pane.active != v.tab || pane.view != ViewMode::Media {
            return false;
        }
        let tab = &pane.tabs[v.tab];
        if tab.generation() != v.gen {
            // Relisted (a watcher saw a change): follow the shown file by name.
            v.items = media_items(tab);
            v.gen = tab.generation();
            match v
                .items
                .iter()
                .position(|&i| tab.entries()[i].name == v.name)
            {
                Some(i) => v.index = i,
                None => return false,
            }
            v.wanted = None;
        }
        let Some(&entry_i) = v.items.get(v.index) else {
            return false;
        };
        let entry = tab.entries()[entry_i].clone();
        let video = media::media_type(&entry) == Some(MediaType::Video);
        let sources = self.library.sources.clone();
        let req = crate::view_media::req_of(&entry, &sources);

        // Sidecars: this file's 1024 px thumbnail (and strip), then its neighbours'.
        let thumb = TexKey::of(&entry, SidecarKind::Thumb1024, 0);
        let strip = TexKey::of(&entry, SidecarKind::Strip, 0);
        if v.wanted != Some(v.index) {
            v.wanted = Some(v.index);
            let mut list = vec![(thumb.clone(), 0, req.clone())];
            if video {
                list.push((strip.clone(), 0, req.clone()));
            }
            for (prio, d) in [(1, 1), (2, -1)] {
                let j = step(v.index, v.items.len(), d);
                if j != v.index {
                    let e = &tab.entries()[v.items[j]];
                    list.push((
                        TexKey::of(e, SidecarKind::Thumb1024, 0),
                        prio,
                        crate::view_media::req_of(e, &sources),
                    ));
                }
            }
            self.media.want(VIEWER_SLOT, list);
            // Twice the screen keeps a 2x zoom sharp; a 100 MP photo at full size would stall
            // the UI thread's upload for nothing.
            let screen = ctx.screen_rect().size() * ctx.pixels_per_point();
            let twice = (2.0 * screen.x.max(screen.y)).max(1024.0) as u32;
            let max_side = (ctx.input(|i| i.max_texture_side) as u32).min(twice);
            let _ = v.jobs.send(FullReq {
                index: v.index,
                req: req.clone(),
                max_side,
                video,
            });
        }
        while let Ok(msg) = v.rx.try_recv() {
            match msg {
                FullMsg::Meta(i, m) if i == v.index => v.meta = m,
                FullMsg::Image(i, Ok(img)) if i == v.index => {
                    v.full =
                        Some(ctx.load_texture("keel-viewer", img, egui::TextureOptions::LINEAR));
                }
                FullMsg::Image(i, Err(e)) if i == v.index => v.full_err = Some(e),
                _ => {}
            }
        }

        // Keys (the file list's key map is off while the viewer is open).
        let (mut close, mut fav, mut play) = (false, false, false);
        let mut zoom_by = None;
        let mut cmd: Option<Cmd> = None;
        ctx.input_mut(|i| {
            let mut k = |key| i.consume_key(egui::Modifiers::NONE, key);
            close = k(Key::Escape);
            if video {
                for (key, c) in [
                    (Key::Space, Cmd::Toggle),
                    (Key::ArrowUp, Cmd::Volume(0.1)),
                    (Key::ArrowDown, Cmd::Volume(-0.1)),
                    (Key::M, Cmd::Mute),
                    (Key::L, Cmd::Loop),
                ] {
                    if k(key) {
                        cmd = Some(c);
                    }
                }
            } else {
                close |= k(Key::Space);
            }
            if k(Key::ArrowRight) {
                v.go(tab, 1);
            }
            if k(Key::ArrowLeft) {
                v.go(tab, -1);
            }
            if k(Key::Home) {
                v.go(tab, isize::MIN / 2);
            }
            if k(Key::End) {
                v.go(tab, isize::MAX / 2);
            }
            if k(Key::Plus) || k(Key::Equals) {
                zoom_by = Some(1.25);
            }
            if k(Key::Minus) {
                zoom_by = Some(0.8);
            }
            if k(Key::Num0) {
                v.zoom = None;
                v.pan = Vec2::ZERO;
            }
            if k(Key::Num1) {
                v.zoom = Some(1.0);
                v.pan = Vec2::ZERO;
            }
            if k(Key::I) {
                v.info = !v.info;
            }
            fav = k(Key::F);
            play = k(Key::Enter);
        });
        if close {
            return false;
        }
        if v.wanted != Some(v.index) {
            // Navigated: next frame requests the new file.
            ctx.request_repaint();
            return true;
        }

        let fav_on = crate::library::real_of(&sources, &entry.path)
            .and_then(|r| self.library.tagged.get(&r))
            .is_some_and(|t| t.contains(&keel_core::FAVORITES));
        let library_on = self.library.is_open();
        let screen = ctx.screen_rect();
        let ppp = ctx.pixels_per_point();
        let thumb_tex = self.media.get(&thumb);
        let strip_tex = if video {
            self.media.get(&strip)
        } else {
            Tex::Missing
        };
        let (n, index) = (v.items.len(), v.index);
        let mut actions: Vec<Action> = Vec::new();
        // Playback: worker news and the due frame; a failure falls back to still frames.
        let area_px = v.area_px;
        let video_tex = v.player.as_mut().and_then(|p| p.frame(area_px));
        if let Some(e) = v.player.as_ref().and_then(|p| p.error.clone()) {
            v.stop_video();
            v.note = Some(format!(
                "Cannot play this video here ({e}). Enter opens the system player."
            ));
        }
        let dur = v.duration();
        let pos = v.player.as_mut().map(VideoPlayer::position);

        egui::Area::new(egui::Id::new("keel-media-viewer"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.set_min_size(screen.size());
                let bg = Color32::from_gray(12);
                ui.painter().rect_filled(screen, 0.0, bg);
                let fg = Color32::from_gray(230);
                let dim = Color32::from_gray(150);

                // Top bar.
                let bar = Rect::from_min_size(screen.min, vec2(screen.width(), BAR_H));
                let mut bar_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(bar.shrink2(vec2(10.0, 4.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                bar_ui.visuals_mut().override_text_color = Some(fg);
                bar_ui.label(egui::RichText::new(&entry.name).strong());
                bar_ui.label(egui::RichText::new(format!("{} / {n}", index + 1)).color(dim));
                bar_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("×").on_hover_text("Close (Esc)").clicked() {
                        close = true;
                    }
                    if ui
                        .selectable_label(v.info, "Info")
                        .on_hover_text("Metadata (I)")
                        .clicked()
                    {
                        v.info = !v.info;
                    }
                    if library_on
                        && ui
                            .selectable_label(fav_on, if fav_on { "★" } else { "☆" })
                            .on_hover_text("Favorite (F)")
                            .clicked()
                    {
                        fav = true;
                    }
                    if video
                        && ui
                            .button("Open")
                            .on_hover_text("Play in the system player (Enter)")
                            .clicked()
                    {
                        play = true;
                    }
                    if ui.button("1:1").on_hover_text("100 % (1)").clicked() {
                        v.zoom = Some(1.0);
                        v.pan = Vec2::ZERO;
                    }
                    if ui.button("Fit").on_hover_text("Fit (0)").clicked() {
                        v.zoom = None;
                        v.pan = Vec2::ZERO;
                    }
                    if ui.button("⏵").on_hover_text("Next (Right)").clicked() {
                        v.go(tab, 1);
                    }
                    if ui.button("⏴").on_hover_text("Previous (Left)").clicked() {
                        v.go(tab, -1);
                    }
                });

                // Areas: image, optional info panel, optional strip.
                let mut body = Rect::from_min_max(pos2(screen.left(), bar.bottom()), screen.max);
                if v.info {
                    let info =
                        Rect::from_min_max(pos2(body.right() - INFO_W, body.top()), body.max);
                    body.max.x = info.left();
                    info_panel(ui, info, &entry, &req, v, &self.library);
                }
                if video {
                    let strip_rect =
                        Rect::from_min_max(pos2(body.left(), body.bottom() - STRIP_H), body.max);
                    let ctrl = Rect::from_min_max(
                        pos2(body.left(), strip_rect.top() - CTRL_H),
                        pos2(body.right(), strip_rect.top()),
                    );
                    body.max.y = ctrl.top();
                    if let Some(c) = controls(ui, ctrl, v, pos, dur) {
                        cmd = Some(c);
                    }
                    // The strip marks the frame nearest the playing position.
                    let marked = match pos {
                        Some(t) if dur > 0.0 => Some(media::strip_frame((t / dur) as f32)),
                        _ => v.seek,
                    };
                    if let Tex::Ready(tex, size) = strip_tex {
                        let r = ui.interact(strip_rect, ui.id().with("strip"), Sense::click());
                        let w = strip_rect.width() / media::STRIP_FRAMES as f32;
                        // Each frame keeps its aspect, centred in its slot.
                        let aspect = size.x / media::STRIP_FRAMES as f32 / size.y.max(1.0);
                        let h = (STRIP_H - 8.0).min((w - 2.0) / aspect.max(0.01));
                        for f in 0..media::STRIP_FRAMES {
                            let cell = Rect::from_center_size(
                                pos2(
                                    strip_rect.left() + (f as f32 + 0.5) * w,
                                    strip_rect.center().y,
                                ),
                                vec2(w - 2.0, h),
                            );
                            ui.painter()
                                .image(tex, cell, media::strip_uv(f), Color32::WHITE);
                            if marked == Some(f) {
                                ui.painter().rect_stroke(
                                    cell,
                                    0.0,
                                    (2.0, Color32::WHITE),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                        if let Some(p) = r.interact_pointer_pos().filter(|_| r.clicked()) {
                            let f =
                                media::strip_frame((p.x - strip_rect.left()) / strip_rect.width());
                            if v.ffmpeg && dur > 0.0 {
                                cmd = Some(Cmd::Seek(strip_time(f, dur)));
                            } else {
                                v.seek = Some(f);
                            }
                        }
                        r.on_hover_text(if v.ffmpeg {
                            "Click a frame to go there"
                        } else {
                            "Click a frame to show it"
                        });
                    }
                }

                // The image: full resolution when decoded, else the 1024 px sidecar; a
                // video shows the chosen strip frame.
                v.area_px = [body.width() * ppp, body.height() * ppp];
                let (tex, size, uv) = match (v.seek, strip_tex, &v.full, thumb_tex) {
                    _ if video_tex.is_some() => {
                        let (t, s) = video_tex.unwrap_or_default();
                        (Some(t), s, media::FULL_UV)
                    }
                    (Some(f), Tex::Ready(t, s), _, _) => {
                        let uv = media::strip_uv(f);
                        (Some(t), vec2(s.x * uv.width(), s.y), uv)
                    }
                    (_, _, Some(full), _) => (Some(full.id()), full.size_vec2(), media::FULL_UV),
                    (_, _, _, Tex::Ready(t, s)) => (Some(t), s, media::FULL_UV),
                    _ => (None, Vec2::ZERO, media::FULL_UV),
                };
                // Natural size: the file's own pixels when known (the thumbnail stretches).
                let natural = match (&v.meta, v.seek) {
                    (Some(m), None) if m.width > 0 && m.height > 0 => {
                        vec2(m.width as f32, m.height as f32)
                    }
                    _ => size,
                } / ppp;
                let resp = ui.interact(body, ui.id().with("image"), Sense::click_and_drag());
                if let Some(t) = tex.filter(|_| natural.x > 0.0) {
                    let area = body.shrink(8.0);
                    let fit = media::fit_scale(natural, area.size());
                    let mut z = v.zoom.unwrap_or(fit);
                    if let Some(f) = zoom_by {
                        let (nz, pan) =
                            zoom_about(z, f, v.pan, area.center(), area.center() + v.pan);
                        z = nz;
                        v.zoom = Some(z);
                        v.pan = pan;
                    }
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if resp.hovered() && scroll != 0.0 {
                        let at = resp.hover_pos().unwrap_or(area.center());
                        let (nz, pan) =
                            zoom_about(z, (scroll / 300.0).exp(), v.pan, area.center(), at);
                        z = nz;
                        v.zoom = Some(z);
                        v.pan = pan;
                    }
                    if resp.dragged() {
                        v.pan += resp.drag_delta();
                    }
                    let rect = Rect::from_center_size(area.center() + v.pan, natural * z);
                    ui.painter()
                        .with_clip_rect(body)
                        .image(t, rect, uv, Color32::WHITE);
                    ui.painter().text(
                        body.right_bottom() + vec2(-10.0, -8.0),
                        egui::Align2::RIGHT_BOTTOM,
                        format!("{:.0} %", z * 100.0),
                        egui::FontId::proportional(12.0),
                        dim,
                    );
                } else {
                    let note = match thumb_tex {
                        Tex::Failed if video || v.full_err.is_some() => match &v.full_err {
                            Some(e) => format!("Cannot show this file: {e}"),
                            None => "No preview frame (is ffmpeg installed?)".into(),
                        },
                        _ => "Loading…".into(),
                    };
                    ui.painter().text(
                        body.center(),
                        egui::Align2::CENTER_CENTER,
                        note,
                        egui::FontId::proportional(16.0),
                        dim,
                    );
                }
                let download = v.player.as_ref().and_then(VideoPlayer::download_text);
                if let Some(note) = v.note.as_ref().or(download.as_ref()) {
                    ui.painter().text(
                        body.left_bottom() + vec2(10.0, -8.0),
                        egui::Align2::LEFT_BOTTOM,
                        note,
                        egui::FontId::proportional(13.0),
                        fg,
                    );
                }
                if let (Some(e), None) = (&v.full_err, &v.full) {
                    if !video && tex.is_some() {
                        ui.painter().text(
                            body.left_bottom() + vec2(10.0, -8.0),
                            egui::Align2::LEFT_BOTTOM,
                            format!("Full resolution unavailable: {e}"),
                            egui::FontId::proportional(12.0),
                            dim,
                        );
                    }
                }
                if resp.double_clicked() {
                    v.zoom = match v.zoom {
                        None => Some(1.0),
                        Some(_) => None,
                    };
                    v.pan = Vec2::ZERO;
                }
            });

        if fav && library_on {
            if let Some(real) = crate::library::real_of(&sources, &entry.path) {
                actions.push(Action::Library(crate::library::LibCmd::TagPaths {
                    paths: vec![real],
                    tag: keel_core::FAVORITES,
                    on: !fav_on,
                }));
            }
        }
        if play && video {
            if let Some(p) = v.player.as_mut().filter(|p| p.transport.playing) {
                p.command(Cmd::Toggle);
            }
            self.launch(entry.path.clone(), crate::platform::open);
        }
        if let Some(c) = cmd.filter(|_| video) {
            self.video_cmd(ctx, v, c, &entry.path, &req);
        }
        for a in actions {
            self.run(v.pane, a);
        }
        !close
    }
}

impl AppState {
    /// A transport command for the shown video: to its player, else Play or a seek starts
    /// one (Play without ffmpeg opens the system player instead).
    fn video_cmd(
        &mut self,
        ctx: &egui::Context,
        v: &mut Viewer,
        c: Cmd,
        path: &keel_vfs::VPath,
        req: &Req,
    ) {
        if let Some(p) = &mut v.player {
            p.command(c);
            return;
        }
        let at = match c {
            Cmd::Toggle => v.seek.map_or(0.0, |f| strip_time(f, v.duration())),
            Cmd::Seek(t) => t,
            _ => {
                v.prefs.apply(c, 0.0);
                return;
            }
        };
        if !v.ffmpeg {
            if c == Cmd::Toggle {
                v.note = Some("ffmpeg is not installed: playing in the system player.".into());
                self.launch(path.clone(), crate::platform::open);
            }
            return;
        }
        let (real, router) = (req.real.clone(), self.media.sh.router.clone());
        let resolve: crate::video_player::Resolve = std::sync::Arc::new(move |cancel, progress| {
            if let Some(local) = real.to_local_path() {
                return Ok(local);
            }
            let provider = router
                .provider_for(&real)
                .ok_or_else(|| anyhow::anyhow!("no provider"))?;
            progress(0, 0);
            let report = |p: keel_vfs::Progress| progress(p.done_bytes, p.total_bytes);
            provider.local_copy_cancellable(&real, &report, cancel)
        });
        let mut p = VideoPlayer::new(ctx.clone(), resolve, &v.prefs, at, v.area_px);
        if c == Cmd::Toggle {
            p.command(Cmd::Toggle);
        }
        v.seek = None;
        v.note = None;
        v.player = Some(p);
    }
}

/// The playback row: play/pause, loop, volume, time and a progress bar to click.
fn controls(ui: &mut egui::Ui, rect: Rect, v: &Viewer, pos: Option<f64>, dur: f64) -> Option<Cmd> {
    let t = v.player.as_ref().map_or(&v.prefs, |p| &p.transport);
    let mut cmd = None;
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(10.0, 3.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    row.visuals_mut().override_text_color = Some(Color32::from_gray(230));
    let (label, tip) = match (t.playing, v.ffmpeg) {
        (true, _) => ("Pause", "Pause (Space)"),
        (false, true) => ("▶ Play", "Play (Space)"),
        (false, false) => (
            "▶ Play",
            "Play in the system player (Space): ffmpeg is not installed",
        ),
    };
    if row.button(label).on_hover_text(tip).clicked() {
        cmd = Some(Cmd::Toggle);
    }
    if row
        .selectable_label(t.looping, "Loop")
        .on_hover_text("Loop (L)")
        .clicked()
    {
        cmd = Some(Cmd::Loop);
    }
    let vol = if t.muted {
        "Muted".to_owned()
    } else {
        format!("Volume {:.0} %", t.volume * 100.0)
    };
    if row
        .button(vol)
        .on_hover_text("Up / Down change the volume; click or M mutes")
        .clicked()
    {
        cmd = Some(Cmd::Mute);
    }
    let at = pos.unwrap_or(0.0);
    row.label(format!("{} / {}", clock_text(at), clock_text(dur)));
    let w = row.available_width().max(40.0);
    let (bar, resp) = row.allocate_exact_size(vec2(w, 16.0), Sense::click());
    let line = Rect::from_center_size(bar.center(), vec2(bar.width(), 4.0));
    row.painter().rect_filled(line, 2.0, Color32::from_gray(70));
    if dur > 0.0 {
        let frac = (at / dur).clamp(0.0, 1.0) as f32;
        let done = Rect::from_min_max(
            line.min,
            pos2(line.left() + frac * line.width(), line.bottom()),
        );
        row.painter()
            .rect_filled(done, 2.0, Color32::from_gray(220));
        if v.ffmpeg {
            if let Some(p) = resp.interact_pointer_pos().filter(|_| resp.clicked()) {
                let f = ((p.x - line.left()) / line.width()).clamp(0.0, 1.0);
                cmd = Some(Cmd::Seek(f64::from(f) * dur));
            }
        }
    }
    cmd
}

fn info_panel(
    ui: &mut egui::Ui,
    rect: Rect,
    entry: &keel_vfs::Entry,
    req: &Req,
    v: &Viewer,
    library: &crate::library::LibraryUi,
) {
    ui.painter().rect_filled(rect, 0.0, Color32::from_gray(24));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink(12.0))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let ui = &mut child;
    ui.visuals_mut().override_text_color = Some(Color32::from_gray(220));
    ui.heading("Info");
    let mut rows: Vec<(&str, String)> = vec![
        ("Name", entry.name.clone()),
        ("Path", req.real.display()),
        (
            "Size",
            humansize::format_size(entry.size, humansize::DECIMAL),
        ),
    ];
    match &v.meta {
        None => rows.push(("Metadata", "reading…".into())),
        Some(m) => rows.extend(meta_rows(m)),
    }
    if library.is_open() {
        let names: Vec<String> = library
            .tagged
            .get(&req.real)
            .into_iter()
            .flatten()
            .filter_map(|id| library.tags.iter().find(|t| t.id == *id))
            .map(|t| t.name.clone())
            .collect();
        rows.push((
            "Tags",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            },
        ));
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("keel-viewer-info")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for (k, val) in rows {
                    ui.weak(k);
                    ui.add(egui::Label::new(val).wrap());
                    ui.end_row();
                }
            });
    });
}

/// The metadata lines shown in the info panel.
pub fn meta_rows(m: &MediaMeta) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if m.width > 0 {
        rows.push(("Dimensions", format!("{} x {}", m.width, m.height)));
    }
    if let Some(t) = m.taken_at {
        let when = chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| t.to_string());
        rows.push(("Taken", when));
    }
    if let Some(c) = &m.camera {
        rows.push(("Camera", c.clone()));
    }
    if let Some(l) = &m.lens {
        rows.push(("Lens", l.clone()));
    }
    if let Some((lat, lon)) = m.gps {
        rows.push(("GPS", format!("{lat:.6}, {lon:.6}")));
    }
    if let Some(d) = m.duration_ms {
        let s = d / 1000;
        rows.push((
            "Duration",
            format!("{}:{:02}.{}", s / 60, s % 60, (d % 1000) / 100),
        ));
    }
    if let Some(c) = &m.codec {
        rows.push(("Codec", c.clone()));
    }
    if let Some(r) = m.rating {
        rows.push(("Rating", format!("{r} / 5")));
    }
    if !m.keywords.is_empty() {
        rows.push(("Keywords", m.keywords.join(", ")));
    }
    if let Some(e) = &m.error {
        rows.push(("Error", e.clone()));
    }
    rows
}
