//! A file pane: tab strip, navigation bar with breadcrumb / editable path, filter box,
//! and the active tab's details or grid view.

use crate::keys::Action;
use crate::tab::{Nav, Tab};
use crate::theme::Theme;
use crate::view_grid::Thumbs;
use crate::{view_details, view_grid};
use egui::{Align, Key, Layout, Modifiers, Sense};
use keel_vfs::VPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Details,
    Grid,
}

pub struct Pane {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub view: ViewMode,
    /// Editable path box text while editing (Ctrl+L or click on the breadcrumb's empty area).
    pub path_edit: Option<String>,
    pub focus_path: bool,
    pub focus_filter: bool,
}

/// What the views need besides the tab itself.
pub struct ViewCx<'a> {
    pub theme: &'a Theme,
    pub show_hidden: bool,
    pub thumbs: &'a mut Thumbs,
    /// Highlight as the keyboard target (dual mode only).
    pub active: bool,
}

impl Pane {
    pub fn new(dir: VPath) -> Self {
        Self {
            tabs: vec![Tab::new(dir)],
            active: 0,
            view: ViewMode::Details,
            path_edit: None,
            focus_path: false,
            focus_filter: false,
        }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Never closes the last tab.
    pub fn close_tab(&mut self, i: usize) {
        if self.tabs.len() > 1 && i < self.tabs.len() {
            self.tabs.remove(i);
            if self.active > i || self.active == self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        }
    }

    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let active = self.tabs[self.active].dir.clone();
        let was_active = from == self.active;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = if was_active {
            to
        } else {
            self.tabs.iter().position(|t| t.dir == active).unwrap_or(0)
        };
    }

    pub fn edit_path(&mut self) {
        self.path_edit = Some(self.tab().dir.display());
        self.focus_path = true;
    }
}

/// Typed path: `scheme://...` or a local path.
pub fn parse_path(text: &str) -> Option<VPath> {
    let text = text.trim().trim_matches('"');
    if text.is_empty() {
        None
    } else if text.contains("://") {
        VPath::parse(text).ok()
    } else {
        Some(VPath::local(text))
    }
}

pub fn ui(ui: &mut egui::Ui, idx: usize, pane: &mut Pane, cx: &mut ViewCx, out: &mut Vec<Action>) {
    ui.style_mut().interaction.selectable_labels = false;
    if cx.active {
        let r = ui.max_rect();
        ui.painter().hline(
            r.x_range(),
            r.top(),
            egui::Stroke::new(2.0_f32, cx.theme.accent()),
        );
    }
    ui.add_space(3.0);
    tab_strip(ui, idx, pane, out);
    let search = pane.tab().is_search();
    if search {
        crate::search_tab::bar(ui, pane, out);
    } else {
        nav_bar(ui, pane, out);
        filter_bar(ui, pane, out);
    }
    let tab_idx = pane.active;
    let tab = &mut pane.tabs[tab_idx];
    if let Some(err) = tab.error.clone() {
        ui.horizontal(|ui| {
            if ui.small_button("✕").clicked() {
                tab.error = None;
            }
            ui.colored_label(ui.visuals().error_fg_color, err);
        });
    }
    if tab.loading && tab.listed_dir.is_none() {
        ui.centered_and_justified(|ui| ui.spinner());
        return;
    }
    match pane.view {
        // Search rows need the Folder column.
        _ if search => view_details::ui(ui, (idx, tab_idx), tab, cx, out),
        ViewMode::Details => view_details::ui(ui, (idx, tab_idx), tab, cx, out),
        ViewMode::Grid => view_grid::ui(ui, (idx, tab_idx), tab, cx, out),
    }
}

