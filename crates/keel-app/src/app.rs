//! The eframe app: message drain, keyboard, sidebar, dual panes, preview panel,
//! popups (jump, palette), status bar, toasts.

use crate::keys::{self, Action};
use crate::pane::{self, DragPayload, ViewCx};
use crate::session::Session;
use crate::settings::{Persist, Settings};
use crate::state::AppState;
use crate::tab::Tab;
use egui::{pos2, Rect, Sense, UiBuilder};
use humansize::{format_size, DECIMAL};
use keel_vfs::{Router, VPath};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long an OS drop waits for a pointer position before landing in the active pane.
const DROP_WAIT: Duration = Duration::from_millis(1000);

/// What `main` loaded before the window opened.
pub struct Boot {
    pub settings: Settings,
    /// Repaired (`Session::repair`).
    pub session: Session,
    /// Startup problems for toasts (a broken config.toml or session.json).
    pub notices: Vec<String>,
    pub home: VPath,
    /// The session as found on disk; `Some` turns saving on (off in tests).
    pub saved: Option<Option<Session>>,
    // --- Task 24 ---
    /// FOLDER / --search from the command line.
    pub request: crate::cli::Request,
    /// This process is the single instance: requests from later `keel` runs arrive here.
    pub server: Option<crate::single_instance::Listener>,
    /// The search backend; None picks the OS default (`keel_search::default_searcher`),
    /// which may walk the home folder. Tests pass their own.
    pub searcher: Option<Arc<dyn keel_search::Searcher>>,
}

impl Boot {
    /// Defaults on `start`, nothing saved: for tests.
    #[cfg(test)]
    pub fn at(start: VPath) -> Self {
        Self {
            settings: Settings::default(),
            session: Session::single(start.clone()),
            notices: Vec::new(),
            home: start,
            saved: None,
            request: Default::default(),
            server: None,
            searcher: Some(Arc::new(keel_search::Unavailable::new(
                "no search in tests",
            ))),
        }
    }
}

