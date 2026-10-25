//! Drawing the Disk Usage window: the toolbar, the scan's progress, the layout (tree list,
//! file types list and the treemap's slot) and the status line.

use super::history;
use super::model::{attributes, Column, FileType, Model};
use super::picker::Answer;
use super::{DiskUsage, DuSettings, Layout, Tab, TITLE};
use crate::a11y::Named;
use crate::i18n::tr;
use crate::keys::Action;
use crate::theme::Theme;
use egui::{Align, Color32, Key, Modifiers, Sense, Ui};
use egui_extras::{Column as Col, TableBuilder};
use humansize::{format_size, DECIMAL};
use keel_vfs::du::{NodeId, NodeKind, Priority, Root, UNKNOWN_TIME};
use keel_vfs::VPath;
use std::collections::{BTreeSet, HashMap};

const ROW_H: f32 = 22.0;
const INDENT: f32 = 14.0;

pub use crate::treemap::shade::PALETTE;

/// The color of the file type ranked `rank` by size, as the treemap colors it: the palette
/// (WinDirStat's 13 colors unless edited in the chart options) repeats after its last.
pub fn type_color(palette: &[[u8; 3]], rank: usize) -> Color32 {
    if rank == usize::MAX {
        return Color32::GRAY;
    }
    let palette = if palette.is_empty() {
        &PALETTE[..]
    } else {
        palette
    };
    let [r, g, b] = palette[rank % palette.len()];
    Color32::from_rgb(r, g, b)
}

/// The Subtree % bar's color at `depth` (a cycle, so neighbors differ).
pub fn depth_color(depth: usize) -> Color32 {
    const DEPTHS: [[u8; 3]; 8] = [
        [64, 128, 255],
        [235, 92, 92],
        [80, 190, 110],
        [230, 170, 50],
        [170, 110, 230],
        [50, 190, 200],
        [230, 110, 180],
        [150, 160, 90],
    ];
    let [r, g, b] = DEPTHS[depth % DEPTHS.len()];
    Color32::from_rgb(r, g, b)
}

pub fn size_text(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}

/// A unix-ns time in local time ("" when not known).
pub fn time_text(ns: i64, seconds: bool) -> String {
    if ns == UNKNOWN_TIME {
        return String::new();
    }
    let t = chrono::DateTime::from_timestamp_nanos(ns).with_timezone(&chrono::Local);
    t.format(if seconds {
        "%Y-%m-%d %H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M"
    })
    .to_string()
}

/// The window and its Select drives dialog; call every frame (it also takes in finished
/// scans while the window is closed).
pub fn window(
    ctx: &egui::Context,
    du: &mut DiskUsage,
    settings: &mut DuSettings,
    drives: &[crate::sidebar::Drive],
    theme: &Theme,
    out: &mut Vec<Action>,
) {
    du.poll(ctx, settings);
    du.palette.clone_from(&settings.chart_options.palette);
    let tree = (du.model.as_ref()).map(|m| (du.generation, m.tree.clone()));
    super::view::share(ctx, super::view::Shared::from_settings(settings, tree));
    if let Some(f) = &mut du.fallback {
        if let Some(done) = f.ui(ctx) {
            du.fallback = None;
            du.note = done.map(|d| crate::i18n::tr("disk_usage.rescan_note").replace("{d}", &d));
        }
    }
    if let Some(p) = &mut du.picker {
        match p.ui(ctx, theme.accent()) {
            Some(Answer::Scan(roots)) => {
                du.device_labels = p.device_labels();
                du.start(roots);
            }
            Some(Answer::Cancel) => du.picker = None,
            None => {}
        }
    }
    if !du.open {
        du.keys = false;
        return;
    }
    // The window has the keys from a click in it until a click elsewhere.
    let press = ctx.input(|i| {
        (i.pointer.any_pressed())
            .then(|| i.pointer.interact_pos())
            .flatten()
    });
    if let (Some(p), Some(rect)) = (press, du.rect) {
        du.keys = rect.contains(p);
    }
    let keys = du.keys && du.picker.is_none() && !ctx.memory(|m| m.any_popup_open());
    let typing = ctx.wants_keyboard_input();
    if keys && !typing && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        match du.model.as_mut().filter(|m| m.filter.is_some()) {
            Some(m) => m.filter_extensions(None),
            None => du.open = false,
        }
    }
    if std::mem::take(&mut du.raise) {
        ctx.move_to_top(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new(TITLE),
        ));
    }
    let mut open = du.open;
    let shown = egui::Window::new(TITLE)
        .open(&mut open)
        .default_size([1100.0, 700.0])
        .min_size([520.0, 320.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            body(ui, du, settings, drives, theme, keys && !typing, out)
        });
    du.rect = shown.map(|s| s.response.rect);
    du.open &= open;
}

