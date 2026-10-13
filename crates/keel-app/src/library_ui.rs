//! Drawing the library (Task 29): the sidebar section, the Overview tab, the add-source
//! wizard, the validate/preview dialog, the tag picker, the duplicate finder, Settings →
//! Library and the library rows of the jobs panel. State and workers are in `library.rs`.

use crate::a11y::Named;
use crate::keys::Action;
use crate::library::{
    count, protection_lines, reclaimable, source_count_rows, source_rows, state_text, volume_rows,
    Dot, Hashing, LibCmd, LibraryUi, FAVORITES_QUERY,
};
use crate::pane::ViewCx;
use crate::state::AppState;
use egui::{Color32, RichText};
use humansize::{format_size, DECIMAL};
use keel_core::{JobStatus, SourceDef, SourceKind, Tag, VolumeState};
use keel_vfs::VPath;

fn lib(cmd: LibCmd) -> Action {
    Action::Library(cmd)
}

/// Folder color's palette (Task 56). A new tag takes one of Finder's eight colors instead
/// (`keel_core::FinderColor`, Task 67).
pub const TAG_COLORS: [(&str, &str); 9] = [
    ("Red", "#e5484d"),
    ("Orange", "#f76b15"),
    ("Yellow", "#ffc53d"),
    ("Green", "#30a46c"),
    ("Teal", "#12a594"),
    ("Blue", "#3b82f6"),
    ("Purple", "#8e4ec6"),
    ("Pink", "#d6409f"),
    ("Gray", "#8b8d98"),
];

/// `#rrggbb` (anything else: the accent blue).
pub fn color_of(s: Option<&str>) -> Color32 {
    let hex = s.and_then(|s| s.strip_prefix('#')).filter(|h| h.len() == 6);
    let parsed = hex.and_then(|h| {
        let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some(Color32::from_rgb(b(0)?, b(2)?, b(4)?))
    });
    parsed.unwrap_or(Color32::from_rgb(0x3b, 0x82, 0xf6))
}

fn dot_color(ui: &egui::Ui, dot: Dot) -> Color32 {
    match dot {
        Dot::Online => crate::design::STATUS_OK,
        Dot::Indexing => crate::design::STATUS_BUSY,
        Dot::Offline => ui.visuals().weak_text_color(),
        Dot::Error => ui.visuals().error_fg_color,
    }
}

/// A tag chip: its name on its color.
pub fn chip(ui: &mut egui::Ui, tag: &Tag) -> egui::Response {
    chip_text(ui, tag, &tag.name)
}

/// A chip in `tag`'s color saying `text`.
fn chip_text(ui: &mut egui::Ui, tag: &Tag, text: &str) -> egui::Response {
    let fill = color_of(tag.color.as_deref());
    let luma = 0.299 * fill.r() as f32 + 0.587 * fill.g() as f32 + 0.114 * fill.b() as f32;
    let text_color = if luma > 150.0 {
        Color32::BLACK
    } else {
        Color32::WHITE
    };
    ui.add(
        egui::Button::new(RichText::new(text).small().color(text_color))
            .fill(fill)
            .corner_radius(crate::design::tokens(ui.ctx()).radius.chip)
            .min_size(egui::vec2(0.0, 14.0)),
    )
}

/// Task 58: tag assign mode's bar above pane `idx`'s file list: which number toggles which
/// tag, with how many of the selected items carry it (the second page while Shift is
/// held). A chip click toggles too; Leave ends the mode.
pub fn assign_bar(
    ui: &mut egui::Ui,
    idx: usize,
    tab: &crate::tab::Tab,
    l: &LibraryUi,
    out: &mut Vec<Action>,
) {
    if l.assign.as_ref().is_none_or(|a| a.pane != idx) {
        return;
    }
    let targets: Vec<VPath> = (tab.targets().iter())
        .filter_map(|e| crate::library::real_of(&l.sources, &e.path))
        .collect();
    let carry = |tag: &Tag| {
        (targets.iter())
            .filter(|t| l.tagged.get(t).is_some_and(|tags| tags.contains(&tag.id)))
            .count()
    };
    let counted: Vec<(&str, usize)> = l.tags.iter().map(|t| (t.name.as_str(), carry(t))).collect();
    let shift = ui.input(|i| i.modifiers.shift);
    let (labels, hint) = crate::library::assign_labels(&counted, shift);
    let page = match shift && l.tags.len() > crate::library::ASSIGN_PAGE {
        true => crate::library::ASSIGN_PAGE,
        false => 0,
    };
    ui.horizontal_wrapped(|ui| {
        ui.strong(
            crate::i18n::tr("library_ui.assign_tags_to")
                .replace("{items}", &crate::jobs::items(targets.len())),
        );
        for (i, label) in labels.iter().enumerate() {
            let tag = &l.tags[page + i];
            let r = chip_text(ui, tag, label)
                .on_hover_text(crate::i18n::tr("library_ui.toggle").replace("{name}", &tag.name));
            if r.clicked() {
                out.push(lib(LibCmd::AssignTag(page + i)));
            }
        }
        ui.weak(hint);
        if ui
            .small_button(crate::i18n::tr("library_ui.leave"))
            .on_hover_text(crate::i18n::tr("library_ui.leave_tip"))
            .clicked()
        {
            out.push(lib(LibCmd::AssignMode));
        }
    });
}

/// Tags shown in the sidebar before "All Tags…".
pub const SIDEBAR_TAGS: usize = 8;

