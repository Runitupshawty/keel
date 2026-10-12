//! Details view: a virtual-row table (Name / Ext / Size / Modified; search results show
//! Name / Folder / Size / Modified). Task 46: the Sizes view is this table with a Share bar
//! in place of Ext, plus Files and Of used columns, biggest first. Task 66: the Finder-style
//! list: disclosure triangles that expand folders inline, columns chosen from the header's
//! right-click menu (Kind, Date Created, ...), relative dates, middle-truncated names and
//! Group by with sticky group headers.

use crate::a11y::Named;
use crate::keys::Action;
use crate::list_view::{self, ListColumn, GROUP_H};
use crate::pane::{context_menu, drag_and_drop, handle_click, ViewCx};
use crate::tab::{NodeState, SortKey, Tab};
use egui::{Align, Key, Layout, Sense};
use egui_extras::{Column, TableBuilder};
use keel_core::{SourceStatus, FAVORITES};
use keel_vfs::{Entry, Kind};

pub const ROW_H: f32 = 22.0;
/// A library search hit with a snippet of its text under the name (Task 62).
pub const SNIPPET_ROW_H: f32 = 40.0;
/// Task 66: indent per level of an expanded folder's rows, and the triangle's width.
pub const INDENT: f32 = 16.0;
const TRIANGLE_W: f32 = 14.0;

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

/// The table's columns, left to right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Col {
    Name,
    /// Ext; Folder in search results, Original location in the Recycle Bin, Share in Sizes.
    Second,
    Kind,
    Size,
    Modified,
    Created,
    Opened,
    /// Sizes view only.
    Files,
    OfUsed,
    Copies,
    Tags,
}

/// The columns shown: the Sizes view keeps its own set; elsewhere the chosen ones.
fn columns(
    (search, trash, sizes): (bool, bool, bool),
    list: &list_view::ListOptions,
    copies: bool,
    tags: bool,
) -> Vec<Col> {
    let mut cols = vec![Col::Name];
    if sizes {
        cols.extend([
            Col::Second,
            Col::Size,
            Col::Modified,
            Col::Files,
            Col::OfUsed,
        ]);
    } else {
        if search || trash || list.shows(ListColumn::Ext) {
            cols.push(Col::Second);
        }
        for (c, col) in [
            (ListColumn::Kind, Col::Kind),
            (ListColumn::Size, Col::Size),
            (ListColumn::Modified, Col::Modified),
            (ListColumn::Created, Col::Created),
            (ListColumn::Opened, Col::Opened),
        ] {
            if list.shows(c) {
                cols.push(col);
            }
        }
    }
    if copies {
        cols.push(Col::Copies);
    }
    if tags {
        cols.push(Col::Tags);
    }
    cols
}

/// One line of the table: a group header or an item (its visible position).
#[derive(Clone, Copy)]
enum Line {
    Group(usize),
    Item(usize),
}

/// The table's lines: with Group by, a header before each group's rows.
fn lines(tab: &Tab) -> Option<Vec<Line>> {
    let starts = tab.group_starts();
    if starts.is_empty() {
        return None;
    }
    let n = tab.visible_cached().len();
    let mut out = Vec::with_capacity(n + starts.len());
    for (g, &(start, _)) in starts.iter().enumerate() {
        let end = starts.get(g + 1).map_or(n, |s| s.0);
        out.push(Line::Group(g));
        out.extend((start..end).map(Line::Item));
    }
    Some(out)
}