fn body(
    ui: &mut Ui,
    du: &mut DiskUsage,
    settings: &mut DuSettings,
    drives: &[crate::sidebar::Drive],
    theme: &Theme,
    keys: bool,
    out: &mut Vec<Action>,
) {
    toolbar(ui, du, settings, drives);
    if let Some((roots, p)) = du.scanning() {
        let what = roots_text(roots, &du.device_labels);
        let mut pause = None;
        let mut cancel = false;
        ui.horizontal(|ui| {
            if p.paused {
                ui.weak(tr("disk_usage.paused"));
            } else {
                crate::anim::spinner(ui);
            }
            ui.label(
                tr("disk_usage.scanning")
                    .replace("{what}", &what)
                    .replace("{files}", &p.files.to_string())
                    .replace("{size}", &size_text(p.bytes))
                    .replace("{rate}", &format!("{:.0}", p.files_per_sec))
                    .replace("{time}", &elapsed_text(p.elapsed)),
            );
            let label = if p.paused {
                tr("disk_usage.resume")
            } else {
                tr("disk_usage.pause")
            };
            if ui.button(label).clicked() {
                pause = Some(!p.paused);
            }
            if ui
                .button(tr("disk_usage.stop"))
                .on_hover_text(tr("disk_usage.stop_tip"))
                .clicked()
            {
                cancel = true;
            }
        });
        if !p.current.as_os_str().is_empty() {
            let at = p.current.display().to_string();
            ui.add(egui::Label::new(egui::RichText::new(at).weak().small()).truncate());
        }
        if let Some(on) = pause {
            du.pause(on);
        }
        if cancel {
            du.cancel();
        }
    }
    if let Some(e) = &du.error {
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    if du.model.is_none() {
        if du.scanning().is_none() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(tr("disk_usage.empty"));
                if ui.button(tr("disk_usage.select_drives")).clicked() {
                    du.picker = Some(super::picker::Picker::new(drives));
                }
                if !settings.history.is_empty() {
                    ui.add_space(12.0);
                    ui.strong(tr("disk_usage.recent"));
                    if let Some(roots) = recent_list(ui, &settings.history) {
                        du.start(roots);
                    }
                }
            });
        }
        return;
    }
    egui::TopBottomPanel::bottom("keel-du-status").show_inside(ui, |ui| status(ui, du));
    let group = settings.group_unregistered;
    match settings.layout {
        Layout::Classic => {
            let r = egui::TopBottomPanel::bottom("keel-du-treemap")
                .resizable(true)
                .default_height(settings.treemap_size)
                .height_range(80.0..=2000.0)
                .show_inside(ui, |ui| treemap_area(ui, du, settings));
            remember(&mut settings.treemap_size, r.response.rect.height());
            let r = egui::SidePanel::right("keel-du-types")
                .resizable(true)
                .default_width(settings.types_width)
                .width_range(200.0..=1200.0)
                .show_inside(ui, |ui| types_area(ui, du, group, out));
            remember(&mut settings.types_width, r.response.rect.width());
            egui::CentralPanel::default()
                .show_inside(ui, |ui| main_area(ui, du, settings, theme, keys, out));
        }
        Layout::TreemapLeft => {
            let r = egui::SidePanel::left("keel-du-treemap-left")
                .resizable(true)
                .default_width(settings.treemap_size.max(200.0))
                .width_range(160.0..=2000.0)
                .show_inside(ui, |ui| treemap_area(ui, du, settings));
            remember(&mut settings.treemap_size, r.response.rect.width());
            let r = egui::TopBottomPanel::top("keel-du-tree-top")
                .resizable(true)
                .default_height(settings.tree_height)
                .height_range(100.0..=2000.0)
                .show_inside(ui, |ui| main_area(ui, du, settings, theme, keys, out));
            remember(&mut settings.tree_height, r.response.rect.height());
            egui::CentralPanel::default().show_inside(ui, |ui| types_area(ui, du, group, out));
        }
        Layout::ListsOnly => {
            let r = egui::SidePanel::right("keel-du-types-only")
                .resizable(true)
                .default_width(settings.types_width)
                .width_range(200.0..=1200.0)
                .show_inside(ui, |ui| types_area(ui, du, group, out));
            remember(&mut settings.types_width, r.response.rect.width());
            egui::CentralPanel::default()
                .show_inside(ui, |ui| main_area(ui, du, settings, theme, keys, out));
        }
        Layout::TreeOnly => {
            egui::CentralPanel::default()
                .show_inside(ui, |ui| main_area(ui, du, settings, theme, keys, out));
        }
    }
}

/// Keeps a splitter's size in the settings (whole points, so a redraw changes nothing).
fn remember(setting: &mut f32, now: f32) {
    let now = now.round();
    if now > 0.0 && (*setting - now).abs() >= 1.0 {
        *setting = now;
    }
}

/// What a scan is of; a device storage by its name in `labels` when it is there.
pub(super) fn roots_text(roots: &[Root], labels: &[(Root, String)]) -> String {
    let names: Vec<String> = (roots.iter())
        .map(|r| match r {
            Root::AllLocalDrives => crate::i18n::tr("disk_usage.roots_all_local").to_owned(),
            Root::Path(p) => p.display().to_string(),
            Root::Mtp { .. } => (labels.iter().find(|(root, _)| root == r))
                .map_or_else(|| "a portable device".to_owned(), |(_, l)| l.clone()),
        })
        .collect();
    names.join(", ")
}