fn tab_strip(ui: &mut egui::Ui, idx: usize, pane: &mut Pane, out: &mut Vec<Action>) {
    let (mut select, mut close, mut moved) = (None, None, None);
    egui::ScrollArea::horizontal()
        .id_salt(("tabs", idx))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in pane.tabs.iter().enumerate() {
                    let drag = ui.dnd_drag_source(ui.id().with(("tab", i)), (idx, i), |ui| {
                        ui.selectable_label(i == pane.active, tab.title())
                            .on_hover_text(tab.dir.display())
                    });
                    let label = drag.inner;
                    if label.clicked() {
                        select = Some(i);
                    }
                    if label.middle_clicked() {
                        close = Some(i);
                    }
                    if let Some(from) = drag.response.dnd_release_payload::<(usize, usize)>() {
                        if from.0 == idx {
                            moved = Some((from.1, i));
                        }
                    }
                    if pane.tabs.len() > 1
                        && ui
                            .add(egui::Button::new("×").frame(false).small())
                            .on_hover_text("Close tab (Ctrl+W)")
                            .clicked()
                    {
                        close = Some(i);
                    }
                }
                if ui
                    .add(egui::Button::new("+").frame(false))
                    .on_hover_text("New tab (Ctrl+T)")
                    .clicked()
                {
                    out.push(Action::NewTab);
                }
            });
        });
    if let Some(i) = select {
        pane.active = i;
    }
    if let Some((from, to)) = moved {
        pane.move_tab(from, to);
    }
    if let Some(i) = close {
        pane.close_tab(i);
    }
}

fn nav_bar(ui: &mut egui::Ui, pane: &mut Pane, out: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let tab = pane.tab();
        let nav = |ui: &mut egui::Ui, enabled: bool, text: &str, tip: &str| {
            ui.add_enabled(enabled, egui::Button::new(text).frame(false))
                .on_hover_text(tip)
                .clicked()
        };
        if nav(ui, !tab.history.is_empty(), "⏴", "Back (Alt+Left)") {
            out.push(Action::Back);
        }
        if nav(ui, !tab.future.is_empty(), "⏵", "Forward (Alt+Right)") {
            out.push(Action::Forward);
        }
        if nav(ui, tab.dir.parent().is_some(), "⏶", "Up (Alt+Up)") {
            out.push(Action::Up);
        }
        if nav(ui, true, "⟳", "Refresh (F5)") {
            out.push(Action::Refresh);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for (mode, text, tip) in [
                (ViewMode::Grid, "Grid", "Grid view with thumbnails"),
                (ViewMode::Details, "Details", "Details view"),
            ] {
                if ui
                    .selectable_label(pane.view == mode, text)
                    .on_hover_text(tip)
                    .clicked()
                {
                    pane.view = mode;
                }
            }
            if pane.tab().loading {
                ui.spinner();
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                path_box(ui, pane, out)
            });
        });
    });
}

fn path_box(ui: &mut egui::Ui, pane: &mut Pane, out: &mut Vec<Action>) {
    if let Some(text) = &mut pane.path_edit {
        let r = ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(f32::INFINITY)
                .hint_text("Path"),
        );
        if std::mem::take(&mut pane.focus_path) {
            r.request_focus();
            // Select all, so typing replaces the shown path.
            let mut state = egui::TextEdit::load_state(ui.ctx(), r.id).unwrap_or_default();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(text.chars().count()),
                )));
            state.store(ui.ctx(), r.id);
        }
        if r.lost_focus() {
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Some(to) = parse_path(text) {
                    out.push(Action::Navigate(to));
                }
            }
            pane.path_edit = None;
        }
        return;
    }
    let mut crumbs = Vec::new();
    let mut cur = Some(pane.tab().dir.clone());
    while let Some(dir) = cur {
        cur = dir.parent();
        crumbs.push(dir);
    }
    crumbs.reverse();
    egui::ScrollArea::horizontal()
        .id_salt("crumbs")
        .stick_to_right(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (i, dir) in crumbs.iter().enumerate() {
                    if i > 0 {
                        ui.weak("›");
                    }
                    let label = match dir.name() {
                        name if i > 0 && !name.is_empty() => name.to_owned(),
                        _ => dir.display(),
                    };
                    let r = ui.add(egui::Button::new(label).frame(false));
                    if r.clicked() {
                        out.push(Action::Navigate(dir.clone()));
                    } else if r.middle_clicked() {
                        out.push(Action::NewTabAt(dir.clone()));
                    }
                }
                let size = egui::vec2(ui.available_width().max(24.0), ui.spacing().interact_size.y);
                let empty = ui.allocate_response(size, Sense::click());
                if empty
                    .on_hover_text("Click to type a path (Ctrl+L)")
                    .clicked()
                {
                    pane.edit_path();
                }
            });
        });
}