/// The line showing visible position `pos` (its group's header for a group's first row,
/// so scrolling to it shows the header too).
fn line_of(tab: &Tab, pos: usize) -> usize {
    let starts = tab.group_starts();
    if starts.is_empty() {
        return pos;
    }
    let g = starts.partition_point(|s| s.0 <= pos).saturating_sub(1);
    if starts[g].0 == pos {
        pos + g
    } else {
        pos + g + 1
    }
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
    let mut toggles: Vec<(String, bool)> = Vec::new();
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
    let tags_col = *cx.tags_column && lib.is_open();
    // --- Task 33 ---: copies badge (library on).
    let copies_col = lib.is_open();
    // --- Task 66 ---
    let cols = columns((search, trash, sizes), cx.list, copies_col, tags_col);
    let relative = cx.list.relative_dates;
    let now = chrono::Local::now();
    let disclosure = tab.disclosure;
    let lines = lines(tab);
    let spacing_y = ui.spacing().item_spacing.y;
    let width_x = ui.max_rect().x_range();
    let panel = ui.visuals().panel_fill;
    let font = egui::TextStyle::Body.resolve(ui.style());
    // The library's opens, for the Date Last Opened column (read by `list_view::prepare`).
    let opened = match cols.contains(&Col::Opened) {
        true => list_view::with_opened(ui.ctx(), |o| o.by_path.clone()),
        false => Default::default(),
    };

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
    // (or a search) gets the wide Original location (Folder) column, not the Ext width,
    // and another set of columns starts from its own widths.
    let mut table = TableBuilder::new(ui)
        .id_salt((id, search, trash, sizes, &cols))
        .striped(true)
        .resizable(true)
        .sense(crate::a11y::NO_TAB)
        .auto_shrink([false, true])
        .max_scroll_height(f32::INFINITY)
        .cell_layout(Layout::left_to_right(Align::Center));
    for c in &cols {
        table = table.column(match c {
            Col::Name => Column::remainder().at_least(120.0).clip(true),
            Col::Second => Column::initial(if search || trash || sizes {
                180.0
            } else {
                56.0
            })
            .at_least(30.0)
            .clip(true),
            Col::Kind => Column::initial(130.0).at_least(50.0).clip(true),
            Col::Size => Column::initial(80.0).at_least(50.0),
            // Room for "Yesterday at 14:05".
            Col::Modified | Col::Created | Col::Opened => {
                Column::initial(140.0).at_least(60.0).clip(true)
            }
            Col::Files | Col::OfUsed => Column::initial(70.0).at_least(40.0).clip(true),
            Col::Copies => Column::initial(64.0).at_least(40.0).clip(true),
            Col::Tags => Column::initial(120.0).at_least(40.0).clip(true),
        });
    }
    if let Some(pos) = tab.scroll_to.take() {
        table = table.scroll_to_row(line_of(tab, pos), None);
    }
    // Task 62: a hit found by the text inside its document is two lines tall.
    let heights: Vec<f32> = {
        let vis = tab.visible_cached();
        let item_h =
            |pos: usize| match search && tab.snippets.contains_key(&tab.entries()[vis[pos]].path) {
                true => SNIPPET_ROW_H,
                false => ROW_H,
            };
        match &lines {
            Some(lines) => (lines.iter())
                .map(|l| match *l {
                    Line::Group(_) => GROUP_H,
                    Line::Item(pos) => item_h(pos),
                })
                .collect(),
            None => (0..vis.len()).map(item_h).collect(),
        }
    };
    let sort = tab.sort;
    let lib_open = lib.is_open();
    let list = &mut *cx.list;
    let tags_setting = &mut *cx.tags_column;
    let scrolled = table
        .header(ROW_H, |mut header| {
            for &c in &cols {
                let (label, key) = match c {
                    Col::Name => ("Name", Some(SortKey::Name)),
                    Col::Second if search => ("Folder", None),
                    Col::Second if sizes => ("Share", None),
                    Col::Second if trash => ("Original location", None),
                    Col::Second => ("Ext", Some(SortKey::Ext)),
                    Col::Kind => ("Kind", None),
                    // Always biggest first.
                    Col::Size if sizes => ("Size ⏷", None),
                    Col::Size => ("Size", Some(SortKey::Size)),
                    Col::Modified if trash => ("Deleted on", Some(SortKey::Modified)),
                    Col::Modified if sizes => ("Modified", None),
                    Col::Modified => ("Modified", Some(SortKey::Modified)),
                    Col::Created => ("Created", None),
                    Col::Opened => ("Last opened", None),
                    Col::Files => ("Files", None),
                    Col::OfUsed => ("Of used", None),
                    Col::Copies => ("Copies", None),
                    Col::Tags => ("Tags", None),
                };
                header.col(|ui| {
                    let Some(key) = key.filter(|_| !sizes) else {
                        let r = ui.add(
                            egui::Label::new(egui::RichText::new(label).strong()).sense(Sense::CLICK),
                        );
                        let r = match c {
                            Col::OfUsed => r.on_hover_text(
                                "Share of the space used on this drive (the drive's own numbers)",
                            ),
                            Col::Copies => r.on_hover_text(
                                "Copies of the content and the failure domains they span \
                                 (hashed files)",
                            ),
                            _ => r,
                        };
                        if !sizes {
                            header_chooser(ui, &r, list, lib_open.then_some(&mut *tags_setting));
                        }
                        return;
                    };
                    let arrow = match sort {
                        (k, true) if k == key => " ⏶",
                        (k, false) if k == key => " ⏷",
                        _ => "",
                    };
                    let text = egui::RichText::new(format!("{label}{arrow}")).strong();
                    let name = match sort {
                        (k, true) if k == key => format!("{label}, sorted ascending"),
                        (k, false) if k == key => format!("{label}, sorted descending"),
                        _ => label.to_owned(),
                    };
                    let r = ui.add(egui::Button::new(text).frame(false));
                    let r = r.named(&name);
                    if r.clicked() {
                        sort_click = Some(key);
                    }
                    if !sizes {
                        header_chooser(ui, &r, list, lib_open.then_some(&mut *tags_setting));
                    }
                });
            }
        })
        .body(|body| {
            let vis = tab.visible_cached();
            let starts = tab.group_starts();
            let draw = |mut row: egui_extras::TableRow<'_, '_>| {
                let pos = match lines.as_ref().map(|l| l[row.index()]) {
                    Some(Line::Group(g)) => {
                        let group = starts[g].1;
                        row.col(|ui| {
                            group_header(ui, tab.group_label(group), tab.group_count(g), muted);
                        });
                        for _ in 1..cols.len() {
                            row.col(|_| {});
                        }
                        return;
                    }
                    Some(Line::Item(pos)) => pos,
                    None => row.index(),
                };
                let e = &tab.entries()[vis[pos]];
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
                for &c in &cols {
                    match c {
                        Col::Name => {
                            row.col(|ui| {
                                tint(ui);
                                let snippet = tab.snippets.get(&e.path).filter(|_| search);
                                let mut line = |ui: &mut egui::Ui| {
                                    // --- Task 66 ---: indent and disclosure triangle.
                                    if disclosure {
                                        ui.add_space(tab.depth(e) as f32 * INDENT);
                                        if e.kind == Kind::Dir {
                                            let open = tab.is_expanded(&e.name);
                                            let r = triangle(ui, open, muted).named(&format!(
                                                "{} {shown}",
                                                if open { "Collapse" } else { "Expand" }
                                            ));
                                            let r = r.on_hover_text(
                                                "Alt+click: every folder inside too",
                                            );
                                            if r.clicked() {
                                                let alt = ui.input(|i| i.modifiers.alt);
                                                toggles.push((e.name.clone(), alt));
                                            }
                                        } else {
                                            ui.add_space(TRIANGLE_W);
                                        }
                                    }
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
                                                egui::TextEdit::singleline(text)
                                                    .desired_width(f32::INFINITY),
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
                                                let w = (ui.available_width() - 14.0).max(0.0);
                                                ui.set_max_width(w);
                                                name_label(ui, shown, color, &font);
                                            });
                                            crate::folder_style::dot(ui, style);
                                        }
                                        _ => name_label(ui, shown, color, &font),
                                    }
                                    match tab.node_state(&e.name) {
                                        Some(NodeState::Wanted | NodeState::Loading) => {
                                            ui.label(cell("…", Some(muted)))
                                                .on_hover_text("Listing this folder");
                                        }
                                        Some(NodeState::Failed(why)) => {
                                            let warn = ui.visuals().warn_fg_color;
                                            ui.label(cell("⚠", Some(warn))).on_hover_text(why);
                                        }
                                        _ => {}
                                    }
                                    if let Some(s) = offline {
                                        let SourceStatus::Offline { last_seen, reason } = s.status
                                        else {
                                            unreachable!("filtered")
                                        };
                                        ui.label(
                                            egui::RichText::new("offline").small().color(muted),
                                        )
                                        .on_hover_text(
                                            format!(
                                                "{} is {}: listed from the library index",
                                                s.label,
                                                crate::library::offline_text(last_seen, reason)
                                            ),
                                        );
                                    }
                                };
                                match snippet {
                                    Some(sn) => {
                                        ui.vertical(|ui| {
                                            ui.horizontal(line);
                                            crate::chip_row::snippet(
                                                ui, &sn.text, &sn.marks, muted,
                                            );
                                        });
                                    }
                                    None => line(ui),
                                }
                            });
                        }
                        Col::Second => {
                            row.col(|ui| {
                                tint(ui);
                                if search {
                                    let mut folder =
                                        e.path.parent().map(|d| d.display()).unwrap_or_default();
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
                        }
                        Col::Kind => {
                            row.col(|ui| {
                                tint(ui);
                                let kind = list_view::kind_text(e);
                                ui.add(egui::Label::new(cell(&kind, Some(muted))).truncate());
                            });
                        }
                        Col::Size => {
                            row.col(|ui| {
                                tint(ui);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let text = crate::sizes::size_text(lib, walked, e);
                                    ui.label(cell(&text, color));
                                });
                            });
                        }
                        Col::Modified => {
                            row.col(|ui| {
                                tint(ui);
                                let d = list_view::date_label(e.modified, relative, now);
                                ui.label(cell(&d, Some(muted)));
                            });
                        }
                        Col::Created => {
                            row.col(|ui| {
                                tint(ui);
                                let t = tab.meta.created.get(&e.name).copied();
                                let d = list_view::date_label(t, relative, now);
                                ui.label(cell(&d, Some(muted)));
                            });
                        }
                        Col::Opened => {
                            row.col(|ui| {
                                tint(ui);
                                let d = list_view::date_label(
                                    list_view::opened_of(&opened, lib, e),
                                    relative,
                                    now,
                                );
                                ui.label(cell(&d, Some(muted)));
                            });
                        }
                        Col::Files | Col::OfUsed => {
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
                                let text = match (c, total, used) {
                                    (Col::Files, Some(t), _) => t.files.to_string(),
                                    (Col::OfUsed, Some(t), Some(used)) => {
                                        format!("{:.1} %", 100.0 * fraction(t.bytes, used))
                                    }
                                    _ => return,
                                };
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(cell(&text, Some(muted)));
                                });
                            });
                        }
                        Col::Copies => {
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
                        Col::Tags => {
                            row.col(|ui| {
                                tint(ui);
                                let Some(real) = real.as_ref().filter(|_| source.is_some()) else {
                                    return;
                                };
                                let on = lib.tagged.get(real).map_or(&[][..], |t| t.as_slice());
                                let fav = on.contains(&FAVORITES);
                                let star =
                                    egui::Button::new(if fav { "★" } else { "☆" }).frame(false);
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
                    }
                }
                let r = row.response();
                r.context_menu(|ui| context_menu(ui, tab, Some(e), out));
                clicks.push((r, e.clone()));
            };
            if heights.iter().all(|h| *h == ROW_H) {
                body.rows(ROW_H, heights.len(), draw);
            } else {
                body.heterogeneous_rows(heights.iter().copied(), draw);
            }
        });

    // Task 66: the group the top row belongs to keeps its header at the top while its rows
    // scroll under it; the next group's header pushes it up.
    if let Some(lines) = &lines {
        sticky_header(
            ui,
            tab,
            (lines, &heights, spacing_y),
            (scrolled.state.offset.y, scrolled.inner_rect),
            width_x,
            (panel, muted),
        );
    }

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
    for (name, all) in toggles {
        tab.toggle_expanded(&name, all);
    }
    // Task 46: the Sizes view walks a folder on a fixed disk outside the library once per
    // listing; elsewhere (network shares, removable drives, remote and cloud folders, where
    // a walk is slow or costs requests) it waits for the button above the table.
    if walk_wanted(tab, lib, walked) && crate::sizes::walks_on_its_own(&tab.dir) {
        out.push(Action::CalculateSizes { fresh: false });
    }
    empty_area(ui, tab, out);
}