fn elapsed_text(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

fn toolbar(
    ui: &mut Ui,
    du: &mut DiskUsage,
    settings: &mut DuSettings,
    drives: &[crate::sidebar::Drive],
) {
    ui.horizontal(|ui| {
        if ui.button(tr("disk_usage.select_drives")).clicked() {
            du.picker = Some(super::picker::Picker::new(drives));
        }
        let can_rescan = !du.roots.is_empty() && du.scanning().is_none();
        if ui
            .add_enabled(can_rescan, egui::Button::new(tr("disk_usage.rescan")))
            .on_hover_text(tr("disk_usage.rescan_tip"))
            .clicked()
        {
            du.rescan();
        }
        ui.menu_button(tr("disk_usage.recent"), |ui| {
            // Scans of folders that are gone are dropped when the menu opens.
            history::prune(&mut settings.history, |p| p.exists());
            if settings.history.is_empty() {
                ui.weak(tr("disk_usage.recent_none"));
                return;
            }
            if let Some(roots) = recent_list(ui, &settings.history) {
                du.start(roots);
                ui.close_menu();
            }
            ui.separator();
            if ui.button(tr("disk_usage.recent_forget")).clicked() {
                settings.history.clear();
                ui.close_menu();
            }
        });
        ui.separator();
        watch_controls(ui, du, settings);
        ui.separator();
        let mut physical = settings.physical;
        ui.selectable_value(&mut physical, false, tr("disk_usage.size"))
            .on_hover_text(tr("disk_usage.size_tip"));
        ui.selectable_value(&mut physical, true, tr("disk_usage.size_on_disk"))
            .on_hover_text(tr("disk_usage.size_on_disk_tip"));
        if physical != settings.physical {
            settings.physical = physical;
            if let Some(m) = &mut du.model {
                m.set_physical(physical);
            }
        }
        ui.separator();
        egui::ComboBox::from_id_salt("keel-du-layout")
            .selected_text(settings.layout.label())
            .show_ui(ui, |ui| {
                for l in Layout::ALL {
                    ui.selectable_value(&mut settings.layout, l, l.label())
                        .on_hover_text(if l.shows_types() && l.shows_treemap() {
                            tr("disk_usage.layout_all_tip")
                        } else if l.shows_types() {
                            tr("disk_usage.layout_lists_tip")
                        } else {
                            tr("disk_usage.layout_tree_tip")
                        });
                }
            })
            .response
            .named("Layout");
        ui.menu_button(tr("disk_usage.columns"), |ui| column_chooser(ui, settings));
        ui.menu_button(tr("disk_usage.options"), |ui| {
            let mut relative = settings.relative;
            ui.radio_value(&mut relative, false, tr("disk_usage.percent_whole"));
            ui.radio_value(&mut relative, true, tr("disk_usage.percent_parent"));
            if relative != settings.relative {
                settings.relative = relative;
                if let Some(m) = &mut du.model {
                    m.set_relative(relative);
                }
            }
            ui.separator();
            ui.checkbox(
                &mut settings.group_unregistered,
                tr("disk_usage.group_unregistered"),
            )
            .on_hover_text(tr("disk_usage.group_unregistered_tip"));
            ui.checkbox(&mut settings.grid, tr("disk_usage.grid"));
            ui.checkbox(&mut settings.stripes, tr("disk_usage.stripes"));
            ui.checkbox(&mut settings.seconds, tr("disk_usage.seconds"));
            ui.separator();
            ui.checkbox(
                &mut settings.follow_junctions,
                tr("disk_usage.follow_junctions"),
            )
            .on_hover_text(tr("disk_usage.next_scan_tip"));
            ui.checkbox(
                &mut settings.follow_mount_points,
                tr("disk_usage.follow_mounts"),
            )
            .on_hover_text(tr("disk_usage.next_scan_tip"));
            ui.checkbox(
                &mut settings.exclude_hidden,
                tr("disk_usage.exclude_hidden"),
            )
            .on_hover_text(tr("disk_usage.next_scan_tip"));
            ui.checkbox(
                &mut settings.exclude_system,
                tr("disk_usage.exclude_system"),
            )
            .on_hover_text(tr("disk_usage.next_scan_tip"));
            ui.separator();
            ui.label(tr("disk_usage.priority"))
                .on_hover_text(tr("disk_usage.priority_tip"));
            for p in Priority::ALL {
                ui.radio_value(&mut settings.priority, p, priority_label(p))
                    .on_hover_text(tr("disk_usage.priority_tip"));
            }
        });
    });
}

/// The scan priority's name.
pub fn priority_label(p: Priority) -> &'static str {
    tr(match p {
        Priority::Normal => "disk_usage.priority_normal",
        Priority::Low => "disk_usage.priority_low",
        Priority::Background => "disk_usage.priority_background",
    })
}

/// The previous scans as buttons; the roots of the one clicked.
fn recent_list(ui: &mut Ui, history: &[history::Entry]) -> Option<Vec<Root>> {
    let mut picked = None;
    for e in history {
        let when = time_text(e.scanned.saturating_mul(1_000_000_000), false);
        let r = ui.button(e.label()).on_hover_text(format!(
            "{}\n{when}: {}, {} {}",
            tr("disk_usage.recent_tip"),
            size_text(e.bytes),
            e.files,
            tr("disk_usage.files"),
        ));
        if r.clicked() {
            picked = Some(e.scan_roots());
        }
    }
    picked
}

/// The File Watcher's toolbar part: on or off, pause, clear.
fn watch_controls(ui: &mut Ui, du: &mut DiskUsage, settings: &mut DuSettings) {
    ui.checkbox(&mut settings.watch, tr("disk_usage.watch"))
        .on_hover_text(tr("disk_usage.watch_tip"));
    let w = &mut du.watch;
    let on = settings.watch && (w.watching() || w.starting());
    let (label, tip) = match w.paused {
        true => ("disk_usage.watch_resume", "disk_usage.watch_resume_tip"),
        false => ("disk_usage.watch_pause", "disk_usage.watch_pause_tip"),
    };
    if ui
        .add_enabled(on, egui::Button::new(tr(label)))
        .on_hover_text(tr(tip))
        .clicked()
    {
        let paused = !w.paused;
        w.set_paused(paused);
    }
    if ui
        .add_enabled(
            !w.log.is_empty(),
            egui::Button::new(tr("disk_usage.watch_clear")),
        )
        .on_hover_text(tr("disk_usage.watch_clear_tip"))
        .clicked()
    {
        w.clear();
    }
}

/// The File Watcher's state for the status line ("" when it is off).
fn watch_status(du: &DiskUsage) -> String {
    let w = &du.watch;
    if let Some(e) = &w.error {
        return tr("disk_usage.watch_error").replace("{error}", e);
    }
    if w.starting() {
        return tr("disk_usage.watch_starting").to_owned();
    }
    if !w.watching() {
        return String::new();
    }
    let mut text = if w.paused {
        tr("disk_usage.watch_paused").replace("{n}", &w.waiting().to_string())
    } else {
        tr("disk_usage.watching").replace("{n}", &w.roots().len().to_string())
    };
    if w.added + w.removed + w.updated > 0 {
        text.push_str(", ");
        text.push_str(
            &tr("disk_usage.watch_totals")
                .replace("{added}", &w.added.to_string())
                .replace("{removed}", &w.removed.to_string())
                .replace("{updated}", &w.updated.to_string()),
        );
    }
    if w.overflowed {
        text.push_str(". ");
        text.push_str(tr("disk_usage.watch_behind"));
    }
    text
}