fn filter_bar(ui: &mut egui::Ui, pane: &mut Pane, out: &mut Vec<Action>) {
    if !pane.tab().filter_open {
        return;
    }
    let focus = std::mem::take(&mut pane.focus_filter);
    let tab = pane.tab_mut();
    ui.horizontal(|ui| {
        ui.label("Filter");
        let id = ui.id().with("filter");
        // List keys keep working while typing a filter.
        if ui.memory(|m| m.has_focus(id)) {
            ui.input_mut(|i| {
                for (key, nav) in [
                    (Key::ArrowUp, Nav::Prev),
                    (Key::ArrowDown, Nav::Next),
                    (Key::PageUp, Nav::PageUp),
                    (Key::PageDown, Nav::PageDown),
                ] {
                    if i.consume_key(Modifiers::NONE, key) {
                        out.push(Action::Move(nav, i.modifiers.shift));
                    }
                }
            });
        }
        let r = ui.add(
            egui::TextEdit::singleline(&mut tab.filter)
                .id(id)
                .desired_width(220.0)
                .hint_text("type to filter"),
        );
        if focus {
            r.request_focus();
            // Put the caret after the character that opened the filter.
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                let end = egui::text::CCursor::new(tab.filter.chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ui.ctx(), id);
            }
        }
        let close = ui
            .small_button("✕")
            .on_hover_text("Clear filter (Esc)")
            .clicked();
        if close || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape))) {
            tab.filter.clear();
            tab.filter_open = false;
        } else if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && tab.cursor.is_none()
        {
            out.push(Action::Move(Nav::Home, false));
        }
    });
}

/// Context menu shared by both views. `entry`: opened on this entry, not empty space;
/// search result rows add Open location; archive files add the extract items.
pub fn context_menu(
    ui: &mut egui::Ui,
    tab: &Tab,
    entry: Option<&keel_vfs::Entry>,
    out: &mut Vec<Action>,
) {
    let on_item = entry.is_some();
    let search = tab.is_search();
    let mut item = |ui: &mut egui::Ui, text: &str, shortcut: &str, action: Action| {
        let button = egui::Button::new(text).shortcut_text(shortcut);
        if ui.add(button).clicked() {
            out.push(action);
            ui.close_menu();
        }
    };
    if on_item {
        item(ui, "Open", "Enter", Action::Enter);
        item(ui, "Open with…", "", Action::OpenWith);
        if search {
            item(ui, "Open location", "Ctrl+Enter", Action::OpenLocation);
        }
        if let Some(e) = entry.filter(|e| crate::jobs::is_archive_file(e)) {
            ui.separator();
            item(ui, "Extract here", "", Action::ExtractHere);
            let folder = format!(
                "Extract to folder \"{}\"",
                crate::jobs::archive_stem(&e.name)
            );
            item(ui, &folder, "", Action::ExtractToFolder);
            item(ui, "Extract to…", "", Action::ExtractTo);
        }
        ui.separator();
        item(ui, "Copy", "Ctrl+C", Action::Copy);
        item(ui, "Cut", "Ctrl+X", Action::Cut);
    }
    item(ui, "Paste", "Ctrl+V", Action::Paste);
    item(ui, "Copy path", "", Action::CopyPath);
    if on_item {
        ui.separator();
        item(ui, "Rename", "F2", Action::Rename);
        item(ui, "Delete", "Del", Action::Delete);
        ui.separator();
        let add = format!("Add to \"{}\"", crate::jobs::zip_name(tab));
        item(ui, &add, "", Action::AddToZip);
        item(ui, "Compress to zip…", "", Action::CompressToZip);
    }
    ui.separator();
    item(ui, "New folder", "Ctrl+Shift+N", Action::NewFolder);
    item(ui, "New file", "", Action::NewFile);
    ui.separator();
    item(ui, "Select all", "Ctrl+A", Action::SelectAll);
    item(ui, "Invert selection", "", Action::InvertSelection);
    ui.separator();
    item(
        ui,
        "Show in system file manager",
        "",
        Action::RevealInSystem,
    );
    item(ui, "Open terminal here", "", Action::OpenTerminal);
    item(ui, "Properties", "", Action::Properties);
}

