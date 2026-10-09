//! Details view: a virtual-row table (Name / Ext / Size / Modified; search results show
//! Name / Folder / Size / Modified).

use crate::keys::Action;
use crate::pane::{context_menu, drag_and_drop, handle_click, ViewCx};
use crate::tab::{SortKey, Tab};
use egui::{Align, Key, Layout, Sense};
use egui_extras::{Column, TableBuilder};
use keel_vfs::{Entry, Kind};

pub const ROW_H: f32 = 22.0;

pub fn size_text(e: &Entry) -> String {
    if e.kind == Kind::Dir {
        String::new()
    } else {
        humansize::format_size(e.size, humansize::DECIMAL)
    }
}

pub fn date_text(e: &Entry) -> String {
    e.modified
        .map(|m| {
            chrono::DateTime::<chrono::Local>::from(m)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn ui(
    ui: &mut egui::Ui,
    id: (usize, usize),
    tab: &mut Tab,
    cx: &mut ViewCx,
    out: &mut Vec<Action>,
) {
    tab.visible(cx.show_hidden);
    tab.row_step = 1;
    tab.page_rows = ((ui.available_height() / ROW_H) as usize)
        .saturating_sub(2)
        .max(1);
    let muted = cx.theme.muted();
    let search = tab.is_search();
    let mut renaming = tab.renaming.take();
    let mut rename_done = false;
    let mut sort_click = None;
    let mut clicks: Vec<(egui::Response, Entry)> = Vec::new();

    let mut table = TableBuilder::new(ui)
        .id_salt(id)
        .striped(true)
        .resizable(true)
        .sense(Sense::click_and_drag())
        .auto_shrink([false, true])
        .max_scroll_height(f32::INFINITY)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(120.0).clip(true))
        .column(
            Column::initial(if search { 180.0 } else { 56.0 })
                .at_least(30.0)
                .clip(true),
        )
        .column(Column::initial(80.0).at_least(50.0))
        .column(Column::initial(124.0).at_least(60.0).clip(true));
    if let Some(row) = tab.scroll_to.take() {
        table = table.scroll_to_row(row, None);
    }
    let sort = tab.sort;
    table
        .header(ROW_H, |mut header| {
            for (label, key) in [
                ("Name", SortKey::Name),
                ("Ext", SortKey::Ext),
                ("Size", SortKey::Size),
                ("Modified", SortKey::Modified),
            ] {
                header.col(|ui| {
                    if search && key == SortKey::Ext {
                        ui.strong("Folder");
                        return;
                    }
                    let arrow = match sort {
                        (k, true) if k == key => " ⏶",
                        (k, false) if k == key => " ⏷",
                        _ => "",
                    };
                    let text = egui::RichText::new(format!("{label}{arrow}")).strong();
                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                        sort_click = Some(key);
                    }
                });
            }
        })
        .body(|body| {
            let vis = tab.visible_cached();
            body.rows(ROW_H, vis.len(), |mut row| {
                let e = &tab.entries()[vis[row.index()]];
                let shown = tab.shown_name(e);
                row.set_selected(tab.selected.contains(&e.name));
                let color = (e.hidden).then_some(muted);
                row.col(|ui| {
                    ui.add(
                        egui::Image::new(crate::icons::icon_for(e))
                            .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                    );
                    match &mut renaming {
                        Some((name, text)) if *name == e.name => {
                            let r = ui
                                .add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                            if r.lost_focus() {
                                rename_done = true;
                                if ui.input(|i| i.key_pressed(Key::Enter))
                                    && !text.trim().is_empty()
                                    && text.trim() != shown
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
                            ui.add(egui::Label::new(cell(shown, color)).truncate());
                        }
                    }
                });
                row.col(|ui| {
                    if search {
                        let folder = e.path.parent().map(|d| d.display()).unwrap_or_default();
                        ui.add(egui::Label::new(cell(&folder, Some(muted))).truncate())
                            .on_hover_text(folder);
                    } else {
                        ui.label(cell(&e.ext, Some(muted)));
                    }
                });
                row.col(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(cell(&size_text(e), color));
                    });
                });
                row.col(|ui| {
                    ui.label(cell(&date_text(e), Some(muted)));
                });
                let r = row.response();
                r.context_menu(|ui| context_menu(ui, true, search, out));
                clicks.push((r, e.clone()));
            });
        });

    for (r, e) in clicks {
        handle_click(&r, tab, &e, out);
        drag_and_drop(&r, id.0, tab, &e, out);
    }
    if !rename_done {
        tab.renaming = renaming;
    }
    if let Some(key) = sort_click {
        tab.sort = (key, if tab.sort.0 == key { !tab.sort.1 } else { true });
    }
    empty_area(ui, tab, out);
}

fn cell(text: &str, color: Option<egui::Color32>) -> egui::RichText {
    let t = egui::RichText::new(text);
    match color {
        Some(c) => t.color(c),
        None => t,
    }
}

/// Space below the last row: click clears the selection, right-click opens the folder menu.
pub fn empty_area(ui: &mut egui::Ui, tab: &mut Tab, out: &mut Vec<Action>) {
    let rest = ui.available_rect_before_wrap();
    if rest.height() < 4.0 {
        return;
    }
    let r = ui.allocate_rect(rest, Sense::click());
    if r.clicked() || r.secondary_clicked() {
        tab.selected.clear();
    }
    r.context_menu(|ui| context_menu(ui, false, false, out));
}