/// The **Changes** tab: the File Watcher's changes, newest first, narrowed by a regular
/// expression on the path.
fn changes_tab(ui: &mut Ui, du: &mut DiskUsage, settings: &DuSettings, out: &mut Vec<Action>) {
    let w = &mut du.watch;
    ui.horizontal(|ui| {
        ui.label(tr("disk_usage.changes_filter"));
        let mut text = w.filter_text.clone();
        let r = ui.add(
            egui::TextEdit::singleline(&mut text)
                .hint_text(tr("disk_usage.changes_filter_hint"))
                .desired_width(320.0),
        );
        let r = r.named(tr("disk_usage.changes_filter"));
        if r.changed() {
            w.set_filter(text);
        }
        if let Some(e) = &w.filter_error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                e.lines().last().unwrap_or(e.as_str()),
            );
        }
    });
    if !settings.watch {
        ui.weak(tr("disk_usage.changes_off"));
    }
    let shown: Vec<&keel_vfs::du::watch::Change> = w.log.newest_first(&w.filter).collect();
    ui.weak(
        tr("disk_usage.changes_count")
            .replace("{shown}", &shown.len().to_string())
            .replace("{total}", &w.log.total.to_string()),
    );
    if shown.is_empty() {
        ui.weak(tr("disk_usage.changes_none"));
        return;
    }
    let seconds = settings.seconds;
    let mut open = None;
    TableBuilder::new(ui)
        .id_salt("keel-du-changes")
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .auto_shrink([false, false])
        .cell_layout(egui::Layout::left_to_right(Align::Center))
        .column(Col::initial(140.0).at_least(60.0).clip(true))
        .column(Col::initial(90.0).at_least(50.0).clip(true))
        .column(Col::remainder().at_least(80.0).clip(true))
        .header(ROW_H, |mut h| {
            for key in [
                "disk_usage.col_time",
                "disk_usage.col_change",
                "disk_usage.col_path",
            ] {
                h.col(|ui| {
                    ui.strong(tr(key));
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, shown.len(), |mut row| {
                let c = shown[row.index()];
                row.col(|ui| {
                    ui.label(time_text(c.at, seconds));
                });
                row.col(|ui| {
                    ui.label(change_label(c.kind));
                });
                row.col(|ui| {
                    ui.add(egui::Label::new(c.path.display().to_string()).truncate());
                });
                let r = row.response();
                if r.double_clicked() {
                    open = Some(c.path.clone());
                }
                r.on_hover_text(c.path.display().to_string());
            });
        });
    if let Some(path) = open {
        out.push(Action::OpenPath(VPath::local(path)));
    }
}

pub fn change_label(k: keel_vfs::du::watch::ChangeKind) -> &'static str {
    use keel_vfs::du::watch::ChangeKind as K;
    tr(match k {
        K::Created => "disk_usage.kind_created",
        K::Deleted => "disk_usage.kind_deleted",
        K::Modified => "disk_usage.kind_modified",
        K::Renamed => "disk_usage.kind_renamed",
    })
}

/// Show or hide each column of the tree list (also on a right-click on its header).
fn column_chooser(ui: &mut Ui, settings: &mut DuSettings) {
    for c in Column::ALL {
        let mut on = c == Column::Name || settings.columns.contains(&c);
        let r = ui.add_enabled(c != Column::Name, egui::Checkbox::new(&mut on, c.label()));
        if r.changed() {
            settings.toggle_column(c);
        }
    }
    if ui.button(tr("disk_usage.default_columns")).clicked() {
        settings.columns = Column::DEFAULT.to_vec();
        ui.close_menu();
    }
}

fn status(ui: &mut Ui, du: &mut DiskUsage) {
    let mut relaunch_error = None;
    let Some(m) = &du.model else { return };
    ui.horizontal(|ui| {
        let what = if m.physical {
            tr("disk_usage.on_disk")
        } else {
            ""
        };
        match m.selected.len() {
            0 => ui.label(tr("disk_usage.nothing_selected")),
            1 => ui.label(format!("{} {what}", size_text(m.selection_bytes()))),
            n => ui.label(
                tr("disk_usage.items_selected")
                    .replace("{n}", &n.to_string())
                    .replace("{size}", &size_text(m.selection_bytes()))
                    .replace("{what}", what),
            ),
        };
        ui.separator();
        let info = &m.tree.info;
        let mut text = tr("disk_usage.scanned_in").replace("{time}", &elapsed_text(info.elapsed));
        if !info.complete {
            text.push_str(tr("disk_usage.stopped_incomplete"));
        }
        if info.unreadable > 0 {
            text.push_str(
                &tr("disk_usage.unreadable").replace("{n}", &info.unreadable.to_string()),
            );
        }
        ui.weak(text);
        let denied = du.denied_roots();
        if !denied.is_empty() {
            ui.label(tr("disk_usage.denied").replace("{n}", &info.denied.to_string()));
            if ui
                .button(tr("disk_usage.relaunch"))
                .on_hover_text(tr("disk_usage.relaunch_tip"))
                .clicked()
            {
                if let Err(e) = super::elevate::relaunch(&denied) {
                    relaunch_error = Some(
                        tr("disk_usage.relaunch_failed").replace("{error}", &format!("{e:#}")),
                    );
                }
            }
        }
        let watching = watch_status(du);
        if !watching.is_empty() {
            ui.separator();
            ui.weak(watching);
        }
        if let Some(f) = &m.filter {
            ui.separator();
            let list: Vec<String> = f.iter().map(|e| format!(".{e}")).collect();
            ui.label(tr("disk_usage.showing_only").replace("{list}", &list.join(" ")));
        }
    });
    if let Some(e) = relaunch_error {
        du.error = Some(e);
    }
}

/// The treemap's place: the chart of the scan ([`super::chart`]). An item picked in it is
/// shown in the tree list and the file types list ([`Model::reveal`]).
fn treemap_area(ui: &mut Ui, du: &mut DiskUsage, settings: &mut DuSettings) {
    du.types(settings.group_unregistered);
    let DiskUsage {
        model,
        chart,
        generation,
        ranks,
        ..
    } = du;
    let Some(model) = model.as_ref() else { return };
    if let Some(id) = super::chart::show(ui, chart, model, *generation, ranks, settings) {
        if let Some(m) = &mut du.model {
            m.reveal(id);
        }
        du.scroll_tree = true;
        du.scroll_types = true;
    }
}

enum TypeEvent {
    Select(BTreeSet<String>),
    Filter(BTreeSet<String>),
    /// List the files of this extension in the window's Search tab.
    SearchTab(String),
}

