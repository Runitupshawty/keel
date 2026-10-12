//! Drawing the Disk Usage window: the toolbar, the scan's progress, the layout (tree list,
//! file types list and the treemap's slot) and the status line.

use super::model::{attributes, Column, FileType, Model};
use super::picker::Answer;
use super::{DiskUsage, DuSettings, Layout, Tab, TITLE};
use crate::a11y::Named;
use crate::keys::Action;
use crate::theme::Theme;
use egui::{Align, Color32, Key, Modifiers, Sense, Ui};
use egui_extras::{Column as Col, TableBuilder};
use humansize::{format_size, DECIMAL};
use keel_vfs::du::{NodeId, NodeKind, Root, UNKNOWN_TIME};
use keel_vfs::VPath;
use std::collections::{BTreeSet, HashMap};

const ROW_H: f32 = 22.0;
const INDENT: f32 = 14.0;

/// WinDirStat's 13 file type colors, by size rank (the treemap uses the same).
pub const PALETTE: [[u8; 3]; 13] = [
    [0, 0, 255],
    [255, 0, 0],
    [0, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 255, 0],
    [150, 150, 255],
    [255, 150, 150],
    [150, 255, 150],
    [150, 255, 255],
    [255, 150, 255],
    [255, 255, 150],
    [255, 255, 255],
];