/// A name middle-truncated to the cell ("My Long Fi…me.pdf"), the full name on hover.
fn name_label(ui: &mut egui::Ui, shown: &str, color: Option<egui::Color32>, font: &egui::FontId) {
    // A little slack: glyph widths ignore kerning.
    let max = (ui.available_width() - 2.0).max(0.0);
    let fitted = list_view::fit_middle(ui, shown, font, max);
    let r = ui.add(egui::Label::new(cell(&fitted, color)).truncate());
    if fitted != shown {
        r.on_hover_text(shown);
    }
}

/// The disclosure triangle of a folder row: pointing right (collapsed) or down (expanded).
fn triangle(ui: &mut egui::Ui, open: bool, color: egui::Color32) -> egui::Response {
    // Pointer-only: the keyboard expands with Right and Left on the cursor row, so the
    // triangle is no Tab stop and never takes the list's keyboard focus.
    let (rect, r) = ui.allocate_exact_size(egui::vec2(TRIANGLE_W, ROW_H), Sense::CLICK);
    let color = if r.hovered() {
        ui.visuals().strong_text_color()
    } else {
        color
    };
    let c = rect.center();
    let s = 3.5;
    let points = if open {
        vec![
            c + egui::vec2(-s, -s * 0.6),
            c + egui::vec2(s, -s * 0.6),
            c + egui::vec2(0.0, s * 0.7),
        ]
    } else {
        vec![
            c + egui::vec2(-s * 0.6, -s),
            c + egui::vec2(-s * 0.6, s),
            c + egui::vec2(s * 0.7, 0.0),
        ]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        color,
        egui::Stroke::NONE,
    ));
    r
}