/// Shared row/tile click handling: select, double-click opens, middle-click new tab.
pub fn handle_click(
    r: &egui::Response,
    tab: &mut Tab,
    entry: &keel_vfs::Entry,
    out: &mut Vec<Action>,
) {
    let mods = r.ctx.input(|i| i.modifiers);
    if r.double_clicked() {
        tab.click(&entry.name, false, false);
        out.push(Action::Enter);
    } else if r.clicked() {
        tab.click(&entry.name, mods.command, mods.shift);
    } else if r.secondary_clicked() && !tab.selected.contains(&entry.name) {
        tab.click(&entry.name, false, false);
    } else if r.middle_clicked() && entry.kind == keel_vfs::Kind::Dir {
        out.push(Action::NewTabAt(entry.path.clone()));
    }
}

/// In-app drag payload: entries dragged out of pane `pane` showing folder `dir`.
pub struct DragPayload {
    pub pane: usize,
    pub dir: VPath,
    pub paths: Vec<VPath>,
}

impl DragPayload {
    /// Dropping this on `dst`: entries dragged out of an archive extract, others copy/move.
    pub fn action(&self, dst: VPath) -> Action {
        match crate::jobs::ArchiveSrc::picked(&self.dir, &self.paths) {
            Some(src) => Action::Extract { src, dst },
            None => Action::Drop {
                paths: self.paths.iter().filter_map(VPath::to_local_path).collect(),
                from: Some((self.pane, self.dir.clone())),
                dst,
            },
        }
    }
}

/// Shared row/tile drag handling: a drag starts with the row's selection (selecting the
/// row first if needed); folders accept drops.
pub fn drag_and_drop(
    r: &egui::Response,
    pane: usize,
    tab: &mut Tab,
    entry: &keel_vfs::Entry,
    out: &mut Vec<Action>,
) {
    if r.drag_started() {
        if !tab.selected.contains(&entry.name) {
            tab.click(&entry.name, false, false);
        }
        let paths = tab.targets().iter().map(|e| e.path.clone()).collect();
        r.dnd_set_drag_payload(DragPayload {
            pane,
            dir: tab.dir.clone(),
            paths,
        });
    }
    if entry.kind != keel_vfs::Kind::Dir {
        return;
    }
    if let Some(p) = r.dnd_hover_payload::<DragPayload>() {
        if !p.paths.contains(&entry.path) {
            let stroke = r.ctx.style().visuals.selection.stroke;
            r.ctx
                .layer_painter(egui::LayerId::new(egui::Order::Foreground, r.id))
                .rect_stroke(r.rect, 3.0, stroke, egui::StrokeKind::Inside);
        }
    }
    if let Some(p) = r.dnd_release_payload::<DragPayload>() {
        if !p.paths.contains(&entry.path) {
            out.push(p.action(entry.path.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_close_and_reorder_keep_the_active_tab() {
        let dir = |p: &str| VPath::parse(&format!("mem://t{p}")).unwrap();
        let mut pane = Pane::new(dir("/a"));
        pane.tabs.push(Tab::new(dir("/b")));
        pane.tabs.push(Tab::new(dir("/c")));
        pane.active = 2;
        pane.move_tab(0, 2);
        assert_eq!(pane.tab().dir, dir("/c"));
        pane.close_tab(pane.active);
        pane.close_tab(0);
        pane.close_tab(0);
        assert_eq!(pane.tabs.len(), 1);
        assert_eq!(pane.tab().dir, dir("/a"));
    }
}