/// The sidebar's Library section (Phase 14): Overview, ★ Favorites, the sources and the
/// saved views (its header is drawn by `sidebar`).
pub fn sidebar(ui: &mut egui::Ui, l: &LibraryUi, current: &VPath, out: &mut Vec<Action>) {
    use crate::sidebar::row;
    use crate::sidebar_glyphs::Glyph;
    use egui::{Id, Sense};
    if !l.is_open() {
        match (&l.error, l.opening) {
            (_, true) => {
                ui.weak(crate::i18n::tr("library_ui.opening"));
            }
            (Some(e), _) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    crate::i18n::tr("library_ui.not_available"),
                )
                .on_hover_text(e);
            }
            _ => {
                ui.weak(crate::i18n::tr("library_ui.off_settings"));
            }
        }
        return;
    }
    let entry = |ui: &mut egui::Ui, glyph, text: &str, cmd: LibCmd, out: &mut Vec<Action>| {
        let id = Id::new(("sidebar-lib", text));
        if row(ui, id, glyph, text, false, Sense::click(), |_| ()).clicked() {
            out.push(lib(cmd));
        }
    };
    entry(
        ui,
        Glyph::Overview,
        crate::i18n::tr("tab.overview"),
        LibCmd::Overview,
        out,
    );
    let favorites = LibCmd::Query(FAVORITES_QUERY.into());
    entry(
        ui,
        Glyph::Star,
        crate::i18n::tr("library_ui.favorites"),
        favorites,
        out,
    );
    for s in source_rows(&l.sources) {
        let on = current.scheme == keel_vfs::library::SCHEME && current.authority == s.id.0;
        let style = l.style_of(&keel_vfs::library::path(&s.id.0, ""));
        let badge = crate::mount_ui::badge_for(ui.ctx(), &s.id.0);
        let glyph = Glyph::Dot(dot_color(ui, s.dot));
        let id = Id::new(("sidebar-source", &s.id.0));
        let r = row(ui, id, glyph, &s.label, on, Sense::click(), |ui| {
            if s.dot == Dot::Indexing {
                ui.spinner();
            }
            if let Some(b) = &badge {
                ui.add(egui::Label::new(RichText::new(b).small().weak()));
            }
            // Task 56: a styled source root shows its folder icon.
            if let Some(style) = style {
                crate::folder_style::slot(ui, crate::icons::folder(), Some(style), true);
            }
        })
        .on_hover_text(&s.detail);
        if r.clicked() {
            out.push(lib(LibCmd::OpenSource(s.id.clone())));
        } else if r.middle_clicked() {
            out.push(Action::NewTabAt(keel_vfs::library::path(&s.id.0, "")));
        }
        r.context_menu(|ui| {
            let pause = if l.hash_paused {
                (
                    crate::i18n::tr("library_ui.resume_hashing"),
                    LibCmd::PauseHashing(false),
                )
            } else {
                (
                    crate::i18n::tr("library_ui.pause_hashing"),
                    LibCmd::PauseHashing(true),
                )
            };
            let adopt = (s.adopt).then(|| {
                (
                    crate::i18n::tr("library_ui.adopt_new_root"),
                    LibCmd::AdoptRoot(s.id.clone()),
                )
            });
            for (text, cmd) in adopt.into_iter().chain([
                (
                    crate::i18n::tr("library_ui.index_now"),
                    LibCmd::IndexNow(s.id.clone()),
                ),
                pause,
                (
                    crate::i18n::tr("library_ui.remove_source"),
                    LibCmd::RemoveSource(s.id.clone()),
                ),
            ]) {
                if ui.button(text).clicked() {
                    out.push(lib(cmd));
                    ui.close_menu();
                }
            }
            if ui
                .button(crate::i18n::tr("library_ui.freeze_now"))
                .clicked()
            {
                out.push(lib(LibCmd::Freeze(s.id.clone())));
                ui.close_menu();
            }
            match l.freezes.get(&s.id) {
                None => out.push(lib(LibCmd::LoadFreezes(s.id.clone()))),
                Some(list) if !list.is_empty() => {
                    ui.menu_button(crate::i18n::tr("library_ui.freezes"), |ui| {
                        for f in list {
                            let at = crate::library::when(Some(f.at));
                            // The date opens the freeze; Delete… asks first.
                            ui.horizontal(|ui| {
                                if ui.button(at).clicked() {
                                    let auth = keel_vfs::library::frozen(&s.id.0, f.id);
                                    out.push(Action::Navigate(keel_vfs::library::path(&auth, "")));
                                    ui.close_menu();
                                }
                                if ui
                                    .small_button(crate::i18n::tr("library_ui.delete"))
                                    .clicked()
                                {
                                    out.push(lib(LibCmd::DeleteFreeze(s.id.clone(), f.id)));
                                    ui.close_menu();
                                }
                            });
                        }
                    });
                }
                Some(_) => {}
            }
            crate::mount_ui::source_menu(ui, &s.id.0, out);
        });
    }
    // Saved views are listed with the bookmarks (`bookmarks::sidebar`).
    entry(
        ui,
        Glyph::Plus,
        crate::i18n::tr("library_ui.add_source"),
        LibCmd::AddSource,
        out,
    );
}

/// The sidebar's Tags section (Phase 14): the first tags with their color dots; "All
/// Tags…" lists every one. A click lists everything with that tag.
pub fn sidebar_tags(ui: &mut egui::Ui, l: &LibraryUi, out: &mut Vec<Action>) {
    use crate::sidebar::row;
    use crate::sidebar_glyphs::Glyph;
    use egui::{Id, Sense};
    let query = |tag: &Tag| lib(LibCmd::Query(format!("tag:\"{}\"", tag.name)));
    for tag in l.tags.iter().take(SIDEBAR_TAGS) {
        let glyph = Glyph::Dot(color_of(tag.color.as_deref()));
        let id = Id::new(("sidebar-tag", &tag.name));
        if row(ui, id, glyph, &tag.name, false, Sense::click(), |_| ()).clicked() {
            out.push(query(tag));
        }
    }
    crate::sidebar::menu_row(ui, crate::i18n::tr("library_ui.all_tags"), |ui| {
        egui::ScrollArea::vertical()
            .max_height(320.0)
            .show(ui, |ui| {
                for tag in &l.tags {
                    if chip(ui, tag).clicked() {
                        out.push(query(tag));
                        ui.close_menu();
                    }
                }
            });
    });
}

/// The sidebar's Collections section: each collection by name (a click opens it in its own
/// order); right-click renames or deletes it.
pub fn sidebar_collections(ui: &mut egui::Ui, l: &LibraryUi, out: &mut Vec<Action>) {
    use crate::sidebar::row;
    use crate::sidebar_glyphs::Glyph;
    use egui::{Id, Sense};
    for c in &l.collections {
        let id = Id::new(("sidebar-collection", c.id));
        let r = row(
            ui,
            id,
            Glyph::Collection,
            &c.name,
            false,
            Sense::click(),
            |_| (),
        )
        .on_hover_text(crate::jobs::items(c.items));
        if r.clicked() {
            out.push(lib(LibCmd::Query(crate::library::collection_query(c))));
        }
        r.context_menu(|ui| {
            for (text, cmd) in [
                (
                    crate::i18n::tr("library_ui.rename"),
                    LibCmd::RenameCollection(c.id),
                ),
                (
                    crate::i18n::tr("library_ui.delete"),
                    LibCmd::DeleteCollection(c.id),
                ),
            ] {
                if ui.button(text).clicked() {
                    out.push(lib(cmd));
                    ui.close_menu();
                }
            }
        });
    }
}