pub struct App {
    pub state: AppState,
    /// Left pane share of the central area in dual mode.
    split: f32,
    /// Files dropped from another app, waiting for a pointer position (see `update`).
    pending_drop: Option<(Vec<PathBuf>, Instant)>,
    /// Settings + session writer; None in tests.
    persist: Option<Persist>,
    /// A frame panicked: the crash dialog is up.
    pub crashed: bool,
    /// False after a crash reset the panes: the saved session keeps the tabs from before.
    pub save_session: bool,
    /// The session (without its stash) and stash version handed to `persist` last.
    seen_session: Option<(Session, u64)>,
    /// Panel widths seen last frame; a change after the first frame is the user's resize.
    seen_widths: (Option<f32>, Option<f32>),
    // --- Task 24 ---
    drag_out: crate::dragout::DragOut,
    /// None in tests and where the OS has no global hotkeys.
    hotkey: Option<crate::hotkey::Hotkey>,
    #[cfg(test)]
    pub panic_next_frame: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, boot: Boot) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        cc.egui_ctx.set_fonts(crate::theme::fonts());
        // Another Keel is the single instance (and holds the global hotkey).
        let not_server = boot.settings.single_instance && boot.server.is_none();
        let persist = boot
            .saved
            .map(|saved| Persist::new(boot.settings.clone(), saved));
        let mut state = AppState::restore(
            cc.egui_ctx.clone(),
            Arc::new(Router::new()),
            boot.session,
            boot.settings,
            boot.home,
        );
        for notice in boot.notices {
            state.toasts.error(notice);
        }
        state.load_searcher(boot.searcher);
        // --- Task 24 ---
        state.external(boot.request);
        if let Some(listener) = boot.server {
            let (tx, ctx) = (state.tx.clone(), cc.egui_ctx.clone());
            let name = crate::single_instance::name(&crate::cli::profile());
            let server = crate::single_instance::serve(listener, &name, move |req| {
                // On the client's thread: `checked` may touch the disk.
                match req.checked() {
                    Ok(req) => {
                        crate::single_instance::bring_to_front(&ctx);
                        crate::worker::send(&tx, &ctx, crate::state::Msg::External(req));
                    }
                    Err(e) => tracing::warn!("single instance: {e}"),
                }
            });
            state.instance = Some(server);
        }
        // Not in tests (`persist` is None there): they would grab the real hotkey.
        let hotkey = persist
            .is_some()
            .then(|| crate::hotkey::Hotkey::new(&cc.egui_ctx, not_server))
            .flatten();
        // --- Task 29 ---: the library opens on a worker (not in tests: no persist).
        if persist.is_some() && state.settings.library.enabled {
            let name = state.settings.library.name.clone();
            let spawn = state.settings.library.daemon;
            let via = crate::library::Via::Auto { spawn };
            state.library.open(&name, state.router.clone(), via);
        }
        // --- end Task 24 ---
        Self {
            state,
            split: 0.5,
            pending_drop: None,
            persist,
            crashed: false,
            save_session: true,
            seen_session: None,
            seen_widths: (None, None),
            drag_out: Default::default(),
            hotkey,
            #[cfg(test)]
            panic_next_frame: false,
        }
    }

    /// The session to save, when it changed since it was last handed over (None: keep
    /// what was saved). The tabs are compared every frame; the stash (up to 50k paths)
    /// only by its version, and copied only when that moved.
    fn changed_session(&mut self) -> Option<Session> {
        if !self.save_session {
            return None;
        }
        let s = &self.state;
        let tabs = Session::of_tabs(s);
        let version = s.dropzone.version;
        if self
            .seen_session
            .as_ref()
            .is_some_and(|(seen, v)| *seen == tabs && *v == version)
        {
            return None;
        }
        self.seen_session = Some((tabs.clone(), version));
        Some(Session {
            stash: s.dropzone.items().to_vec(),
            ..tabs
        })
    }

    /// A frame panicked (the hook wrote crash.log): drop popups, reset both panes to one
    /// home tab and clear the preview, once. While the dialog is up a panic that repeats
    /// every frame changes nothing more. The reset tabs are never saved as the session.
    fn recover(&mut self) {
        if self.crashed {
            return;
        }
        self.crashed = true;
        self.save_session = false;
        self.pending_drop = None;
        let s = &mut self.state;
        s.dialog = None;
        s.jump.open = false;
        s.palette.open = false;
        s.preview.reset();
        for p in 0..2 {
            s.panes[p].tabs = vec![Tab::new(s.home.clone())];
            s.panes[p].active = 0;
            s.list(p, 0);
        }
    }

    fn frame(&mut self, ctx: &egui::Context) {
        #[cfg(test)]
        if std::mem::take(&mut self.panic_next_frame) {
            panic!("test panic inside a frame");
        }
        let s = &mut self.state;
        crate::anim::apply(ctx, s.settings.reduce_motion); // Task 23
        s.drain();
        s.tick();
        s.jobs.tick();
        // Every frame, so key state stays right while a modal is open.
        // A context menu acts on what it was opened on: no key may move the focus or
        // selection under it (Task 23: the columns view's keyboard column).
        let keys_on = s.dialog.is_none()
            && !s.jump.open
            && !s.palette.open
            && !self.crashed
            && s.viewer.is_none() // Task 32: the viewer reads its own keys
            && !ctx.is_context_menu_open();
        for action in keys::actions_with_terminal(ctx, keys_on, s.terminal.focused(ctx)) {
            s.run(s.active, action);
        }
        s.terminal.input(ctx, keys_on, &s.settings, &s.tx);

        let mut out: Vec<(usize, Action)> = Vec::new();
        let mut pane_rects: Vec<Rect> = Vec::new();
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| status_bar(ui, s, &mut out));
        // --- Task 23 ---
        if let Some(action) = crate::dropzone::panel(ctx, s) {
            out.push((s.active, action));
        }
        // Task 29: library jobs (index, hash, operations) share the panel.
        let library_jobs = !s.library.jobs.is_empty();
        let mut job_acts = Vec::new();
        egui::TopBottomPanel::bottom("jobs").show_animated(
            ctx,
            !s.jobs.list.is_empty() || library_jobs,
            |ui| {
                s.jobs.ui(ui);
                crate::library_ui::jobs(ui, &s.library, &mut job_acts);
            },
        );
        out.extend(job_acts.into_iter().map(|a| (s.active, a)));
        s.terminal.panel(ctx, &mut s.settings, &s.tx);
        let sidebar = egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(s.settings.sidebar_width)
            .width_range(140.0..=420.0)
            .show(ctx, |ui| {
                let mut acts = Vec::new();
                let current = s.panes[s.active].tab().dir.clone();
                let archives = s.open_archives();
                s.sidebar
                    .ui(ui, &s.theme, &current, &archives, &s.library, &mut acts);
                out.extend(acts.into_iter().map(|a| (s.active, a)));
            });
        let w = sidebar.response.rect.width().round();
        if self.seen_widths.0.is_some_and(|seen| seen != w) {
            s.settings.sidebar_width = w;
        }
        self.seen_widths.0 = Some(w);
        // --- Task 23 ---: the columns view previews inline while the panel is closed.
        let inline = !s.preview.open && crate::view_columns::wants_preview(&s.panes[s.active]);
        if s.preview.open || inline {
            let target = s.preview_target();
            if s.preview.follow(ctx, target.as_ref()) {
                s.toasts.error(crate::preview_panel::LOCKED);
            }
        }
        // Slides open and shut (Task 23); the width is the user's once fully open.
        let panel = egui::SidePanel::right("preview")
            .resizable(true)
            .default_width(s.settings.preview_width)
            .width_range(200.0..=1000.0)
            .show_animated(ctx, s.preview.open, |ui| {
                s.preview.width_px = (ui.available_width() * ctx.pixels_per_point()).round() as u32;
                s.preview.ui(ui, s.theme.muted());
            });
        if let Some(panel) = panel {
            let w = panel.response.rect.width().round();
            if self.seen_widths.1.is_some_and(|seen| seen != w) {
                s.settings.preview_width = w;
            }
            self.seen_widths.1 = Some(w);
        } else {
            self.seen_widths.1 = None;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(4.0))
            .show(ctx, |ui| {
                let full = ui.available_rect_before_wrap();
                // --- Task 23 ---: the split slides when dual pane is toggled; a drag
                // follows the pointer.
                let split_id = egui::Id::new("keel-split");
                let share =
                    crate::anim::value(ctx, split_id, if s.dual { self.split } else { 1.0 });
                let rects = if share < 0.999 {
                    let x = full.left() + full.width() * share;
                    let handle = Rect::from_x_y_ranges(x - 3.0..=x + 3.0, full.y_range());
                    let r = ui
                        .interact(handle, ui.id().with("split"), Sense::drag())
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                    if let Some(p) = r.dragged().then(|| r.interact_pointer_pos()).flatten() {
                        self.split = ((p.x - full.left()) / full.width()).clamp(0.15, 0.85);
                        crate::anim::snap(ctx, split_id, self.split);
                    }
                    ui.painter().vline(
                        x,
                        full.y_range(),
                        ui.visuals().widgets.noninteractive.bg_stroke,
                    );
                    vec![
                        Rect::from_min_max(full.min, pos2(x - 4.0, full.max.y)),
                        Rect::from_min_max(pos2(x + 4.0, full.min.y), full.max),
                    ]
                } else {
                    vec![full]
                };
                let (pressed, released, dropped, moved) = ctx.input(|i| {
                    let at = |yes: bool| yes.then(|| i.pointer.interact_pos()).flatten();
                    let moved = i.events.iter().rev().find_map(|e| match e {
                        egui::Event::PointerMoved(pos) => Some(*pos),
                        _ => None,
                    });
                    (
                        at(i.pointer.any_pressed()),
                        at(i.pointer.any_released()),
                        i.raw.dropped_files.clone(),
                        moved,
                    )
                });
                // Files dropped from another app land in the pane under the pointer. winit
                // sends no pointer moves during an OS drag, so the position known at drop
                // time is stale: wait for the first move after the drop (or DROP_WAIT,
                // then the active pane). OS drops always copy; Shift is not seen there.
                let dropped: Vec<_> = dropped.into_iter().filter_map(|f| f.path).collect();
                if !dropped.is_empty() {
                    self.pending_drop = Some((dropped, Instant::now()));
                }
                if let Some((_, at)) = &self.pending_drop {
                    let target = match moved {
                        Some(pos) => Some((
                            rects
                                .iter()
                                .position(|r| r.contains(pos))
                                .filter(|&p| p == 0 || s.dual)
                                .unwrap_or(s.active),
                            Some(pos),
                        )),
                        None if at.elapsed() >= DROP_WAIT => Some((s.active, None)),
                        None => {
                            ctx.request_repaint_after(Duration::from_millis(50));
                            None
                        }
                    };
                    if let (Some((p, pos)), Some((paths, _))) = (target, self.pending_drop.take()) {
                        // Task 23: the column under the pointer, not the keyboard column.
                        let dst = pos
                            .filter(|_| s.panes[p].view == pane::ViewMode::Columns)
                            .and_then(|pos| crate::view_columns::folder_at(ctx, p, pos))
                            .unwrap_or_else(|| s.tab(p).dir.clone());
                        out.push((
                            p,
                            Action::Drop {
                                paths: paths.into_iter().map(VPath::local).collect(),
                                from: None,
                                dst,
                            },
                        ));
                    }
                }
                pane_rects = rects.clone();
                for (p, rect) in rects.into_iter().enumerate() {
                    // Pane 1 still sliding shut after dual pane was turned off: shown only.
                    let live = p == 0 || s.dual;
                    if live && pressed.is_some_and(|pos| rect.contains(pos)) {
                        s.active = p;
                    }
                    let mut child =
                        ui.new_child(UiBuilder::new().max_rect(rect).id_salt(("pane", p)));
                    child.set_clip_rect(rect);
                    let mut acts = Vec::new();
                    let banner = s.remotes.banner(s.panes[p].tab());
                    let mut cx = ViewCx {
                        theme: &s.theme,
                        show_hidden: s.show_hidden,
                        thumbs: &mut s.thumbs,
                        active: s.dual && s.active == p,
                        banner,
                        preview: (inline && p == s.active).then_some(&mut s.preview),
                        column_widths: &mut s.settings.column_widths,
                        library: &s.library,
                        drives: &s.sidebar.drives,
                        searcher: s.searcher.as_ref().map(|x| x.name()),
                        tags_column: s.settings.library.tags_column,
                        media: &mut s.media,
                    };
                    pane::ui(&mut child, p, &mut s.panes[p], &mut cx, &mut acts);
                    if !live {
                        continue;
                    }
                    out.extend(acts.into_iter().map(|a| (p, a)));
                    // An in-app drag released over the pane but not on a folder row.
                    if released.is_some_and(|pos| rect.contains(pos)) {
                        if let Some(drag) = egui::DragAndDrop::take_payload::<DragPayload>(ctx) {
                            out.push((p, drag.action(s.tab(p).dir.clone())));
                        }
                    }
                }
            });
        // --- Task 24 ---
        self.drag_out.check(ctx, s);
        if let Some(e) = (self.hotkey.as_mut()).and_then(|h| h.sync(&s.settings.hotkey)) {
            s.toasts.error(e);
        }
        // --- end Task 24 ---
        if let Some(drag) = egui::DragAndDrop::payload::<DragPayload>(ctx) {
            let (shift, hover) = ctx.input(|i| (i.modifiers.shift, i.pointer.hover_pos()));
            let over = hover.and_then(|pos| pane_rects.iter().position(|r| r.contains(pos)));
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            egui::show_tooltip_at_pointer(
                ctx,
                egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("keel-drag")),
                egui::Id::new("keel-drag-tip"),
                |ui| ui.label(drag_verb(&drag, over, shift)),
            );
        }
        // Recent "Open with" apps for the context menu (read by `pane::context_menu`).
        let recent = s.settings.open_with.clone();
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("keel-open-with"), recent));
        s.viewer_ui(ctx); // Task 32
        if let Some(action) = crate::dialogs::show(ctx, &mut s.dialog) {
            out.push((s.active, action));
        }
        // --- Task 29 ---
        let mut acts = Vec::new();
        crate::library_ui::windows(ctx, s, &mut acts);
        crate::devices::windows(ctx, s, &mut acts); // Task 36
        out.extend(acts.into_iter().map(|a| (s.active, a)));
        if s.jump.open {
            if let Some(action) = s.jump.ui(ctx) {
                out.push((s.active, action));
            }
        }
        if s.palette.open {
            if let Some(action) = s.palette.ui(ctx) {
                out.push((s.active, action));
            }
        }
        for (p, action) in out {
            s.run(p, action);
        }
        s.settings_ui(ctx);
        s.remotes.modals(ctx, &mut s.settings, &s.tx);
        s.cloud_modals(ctx);
        let error = ctx.style().visuals.error_fg_color;
        if let Some(action) = s.toasts.show(ctx, error) {
            s.run(s.active, action);
        }
    }
}