/// Right-click on a column header opens the column chooser below it. A popup, not
/// `Response::context_menu`: that one adds a second focusable interaction to the header
/// button, which breaks Shift+Tab through the headers.
fn header_chooser(
    ui: &egui::Ui,
    r: &egui::Response,
    list: &mut crate::list_view::ListOptions,
    tags: Option<&mut bool>,
) {
    let id = r.id.with("keel-header-chooser");
    if r.secondary_clicked() {
        ui.memory_mut(|m| m.open_popup(id));
    }
    egui::popup_below_widget(
        ui,
        id,
        r,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(190.0);
            crate::list_view::header_menu(ui, list, tags)
        },
    );
}

/// A group header's text: the group and how many items it has.
pub fn group_header(ui: &mut egui::Ui, label: &str, count: usize, muted: egui::Color32) {
    ui.label(egui::RichText::new(label).strong());
    ui.add_space(6.0);
    let items = if count == 1 { "item" } else { "items" };
    ui.label(
        egui::RichText::new(format!("{count} {items}"))
            .small()
            .color(muted),
    );
}

/// Paints the header of the group at the top of the scrolled table over the first rows.
fn sticky_header(
    ui: &mut egui::Ui,
    tab: &Tab,
    (lines, heights, spacing): (&[Line], &[f32], f32),
    (offset, view): (f32, egui::Rect),
    x: egui::Rangef,
    (fill, muted): (egui::Color32, egui::Color32),
) {
    if offset <= 0.0 {
        return;
    }
    // The last header at or above the top, and the next one.
    let (mut y, mut current, mut next) = (0.0f32, None, None);
    for (l, h) in lines.iter().zip(heights) {
        if let Line::Group(g) = *l {
            if y <= offset {
                current = Some(g);
            } else {
                next = Some(y);
                break;
            }
        }
        if y > offset + view.height() {
            break;
        }
        y += h + spacing;
    }
    let Some(g) = current else {
        return;
    };
    let mut top = view.top();
    if let Some(next) = next {
        top = top.min(view.top() + (next - offset) - GROUP_H);
    }
    let rect = egui::Rect::from_x_y_ranges(x, top..=top + GROUP_H);
    let group = tab.group_starts()[g].1;
    let painter = ui.painter().with_clip_rect(view.intersect(ui.clip_rect()));
    painter.rect_filled(rect, 0.0, fill);
    painter.hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0_f32, muted.gamma_multiply(0.4)),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(6.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.set_clip_rect(view.intersect(ui.clip_rect()));
    group_header(
        &mut child,
        tab.group_label(group),
        tab.group_count(g),
        muted,
    );
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