const CARD_W: f32 = 240.0;

/// A dashboard card: a fixed-width framed column.
fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(CARD_W);
            ui.set_min_height(90.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).small().strong());
                ui.add_space(4.0);
                body(ui);
            });
        });
}

type Card<'a> = (&'a str, Box<dyn FnOnce(&mut egui::Ui) + 'a>);

/// Cards in rows of as many as fit.
fn cards(ui: &mut egui::Ui, list: Vec<Card<'_>>) {
    let per_row = ((ui.available_width() / (CARD_W + 32.0)) as usize).max(1);
    let mut list = list.into_iter().peekable();
    while list.peek().is_some() {
        ui.horizontal_top(|ui| {
            for (title, body) in list.by_ref().take(per_row) {
                card(ui, title, body);
            }
        });
    }
}

/// The Overview tab: counts, storage, sources, jobs and duplicates.
/// The Overview's library sync line.
pub fn synced_text(devices: usize) -> String {
    match devices {
        0 => crate::i18n::tr("library_ui.synced_none").into(),
        1 => crate::i18n::tr("library_ui.synced_one").into(),
        n => crate::i18n::tr("library_ui.synced_n").replace("{n}", &n.to_string()),
    }
}

pub fn overview(ui: &mut egui::Ui, cx: &mut ViewCx, out: &mut Vec<Action>) {
    let l = cx.library;
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(4.0);
        ui.heading(crate::i18n::tr("library_ui.overview_title"));
        if !l.is_open() {
            ui.weak(if l.opening {
                crate::i18n::tr("library_ui.opening_long")
            } else {
                crate::i18n::tr("library_ui.off_turn_on")
            });
            return;
        }
        match l.remote() {
            Some(r) if l.lost => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    crate::i18n::tr("library_ui.daemon_stopped_readonly")
                        .replace("{pid}", &r.version.pid.to_string()),
                );
            }
            Some(r) => {
                ui.weak(
                    crate::i18n::tr("library_ui.daemon_connected")
                        .replace("{pid}", &r.version.pid.to_string())
                        .replace("{library}", &r.version.library),
                );
            }
            None => {
                if let Some(note) = &l.note {
                    ui.weak(crate::i18n::tr("library_ui.open_here_note").replace("{note}", note));
                }
            }
        }
        ui.weak(synced_text(l.synced_with));
        let st = &l.stats;
        // The cards' closures share one action list.
        let acts = std::cell::RefCell::new(Vec::new());
        let push = |a: Action| acts.borrow_mut().push(a);
        cards(
            ui,
            vec![
                (
                    crate::i18n::tr("library_ui.card_files"),
                    Box::new(|ui: &mut egui::Ui| {
                        ui.heading(count(st.files));
                        ui.weak(
                            crate::i18n::tr("library_ui.records_and_size")
                                .replace("{records}", &count(st.records))
                                .replace("{size}", &format_size(st.bytes, DECIMAL)),
                        );
                    }),
                ),
                (
                    crate::i18n::tr("library_ui.card_unique"),
                    Box::new(|ui: &mut egui::Ui| {
                        ui.heading(count(st.unique_content));
                        ui.weak(crate::i18n::tr("library_ui.unique_hint"));
                    }),
                ),
                (
                    crate::i18n::tr("library_ui.card_duplicates"),
                    Box::new(|ui: &mut egui::Ui| {
                        match l.dup_summary {
                            Some((0, _)) => {
                                ui.label(crate::i18n::tr("library_ui.none_found"));
                            }
                            Some((n, bytes)) => {
                                ui.label(
                                    crate::i18n::tr("library_ui.groups_reclaimable")
                                        .replace(
                                            "{groups}",
                                            &crate::library::counted(n as u64, "group"),
                                        )
                                        .replace("{size}", &format_size(bytes, DECIMAL)),
                                );
                            }
                            None => {
                                ui.weak(crate::i18n::tr("library_ui.counting"));
                            }
                        }
                        if ui
                            .button(crate::i18n::tr("library_ui.open_dup_finder"))
                            .clicked()
                        {
                            push(lib(LibCmd::Duplicates));
                        }
                    }),
                ),
                (
                    crate::i18n::tr("library_ui.card_sources"),
                    Box::new(|ui: &mut egui::Ui| {
                        ui.label(
                            crate::i18n::tr("library_ui.sources_offline")
                                .replace(
                                    "{sources}",
                                    &crate::library::counted(st.sources as u64, "source"),
                                )
                                .replace("{n}", &st.offline_sources.to_string()),
                        );
                        for s in source_rows(&l.sources) {
                            ui.horizontal(|ui| {
                                let (rect, _) = ui
                                    .allocate_exact_size([12.0, 12.0].into(), egui::Sense::hover());
                                ui.painter().circle_filled(
                                    rect.center(),
                                    4.0,
                                    dot_color(ui, s.dot),
                                );
                                if ui.link(&s.label).clicked() {
                                    push(lib(LibCmd::OpenSource(s.id.clone())));
                                }
                                ui.add(
                                    egui::Label::new(RichText::new(&s.detail).small().weak())
                                        .truncate(),
                                );
                            });
                        }
                        if ui
                            .small_button(crate::i18n::tr("library_ui.add_source"))
                            .clicked()
                        {
                            push(lib(LibCmd::AddSource));
                        }
                    }),
                ),
                // --- Task 33 ---
                (
                    crate::i18n::tr("inspector.protection"),
                    Box::new(|ui: &mut egui::Ui| {
                        let Some(p) = &l.protection else {
                            ui.weak(crate::i18n::tr("library_ui.counting"));
                            return;
                        };
                        for line in protection_lines(p) {
                            let text = if line.warn {
                                RichText::new(line.text).color(ui.visuals().warn_fg_color)
                            } else {
                                RichText::new(line.text)
                            };
                            ui.label(text).on_hover_text(line.how);
                        }
                        if ui
                            .small_button(crate::i18n::tr("library_ui.check_integrity"))
                            .on_hover_text(crate::i18n::tr("library_ui.check_integrity_tip"))
                            .clicked()
                        {
                            push(lib(LibCmd::CheckIntegrity));
                        }
                    }),
                ),
            ],
        );
        out.extend(acts.into_inner());
        ui.add_space(8.0);
        ui.label(RichText::new(crate::i18n::tr("library_ui.storage")).strong());
        for (name, label, free, total) in cx.drives {
            if *total == 0 {
                continue;
            }
            let used = total.saturating_sub(*free);
            ui.horizontal(|ui| {
                let text = if label.is_empty() {
                    name.clone()
                } else {
                    format!("{label} ({name})")
                };
                ui.add_sized([160.0, 18.0], egui::Label::new(text).truncate());
                ui.add(
                    egui::ProgressBar::new(used as f32 / *total as f32)
                        .desired_width(240.0)
                        .text(
                            crate::i18n::tr("library_ui.used_of")
                                .replace("{used}", &format_size(used, DECIMAL))
                                .replace("{total}", &format_size(*total, DECIMAL)),
                        ),
                );
            });
        }
        ui.add_space(8.0);
        source_table(ui, l, out);
        // --- Task 33 ---
        ui.add_space(8.0);
        volume_table(ui, l, out);
        ui.add_space(8.0);
        ui.label(RichText::new(crate::i18n::tr("library_ui.running_jobs")).strong());
        let running: Vec<_> = l.jobs.iter().filter(|(_, j)| j.active()).collect();
        if running.is_empty() {
            ui.weak(crate::i18n::tr("toolbar.none"));
        }
        for (_, job) in running {
            ui.horizontal(|ui| {
                ui.add_sized([160.0, 18.0], egui::Label::new(job.title()).truncate());
                ui.add(
                    egui::ProgressBar::new(job.progress)
                        .desired_width(240.0)
                        .show_percentage(),
                );
            });
        }
    });
}