impl eframe::App for App {
    // --- Task 24 ---
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.drag_out.input_hook(raw_input);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A panic inside a frame is logged by the panic hook; the app keeps running.
        let frame = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.frame(ctx)));
        if frame.is_err() {
            self.recover();
        }
        if self.crashed {
            crate::crash::modal(ctx, &mut self.crashed);
        }
        if self.persist.is_some() {
            let session = self.changed_session();
            let persist = self.persist.as_mut().expect("checked");
            persist.update(&self.state.settings, session);
            if let Some(e) = persist.error() {
                self.state.toasts.error(e);
            }
        }
    }

    fn on_exit(&mut self) {
        if let Some(persist) = &mut self.persist {
            let session = self.save_session.then(|| Session::of(&self.state));
            persist.finish(&self.state.settings, session);
        }
        // Task 36: the node first (it serves the library).
        self.state.devices.close_now();
        // Task 29: jobs checkpoint and resume next time.
        self.state.library.close_now();
    }
}

/// The drag tooltip: exactly what a drop over pane `over` would do, by the same rule as
/// `Action::Drop` (entries dragged out of an archive are extracted: a copy).
fn drag_verb(drag: &DragPayload, over: Option<usize>, shift: bool) -> &'static str {
    let extract = drag.dir.split_archive().is_some();
    let moves = over.is_some_and(|p| pane::drop_moves(Some(drag.pane), p, shift));
    if moves && !extract {
        "Move"
    } else {
        "Copy"
    }
}

