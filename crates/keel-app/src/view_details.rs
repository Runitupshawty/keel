//! Details view: a virtual-row table (Name / Ext / Size / Modified; search results show
//! Name / Folder / Size / Modified).

use crate::keys::Action;
use crate::pane::{context_menu, drag_and_drop, handle_click, ViewCx};
use crate::tab::{SortKey, Tab};
use egui::{Align, Key, Layout, Sense};
use egui_extras::{Column, TableBuilder};
use keel_core::{SourceStatus, FAVORITES};
use keel_vfs::{Entry, Kind};

pub const ROW_H: f32 = 22.0;

pub fn size_text(e: &Entry) -> String {
    if e.kind == Kind::Dir {
        String::new()
    } else {
        size_text_of(e.size)
    }
}

pub fn size_text_of(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::DECIMAL)
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
    let trash = tab.is_trash();
    let mut renaming = tab.renaming.take();
    let mut rename_done = false;
    let mut sort_click = None;
    let mut clicks: Vec<(egui::Response, Entry)> = Vec::new();
    // The row that just got the cursor fades its highlight in (Task 23).
    let fade = crate::anim::fade_in(
        ui.ctx(),
        egui::Id::new(("keel-fade", id)),
        tab.cursor.as_deref(),
    );
    let selection = ui.visuals().selection.bg_fill;
    // --- Task 29 ---: tags and favorites (library on), offline badges.
    let lib = cx.library;
    let tags_col = cx.tags_column && lib.is_open();
    // --- Task 33 ---: copies badge (library on).
    let copies_col = lib.is_open();

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
            Column::initial(if search || trash { 180.0 } else { 56.0 })
                .at_least(30.0)
                .clip(true),
        )
        .column(Column::initial(80.0).at_least(50.0))
        .column(Column::initial(124.0).at_least(60.0).clip(true));
    if copies_col {
        table = table.column(Column::initial(64.0).at_least(40.0).clip(true));
    }
    if tags_col {
        table = table.column(Column::initial(120.0).at_least(40.0).clip(true));
    }
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
                    if trash && key == SortKey::Ext {
                        ui.strong("Original location");
                        return;
                    }
                    let arrow = match sort {
                        (k, true) if k == key => " ⏶",
                        (k, false) if k == key => " ⏷",
                        _ => "",
                    };
                    let label = if trash && key == SortKey::Modified {
                        "Deleted on"
                    } else {
                        label
                    };
                    let text = egui::RichText::new(format!("{label}{arrow}")).strong();
                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                        sort_click = Some(key);
                    }
                });
            }
            if copies_col {
                header.col(|ui| {
                    ui.strong("Copies").on_hover_text(
                        "Copies of the content and the failure domains they span (hashed files)",
                    );
                });
            }
            if tags_col {
                header.col(|ui| {
                    ui.strong("Tags");
                });
            }
        })
        .body(|body| {
            let vis = tab.visible_cached();
            body.rows(ROW_H, vis.len(), |mut row| {
                let e = &tab.entries()[vis[row.index()]];
                let shown = tab.shown_name(e);
                let selected = tab.selected.contains(&e.name);
                let fading = selected && fade < 1.0 && tab.cursor.as_deref() == Some(&e.name);
                row.set_selected(selected && !fading);
                let tint = |ui: &mut egui::Ui| {
                    if fading {
                        let r = ui.max_rect().expand2(0.5 * ui.spacing().item_spacing);
                        ui.painter()
                            .rect_filled(r, 0.0, selection.gamma_multiply(fade));
                    }
                };
                let color = (e.hidden).then_some(muted);
                let real = lib
                    .is_open()
                    .then(|| crate::library::real_of(&lib.sources, &e.path))
                    .flatten();
                let source = real
                    .as_ref()
                    .and_then(|r| crate::library::source_of(&lib.sources, r))
                    .map(|(s, _)| s);
                let offline =
                    source.filter(|s| matches!(s.status, keel_core::SourceStatus::Offline { .. }));
                row.col(|ui| {
                    tint(ui);
                    ui.add(
                        egui::Image::new(crate::icons::icon_for(e))
                            .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                    );
                    if e.encrypted {
                        ui.add(
                            egui::Image::new(crate::icons::lock())
                                .fit_to_exact_size(egui::vec2(12.0, 12.0)),
                        )
                        .on_hover_text("Password-protected");
                    }
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
                    if let Some(s) = offline {
                        let SourceStatus::Offline { last_seen, reason } = s.status else {
                            unreachable!("filtered")
                        };
                        ui.label(egui::RichText::new("offline").small().color(muted))
                            .on_hover_text(format!(
                                "{} is {}: listed from the library index",
                                s.label,
                                crate::library::offline_text(last_seen, reason)
                            ));
                    }
                });
                row.col(|ui| {
                    tint(ui);
                    if search {
                        let mut folder = e.path.parent().map(|d| d.display()).unwrap_or_default();
                        if let Some(s) = source.filter(|_| tab.library_search) {
                            folder = format!("{} · {folder}", s.label);
                        }
                        ui.add(egui::Label::new(cell(&folder, Some(muted))).truncate())
                            .on_hover_text(folder);
                    } else if trash {
                        let from = crate::trash_ui::original_location(e);
                        ui.add(egui::Label::new(cell(&from, Some(muted))).truncate())
                            .on_hover_text(from);
                    } else {
                        ui.label(cell(&e.ext, Some(muted)));
                    }
                });
                row.col(|ui| {
                    tint(ui);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(cell(&size_text(e), color));
                    });
                });
                row.col(|ui| {
                    tint(ui);
                    ui.label(cell(&date_text(e), Some(muted)));
                });
                if copies_col {
                    row.col(|ui| {
                        tint(ui);
                        let badge = real
                            .as_ref()
                            .filter(|_| source.is_some() && e.kind != Kind::Dir)
                            .and_then(|r| lib.badge(r));
                        if let Some(b) = badge {
                            let color = if b.risk {
                                ui.visuals().warn_fg_color
                            } else {
                                muted
                            };
                            ui.label(cell(&b.text, Some(color))).on_hover_text(&b.hover);
                        }
                    });
                }
                if tags_col {
                    row.col(|ui| {
                        tint(ui);
                        let Some(real) = real.as_ref().filter(|_| source.is_some()) else {
                            return;
                        };
                        let on = lib.tagged.get(real).map_or(&[][..], |t| t.as_slice());
                        let fav = on.contains(&FAVORITES);
                        let star = egui::Button::new(if fav { "★" } else { "☆" }).frame(false);
                        if ui.add(star).on_hover_text("Favorite (Ctrl+D)").clicked() {
                            out.push(Action::Library(crate::library::LibCmd::TagPaths {
                                paths: vec![e.path.clone()],
                                tag: FAVORITES,
                                on: !fav,
                            }));
                        }
                        for tag in lib.tags.iter().filter(|t| on.contains(&t.id)) {
                            crate::library_ui::chip(ui, tag);
                        }
                    });
                }
                let r = row.response();
                r.context_menu(|ui| context_menu(ui, tab, Some(e), out));
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
    r.context_menu(|ui| context_menu(ui, tab, None, out));
}