/// Each source's counts: files, folders, size, how much is hashed, the last walk.
fn source_table(ui: &mut egui::Ui, l: &LibraryUi, out: &mut Vec<Action>) {
    let rows = source_count_rows(&l.stats.per_source);
    if rows.is_empty() {
        return;
    }
    ui.label(RichText::new(crate::i18n::tr("library_ui.per_source")).strong())
        .on_hover_text(crate::i18n::tr("library_ui.per_source_tip"));
    egui::Grid::new("keel-source-counts")
        .num_columns(7)
        .striped(true)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            for h in [
                crate::i18n::tr("inspector.source"),
                crate::i18n::tr("inspector.files"),
                crate::i18n::tr("inspector.folders"),
                crate::i18n::tr("toolbar.size"),
                crate::i18n::tr("library_ui.col_hashed"),
                crate::i18n::tr("library_ui.col_last_walk"),
            ] {
                ui.label(RichText::new(h).small().strong());
            }
            ui.label(
                RichText::new(crate::i18n::tr("library_ui.contents"))
                    .small()
                    .strong(),
            )
            .on_hover_text(crate::i18n::tr("library_ui.contents_tip"));
            ui.end_row();
            for r in rows {
                let link = ui.link(&r.label);
                let link = if r.offline {
                    link.on_hover_text(crate::i18n::tr("library_ui.offline_counts"))
                } else {
                    link
                };
                if link.clicked() {
                    out.push(lib(LibCmd::OpenSource(r.id.clone())));
                }
                ui.label(&r.files);
                ui.label(&r.folders);
                ui.label(&r.size);
                ui.label(&r.hashed)
                    .on_hover_text(crate::i18n::tr("library_ui.hashed_tip"));
                let walk = RichText::new(if r.offline {
                    crate::i18n::tr("library_ui.offline_suffix").replace("{walk}", &r.last_walk)
                } else {
                    r.last_walk.clone()
                });
                ui.label(if r.offline {
                    walk.color(ui.visuals().warn_fg_color)
                } else {
                    walk
                });
                let mut on = r.contents;
                let box_ = ui
                    .checkbox(&mut on, &r.documents)
                    .on_hover_text(crate::i18n::tr("library_ui.documents_tip"));
                let name =
                    crate::i18n::tr("library_ui.index_contents_of").replace("{name}", &r.label);
                box_.widget_info(|| {
                    egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, on, &name)
                });
                if box_.changed() {
                    out.push(lib(LibCmd::Contents {
                        source: Some(r.id.clone()),
                        on: Some(on),
                    }));
                }
                ui.end_row();
            }
        });
}

// --- Task 33 ---
/// The drive inventory: one row per volume with its state menu and backup checkbox.
fn volume_table(ui: &mut egui::Ui, l: &LibraryUi, out: &mut Vec<Action>) {
    ui.label(RichText::new(crate::i18n::tr("search_settings.tab_volumes")).strong())
        .on_hover_text(crate::i18n::tr("library_ui.volumes_tip"));
    let rows = volume_rows(&l.volumes);
    if rows.is_empty() {
        ui.weak(crate::i18n::tr("library_ui.no_volumes_yet"));
        return;
    }
    egui::Grid::new("keel-volumes")
        .num_columns(7)
        .striped(true)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            for h in [
                crate::i18n::tr("inspector.volume"),
                crate::i18n::tr("toolbar.kind"),
                crate::i18n::tr("library_ui.col_state"),
                crate::i18n::tr("library_ui.failure_domain"),
                crate::i18n::tr("library_ui.col_backup"),
                crate::i18n::tr("library_ui.used_total"),
                crate::i18n::tr("library_ui.last_seen"),
            ] {
                ui.label(RichText::new(h).small().strong());
            }
            ui.end_row();
            for r in rows {
                ui.label(&r.label).on_hover_text(&r.id);
                ui.label(r.kind);
                let warn = matches!(r.state, VolumeState::Offline | VolumeState::Lost);
                let text = RichText::new(state_text(r.state));
                let text = if warn {
                    text.color(ui.visuals().warn_fg_color)
                } else {
                    text
                };
                ui.menu_button(text, |ui| {
                    for (state, what) in [
                        (
                            VolumeState::Online,
                            crate::i18n::tr("library_ui.state_automatic"),
                        ),
                        (
                            VolumeState::Archived,
                            crate::i18n::tr("library_ui.state_archived"),
                        ),
                        (VolumeState::Lost, crate::i18n::tr("library_ui.state_lost")),
                        (
                            VolumeState::Retired,
                            crate::i18n::tr("library_ui.state_retired"),
                        ),
                    ] {
                        if ui.button(what).clicked() {
                            out.push(lib(LibCmd::SetVolumeState {
                                volume: r.id.clone(),
                                state,
                            }));
                            ui.close_menu();
                        }
                    }
                });
                // Editable: the text being typed lives in egui's memory until Enter or
                // leaving the field; blank goes back to the detected domain.
                let id = ui.id().with(("keel-volume-domain", &r.id));
                let mut text = ui
                    .data_mut(|d| d.get_temp::<String>(id))
                    .unwrap_or_else(|| r.domain.clone());
                let field = ui
                    .add(
                        egui::TextEdit::singleline(&mut text)
                            .desired_width(150.0)
                            .font(egui::TextStyle::Small),
                    )
                    .on_hover_text(if r.domain_set {
                        crate::i18n::tr("library_ui.domain_set_tip")
                    } else {
                        crate::i18n::tr("library_ui.domain_detected_tip")
                    });
                if field.changed() {
                    ui.data_mut(|d| d.insert_temp(id, text.clone()));
                }
                if field.lost_focus() {
                    ui.data_mut(|d| d.remove::<String>(id));
                    if text.trim() != r.domain || (r.domain_set && text.trim().is_empty()) {
                        out.push(lib(LibCmd::SetDomain {
                            volume: r.id.clone(),
                            domain: text,
                        }));
                    }
                }
                let mut backup = r.backup;
                if ui
                    .checkbox(&mut backup, "")
                    .on_hover_text(crate::i18n::tr("library_ui.mark_backup"))
                    .changed()
                {
                    out.push(lib(LibCmd::SetBackup {
                        volume: r.id.clone(),
                        on: backup,
                    }));
                }
                ui.label(&r.usage);
                ui.label(RichText::new(&r.last_seen).small());
                ui.end_row();
            }
        });
}

