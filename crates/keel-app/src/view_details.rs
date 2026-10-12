//! Details view: a virtual-row table (Name / Ext / Size / Modified; search results show
//! Name / Folder / Size / Modified). Task 46: the Sizes view is this table with a Share bar
//! in place of Ext, plus Files and Of used columns, biggest first.

use crate::a11y::Named;
use crate::keys::Action;
use crate::pane::{context_menu, drag_and_drop, handle_click, ViewCx};
use crate::tab::{SortKey, Tab};
use egui::{Align, Key, Layout, Sense};
use egui_extras::{Column, TableBuilder};
use keel_core::{SourceStatus, FAVORITES};
use keel_vfs::{Entry, Kind};

pub const ROW_H: f32 = crate::design::CLASSIC.rows.list;
/// A library search hit with a snippet of its text under the name (Task 62).
pub const SNIPPET_ROW_H: f32 = crate::design::CLASSIC.rows.snippet;

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
    crate::sizes::sync_tab(tab, cx.library, cx.sizes);
    tab.visible(cx.show_hidden);
    tab.row_step = 1;
    tab.page_rows = ((ui.available_height() / ROW_H) as usize)
        .saturating_sub(2)
        .max(1);
    let muted = cx.theme.muted();
    let search = tab.is_search();
    let trash = tab.is_trash();
    // --- Task 46 ---: the Sizes view: shares of the folder and of the volume's used space.
    let sizes = tab.sizes_view;
    let (lib, walked) = (cx.library, cx.sizes);
    let folder_sum: u64 = if sizes {
        tab.entries()
            .iter()
            .filter_map(|e| crate::sizes::bytes_of(lib, walked, e))
            .sum()
    } else {
        0
    };
    let used = crate::sidebar::drive_in(cx.drives, &tab.dir).map(|d| d.3.saturating_sub(d.2));
    let accent = cx.theme.accent();
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
    let text = ui.visuals().text_color();
    let selection = ui.visuals().selection.bg_fill;
    // --- Task 29 ---: tags and favorites (library on), offline badges.
    let tags_col = cx.tags_column && lib.is_open();
    // --- Task 33 ---: copies badge (library on).
    let copies_col = lib.is_open();

    // Task 46: network, removable, remote and cloud folders are walked on request only.
    if walk_wanted(tab, lib, walked) && !crate::sizes::walks_on_its_own(&tab.dir) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Folder sizes here are only walked on request.").color(muted),
            );
            if ui.button("Calculate sizes").clicked() {
                out.push(Action::CalculateSizes { fresh: false });
            }
        });
    }

    // Column widths are remembered per table: a folder tab that becomes the Recycle Bin
    // (or a search) gets the wide Original location (Folder) column, not the Ext width.
    let mut table = TableBuilder::new(ui)
        .id_salt((id, search, trash, sizes))
        .striped(true)
        .resizable(true)
        .sense(crate::a11y::NO_TAB)
        .auto_shrink([false, true])
        .max_scroll_height(f32::INFINITY)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(120.0).clip(true))
        .column(
            Column::initial(if search || trash || sizes {
                180.0
            } else {
                56.0
            })
            .at_least(30.0)
            .clip(true),
        )
        .column(Column::initial(80.0).at_least(50.0))
        .column(Column::initial(124.0).at_least(60.0).clip(true));
    if sizes {
        table = table
            .column(Column::initial(70.0).at_least(40.0).clip(true))
            .column(Column::initial(70.0).at_least(40.0).clip(true));
    }
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
                    if sizes {
                        // Always biggest first.
                        let label = match key {
                            SortKey::Ext => "Share",
                            SortKey::Size => "Size ⏷",
                            _ => label,
                        };
                        ui.strong(label);
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
                    let name = match sort {
                        (k, true) if k == key => format!("{label}, sorted ascending"),
                        (k, false) if k == key => format!("{label}, sorted descending"),
                        _ => label.to_owned(),
                    };
                    let r = ui.add(egui::Button::new(text).frame(false));
                    if r.named(&name).clicked() {
                        sort_click = Some(key);
                    }
                });
            }
            if sizes {
                header.col(|ui| {
                    ui.strong("Files");
                });
                header.col(|ui| {
                    ui.strong("Of used").on_hover_text(
                        "Share of the space used on this drive (the drive's own numbers)",
                    );
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
            // Task 62: a hit found by the text inside its document is two lines tall.
            let heights: Vec<f32> = (vis.iter())
                .map(
                    |&i| match search && tab.snippets.contains_key(&tab.entries()[i].path) {
                        true => SNIPPET_ROW_H,
                        false => ROW_H,
                    },
                )
                .collect();
            let draw = |mut row: egui_extras::TableRow<'_, '_>| {
                let e = &tab.entries()[vis[row.index()]];
                let shown = tab.shown_name(e);
                let selected = tab.selected.contains(&e.name);
                // Secondary text on a selected row is in the text colour (contrast).
                let muted = if selected { text } else { muted };
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
                // Task 56: a folder's color and icon.
                let style = (e.kind == Kind::Dir)
                    .then(|| real.as_ref().and_then(|r| lib.style_of(r)))
                    .flatten();
                row.col(|ui| {
                    tint(ui);
                    let snippet = tab.snippets.get(&e.path).filter(|_| search);
                    let mut line = |ui: &mut egui::Ui| {
                        crate::folder_style::slot(ui, crate::icons::icon_for(e), style);
                        if e.encrypted {
                            ui.add(
                                egui::Image::new(crate::icons::lock())
                                    .fit_to_exact_size(egui::vec2(12.0, 12.0)),
                            )
                            .on_hover_text("Password-protected");
                        }
                        match &mut renaming {
                            Some((name, text)) if *name == e.name => {
                                let r = ui.add(
                                    egui::TextEdit::singleline(text).desired_width(f32::INFINITY),
                                );
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
                                    crate::pane::rename_focus(
                                        &r,
                                        text,
                                        e.kind == keel_vfs::Kind::Dir,
                                    );
                                }
                            }
                            _ if style.is_some_and(|s| s.color.is_some()) => {
                                // The name leaves room for the color dot after it.
                                ui.scope(|ui| {
                                    ui.set_max_width((ui.available_width() - 14.0).max(0.0));
                                    ui.add(egui::Label::new(cell(shown, color)).truncate());
                                });
                                crate::folder_style::dot(ui, style);
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
                    };
                    match snippet {
                        Some(sn) => {
                            ui.vertical(|ui| {
                                ui.horizontal(line);
                                crate::chip_row::snippet(ui, &sn.text, &sn.marks, muted);
                            });
                        }
                        None => line(ui),
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
                    } else if sizes {
                        let bytes = crate::sizes::bytes_of(lib, walked, e);
                        if let Some(share) = bytes.map(|b| fraction(b, folder_sum)) {
                            share_bar(ui, share, accent, muted);
                        }
                    } else {
                        ui.label(cell(&e.ext, Some(muted)));
                    }
                });
                row.col(|ui| {
                    tint(ui);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let text = crate::sizes::size_text(lib, walked, e);
                        ui.label(cell(&text, color));
                    });
                });
                row.col(|ui| {
                    tint(ui);
                    ui.label(cell(&date_text(e), Some(muted)));
                });
                if sizes {
                    let total = match crate::sizes::size_of(lib, walked, e) {
                        Some(crate::sizes::Size::Known(t)) => Some(t),
                        _ if e.kind != Kind::Dir => Some(keel_core::Total {
                            bytes: e.size,
                            files: 1,
                        }),
                        _ => None,
                    };
                    row.col(|ui| {
                        tint(ui);
                        if let Some(t) = total {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(cell(&t.files.to_string(), Some(muted)));
                            });
                        }
                    });
                    row.col(|ui| {
                        tint(ui);
                        if let (Some(t), Some(used)) = (total, used) {
                            let pct = 100.0 * fraction(t.bytes, used);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(cell(&format!("{pct:.1} %"), Some(muted)));
                            });
                        }
                    });
                }
                if copies_col {
                    row.col(|ui| {
                        tint(ui);
                        let badge = real
                            .as_ref()
                            .filter(|_| source.is_some() && e.kind != Kind::Dir)
                            .and_then(|r| lib.badge(r));
                        if let Some(b) = badge {
                            let color = if b.risk && !selected {
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
                        if ui.add(star).tip("Favorite (Ctrl+D)").clicked() {
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
            };
            if heights.iter().all(|h| *h == ROW_H) {
                body.rows(ROW_H, vis.len(), draw);
            } else {
                body.heterogeneous_rows(heights.into_iter(), draw);
            }
        });

    for (r, e) in clicks {
        crate::pane::a11y_item(ui, &r, tab, &e, cx.keys);
        handle_click(&r, tab, &e, out);
        drag_and_drop(&r, id.0, tab, &e, out);
    }
    if !rename_done {
        tab.renaming = renaming;
    }
    if let Some(key) = sort_click {
        tab.sort = (key, if tab.sort.0 == key { !tab.sort.1 } else { true });
    }
    // Task 46: the Sizes view walks a folder on a fixed disk outside the library once per
    // listing; elsewhere (network shares, removable drives, remote and cloud folders, where
    // a walk is slow or costs requests) it waits for the button above the table.
    if walk_wanted(tab, lib, walked) && crate::sizes::walks_on_its_own(&tab.dir) {
        out.push(Action::CalculateSizes { fresh: false });
    }
    empty_area(ui, tab, out);
}

/// Whether the Sizes view of `tab` lacks folder totals that a walk would find: outside the
/// library, listed, not walked since its listing, with a folder of unknown size.
fn walk_wanted(tab: &Tab, lib: &crate::library::LibraryUi, walked: &crate::sizes::Sizes) -> bool {
    let indexed = lib.is_open() && crate::library::source_of(&lib.sources, &tab.dir).is_some();
    tab.sizes_view
        && !indexed
        && !tab.loading
        && !walked.started(&tab.dir)
        && (tab.entries().iter())
            .any(|e| e.kind == Kind::Dir && !e.is_link && walked.get(e).is_none())
}

/// `part` of `whole`, 0..=1 (0 when the whole is 0).
fn fraction(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64).clamp(0.0, 1.0) as f32
    }
}

/// The Sizes view's bar: the share filled in the accent colour, and the percentage.
fn share_bar(ui: &mut egui::Ui, share: f32, fill: egui::Color32, text: egui::Color32) {
    let width = (ui.available_width() - 44.0).max(12.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 8.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, 2.0, text.gamma_multiply(0.2));
    let mut done = rect;
    done.set_width(rect.width() * share);
    ui.painter().rect_filled(done, 2.0, fill);
    ui.label(cell(&format!("{:.0} %", 100.0 * share), Some(text)));
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
    let r = ui.allocate_rect(rest, Sense::CLICK);
    crate::a11y::not_a_stop(&r);
    if r.clicked() || r.secondary_clicked() {
        tab.selected.clear();
    }
    r.context_menu(|ui| context_menu(ui, tab, None, out));
}
