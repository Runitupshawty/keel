//! The eframe app: message drain, keyboard, sidebar, dual panes, preview panel,
//! popups (jump, palette), status bar, toasts.

use crate::keys::{self, Action};
use crate::pane::{self, DragPayload, ViewCx};
use crate::state::AppState;
use egui::{pos2, Rect, Sense, UiBuilder};
use humansize::{format_size, DECIMAL};
use keel_vfs::{Router, VPath};
use std::sync::Arc;

pub struct App {
    pub state: AppState,
    /// Left pane share of the central area in dual mode.
    split: f32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, start: VPath) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let state = AppState::new(cc.egui_ctx.clone(), Arc::new(Router::new()), start);
        state.load_searcher();
        Self { state, split: 0.5 }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let s = &mut self.state;
        s.drain();
        s.tick();
        s.jobs.tick();
        if s.dialog.is_none() && !s.jump.open && !s.palette.open {
            for action in keys::actions(ctx) {
                s.run(s.active, action);
            }
        }

        let mut out: Vec<(usize, Action)> = Vec::new();
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| status_bar(ui, s, &mut out));
        egui::TopBottomPanel::bottom("jobs")
            .show_animated(ctx, !s.jobs.list.is_empty(), |ui| s.jobs.ui(ui));
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(210.0)
            .width_range(140.0..=420.0)
            .show(ctx, |ui| {
                let mut acts = Vec::new();
                let current = s.panes[s.active].tab().dir.clone();
                s.sidebar.ui(ui, &s.theme, &current, &mut acts);
                out.extend(acts.into_iter().map(|a| (s.active, a)));
            });
        if s.preview.open {
            let target = s.preview_target();
            s.preview.follow(ctx, target.as_ref());
            egui::SidePanel::right("preview")
                .resizable(true)
                .default_width(380.0)
                .width_range(200.0..=1000.0)
                .show(ctx, |ui| {
                    s.preview.width_px =
                        (ui.available_width() * ctx.pixels_per_point()).round() as u32;
                    s.preview.ui(ui, s.theme.muted());
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(4.0))
            .show(ctx, |ui| {
                let full = ui.available_rect_before_wrap();
                let rects = if s.dual {
                    let x = full.left() + full.width() * self.split;
                    let handle = Rect::from_x_y_ranges(x - 3.0..=x + 3.0, full.y_range());
                    let r = ui
                        .interact(handle, ui.id().with("split"), Sense::drag())
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                    if let Some(p) = r.dragged().then(|| r.interact_pointer_pos()).flatten() {
                        self.split = ((p.x - full.left()) / full.width()).clamp(0.15, 0.85);
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
                let (pressed, released, dropped, hover) = ctx.input(|i| {
                    let at = |yes: bool| yes.then(|| i.pointer.interact_pos()).flatten();
                    (
                        at(i.pointer.any_pressed()),
                        at(i.pointer.any_released()),
                        i.raw.dropped_files.clone(),
                        i.pointer.hover_pos(),
                    )
                });
                // Files dropped from another app land in the pane under the pointer.
                let dropped: Vec<_> = dropped.into_iter().filter_map(|f| f.path).collect();
                if !dropped.is_empty() {
                    let p = hover
                        .and_then(|pos| rects.iter().position(|r| r.contains(pos)))
                        .unwrap_or(s.active);
                    let dst = s.tab(p).dir.clone();
                    out.push((
                        p,
                        Action::Drop {
                            paths: dropped,
                            from: None,
                            dst,
                        },
                    ));
                }
                for (p, rect) in rects.into_iter().enumerate() {
                    if pressed.is_some_and(|pos| rect.contains(pos)) {
                        s.active = p;
                    }
                    let mut child =
                        ui.new_child(UiBuilder::new().max_rect(rect).id_salt(("pane", p)));
                    child.set_clip_rect(rect);
                    let mut acts = Vec::new();
                    let mut cx = ViewCx {
                        theme: &s.theme,
                        show_hidden: s.show_hidden,
                        thumbs: &mut s.thumbs,
                        active: s.dual && s.active == p,
                    };
                    pane::ui(&mut child, p, &mut s.panes[p], &mut cx, &mut acts);
                    out.extend(acts.into_iter().map(|a| (p, a)));
                    // An in-app drag released over the pane but not on a folder row.
                    if released.is_some_and(|pos| rect.contains(pos)) {
                        if let Some(drag) = egui::DragAndDrop::take_payload::<DragPayload>(ctx) {
                            out.push((
                                p,
                                Action::Drop {
                                    paths: drag.local_paths(),
                                    from: Some((drag.pane, drag.dir.clone())),
                                    dst: s.tab(p).dir.clone(),
                                },
                            ));
                        }
                    }
                }
            });
        if let Some(drag) = egui::DragAndDrop::payload::<DragPayload>(ctx) {
            let verb = if ctx.input(|i| i.modifiers.shift) {
                "Move"
            } else {
                "Copy / move"
            };
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            egui::show_tooltip_at_pointer(
                ctx,
                egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("keel-drag")),
                egui::Id::new("keel-drag-tip"),
                |ui| ui.label(format!("{verb} {}", crate::jobs::items(drag.paths.len()))),
            );
        }
        if let Some(action) = crate::dialogs::show(ctx, &mut s.dialog) {
            out.push((s.active, action));
        }
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
        let error = ctx.style().visuals.error_fg_color;
        s.toasts.show(ctx, error);
    }
}

fn status_bar(ui: &mut egui::Ui, s: &mut AppState, out: &mut Vec<(usize, Action)>) {
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
        ui.label(format!("{n} items"));
        if picked {
            ui.separator();
            ui.label(format!("{} selected", tab.selected.len()));
        }
        ui.separator();
        ui.label(format_size(bytes, DECIMAL));
        if !tab.filter.is_empty() {
            ui.separator();
            ui.label(format!("filter: {}", tab.filter));
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
                let (text, tip) = match (&s.searcher, &s.search_reason) {
                    (None, _) => ("Everything: …", "Loading the search backend"),
                    (Some(_), None) => ("Everything: ok", "Everything search is available"),
                    (Some(_), Some(_)) => ("Everything: not running", "Click to check again"),
                };
                if ui
                    .add(egui::Button::new(text).frame(false))
                    .on_hover_text(tip)
                    .clicked()
                {
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
            .build_eframe(|cc| App::new(cc, start));
        wait_listed(&mut harness);
        assert_eq!(harness.state().state.tab(0).entries().len(), 8);

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

    /// GPU-dependent: run locally with `cargo test -p keel-app -- --ignored`
    /// (`UPDATE_SNAPSHOTS=1` to rewrite `tests/snapshots/main.png`).
    #[test]
    #[ignore]
    fn main_window_snapshot() {
        let start = fixture("keel-snapshot-fixture");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, start));
        harness.state_mut().state.panes[1].view = crate::pane::ViewMode::Grid;
        wait_listed(&mut harness);
        harness.run_steps(4);
        harness.snapshot("main");
    }
}