/// The library rows of the jobs panel (progress, pause/resume for hashing, cancel).
pub fn jobs(ui: &mut egui::Ui, l: &LibraryUi, out: &mut Vec<Action>) {
    for (id, job) in &l.jobs {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if job.active() {
                    if ui.button(crate::i18n::tr("dialogs.cancel")).clicked() {
                        out.push(lib(LibCmd::CancelJob(*id)));
                    }
                    if job.kind == "hash" {
                        let (text, on) = if l.hash_paused {
                            (crate::i18n::tr("library_ui.resume"), false)
                        } else {
                            (crate::i18n::tr("library_ui.pause"), true)
                        };
                        if ui.button(text).clicked() {
                            out.push(lib(LibCmd::PauseHashing(on)));
                        }
                    }
                }
                ui.add(
                    egui::ProgressBar::new(job.progress)
                        .desired_width(180.0)
                        .show_percentage(),
                );
                let status = match job.status {
                    JobStatus::Queued => crate::i18n::tr("library_ui.job_queued"),
                    JobStatus::Running if job.kind == "hash" && l.hash_paused => {
                        crate::i18n::tr("library_ui.job_paused")
                    }
                    JobStatus::Running => crate::i18n::tr("library_ui.job_running"),
                    JobStatus::Done => crate::i18n::tr("library_ui.job_done"),
                    JobStatus::Failed => crate::i18n::tr("library_ui.job_failed"),
                    JobStatus::Cancelled => crate::i18n::tr("library_ui.job_cancelled"),
                };
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.strong(job.title());
                    ui.label(status);
                });
            });
        });
    }
}

/// The library's windows: add source, plan preview, tag picker, duplicate finder.
pub fn windows(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    lost_banner(ctx, &s.library, out);
    add_source(ctx, s, out);
    plan_dialog(ctx, s, out);
    tag_picker(ctx, s, out);
    dup_finder(ctx, s, out);
}

/// The attached daemon went away: Reconnect, or open the library in this window (nothing
/// is written until one is chosen).
fn lost_banner(ctx: &egui::Context, l: &LibraryUi, out: &mut Vec<Action>) {
    if !l.lost {
        return;
    }
    egui::Area::new(egui::Id::new("keel-daemon-lost"))
        .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        crate::i18n::tr("library_ui.daemon_stopped_banner"),
                    );
                    // While one runs (it waits for the stopping daemon to let go).
                    let idle = !l.opening;
                    if ui
                        .add_enabled(
                            idle,
                            egui::Button::new(crate::i18n::tr("library_ui.reconnect")),
                        )
                        .on_hover_text(crate::i18n::tr("library_ui.reconnect_tip"))
                        .clicked()
                    {
                        out.push(lib(LibCmd::Reconnect));
                    }
                    if ui
                        .add_enabled(
                            idle,
                            egui::Button::new(crate::i18n::tr("library_ui.open_here")),
                        )
                        .clicked()
                    {
                        out.push(lib(LibCmd::OpenHere));
                    }
                    if l.opening {
                        crate::anim::spinner(ui);
                    }
                });
            });
        });
}