fn types_area(ui: &mut Ui, du: &mut DiskUsage, group: bool, out: &mut Vec<Action>) {
    let scroll = std::mem::take(&mut du.scroll_types);
    du.types(group);
    let Some(model) = &du.model else { return };
    let types = du.types_cached();
    let palette = &du.palette;
    let physical = model.physical;
    let total: u64 = types.iter().map(|t| t.bytes(physical)).sum();
    let selected = |t: &FileType| !t.extensions.is_disjoint(&model.highlight_extensions);
    let scroll_row = scroll.then(|| types.iter().position(selected)).flatten();
    let mut event = None;
    let mut table = TableBuilder::new(ui)
        .id_salt("keel-du-types-table")
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .auto_shrink([false, false])
        .cell_layout(egui::Layout::left_to_right(Align::Center))
        .column(Col::exact(18.0))
        .column(Col::initial(90.0).at_least(40.0).clip(true))
        .column(Col::initial(160.0).at_least(60.0).clip(true))
        .column(Col::initial(80.0).at_least(40.0).clip(true))
        .column(Col::initial(56.0).at_least(36.0).clip(true))
        .column(Col::remainder().at_least(40.0).clip(true));
    if let Some(row) = scroll_row {
        table = table.scroll_to_row(row, None);
    }
    table
        .header(ROW_H, |mut h| {
            for label in [
                "",
                tr("disk_usage.col_extension"),
                tr("disk_usage.col_description"),
                tr("disk_usage.col_bytes"),
                "%",
                tr("disk_usage.col_files"),
            ] {
                h.col(|ui| {
                    ui.strong(label);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_H, types.len(), |mut row| {
                let rank = row.index();
                let t = &types[rank];
                row.set_selected(selected(t));
                row.col(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
                    ui.painter()
                        .rect_filled(rect, 2.0, type_color(palette, rank));
                });
                row.col(|ui| {
                    ui.add(egui::Label::new(&t.label).truncate());
                });
                row.col(|ui| {
                    ui.add(egui::Label::new(&t.description).truncate());
                });
                let bytes = t.bytes(physical);
                right(&mut row, &size_text(bytes));
                let share = if total == 0 {
                    0.0
                } else {
                    100.0 * bytes as f64 / total as f64
                };
                right(&mut row, &format!("{share:.1} %"));
                right(&mut row, &t.files.to_string());
                let r = row.response();
                if r.double_clicked() {
                    event = Some(TypeEvent::Filter(t.extensions.clone()));
                } else if r.clicked() {
                    event = Some(TypeEvent::Select(t.extensions.clone()));
                }
                let exts = t.extensions.clone();
                let label = t.label.clone();
                r.on_hover_ui(|ui| {
                    ui.label(type_hover(t));
                })
                .context_menu(|ui| {
                    if ui.button(tr("disk_usage.show_only_these")).clicked() {
                        event = Some(TypeEvent::Filter(exts.clone()));
                        ui.close_menu();
                    }
                    let single = exts.len() == 1 && exts.iter().all(|e| !e.is_empty());
                    if single
                        && ui
                            .button(tr("disk_usage.search_type_new_tab").replace("{label}", &label))
                            .clicked()
                    {
                        let ext = exts.iter().next().cloned().unwrap_or_default();
                        out.push(Action::SearchFor(format!("*.{ext}")));
                        ui.close_menu();
                    }
                    if single && ui.button(tr("disk_usage.list_in_search")).clicked() {
                        event = Some(TypeEvent::SearchTab(
                            exts.iter().next().cloned().unwrap_or_default(),
                        ));
                        ui.close_menu();
                    }
                });
            });
        });
    let Some(model) = &mut du.model else { return };
    match event {
        Some(TypeEvent::Select(exts)) => {
            du.keys = true;
            model.highlight_extensions = exts;
        }
        Some(TypeEvent::Filter(exts)) => {
            model.highlight_extensions = exts.clone();
            model.filter_extensions(Some(exts));
        }
        Some(TypeEvent::SearchTab(ext)) => {
            du.keys = true;
            du.tabs.search = crate::du_tabs::search::State::for_extension(&ext);
            du.tab = Tab::Search;
        }
        None => {}
    }
}

fn type_hover(t: &FileType) -> String {
    if t.extensions.len() > 1 {
        let list: Vec<String> = t
            .extensions
            .iter()
            .take(40)
            .map(|e| format!(".{e}"))
            .collect();
        let more = if t.extensions.len() > 40 { " …" } else { "" };
        format!(
            "{}\n{}{more}\n{}",
            t.description,
            list.join(" "),
            crate::i18n::tr("disk_usage.type_hover_double_click")
        )
    } else {
        format!(
            "{}\n{}",
            t.description,
            crate::i18n::tr("disk_usage.type_hover_double_click")
        )
    }
}

fn right(row: &mut egui_extras::TableRow<'_, '_>, text: &str) {
    row.col(|ui| {
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.add(egui::Label::new(text).truncate());
        });
    });
}

/// The tab strip, the steps' progress and the tree or the analysis tab shown.
fn main_area(
    ui: &mut Ui,
    du: &mut DiskUsage,
    settings: &mut DuSettings,
    theme: &Theme,
    keys: bool,
    out: &mut Vec<Action>,
) {
    ui.horizontal_wrapped(|ui| {
        for t in Tab::ALL {
            ui.selectable_value(&mut du.tab, t, t.label());
        }
    });
    if let Some(status) = du.steps.status() {
        ui.horizontal(|ui| {
            crate::anim::spinner(ui);
            ui.label(status);
            if !du.steps.queue.is_empty()
                && ui
                    .button(tr("disk_usage.stop_after_step"))
                    .on_hover_text(tr("disk_usage.stop_after_step_tip"))
                    .clicked()
            {
                du.steps.queue.clear();
            }
        });
    }
    let mut dismiss = false;
    if let Some(note) = &du.note {
        ui.horizontal(|ui| {
            ui.label(note);
            dismiss = ui
                .small_button("×")
                .named(crate::i18n::tr("disk_usage.dismiss"))
                .clicked();
        });
    }
    if dismiss {
        du.note = None;
    }
    ui.separator();
    if du.tab == Tab::Tree {
        return tree_area(ui, du, settings, theme, keys, out);
    }
    if du.tab == Tab::Changes {
        return changes_tab(ui, du, settings, out);
    }
    let Some(tree) = du.model.as_ref().map(|m| m.tree.clone()) else {
        return;
    };
    use crate::du_tabs::{self as tabs, Outcome};
    let t = &mut du.tabs;
    let outcome = match du.tab {
        Tab::Tree | Tab::Changes => None,
        Tab::Largest => tabs::largest::ui(ui, &tree, &mut t.largest),
        Tab::Duplicates => tabs::duplicates::ui(ui, &tree, &mut t.duplicates),
        Tab::Search => tabs::search::ui(ui, &tree, &mut t.search),
        Tab::Storage => {
            tabs::storage::ui(ui, &tree, &mut t.storage);
            None
        }
        Tab::Permissions => tabs::permissions::ui(ui, &tree, &mut t.permissions),
    };
    match outcome {
        Some(Outcome::Select(id)) => {
            if let Some(m) = &mut du.model {
                m.reveal(id);
            }
            du.scroll_tree = true;
            du.scroll_types = true;
        }
        Some(Outcome::Plans(ops)) => {
            du.note = None;
            out.push(Action::DiskUsage(super::DuCmd::Plans(ops)));
        }
        None => {}
    }
}