/// The color of the file type ranked `rank` by size (the 13th and later share the last).
pub fn type_color(rank: usize) -> Color32 {
    let [r, g, b] = PALETTE[rank.min(PALETTE.len() - 1)];
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
    if let Some(f) = &mut du.fallback {
        if let Some(done) = f.ui(ctx) {
            du.fallback = None;
            du.note = done.map(|d| format!("{d}. Rescan to see the change."));
        }
    }
    if let Some(p) = &mut du.picker {
        match p.ui(ctx, theme.accent()) {
            Some(Answer::Scan(roots)) => du.start(roots),
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
        let what = roots_text(roots);
        let mut pause = None;
        let mut cancel = false;
        ui.horizontal(|ui| {
            if p.paused {
                ui.weak("Paused");
            } else {
                crate::anim::spinner(ui);
            }
            ui.label(format!(
                "Scanning {what}: {} files, {} ({:.0} files/s), {}",
                p.files,
                size_text(p.bytes),
                p.files_per_sec,
                elapsed_text(p.elapsed)
            ));
            let label = if p.paused { "Resume" } else { "Pause" };
            if ui.button(label).clicked() {
                pause = Some(!p.paused);
            }
            if ui
                .button("Stop")
                .on_hover_text("Stop scanning and show what was found")
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
                ui.label("Scan drives or folders to see what takes their space.");
                if ui.button("Select drives…").clicked() {
                    du.picker = Some(super::picker::Picker::new(drives));
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
                .show_inside(ui, |ui| treemap_area(ui, du, group));
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
                .show_inside(ui, |ui| treemap_area(ui, du, group));
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
    }
}

/// Keeps a splitter's size in the settings (whole points, so a redraw changes nothing).
fn remember(setting: &mut f32, now: f32) {
    let now = now.round();
    if now > 0.0 && (*setting - now).abs() >= 1.0 {
        *setting = now;
    }
}

fn roots_text(roots: &[Root]) -> String {
    let names: Vec<String> = (roots.iter())
        .map(|r| match r {
            Root::AllLocalDrives => "all local drives".to_owned(),
            Root::Path(p) => p.display().to_string(),
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
        if ui.button("Select drives…").clicked() {
            du.picker = Some(super::picker::Picker::new(drives));
        }
        let can_rescan = !du.roots.is_empty() && du.scanning().is_none();
        if ui
            .add_enabled(can_rescan, egui::Button::new("Rescan"))
            .on_hover_text("Scan the same drives and folders again")
            .clicked()
        {
            du.rescan();
        }
        ui.separator();
        let mut physical = settings.physical;
        ui.selectable_value(&mut physical, false, "Size")
            .on_hover_text("Sizes are the files' lengths");
        ui.selectable_value(&mut physical, true, "Size on disk")
            .on_hover_text("Sizes are the space the files take on the drive");
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
                    ui.selectable_value(&mut settings.layout, l, l.label());
                }
            })
            .response
            .named("Layout");
        ui.menu_button("Columns", |ui| column_chooser(ui, settings));
        ui.menu_button("Options", |ui| {
            let mut relative = settings.relative;
            ui.radio_value(&mut relative, false, "Percentage of the whole scan");
            ui.radio_value(&mut relative, true, "Percentage of the parent folder");
            if relative != settings.relative {
                settings.relative = relative;
                if let Some(m) = &mut du.model {
                    m.set_relative(relative);
                }
            }
            ui.separator();
            ui.checkbox(
                &mut settings.group_unregistered,
                "Group unregistered extensions",
            )
            .on_hover_text("One row for every extension this system has no file type for");
            ui.checkbox(&mut settings.grid, "Grid lines");
            ui.checkbox(&mut settings.stripes, "Striped rows");
            ui.checkbox(&mut settings.seconds, "Seconds in times");
        });
    });
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
    if ui.button("Default columns").clicked() {
        settings.columns = Column::DEFAULT.to_vec();
        ui.close_menu();
    }
}

fn status(ui: &mut Ui, du: &DiskUsage) {
    let Some(m) = &du.model else { return };
    ui.horizontal(|ui| {
        let what = if m.physical { "on disk" } else { "" };
        match m.selected.len() {
            0 => ui.label("Nothing selected"),
            1 => ui.label(format!("{} {what}", size_text(m.selection_bytes()))),
            n => ui.label(format!(
                "{n} items selected: {} {what}",
                size_text(m.selection_bytes())
            )),
        };
        ui.separator();
        let info = &m.tree.info;
        let mut text = format!("Scanned in {}", elapsed_text(info.elapsed));
        if !info.complete {
            text.push_str(" (stopped: incomplete)");
        }
        if info.unreadable > 0 {
            text.push_str(&format!(", {} folders could not be read", info.unreadable));
        }
        ui.weak(text);
        if let Some(f) = &m.filter {
            ui.separator();
            let list: Vec<String> = f.iter().map(|e| format!(".{e}")).collect();
            ui.label(format!("Showing only {}", list.join(" ")));
        }
    });
}

/// The treemap's place (Task 83's chart is drawn here). It gets the scan
/// (`model.tree`), the selection (`model.cursor`, `model.selected`), the file types to
/// outline (`model.highlight_extensions`) and the type colors by rank (`ranks`,
/// [`type_color`]); an item picked in it is returned and shown in the tree
/// ([`Model::reveal`]).
pub fn treemap_slot(ui: &mut Ui, model: &Model, ranks: &HashMap<String, usize>) -> Option<NodeId> {
    let _ = (model, ranks);
    let rect = ui.available_rect_before_wrap();
    let r = ui.allocate_rect(rect, Sense::hover()).named("Treemap");
    ui.painter()
        .rect_filled(r.rect, 4.0, ui.visuals().extreme_bg_color);
    ui.painter().text(
        r.rect.center(),
        egui::Align2::CENTER_CENTER,
        "Treemap",
        egui::TextStyle::Body.resolve(ui.style()),
        ui.visuals().weak_text_color(),
    );
    None
}

fn treemap_area(ui: &mut Ui, du: &mut DiskUsage, group: bool) {
    du.types(group);
    let Some(model) = &du.model else { return };
    if let Some(id) = treemap_slot(ui, model, du.ranks()) {
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
}

fn types_area(ui: &mut Ui, du: &mut DiskUsage, group: bool, out: &mut Vec<Action>) {
    let scroll = std::mem::take(&mut du.scroll_types);
    du.types(group);
    let Some(model) = &du.model else { return };
    let types = du.types_cached();
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
            for label in ["", "Extension", "Description", "Bytes", "%", "Files"] {
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
                    ui.painter().rect_filled(rect, 2.0, type_color(rank));
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
                    if ui.button("Show only these files in the tree").clicked() {
                        event = Some(TypeEvent::Filter(exts.clone()));
                        ui.close_menu();
                    }
                    let single = exts.len() == 1 && exts.iter().all(|e| !e.is_empty());
                    if single
                        && ui
                            .button(format!("Search for {label} in a new tab"))
                            .clicked()
                    {
                        let ext = exts.iter().next().cloned().unwrap_or_default();
                        out.push(Action::SearchFor(format!("*.{ext}")));
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
            "{}\n{}{more}\nDouble-click to list only these files",
            t.description,
            list.join(" ")
        )
    } else {
        format!("{}\nDouble-click to list only these files", t.description)
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
                    .button("Stop after this step")
                    .on_hover_text("The steps after this one are not run")
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
            dismiss = ui.small_button("✕").named("Dismiss").clicked();
        });
    }
    if dismiss {
        du.note = None;
    }
    ui.separator();
    if du.tab == Tab::Tree {
        return tree_area(ui, du, settings, theme, keys, out);
    }
    let Some(tree) = du.model.as_ref().map(|m| m.tree.clone()) else {
        return;
    };
    use crate::du_tabs::{self as tabs, Outcome};
    let t = &mut du.tabs;
    let outcome = match du.tab {
        Tab::Tree => None,
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
    let page = ((ui.available_height() / ROW_H) as isize - 2).max(1);
    if keys {
        if let Some(m) = &mut du.model {
            if tree_keys(ui.ctx(), m, page) {
                du.scroll_tree = true;
                du.scroll_types = true;
            }
        }
    }
    let columns = settings.shown_columns();
    if let Some(m) = &du.model {
        if columns.iter().any(|c| c.metadata()) {
            let start = ui.data(|d| d.get_temp::<usize>(egui::Id::new("keel-du-first-row")));
            let from = start.unwrap_or(0).min(m.rows.len());
            let ids: Vec<NodeId> = (m.rows[from..].iter())
                .take(page as usize + 8)
                .map(|r| r.0)
                .collect();
            du.want_metadata(ids.into_iter());
        }
    }
    let scroll = std::mem::take(&mut du.scroll_tree);
    let ranks = du.ranks();
    let Some(model) = &du.model else { return };
    let mut events = Vec::new();
    let muted = theme.muted();
    let text_color = ui.visuals().text_color();
    let accent = theme.accent();
    let grid = settings.grid;
    let seconds = settings.seconds;
    let grid_stroke =
        egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let mut table = TableBuilder::new(ui)
        .id_salt("keel-du-tree-table")
        .striped(settings.stripes)
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
    if let Some(row) = scroll
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
                        .on_hover_text("Click to sort, right-click to choose the columns");
                    let name = match model.sort {
                        (k, true) if k == c => format!("{}, sorted ascending", c.label()),
                        (k, false) if k == c => format!("{}, sorted descending", c.label()),
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
                                        .named(if open { "Collapse" } else { "Expand" });
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
                                            .unwrap_or(PALETTE.len());
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(6.0, 14.0),
                                            Sense::hover(),
                                        );
                                        ui.painter().rect_filled(rect, 1.0, type_color(rank));
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
                                let (rect, _) =
                                    ui.allocate_exact_size(egui::vec2(width, 10.0), Sense::hover());
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
                            Column::Allocated => right_in(ui, &size_text(n.allocated), Some(muted)),
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
                                    egui::RichText::new(time_text(n.newest, seconds)).color(muted),
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
    if let Some(f) = first_row {
        ui.data_mut(|d| d.insert_temp(egui::Id::new("keel-du-first-row"), f));
    }
    for e in events {
        match e {
            TreeEvent::Column(c) => settings.toggle_column(c),
            TreeEvent::Sort(c) => {
                if let Some(m) = &mut du.model {
                    m.set_sort(c);
                }
            }
            TreeEvent::Toggle(id) => {
                if let Some(m) = &mut du.model {
                    if m.filter.is_none() {
                        m.toggle(id);
                    }
                }
            }
            TreeEvent::Click { id, toggle, range } => {
                du.keys = true;
                if let Some(m) = &mut du.model {
                    m.select(id, toggle, range);
                }
                du.scroll_types = true;
            }
        }
    }
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
        text.push_str(&format!(
            ", {:.1} % of the drive",
            100.0 * model.fraction(id, true)
        ));
        return text;
    }
    let parent = n.parent;
    if parent != keel_vfs::du::TOP && parent != keel_vfs::du::NONE {
        text.push_str(&format!(
            "\n{:.1} % of {} ({})",
            100.0 * model.fraction(id, true),
            model.tree.node(parent).name,
            size_text(model.bytes(parent))
        ));
    }
    text.push_str(&format!(
        "\n{:.1} % of everything scanned ({})",
        100.0 * model.fraction(id, false),
        size_text(model.bytes(keel_vfs::du::TOP))
    ));
    if n.has(keel_vfs::du::flag::HARDLINK_DUP) {
        text.push_str("\nAnother name of a file counted elsewhere (hard link)");
    }
    if n.has(keel_vfs::du::flag::UNREADABLE) {
        text.push_str("\nCould not be read");
    }
    if n.has(keel_vfs::du::flag::CLOUD) {
        text.push_str("\nCloud placeholder: not downloaded");
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
            ui.weak("Free and unknown space have no files");
            return;
        };
        if ui.button("Show in Keel").clicked() {
            out.push(Action::OpenPath(VPath::local(path)));
            ui.close_menu();
        }
        if container && ui.button("Open in a new tab").clicked() {
            out.push(Action::NewTabAt(VPath::local(path)));
            ui.close_menu();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(path.display().to_string());
            ui.close_menu();
        }
        if container && model.expandable(id) && model.filter.is_none() {
            let open = model.expanded.contains(&id);
            if ui
                .button(if open { "Collapse" } else { "Expand" })
                .clicked()
            {
                events.push(TreeEvent::Toggle(id));
                ui.close_menu();
            }
        }
        if let Some(ext) = ext.filter(|e| !e.is_empty()) {
            if ui
                .button(format!("Search for .{ext} in a new tab"))
                .clicked()
            {
                out.push(Action::SearchFor(format!("*.{ext}")));
                ui.close_menu();
            }
        }
    });
}

/// The tree list's keys while the window has them; true when the cursor moved.
fn tree_keys(ctx: &egui::Context, m: &mut Model, page: isize) -> bool {
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