fn add_source(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let (tx, ectx) = (s.tx.clone(), s.ctx.clone());
    // FTP remotes cannot be library sources (no change times to poll, no ranged hashing).
    let remotes: Vec<(String, VPath)> = (s.settings.remotes.iter())
        .filter(|h| h.protocol == keel_vfs::RemoteProtocol::Sftp)
        .map(|h| (h.label.clone(), crate::remotes::home_of(h)))
        .chain((s.settings.clouds.iter()).map(|c| (c.label.clone(), crate::clouds::root_of(&c.id))))
        .collect();
    let Some(add) = &mut s.library.add else {
        return;
    };
    let mut open = true;
    let mut close = false;
    // Esc closes it (a11y).
    crate::a11y::esc_closes(
        ctx,
        crate::i18n::tr("library_ui.add_source_title"),
        &mut open,
    );
    egui::Window::new(crate::i18n::tr("library_ui.add_source_title"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("add-source")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label(crate::i18n::tr("inspector.folder"));
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut add.root).desired_width(260.0));
                        let pick = ui.add_enabled(
                            !add.picking,
                            egui::Button::new(crate::i18n::tr("dialogs.browse")),
                        );
                        if pick.clicked() {
                            add.picking = true;
                            // The OS dialog blocks: a worker; the answer comes back as a toast-free Msg.
                            let start = add.root.clone();
                            // The language is per thread: look the title up here, not on the worker.
                            let title = crate::i18n::tr("library_ui.pick_source_title").to_owned();
                            crate::worker::spawn("keel-pick-source", move || {
                                let picked = crate::platform::file_dialog(&title, |d| {
                                    d.set_directory(&start).pick_folder()
                                });
                                crate::worker::send(
                                    &tx,
                                    &ectx,
                                    crate::state::Msg::Library(crate::library::LibMsg::Picked(
                                        picked.map(|p| p.display().to_string()),
                                    )),
                                );
                            });
                        }
                    });
                    ui.end_row();
                    if !remotes.is_empty() {
                        ui.label(crate::i18n::tr("library_ui.or_remote"));
                        let combo = egui::ComboBox::from_id_salt("add-source-remote")
                            .selected_text(crate::i18n::tr("library_ui.pick_remote_hint"))
                            .show_ui(ui, |ui| {
                                for (label, root) in &remotes {
                                    if ui.selectable_label(false, label).clicked() {
                                        add.root = format!(
                                            "{}://{}{}",
                                            root.scheme, root.authority, root.path
                                        );
                                        add.label = label.clone();
                                    }
                                }
                            });
                        let _ = combo
                            .response
                            .named(crate::i18n::tr("library_ui.or_remote"));
                        ui.end_row();
                    }
                    ui.label(crate::i18n::tr("library_ui.label"));
                    ui.text_edit_singleline(&mut add.label);
                    ui.end_row();
                    ui.label(crate::i18n::tr("settings.hidden_files"));
                    ui.checkbox(
                        &mut add.include_hidden,
                        crate::i18n::tr("library_ui.include"),
                    );
                    ui.end_row();
                    ui.label(crate::i18n::tr("library_ui.ignore"));
                    ui.add(
                        egui::TextEdit::multiline(&mut add.ignore)
                            .desired_rows(3)
                            .hint_text(crate::i18n::tr("library_ui.ignore_hint")),
                    );
                    ui.end_row();
                });
            let root = parse_root(&add.root);
            ui.horizontal(|ui| {
                let ok = root.is_some() && !add.label.trim().is_empty();
                if ui
                    .add_enabled(
                        ok,
                        egui::Button::new(crate::i18n::tr("library_ui.add_and_index")),
                    )
                    .clicked()
                {
                    if let Some(root) = root {
                        out.push(lib(LibCmd::Register(SourceDef {
                            label: add.label.trim().to_owned(),
                            kind: match root.scheme.as_str() {
                                "file" if root.parent().is_none() => SourceKind::Drive,
                                "file" => SourceKind::Folder,
                                "cloud" => SourceKind::Cloud,
                                "node" => SourceKind::Device,
                                _ => SourceKind::Share,
                            },
                            root,
                            include_hidden: add.include_hidden,
                            ignore: (add.ignore.lines())
                                .map(str::trim)
                                .filter(|l| !l.is_empty())
                                .map(str::to_owned)
                                .collect(),
                            poll_secs: None,
                            hash_shares: false,
                        })));
                    }
                }
                close |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
            });
        });
    if !open || close {
        s.library.add = None;
    }
}

/// The wizard's folder text as a source root: `scheme://…` or an absolute local path.
pub fn parse_root(text: &str) -> Option<VPath> {
    let text = text.trim().trim_matches('"');
    if text.contains("://") {
        // Never a library view or the Overview as a source.
        return VPath::parse(text)
            .ok()
            .filter(|p| p.split_archive().is_none())
            .filter(|p| !matches!(p.scheme.as_str(), "library" | "keel"));
    }
    std::path::Path::new(text)
        .is_absolute()
        .then(|| VPath::local(text))
}

fn plan_dialog(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let sources = s.library.sources.clone();
    let Some(d) = &mut s.library.plan else { return };
    let mut cancel = false;
    egui::Modal::new(egui::Id::new("keel-plan")).show(ctx, |ui| {
        ui.set_width(460.0);
        if d.changed {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                crate::i18n::tr("library_ui.plan_changed"),
            );
        }
        ui.strong(d.plan.summary());
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .id_salt("plan-changes")
            .show(ui, |ui| {
                for line in d.plan.changes() {
                    ui.add(egui::Label::new(line).truncate());
                }
            });
        let warnings = d.plan.warnings(&sources);
        if !warnings.is_empty() {
            ui.add_space(6.0);
            for w in warnings {
                ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {w}"));
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let confirm = ui.add_enabled(
                !d.running,
                egui::Button::new(crate::i18n::tr("library_ui.confirm")),
            );
            // Enter confirms (Esc cancels), as the button says: a fresh press only, never
            // the key repeat of an Enter held since the palette or the dialog before (a held
            // Enter would confirm a preview nobody read), and not on the frame it opened.
            let enter = d.armed && ui.input(fresh_enter);
            if confirm.clicked() || (enter && !d.running) {
                out.push(lib(LibCmd::Execute));
            }
            cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
        });
    });
    d.armed = true;
    if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        s.library.plan = None;
    }
}

/// Enter went down this frame (not a key repeat).
fn fresh_enter(i: &egui::InputState) -> bool {
    i.events.iter().any(|e| {
        matches!(
            e,
            egui::Event::Key {
                key: egui::Key::Enter,
                pressed: true,
                repeat: false,
                ..
            }
        )
    })
}

fn tag_picker(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let l = &mut s.library;
    let Some(picker) = &mut l.picker else { return };
    let mut open = true;
    // Esc closes it (a11y).
    crate::a11y::esc_closes(ctx, crate::i18n::tr("sidebar.tags"), &mut open);
    egui::Window::new(crate::i18n::tr("sidebar.tags"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.weak(
                crate::i18n::tr("library_ui.for_items")
                    .replace("{items}", &crate::jobs::items(picker.targets.len())),
            );
            let on_all = |tag| {
                picker.targets.iter().all(|t| {
                    let real = crate::library::real_of(&l.sources, t);
                    real.and_then(|r| l.tagged.get(&r))
                        .is_some_and(|tags| tags.contains(&tag))
                })
            };
            for tag in &l.tags {
                let mut on = on_all(tag.id);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut on, "").changed() {
                        out.push(lib(LibCmd::SetTag { tag: tag.id, on }));
                    }
                    chip(ui, tag);
                });
            }
            ui.separator();
            // Task 67: Finder's eight tag colors (None: no color).
            ui.horizontal(|ui| {
                for c in keel_core::FinderColor::MENU {
                    let (rect, r) =
                        ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
                    let center = rect.center();
                    match c.hex().and_then(crate::tag_dots::color32) {
                        Some(fill) => {
                            ui.painter().circle_filled(center, 6.5, fill);
                        }
                        None => {
                            let weak = egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color());
                            ui.painter().circle_stroke(center, 6.0, weak);
                        }
                    }
                    if picker.new_color == c {
                        let ring = egui::Stroke::new(1.5_f32, ui.visuals().text_color());
                        ui.painter().circle_stroke(center, 9.0, ring);
                    }
                    if r.tip(
                        &crate::i18n::tr("library_ui.new_tag_color").replace("{name}", c.name()),
                    )
                    .clicked()
                    {
                        picker.new_color = c;
                    }
                }
            });
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut picker.new_name)
                        .hint_text(crate::i18n::tr("library_ui.new_tag_hint"))
                        .desired_width(160.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button(crate::i18n::tr("dialogs.create")).clicked() || enter)
                    && !picker.new_name.trim().is_empty()
                {
                    out.push(lib(LibCmd::CreateTag {
                        name: std::mem::take(&mut picker.new_name),
                        color: picker.new_color.hex().unwrap_or_default().to_owned(),
                    }));
                }
            });
        });
    if !open {
        l.picker = None;
    }
}