enum TreeEvent {
    Click {
        id: NodeId,
        toggle: bool,
        range: bool,
    },
    Toggle(NodeId),
    Sort(Column),
    Column(Column),
}

/// How a tree list is drawn: the window's settings, or a pane's own.
pub struct ListStyle<'a> {
    /// The list's egui id (the window's and each pane's are apart).
    pub salt: egui::Id,
    pub columns: Vec<Column>,
    pub stripes: bool,
    pub grid: bool,
    pub seconds: bool,
    /// Each extension's color rank, and the colors.
    pub ranks: &'a HashMap<String, usize>,
    pub palette: &'a [[u8; 3]],
    /// Scroll to the cursor this frame.
    pub scroll: bool,
}

/// What happened in a tree list besides what it did to the model.
#[derive(Default)]
pub struct ListOutcome {
    /// A row was clicked (its window or pane takes the keys).
    pub clicked: bool,
    /// Columns shown or hidden from the header's menu.
    pub columns: Vec<Column>,
}

/// Rows a page of the list holds.
pub fn page_rows(ui: &Ui) -> isize {
    ((ui.available_height() / ROW_H) as isize - 2).max(1)
}

/// The rows of `model` likely visible in the list `salt` (from where it was scrolled last
/// frame), to read their owners and times.
pub fn visible_rows(ui: &Ui, salt: egui::Id, model: &Model, page: usize) -> Vec<NodeId> {
    let start = ui.data(|d| d.get_temp::<usize>(salt.with("first-row")));
    let from = start.unwrap_or(0).min(model.rows.len());
    (model.rows[from..].iter())
        .take(page + 8)
        .map(|r| r.0)
        .collect()
}

fn tree_area(
    ui: &mut Ui,
    du: &mut DiskUsage,
    settings: &mut DuSettings,
    theme: &Theme,
    keys: bool,
    out: &mut Vec<Action>,
) {
    let group = settings.group_unregistered;
    du.types(group);
    let page = page_rows(ui);
    if keys {
        if let Some(m) = &mut du.model {
            if tree_keys(ui.ctx(), m, page) {
                du.scroll_tree = true;
                du.scroll_types = true;
            }
        }
    }
    let salt = egui::Id::new("keel-du-tree-table");
    let columns = settings.shown_columns();
    if columns.iter().any(|c| c.metadata()) {
        if let Some(m) = &du.model {
            let ids = visible_rows(ui, salt, m, page as usize);
            du.meta.want(m, ids);
        }
    }
    let scroll = std::mem::take(&mut du.scroll_tree);
    let DiskUsage {
        model,
        ranks,
        palette,
        ..
    } = du;
    let Some(model) = model.as_mut() else { return };
    let style = ListStyle {
        salt,
        columns,
        stripes: settings.stripes,
        grid: settings.grid,
        seconds: settings.seconds,
        ranks,
        palette,
        scroll,
    };
    let o = tree_list(ui, model, &style, theme, out);
    if o.clicked {
        du.keys = true;
        du.scroll_types = true;
    }
    for c in o.columns {
        settings.toggle_column(c);
    }
}