/// Below this width the status bar's left group (counts, size, filter) is one line that
/// truncates, so it never runs under the right group.
const NARROW_STATUS: f32 = 800.0;

fn status_bar(ui: &mut egui::Ui, s: &mut AppState, out: &mut Vec<(usize, Action)>) {
    let narrow = ui.available_width() < NARROW_STATUS;
    let left_max = ui.available_width() * 0.4;
    ui.horizontal(|ui| {
        let show_hidden = s.show_hidden;
        let p = s.active;
        let tab = s.panes[p].tab_mut();
        let n = tab.visible(show_hidden).len();
        let selected = tab.targets();
        let picked = !tab.selected.is_empty();
        let bytes: u64 = if picked {
            selected.iter().map(|e| e.size).sum()
        } else {
            tab.visible_cached()
                .iter()
                .map(|&i| tab.entries()[i].size)
                .sum()
        };
        let mut left = vec![format!("{n} items")];
        if picked {
            left.push(format!("{} selected", tab.selected.len()));
        }
        left.push(format_size(bytes, DECIMAL));
        if !tab.filter.is_empty() {
            left.push(format!("filter: {}", tab.filter));
        }
        if narrow {
            let text = left.join("  ·  ");
            ui.allocate_ui_with_layout(
                egui::vec2(left_max, ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| ui.add(egui::Label::new(text).truncate()),
            );
        } else {
            for (i, part) in left.into_iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                ui.label(part);
            }
        }
        let dir = tab.dir.clone();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = format!("Theme: {}", s.theme.name);
            if ui
                .add(egui::Button::new(label).frame(false))
                .on_hover_text("Toggle dark / light")
                .clicked()
            {
                out.push((p, Action::ToggleTheme));
            }
            if let Some((name, _, free, total)) = s.sidebar.drive_of(&dir) {
                ui.separator();
                ui.label(format!(
                    "{name}  {} free of {}",
                    format_size(*free, DECIMAL),
                    format_size(*total, DECIMAL)
                ));
            }
            if cfg!(windows) {
                ui.separator();
                // --- Task 24: backend name + its status note ---
                if crate::index_ui::status_bar(ui, s) {
                    s.probe_search();
                }
            }
            if s.show_hidden {
                ui.separator();
                ui.weak("hidden shown");
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Key;
    use egui_kittest::Harness;

    fn fixture(name: &str) -> VPath {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        for (name, body) in [
            (
                "main.rs",
                "fn main() {}
",
            ),
            (
                "README.md",
                "# Keel
",
            ),
            (
                "Cargo.toml",
                "[package]
",
            ),
            (
                "notes.txt",
                "hello
",
            ),
            (
                "data.csv", "a,b
1,2
",
            ),
        ] {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let png = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../keel-preview/tests/fixtures/sample.png"
        );
        std::fs::copy(png, dir.join("sample.png")).unwrap();
        VPath::local(&dir)
    }

    fn wait_listed(harness: &mut Harness<App>) {
        for _ in 0..250 {
            harness.step();
            let s = &harness.state().state;
            if !s.tab(0).loading && !s.tab(1).loading {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        harness.run_steps(2);
    }

    #[test]
    fn keyboard_moves_cursor_filters_and_switches_pane() {
        let start = fixture("keel-keys-fixture");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .build_eframe(|cc| App::new(cc, Boot::at(start)));
        wait_listed(&mut harness);
        assert_eq!(harness.state().state.tab(0).entries().len(), 8);
        // Opening the window changes no setting (nothing to save).
        assert_eq!(harness.state().state.settings, Settings::default());

        harness.press_key(Key::ArrowDown);
        harness.press_key(Key::ArrowDown);
        harness.step();
        assert_eq!(harness.state().state.tab(0).cursor.as_deref(), Some("src"));

        // Typing opens the filter; further keys go to the focused filter box.
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("m".into()));
        harness.run_steps(2);
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("a".into()));
        harness.run_steps(2);
        let tab = harness.state_mut().state.tab_mut(0);
        assert!(tab.filter_open);
        assert_eq!(tab.filter, "ma");
        let visible: Vec<String> = tab
            .visible(false)
            .to_vec()
            .iter()
            .map(|&i| tab.entries()[i].name.clone())
            .collect();
        assert_eq!(visible, ["main.rs"]);

        // Esc in the filter box clears it; F6 then moves to the other pane.
        harness.press_key(Key::Escape);
        harness.run_steps(2);
        assert!(!harness.state().state.tab(0).filter_open);
        harness.press_key(Key::F6);
        harness.step();
        assert_eq!(harness.state().state.active, 1);
    }

    #[test]
    fn drag_tooltip_says_move_or_copy_by_the_drop_rule() {
        let dir = VPath::parse("mem://t/").unwrap();
        let drag = DragPayload {
            pane: 0,
            dir: dir.clone(),
            paths: vec![dir.join("a")],
        };
        assert_eq!(drag_verb(&drag, Some(0), false), "Move");
        assert_eq!(drag_verb(&drag, Some(1), false), "Copy");
        assert_eq!(drag_verb(&drag, Some(1), true), "Move");
        assert_eq!(drag_verb(&drag, None, false), "Copy");
        let zip = VPath::join_archive(&VPath::local(std::env::temp_dir().join("a.zip")), "");
        let out_of_zip = DragPayload { dir: zip, ..drag };
        assert_eq!(drag_verb(&out_of_zip, Some(0), true), "Copy", "extracts");
    }

    /// Polish backlog: in a narrow window the status bar's left group is one truncated
    /// line inside 40 % of the width.
    #[test]
    fn narrow_status_bar_truncates_its_left_group() {
        let start = fixture("keel-status-fixture");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(640.0, 400.0))
            .build_eframe(|cc| App::new(cc, Boot::at(start)));
        wait_listed(&mut harness);
        harness.state_mut().state.tab_mut(0).filter = "a-very-long-filter-text-".repeat(8);
        harness.run_steps(2);
        let node =
            egui_kittest::kittest::Queryable::get_by_label_contains(&harness, "filter: a-very");
        let width = node.raw_bounds().map(|b| b.x1 - b.x0).unwrap_or_default();
        assert!(width > 0.0 && width <= 640.0 * 0.4 + 1.0, "{width}");
    }

    #[test]
    fn panicking_frame_is_logged_and_survived() {
        let start = fixture("keel-crash-fixture");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .build_eframe(|cc| App::new(cc, Boot::at(start.clone())));
        wait_listed(&mut harness);
        harness
            .state_mut()
            .state
            .run(0, Action::NewTabAt(start.join("src")));
        assert_eq!(harness.state().state.panes[0].tabs.len(), 2);

        harness.state_mut().panic_next_frame = true;
        harness.step();
        let app = harness.state();
        assert!(app.crashed, "crash dialog up");
        assert!(!app.save_session, "the reset tabs are not saved");
        for p in 0..2 {
            assert_eq!(app.state.panes[p].tabs.len(), 1, "pane reset");
            assert_eq!(app.state.tab(p).dir, start, "home tab");
        }
        // A panic that repeats while the dialog is up closes nothing more.
        harness
            .state_mut()
            .state
            .run(0, Action::NewTabAt(start.join("docs")));
        harness.state_mut().panic_next_frame = true;
        harness.step();
        assert_eq!(harness.state().state.panes[0].tabs.len(), 2);
        // Later frames run normally; OK dismisses the dialog.
        harness.run_steps(3);
        egui_kittest::kittest::Queryable::get_by_label(&harness, "OK").click();
        harness.run_steps(2);
        assert!(!harness.state().crashed);

        // Ctrl+, opens Settings once the dialog is gone.
        harness.press_key_modifiers(egui::Modifiers::COMMAND, Key::Comma);
        harness.run_steps(2);
        assert!(harness.state().state.settings_open);
    }

    /// Manual end-to-end check on a real zip (GPU): `KEEL_DEMO_ZIP=C:\\x\\demo.zip
    /// KEEL_SHOT=out.png cargo test -p keel-app -- --ignored archive_live`. Opens the zip
    /// with the keyboard, previews its first text file, renders the window to `KEEL_SHOT`,
    /// then runs Extract here and checks the files landed next to the zip.
    #[test]
    #[ignore]
    fn archive_live() {
        let Some(zip) = std::env::var_os("KEEL_DEMO_ZIP").map(PathBuf::from) else {
            return;
        };
        let folder = VPath::local(zip.parent().unwrap());
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 720.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, Boot::at(folder)));
        wait_listed(&mut harness);
        let name = zip.file_name().unwrap().to_string_lossy().into_owned();
        harness.state_mut().state.tab_mut(0).cursor = Some(name.clone());
        harness.press_key(Key::Enter);
        harness.run_steps(2);
        wait_listed(&mut harness);
        let tab = harness.state().state.tab(0);
        assert_eq!(tab.title(), name);
        let text = tab
            .entries()
            .iter()
            .find(|e| e.ext == "txt")
            .expect("a .txt in the demo zip")
            .name
            .clone();
        harness.state_mut().state.tab_mut(0).cursor = Some(text);
        harness.press_key(Key::F3);
        for _ in 0..250 {
            harness.step();
            if matches!(
                harness.state().state.preview.current,
                Some(keel_preview::Preview::Text { .. })
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        harness.run_steps(4);
        assert!(matches!(
            harness.state().state.preview.current,
            Some(keel_preview::Preview::Text { .. })
        ));
        if let Some(shot) = std::env::var_os("KEEL_SHOT") {
            harness.render().unwrap().save(shot).unwrap();
        }
        // Up selects the zip in its folder; Extract here lands its files beside it.
        harness.press_key_modifiers(egui::Modifiers::ALT, Key::ArrowUp);
        wait_listed(&mut harness);
        assert_eq!(
            harness.state().state.tab(0).cursor.as_deref(),
            Some(name.as_str())
        );
        harness.state_mut().state.run(0, Action::ExtractHere);
        for _ in 0..250 {
            harness.step();
            let jobs = &harness.state().state.jobs.list;
            if jobs.first().is_some_and(|j| j.done.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let job = &harness.state().state.jobs.list[0];
        assert_eq!(job.title, format!("Extracting {name}"));
        assert!(job.done.as_ref().unwrap().is_ok(), "{:?}", job.done);
    }

    /// GPU-dependent: run locally with `cargo test -p keel-app -- --ignored`
    /// (`UPDATE_SNAPSHOTS=1` to rewrite `tests/snapshots/main.png`).
    #[test]
    #[ignore]
    fn main_window_snapshot() {
        let start = fixture("keel-snapshot-fixture");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, Boot::at(start)));
        harness.state_mut().state.panes[1].view = crate::pane::ViewMode::Grid;
        wait_listed(&mut harness);
        harness.run_steps(4);
        harness.snapshot("main");
    }

    /// Startup to the first frame (release, GPU): the window state built (`App::new`, wgpu
    /// set up), the first frame run and rendered, both panes listed.
    /// `cargo test -p keel-app --release perf_first_frame -- --ignored --nocapture`
    #[test]
    #[ignore = "release measurement; needs a GPU"]
    fn perf_first_frame() {
        let start = fixture("keel-perf-first-frame");
        let t = std::time::Instant::now();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, Boot::at(start)));
        let built = t.elapsed();
        harness.step();
        harness.render().unwrap();
        let first = t.elapsed();
        wait_listed(&mut harness);
        let listed = t.elapsed();
        eprintln!(
            "PERF first_frame: app built {built:?}, first frame rendered {first:?}, both panes listed {listed:?}"
        );
        assert!(first < std::time::Duration::from_secs(3), "{first:?}");
    }

    /// Opening a folder of 100,000 files (kept under `target/perf-list-100k`) in both panes:
    /// listing, sorting and the first frame that shows it, then a frame with it shown.
    /// `cargo test -p keel-app --release perf_open_100k -- --ignored --nocapture`
    #[test]
    #[ignore = "release measurement: 100,000 files under target/"]
    fn perf_open_100k_folder() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/perf-list-100k");
        let done = dir.with_extension("complete");
        if !done.exists() {
            std::fs::create_dir_all(&dir).unwrap();
            for n in 0..100_000 {
                std::fs::write(dir.join(format!("File {n} report-{}.txt", n % 977)), b"").unwrap();
            }
            std::fs::write(&done, b"").unwrap();
        }
        let t = std::time::Instant::now();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .build_eframe(|cc| App::new(cc, Boot::at(VPath::local(&dir))));
        for _ in 0..1_000 {
            harness.step();
            let s = &harness.state().state;
            if !s.tab(0).loading && !s.tab(1).loading {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        harness.step();
        let shown = t.elapsed();
        assert_eq!(harness.state().state.tab(0).entries().len(), 100_000);
        let frames = (0..20)
            .map(|_| {
                let t = std::time::Instant::now();
                harness.step();
                t.elapsed()
            })
            .max()
            .unwrap();
        eprintln!(
            "PERF open_100k: both panes listed and shown {shown:?}, slowest frame after {frames:?}"
        );
        assert!(shown < std::time::Duration::from_secs(5), "{shown:?}");
        assert!(frames < std::time::Duration::from_millis(50), "{frames:?}");
    }
}