fn dup_finder(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let Some(finder) = &s.library.dups else {
        return;
    };
    let mut open = true;
    // Esc closes it (a11y).
    crate::a11y::esc_closes(
        ctx,
        crate::i18n::tr("library_ui.dup_finder_title"),
        &mut open,
    );
    egui::Window::new(crate::i18n::tr("library_ui.dup_finder_title"))
        .open(&mut open)
        .default_size([620.0, 420.0])
        .show(ctx, |ui| match &finder.groups {
            None => {
                crate::anim::spinner(ui);
                ui.weak(crate::i18n::tr("library_ui.finding_dups"));
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            Some(Ok(groups)) if groups.is_empty() => {
                ui.label(crate::i18n::tr("library_ui.no_dups"));
            }
            Some(Ok(groups)) => {
                ui.strong(
                    crate::i18n::tr("library_ui.groups_reclaimable")
                        .replace(
                            "{groups}",
                            &crate::library::counted(groups.len() as u64, "group"),
                        )
                        .replace("{size}", &format_size(reclaimable(groups), DECIMAL)),
                );
                // Task 85: local copies on one volume share their bytes (preview first).
                let dedup = ui
                    .button(crate::i18n::tr("library_ui.replace_hard_links"))
                    .on_hover_text(crate::i18n::tr("library_ui.replace_hard_links_tip"));
                if dedup.clicked() {
                    out.push(Action::System(crate::system_cleanup::SysCmd::DedupPreview));
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (g, group) in groups.iter().enumerate() {
                        let title = crate::i18n::tr("library_ui.group_title")
                            .replace("{count}", &group.records.len().to_string())
                            .replace("{size}", &format_size(group.size, DECIMAL))
                            .replace("{name}", group.records[0].path.name());
                        egui::CollapsingHeader::new(title)
                            .id_salt(("dup", g))
                            .show(ui, |ui| {
                                for (i, r) in group.records.iter().enumerate() {
                                    ui.horizontal(|ui| {
                                        if ui
                                            .small_button(crate::i18n::tr(
                                                "library_ui.keep_this_one",
                                            ))
                                            .clicked()
                                        {
                                            out.push(lib(LibCmd::KeepOne { group: g, keep: i }));
                                        }
                                        if ui
                                            .small_button(crate::i18n::tr("library_ui.open_folder"))
                                            .clicked()
                                        {
                                            if let Some(dir) = r.path.parent() {
                                                out.push(Action::Navigate(dir));
                                            }
                                        }
                                        ui.weak(&r.source);
                                        if r.offline {
                                            ui.colored_label(
                                                ui.visuals().warn_fg_color,
                                                crate::i18n::tr("library_ui.offline"),
                                            );
                                        }
                                        ui.add(egui::Label::new(r.path.display()).truncate());
                                    });
                                }
                            });
                    }
                });
            }
        });
    if !open {
        s.library.dups = None;
    }
}