/// The tree list of `model` (sortable columns, expandable rows, multi-select, tooltips and
/// a context menu per row); clicks, sorting and opening act on `model`.
pub fn tree_list(
    ui: &mut Ui,
    model: &mut Model,
    style: &ListStyle,
    theme: &Theme,
    out: &mut Vec<Action>,
) -> ListOutcome {
    let columns = style.columns.clone();
    let ranks = style.ranks;
    let palette = style.palette;
    let mut events = Vec::new();
    let first_row = {
        let model: &Model = model;
        let muted = theme.muted();
        let text_color = ui.visuals().text_color();
        let accent = theme.accent();
        let grid = style.grid;
        let seconds = style.seconds;
        let grid_stroke =
            egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color);
        let mut table = TableBuilder::new(ui)
            .id_salt(style.salt)
            .striped(style.stripes)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(Align::Center));
        for c in &columns {
            let width = match c {
                Column::Name => 280.0,
                Column::Subtree => 120.0,
                Column::Changed | Column::Accessed | Column::Created => 130.0,
                Column::Owner => 150.0,
                Column::Attributes => 70.0,
                Column::Allocated | Column::Size => 86.0,
                _ => 66.0,
            };
            table = table.column(Col::initial(width).at_least(36.0).clip(true));
        }
        if let Some(row) = style
            .scroll
            .then(|| model.cursor.and_then(|c| model.row_of(c)))
            .flatten()
        {
            table = table.scroll_to_row(row, None);
        }
        let mut first_row = None;
        table
            .header(ROW_H, |mut h| {
                for &c in &columns {
                    h.col(|ui| {
                        let arrow = match model.sort {
                            (k, true) if k == c => " ⏶",
                            (k, false) if k == c => " ⏷",
                            _ => "",
                        };
                        let text = egui::RichText::new(format!("{}{arrow}", c.label())).strong();
                        let r = ui
                            .add(egui::Button::new(text).frame(false))
                            .on_hover_text(tr("disk_usage.header_tip"));
                        let name = match model.sort {
                            (k, true) if k == c => {
                                tr("disk_usage.sorted_ascending").replace("{label}", c.label())
                            }
                            (k, false) if k == c => {
                                tr("disk_usage.sorted_descending").replace("{label}", c.label())
                            }
                            _ => c.label().to_owned(),
                        };
                        let r = r.named(&name);
                        if r.clicked() {
                            events.push(TreeEvent::Sort(c));
                        }
                        r.context_menu(|ui| {
                            for col in Column::ALL {
                                let mut on = col == Column::Name || columns.contains(&col);
                                let r = ui.add_enabled(
                                    col != Column::Name,
                                    egui::Checkbox::new(&mut on, col.label()),
                                );
                                if r.changed() {
                                    events.push(TreeEvent::Column(col));
                                }
                            }
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_H, model.rows.len(), |mut row| {
                    let index = row.index();
                    first_row = Some(first_row.map_or(index, |f: usize| f.min(index)));
                    let (id, depth) = model.rows[index];
                    let n = model.tree.node(id);
                    let selected = model.selected.contains(&id);
                    row.set_selected(selected);
                    let muted = if selected { text_color } else { muted };
                    let ext = (n.kind == NodeKind::File).then(|| model.tree.ext(id));
                    let highlighted = ext.is_some_and(|e| model.highlight_extensions.contains(e));
                    let line = |ui: &mut Ui| {
                        if grid {
                            let r = ui.max_rect();
                            ui.painter()
                                .vline(r.right() + 2.0, r.y_range(), grid_stroke);
                        }
                    };
                    for &c in &columns {
                        row.col(|ui| {
                            line(ui);
                            match c {
                                Column::Name => {
                                    ui.add_space(depth as f32 * INDENT);
                                    if model.expandable(id) {
                                        let open =
                                            model.expanded.contains(&id) || model.filter.is_some();
                                        let r = ui
                                            .add(
                                                egui::Button::new(if open { "⏷" } else { "⏵" })
                                                    .frame(false)
                                                    .small(),
                                            )
                                            .named(if open {
                                                tr("disk_usage.collapse")
                                            } else {
                                                tr("disk_usage.expand")
                                            });
                                        if r.clicked() {
                                            events.push(TreeEvent::Toggle(id));
                                        }
                                    } else {
                                        ui.add_space(INDENT);
                                    }
                                    match n.kind {
                                        NodeKind::Free | NodeKind::Unknown => {
                                            let (rect, _) = ui.allocate_exact_size(
                                                egui::vec2(14.0, 14.0),
                                                Sense::hover(),
                                            );
                                            ui.painter().rect_filled(
                                                rect,
                                                2.0,
                                                muted.gamma_multiply(0.6),
                                            );
                                        }
                                        NodeKind::File => {
                                            let rank = ranks
                                                .get(ext.unwrap_or(""))
                                                .copied()
                                                .unwrap_or(usize::MAX);
                                            let (rect, _) = ui.allocate_exact_size(
                                                egui::vec2(6.0, 14.0),
                                                Sense::hover(),
                                            );
                                            ui.painter().rect_filled(
                                                rect,
                                                1.0,
                                                type_color(palette, rank),
                                            );
                                            ui.add(
                                                egui::Image::new(crate::icons::file_icon(&n.name))
                                                    .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                                            );
                                        }
                                        _ => {
                                            let icon = if model.expanded.contains(&id) {
                                                crate::icons::folder_open()
                                            } else {
                                                crate::icons::folder()
                                            };
                                            ui.add(
                                                egui::Image::new(icon)
                                                    .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                                            );
                                        }
                                    }
                                    let mut name = egui::RichText::new(&*n.name);
                                    if n.is_pseudo() {
                                        name = name.italics().color(muted);
                                    } else if highlighted && !selected {
                                        name = name.color(accent);
                                    }
                                    ui.add(egui::Label::new(name).truncate().selectable(false));
                                }
                                Column::Subtree => {
                                    let share = model.fraction(id, true) as f32;
                                    let width = (ui.available_width() - 4.0).max(8.0);
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(width, 10.0),
                                        Sense::hover(),
                                    );
                                    ui.painter()
                                        .rect_filled(rect, 2.0, muted.gamma_multiply(0.2));
                                    let mut done = rect;
                                    done.set_width(rect.width() * share.clamp(0.0, 1.0));
                                    let color = if n.is_pseudo() {
                                        muted
                                    } else {
                                        depth_color(depth)
                                    };
                                    ui.painter().rect_filled(done, 2.0, color);
                                }
                                Column::Percentage => {
                                    let share = 100.0 * model.fraction(id, model.relative);
                                    right_in(ui, &format!("{share:.1} %"), None);
                                }
                                Column::Size => right_in(ui, &size_text(model.bytes(id)), None),
                                Column::Allocated => {
                                    right_in(ui, &size_text(n.allocated), Some(muted))
                                }
                                Column::Items | Column::Files | Column::Folders => {
                                    if n.is_container() {
                                        let v = match c {
                                            Column::Items => n.items,
                                            Column::Files => n.files,
                                            _ => n.folders,
                                        };
                                        right_in(ui, &v.to_string(), Some(muted));
                                    }
                                }
                                Column::Changed => {
                                    ui.label(
                                        egui::RichText::new(time_text(n.newest, seconds))
                                            .color(muted),
                                    );
                                }
                                Column::Attributes => {
                                    ui.label(
                                        egui::RichText::new(attributes(n.attributes))
                                            .color(muted)
                                            .monospace(),
                                    );
                                }
                                Column::Owner | Column::Accessed | Column::Created => {
                                    let meta = model.metadata.get(&id);
                                    let text = match c {
                                        Column::Owner => {
                                            meta.and_then(|m| m.owner.clone()).unwrap_or_default()
                                        }
                                        Column::Accessed => meta
                                            .and_then(|m| m.accessed)
                                            .map(|t| time_text(t, seconds))
                                            .unwrap_or_default(),
                                        _ => meta
                                            .and_then(|m| m.created)
                                            .map(|t| time_text(t, seconds))
                                            .unwrap_or_default(),
                                    };
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(text).color(muted))
                                            .truncate(),
                                    );
                                }
                            }
                        });
                    }
                    let r = row.response();
                    if r.double_clicked() {
                        if model.expandable(id) {
                            events.push(TreeEvent::Toggle(id));
                        } else if let Some(path) = model.tree.path(id) {
                            out.push(Action::OpenPath(VPath::local(path)));
                        }
                    } else if r.clicked() {
                        let m = r.ctx.input(|i| i.modifiers);
                        events.push(TreeEvent::Click {
                            id,
                            toggle: m.command,
                            range: m.shift,
                        });
                    } else if r.secondary_clicked() && !selected {
                        events.push(TreeEvent::Click {
                            id,
                            toggle: false,
                            range: false,
                        });
                    }
                    let r = r.on_hover_ui(|ui| {
                        ui.label(row_hover(model, id));
                    });
                    row_menu(&r, model, id, ext, out, &mut events);
                });
            });
        first_row
    };
    if let Some(f) = first_row {
        ui.data_mut(|d| d.insert_temp(style.salt.with("first-row"), f));
    }
    let mut outcome = ListOutcome::default();
    for e in events {
        match e {
            TreeEvent::Column(c) => outcome.columns.push(c),
            TreeEvent::Sort(c) => model.set_sort(c),
            TreeEvent::Toggle(id) => {
                if model.filter.is_none() {
                    model.toggle(id);
                }
            }
            TreeEvent::Click { id, toggle, range } => {
                outcome.clicked = true;
                model.select(id, toggle, range);
            }
        }
    }
    outcome
}

fn right_in(ui: &mut Ui, text: &str, color: Option<Color32>) {
    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
        let mut t = egui::RichText::new(text);
        if let Some(c) = color {
            t = t.color(c);
        }
        ui.add(egui::Label::new(t).truncate().selectable(false));
    });
}

/// A row's tooltip: its path and its size as a share of its folder and of the scan.
fn row_hover(model: &Model, id: NodeId) -> String {
    let n = model.tree.node(id);
    let bytes = model.bytes(id);
    let mut text = match model.tree.path(id) {
        Some(p) => p.display().to_string(),
        None => n.name.to_string(),
    };
    text.push_str(&format!("\n{}", size_text(bytes)));
    if n.is_pseudo() {
        let pct = format!("{:.1}", 100.0 * model.fraction(id, true));
        text.push_str(&crate::i18n::tr("disk_usage.row_hover_drive").replace("{pct}", &pct));
        return text;
    }
    let parent = n.parent;
    if parent != keel_vfs::du::TOP && parent != keel_vfs::du::NONE {
        let pct = format!("{:.1}", 100.0 * model.fraction(id, true));
        let name = model.tree.node(parent).name.to_string();
        let bytes = size_text(model.bytes(parent));
        text.push_str(
            &crate::i18n::tr("disk_usage.row_hover_parent")
                .replace("{pct}", &pct)
                .replace("{name}", &name)
                .replace("{bytes}", &bytes),
        );
    }
    let pct = format!("{:.1}", 100.0 * model.fraction(id, false));
    let bytes = size_text(model.bytes(keel_vfs::du::TOP));
    text.push_str(
        &crate::i18n::tr("disk_usage.row_hover_total")
            .replace("{pct}", &pct)
            .replace("{bytes}", &bytes),
    );
    if n.has(keel_vfs::du::flag::HARDLINK_DUP) {
        text.push_str(crate::i18n::tr("disk_usage.row_hover_hardlink"));
    }
    if n.has(keel_vfs::du::flag::UNREADABLE) {
        text.push_str(crate::i18n::tr("disk_usage.row_hover_unreadable"));
    }
    if n.has(keel_vfs::du::flag::CLOUD) {
        text.push_str(crate::i18n::tr("disk_usage.row_hover_cloud"));
    }
    text
}

fn row_menu(
    r: &egui::Response,
    model: &Model,
    id: NodeId,
    ext: Option<&str>,
    out: &mut Vec<Action>,
    events: &mut Vec<TreeEvent>,
) {
    let path = model.tree.path(id);
    let container = model.tree.node(id).is_container();
    r.context_menu(|ui| {
        let Some(path) = &path else {
            ui.weak(tr("disk_usage.free_no_files"));
            return;
        };
        if ui.button(tr("disk_usage.show_in_keel")).clicked() {
            out.push(Action::OpenPath(VPath::local(path)));
            ui.close_menu();
        }
        if container && ui.button(tr("disk_usage.open_new_tab")).clicked() {
            out.push(Action::NewTabAt(VPath::local(path)));
            ui.close_menu();
        }
        if ui.button(tr("disk_usage.copy_path")).clicked() {
            ui.ctx().copy_text(path.display().to_string());
            ui.close_menu();
        }
        if ui.button(tr("disk_usage.copy_rows")).clicked() {
            ui.ctx().copy_text(model.selected_tsv(id));
            ui.close_menu();
        }
        if container && model.expandable(id) && model.filter.is_none() {
            let open = model.expanded.contains(&id);
            if ui
                .button(if open {
                    tr("disk_usage.collapse")
                } else {
                    tr("disk_usage.expand")
                })
                .clicked()
            {
                events.push(TreeEvent::Toggle(id));
                ui.close_menu();
            }
        }
        if let Some(ext) = ext.filter(|e| !e.is_empty()) {
            if ui
                .button(tr("disk_usage.search_ext_new_tab").replace("{ext}", ext))
                .clicked()
            {
                out.push(Action::SearchFor(format!("*.{ext}")));
                ui.close_menu();
            }
        }
    });
}

/// The tree list's keys while the window has them; true when the cursor moved.
pub(super) fn tree_keys(ctx: &egui::Context, m: &mut Model, page: isize) -> bool {
    let shift = ctx.input(|i| i.modifiers.shift);
    let mut moved = false;
    let mut step = |m: &mut Model, key: Key, delta: isize| {
        let n = ctx.input_mut(|i| {
            i.count_and_consume_key(Modifiers::NONE, key)
                + i.count_and_consume_key(Modifiers::SHIFT, key)
        });
        for _ in 0..n {
            m.move_cursor(delta, shift);
            moved = true;
        }
    };
    step(m, Key::ArrowDown, 1);
    step(m, Key::ArrowUp, -1);
    step(m, Key::PageDown, page);
    step(m, Key::PageUp, -page);
    step(m, Key::End, isize::MAX / 2);
    step(m, Key::Home, -isize::MAX / 2);
    let pressed = |key: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key));
    if pressed(Key::ArrowLeft) {
        m.left();
        moved = true;
    }
    if pressed(Key::ArrowRight) {
        m.right();
        moved = true;
    }
    if pressed(Key::Enter) {
        if let Some(c) = m.cursor.filter(|&c| m.expandable(c) && m.filter.is_none()) {
            m.toggle(c);
        }
    }
    if pressed(Key::Space) {
        if let Some(c) = m.cursor {
            m.select(c, true, false);
        }
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
        m.select_all();
    }
    moved
}