/// Settings → Library.
pub fn settings_page(ui: &mut egui::Ui, s: &mut crate::settings::Settings, l: &mut LibraryUi) {
    let lib_settings = &mut s.library;
    let mut enabled = lib_settings.enabled;
    if ui
        .checkbox(&mut enabled, crate::i18n::tr("library_ui.enable"))
        .changed()
    {
        l.pending.push(LibCmd::Enable(enabled));
    }
    ui.add_space(6.0);
    egui::Grid::new("settings-library")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label(crate::i18n::tr("settings.library"));
            let combo = egui::ComboBox::from_id_salt("settings-library-name")
                .selected_text(lib_settings.name.as_str())
                .show_ui(ui, |ui| {
                    for summary in &l.libraries {
                        let sources = crate::library::counted(summary.sources as u64, "source");
                        let label = format!("{} ({sources})", summary.name);
                        if ui
                            .selectable_label(summary.name == lib_settings.name, label)
                            .clicked()
                            && summary.name != lib_settings.name
                        {
                            l.pending.push(LibCmd::Switch(summary.name.clone()));
                        }
                    }
                });
            let _ = combo.response.named(crate::i18n::tr("settings.library"));
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.new_library"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut l.new_name).desired_width(140.0));
                if ui
                    .add_enabled(
                        !l.new_name.trim().is_empty(),
                        egui::Button::new(crate::i18n::tr("dialogs.create")),
                    )
                    .clicked()
                {
                    l.pending
                        .push(LibCmd::Switch(std::mem::take(&mut l.new_name)));
                }
            });
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.hashing"));
            ui.vertical(|ui| {
                for (value, text, tip) in [
                    (
                        Hashing::IdleOnly,
                        crate::i18n::tr("library_ui.hash_idle"),
                        crate::i18n::tr("library_ui.hash_idle_tip"),
                    ),
                    (
                        Hashing::PauseOnBattery,
                        crate::i18n::tr("library_ui.hash_battery"),
                        crate::i18n::tr("library_ui.hash_battery_tip"),
                    ),
                    (
                        Hashing::Off,
                        crate::i18n::tr("library_ui.off"),
                        crate::i18n::tr("library_ui.off_tip"),
                    ),
                ] {
                    ui.radio_value(&mut lib_settings.hashing, value, text)
                        .on_hover_text(tip);
                }
            });
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.remote_hashing"));
            ui.checkbox(
                &mut lib_settings.hash_remote,
                crate::i18n::tr("library_ui.hash_sftp"),
            );
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.cloud_hashing"));
            ui.vertical(|ui| {
                ui.checkbox(
                    &mut lib_settings.hash_cloud,
                    crate::i18n::tr("library_ui.hash_cloud"),
                );
                ui.weak(crate::i18n::tr("library_ui.cloud_off_note"));
            });
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.remote_size_cap"));
            let mut mib = lib_settings.remote_hash_max_bytes >> 20;
            if ui
                .add(
                    egui::DragValue::new(&mut mib)
                        .range(1..=1 << 20)
                        .suffix(" MiB"),
                )
                .on_hover_text(crate::i18n::tr("library_ui.remote_size_cap_tip"))
                .changed()
            {
                lib_settings.remote_hash_max_bytes = mib << 20;
            }
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.check_remotes_every"));
            let _ = ui
                .add(
                    egui::Slider::new(
                        &mut lib_settings.remote_poll_secs,
                        keel_api::config::REMOTE_POLL_SECS,
                    )
                    .logarithmic(true)
                    .suffix(" s"),
                )
                .on_hover_text(crate::i18n::tr("library_ui.check_remotes_every_tip"))
                .named(crate::i18n::tr("library_ui.check_remotes_every"));
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.details_view"));
            ui.checkbox(
                &mut lib_settings.tags_column,
                crate::i18n::tr("library_ui.tags_column"),
            );
            ui.end_row();
            // --- Task 62 ---
            ui.label(crate::i18n::tr("library_ui.contents"));
            ui.vertical(|ui| {
                let mut on = l.stats.index_contents;
                if ui
                    .checkbox(&mut on, crate::i18n::tr("library_ui.index_contents"))
                    .changed()
                {
                    l.stats.index_contents = on;
                    l.pending.push(LibCmd::Contents {
                        source: None,
                        on: Some(on),
                    });
                }
                ui.weak(crate::i18n::tr("library_ui.index_contents_note"));
            });
            ui.end_row();
            // --- Task 53 ---
            ui.label(crate::i18n::tr("library_ui.last_backup"));
            ui.horizontal(|ui| {
                ui.label(crate::library::when(l.stats.last_backup));
                if ui
                    .button(crate::i18n::tr("library_ui.backup_btn"))
                    .clicked()
                {
                    l.pending.push(LibCmd::Backup);
                }
                if ui
                    .button(crate::i18n::tr("library_ui.restore_btn"))
                    .clicked()
                {
                    l.pending.push(LibCmd::Restore);
                }
            });
            ui.end_row();
            // --- Task 33 ---
            ui.label(crate::i18n::tr("library_ui.integrity_check"));
            ui.horizontal(|ui| {
                let combo = egui::ComboBox::from_id_salt("settings-library-integrity")
                    .selected_text(match lib_settings.integrity_days {
                        0 => crate::i18n::tr("library_ui.off").to_owned(),
                        1 => crate::i18n::tr("library_ui.daily").to_owned(),
                        7 => crate::i18n::tr("library_ui.weekly").to_owned(),
                        30 => crate::i18n::tr("library_ui.monthly").to_owned(),
                        d => crate::i18n::tr("library_ui.every_n_days")
                            .replace("{d}", &d.to_string()),
                    })
                    .show_ui(ui, |ui| {
                        for (days, text) in [
                            (0, crate::i18n::tr("library_ui.off")),
                            (1, crate::i18n::tr("library_ui.daily")),
                            (7, crate::i18n::tr("library_ui.weekly")),
                            (30, crate::i18n::tr("library_ui.monthly")),
                        ] {
                            ui.selectable_value(&mut lib_settings.integrity_days, days, text);
                        }
                    });
                let _ = combo
                    .response
                    .named(crate::i18n::tr("library_ui.integrity_check"));
                let _ = ui
                    .add(
                        egui::Slider::new(&mut lib_settings.integrity_pct, 0.1..=10.0)
                            .logarithmic(true)
                            .suffix(crate::i18n::tr("library_ui.pct_of_files")),
                    )
                    .on_hover_text(crate::i18n::tr("library_ui.integrity_pct_tip"))
                    .named(crate::i18n::tr("library_ui.integrity_pct_name"));
            });
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.index"));
            if ui
                .add_enabled(
                    l.is_open(),
                    egui::Button::new(crate::i18n::tr("library_ui.rebuild_index")),
                )
                .on_hover_text(crate::i18n::tr("library_ui.rebuild_index_tip"))
                .clicked()
            {
                l.pending.push(LibCmd::RebuildIndex);
            }
            ui.end_row();
            ui.label(crate::i18n::tr("library_ui.background"));
            ui.vertical(|ui| {
                let attached = l.remote().is_some();
                if ui
                    .checkbox(
                        &mut lib_settings.daemon,
                        crate::i18n::tr("library_ui.run_daemon"),
                    )
                    .on_hover_text(crate::i18n::tr("library_ui.run_daemon_tip"))
                    .changed()
                    && lib_settings.daemon
                    && l.is_open()
                    && !attached
                {
                    // Hand the library over now: close it here, then attach.
                    l.pending.push(LibCmd::Restart);
                }
                ui.weak(match l.remote() {
                    Some(r) => {
                        crate::i18n::tr("library_ui.daemon_connected_short")
                            .replace("{pid}", &r.version.pid.to_string())
                            + if l.spawned {
                                crate::i18n::tr("library_ui.started_here")
                            } else {
                                ""
                            }
                    }
                    None if l.is_open() => crate::i18n::tr("library_ui.open_here").to_owned(),
                    None => String::new(),
                });
            });
            ui.end_row();
        });
    if let Some(dir) = crate::settings::data_dir() {
        ui.weak(
            crate::i18n::tr("library_ui.stored_in")
                .replace("{path}", &dir.join("library").display().to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_roots_parse() {
        assert_eq!(color_of(Some("#ff0000")), Color32::from_rgb(255, 0, 0));
        assert_eq!(color_of(Some("red")), Color32::from_rgb(0x3b, 0x82, 0xf6));
        assert_eq!(parse_root("relative/dir"), None);
        assert_eq!(
            parse_root("sftp://nas/home"),
            Some(VPath::parse("sftp://nas/home").unwrap())
        );
        let abs = std::env::temp_dir();
        assert_eq!(
            parse_root(&abs.display().to_string()),
            Some(VPath::local(&abs))
        );
    }
}
