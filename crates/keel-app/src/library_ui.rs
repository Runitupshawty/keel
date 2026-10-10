//! Drawing the library (Task 29): the sidebar section, the Overview tab, the add-source
//! wizard, the validate/preview dialog, the tag picker, the duplicate finder, Settings →
//! Library and the library rows of the jobs panel. State and workers are in `library.rs`.

use crate::keys::Action;
use crate::library::{
    count, plan_summary, protection_lines, reclaimable, source_rows, state_text, volume_rows,
    warning_text, Dot, Hashing, LibCmd, LibraryUi, FAVORITES_QUERY, RECENTS_QUERY,
};
use crate::pane::ViewCx;
use crate::state::AppState;
use crate::theme::Theme;
use egui::{Color32, RichText};
use humansize::{format_size, DECIMAL};
use keel_core::{Action as PlanAction, JobStatus, SourceDef, SourceKind, Tag, VolumeState};
use keel_vfs::VPath;

fn lib(cmd: LibCmd) -> Action {
    Action::Library(cmd)
}

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
        Dot::Online => Color32::from_rgb(70, 180, 90),
        Dot::Indexing => Color32::from_rgb(230, 180, 40),
        Dot::Offline => ui.visuals().weak_text_color(),
        Dot::Error => ui.visuals().error_fg_color,
    }
}

/// A tag chip: its name on its color.
pub fn chip(ui: &mut egui::Ui, tag: &Tag) -> egui::Response {
    let fill = color_of(tag.color.as_deref());
    let luma = 0.299 * fill.r() as f32 + 0.587 * fill.g() as f32 + 0.114 * fill.b() as f32;
    let text = if luma > 150.0 {
        Color32::BLACK
    } else {
        Color32::WHITE
    };
    ui.add(
        egui::Button::new(RichText::new(&tag.name).small().color(text))
            .fill(fill)
            .corner_radius(8.0)
            .min_size(egui::vec2(0.0, 14.0)),
    )
}

fn section(ui: &mut egui::Ui, title: &str, theme: &Theme) {
    ui.label(RichText::new(title).small().strong().color(theme.muted()));
}

/// The sidebar's library sections (above Quick access).
pub fn sidebar(
    ui: &mut egui::Ui,
    l: &LibraryUi,
    theme: &Theme,
    current: &VPath,
    out: &mut Vec<Action>,
) {
    section(ui, "Library", theme);
    if !l.is_open() {
        match (&l.error, l.opening) {
            (_, true) => {
                ui.weak("Opening…");
            }
            (Some(e), _) => {
                ui.colored_label(ui.visuals().error_fg_color, "Not available")
                    .on_hover_text(e);
            }
            _ => {
                ui.weak("Off (Settings → Library)");
            }
        }
        ui.add_space(8.0);
        return;
    }
    let row = |ui: &mut egui::Ui, text: &str, cmd: LibCmd, out: &mut Vec<Action>| {
        if ui.add(egui::Button::new(text).frame(false)).clicked() {
            out.push(lib(cmd));
        }
    };
    row(ui, "Overview", LibCmd::Overview, out);
    row(
        ui,
        "★ Favorites",
        LibCmd::Query(FAVORITES_QUERY.into()),
        out,
    );
    row(ui, "Recents", LibCmd::Query(RECENTS_QUERY.into()), out);
    ui.add_space(6.0);
    section(ui, "Sources", theme);
    for s in source_rows(&l.sources) {
        let r = ui
            .horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size([16.0, 16.0].into(), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 4.5, dot_color(ui, s.dot));
                let here =
                    current.scheme == keel_vfs::library::SCHEME && current.authority == s.id.0;
                let r = ui.add(
                    egui::Button::new(s.label.as_str())
                        .frame(false)
                        .selected(here),
                );
                if s.dot == Dot::Indexing {
                    ui.add(egui::Label::new(RichText::new(&s.detail).small().weak()).truncate());
                }
                r
            })
            .inner
            .on_hover_text(&s.detail);
        if r.clicked() {
            out.push(lib(LibCmd::OpenSource(s.id.clone())));
        } else if r.middle_clicked() {
            out.push(Action::NewTabAt(keel_vfs::library::path(&s.id.0, "")));
        }
        r.context_menu(|ui| {
            let pause = if l.hash_paused {
                ("Resume hashing", LibCmd::PauseHashing(false))
            } else {
                ("Pause hashing", LibCmd::PauseHashing(true))
            };
            let adopt = (s.adopt).then(|| ("Adopt new root", LibCmd::AdoptRoot(s.id.clone())));
            for (text, cmd) in adopt.into_iter().chain([
                ("Index now", LibCmd::IndexNow(s.id.clone())),
                pause,
                ("Remove…", LibCmd::RemoveSource(s.id.clone())),
            ]) {
                if ui.button(text).clicked() {
                    out.push(lib(cmd));
                    ui.close_menu();
                }
            }
        });
    }
    if ui.small_button("Add source…").clicked() {
        out.push(lib(LibCmd::AddSource));
    }
    if !l.tags.is_empty() {
        ui.add_space(6.0);
        section(ui, "Tags", theme);
        ui.horizontal_wrapped(|ui| {
            for tag in &l.tags {
                if chip(ui, tag).clicked() {
                    out.push(lib(LibCmd::Query(format!("tag:\"{}\"", tag.name))));
                }
            }
        });
    }
    if !l.views.is_empty() {
        ui.add_space(6.0);
        section(ui, "Views", theme);
        for v in &l.views {
            row(ui, &v.name, LibCmd::Query(v.query.clone()), out);
        }
    }
    ui.add_space(8.0);
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
pub fn overview(ui: &mut egui::Ui, cx: &mut ViewCx, out: &mut Vec<Action>) {
    let l = cx.library;
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(4.0);
        ui.heading("Library overview");
        if !l.is_open() {
            ui.weak(if l.opening {
                "The library is opening…"
            } else {
                "The library is off. Turn it on in Settings → Library."
            });
            return;
        }
        let st = &l.stats;
        // The cards' closures share one action list.
        let acts = std::cell::RefCell::new(Vec::new());
        let push = |a: Action| acts.borrow_mut().push(a);
        cards(
            ui,
            vec![
                (
                    "Files indexed",
                    Box::new(|ui: &mut egui::Ui| {
                        ui.heading(count(st.files));
                        ui.weak(format!(
                            "{} records, {}",
                            count(st.records),
                            format_size(st.bytes, DECIMAL)
                        ));
                    }),
                ),
                (
                    "Unique contents",
                    Box::new(|ui: &mut egui::Ui| {
                        ui.heading(count(st.unique_content));
                        ui.weak("distinct content ids (hashed files)");
                    }),
                ),
                (
                    "Duplicates",
                    Box::new(|ui: &mut egui::Ui| {
                        match l.dup_summary {
                            Some((0, _)) => {
                                ui.label("None found");
                            }
                            Some((n, bytes)) => {
                                ui.label(format!(
                                    "{} groups, {} reclaimable",
                                    count(n as u64),
                                    format_size(bytes, DECIMAL)
                                ));
                            }
                            None => {
                                ui.weak("Counting…");
                            }
                        }
                        if ui.button("Open duplicate finder").clicked() {
                            push(lib(LibCmd::Duplicates));
                        }
                    }),
                ),
                (
                    "Sources",
                    Box::new(|ui: &mut egui::Ui| {
                        ui.label(format!(
                            "{} sources, {} offline",
                            st.sources, st.offline_sources
                        ));
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
                        if ui.small_button("Add source…").clicked() {
                            push(lib(LibCmd::AddSource));
                        }
                    }),
                ),
                // --- Task 33 ---
                (
                    "Protection",
                    Box::new(|ui: &mut egui::Ui| {
                        let Some(p) = &l.protection else {
                            ui.weak("Counting…");
                            return;
                        };
                        for (i, (text, how)) in protection_lines(p).into_iter().enumerate() {
                            let warn = match i {
                                0 => p.single_copy > 0,
                                1 => p.single_domain > 0,
                                3 => p.drifted > 0,
                                4 => p.offline_volumes > 0,
                                _ => false,
                            };
                            let text = if warn {
                                RichText::new(text).color(ui.visuals().warn_fg_color)
                            } else {
                                RichText::new(text)
                            };
                            ui.label(text).on_hover_text(how);
                        }
                        if ui
                            .small_button("Check integrity now")
                            .on_hover_text("Re-hash a sample of hashed files (Settings → Library)")
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
        ui.label(RichText::new("Storage").strong());
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
                        .text(format!(
                            "{} used of {}",
                            format_size(used, DECIMAL),
                            format_size(*total, DECIMAL)
                        )),
                );
            });
        }
        // --- Task 33 ---
        ui.add_space(8.0);
        volume_table(ui, l, out);
        ui.add_space(8.0);
        ui.label(RichText::new("Running jobs").strong());
        let running: Vec<_> = l.jobs.iter().filter(|(_, j)| j.active()).collect();
        if running.is_empty() {
            ui.weak("None");
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

// --- Task 33 ---
/// The drive inventory: one row per volume with its state menu and backup checkbox.
fn volume_table(ui: &mut egui::Ui, l: &LibraryUi, out: &mut Vec<Action>) {
    ui.label(RichText::new("Volumes").strong()).on_hover_text(
        "Every drive, share, cloud account or host a source was seen on. Copies on one \
             failure domain (one physical disk, account or host) count as one for protection.",
    );
    let rows = volume_rows(&l.volumes);
    if rows.is_empty() {
        ui.weak("None yet (sources are placed on their volume when indexed)");
        return;
    }
    egui::Grid::new("keel-volumes")
        .num_columns(7)
        .striped(true)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            for h in [
                "Volume",
                "Kind",
                "State",
                "Failure domain",
                "Backup",
                "Used / total",
                "Last seen",
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
                        (VolumeState::Online, "Automatic (online / offline)"),
                        (VolumeState::Archived, "Archived (on a shelf; copies count)"),
                        (VolumeState::Lost, "Lost (copies no longer count)"),
                        (VolumeState::Retired, "Retired (copies no longer count)"),
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
                ui.add_sized(
                    [150.0, 16.0],
                    egui::Label::new(RichText::new(&r.domain).small().weak()).truncate(),
                )
                .on_hover_text(&r.domain);
                let mut backup = r.backup;
                if ui
                    .checkbox(&mut backup, "")
                    .on_hover_text("Mark as backup")
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
                    if ui.button("Cancel").clicked() {
                        out.push(lib(LibCmd::CancelJob(*id)));
                    }
                    if job.kind == "hash" {
                        let (text, on) = if l.hash_paused {
                            ("Resume", false)
                        } else {
                            ("Pause", true)
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
                    JobStatus::Queued => "Queued",
                    JobStatus::Running if job.kind == "hash" && l.hash_paused => "Paused",
                    JobStatus::Running => "Running",
                    JobStatus::Done => "Done",
                    JobStatus::Failed => "Failed (see the log in Settings → Library)",
                    JobStatus::Cancelled => "Cancelled",
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
    add_source(ctx, s, out);
    plan_dialog(ctx, s, out);
    tag_picker(ctx, s, out);
    dup_finder(ctx, s, out);
}

fn add_source(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let (tx, ectx) = (s.tx.clone(), s.ctx.clone());
    let remotes: Vec<(String, VPath)> = (s.settings.remotes.iter())
        .map(|h| (h.label.clone(), crate::remotes::home_of(h)))
        .chain((s.settings.clouds.iter()).map(|c| (c.label.clone(), crate::clouds::root_of(&c.id))))
        .collect();
    let Some(add) = &mut s.library.add else {
        return;
    };
    let mut open = true;
    let mut close = false;
    egui::Window::new("Add a library source")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("add-source")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Folder");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut add.root).desired_width(260.0));
                        let pick = ui.add_enabled(!add.picking, egui::Button::new("Browse…"));
                        if pick.clicked() {
                            add.picking = true;
                            // The OS dialog blocks: a worker; the answer comes back as a toast-free Msg.
                            let start = add.root.clone();
                            crate::worker::spawn("keel-pick-source", move || {
                                let picked = rfd::FileDialog::new()
                                    .set_title("Library source")
                                    .set_directory(&start)
                                    .pick_folder();
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
                        ui.label("Or a remote");
                        egui::ComboBox::from_id_salt("add-source-remote")
                            .selected_text("Pick a host or cloud account")
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
                        ui.end_row();
                    }
                    ui.label("Label");
                    ui.text_edit_singleline(&mut add.label);
                    ui.end_row();
                    ui.label("Hidden files");
                    ui.checkbox(&mut add.include_hidden, "Include");
                    ui.end_row();
                    ui.label("Ignore");
                    ui.add(
                        egui::TextEdit::multiline(&mut add.ignore)
                            .desired_rows(3)
                            .hint_text("gitignore patterns, one per line (node_modules/, *.tmp)"),
                    );
                    ui.end_row();
                });
            let root = parse_root(&add.root);
            ui.horizontal(|ui| {
                let ok = root.is_some() && !add.label.trim().is_empty();
                if ui
                    .add_enabled(ok, egui::Button::new("Add and index"))
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
                close |= ui.button("Cancel").clicked();
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
                "The sources changed since the preview. Review the new one:",
            );
        }
        ui.strong(plan_summary(&d.plan));
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .id_salt("plan-changes")
            .show(ui, |ui| {
                for c in &d.plan.changes {
                    let verb = match c.action {
                        PlanAction::Copy => "Copy",
                        PlanAction::Move => "Move",
                        PlanAction::Delete => "Delete",
                        PlanAction::Rename => "Rename",
                    };
                    let to = c.to.as_ref().map(|t| format!(" → {}", t.display()));
                    ui.add(
                        egui::Label::new(format!(
                            "{verb} {}{}  ({}, {})",
                            c.from.display(),
                            to.unwrap_or_default(),
                            crate::jobs::items(c.files as usize),
                            format_size(c.bytes, DECIMAL)
                        ))
                        .truncate(),
                    );
                }
            });
        if !d.plan.warnings.is_empty() {
            ui.add_space(6.0);
            for w in &d.plan.warnings {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("⚠ {}", warning_text(w, &sources)),
                );
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let confirm = ui.add_enabled(!d.running, egui::Button::new("Confirm"));
            if confirm.clicked() {
                out.push(lib(LibCmd::Execute));
            }
            cancel |= ui.button("Cancel").clicked();
        });
    });
    if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        s.library.plan = None;
    }
}

fn tag_picker(ctx: &egui::Context, s: &mut AppState, out: &mut Vec<Action>) {
    let l = &mut s.library;
    let Some(picker) = &mut l.picker else { return };
    let mut open = true;
    egui::Window::new("Tags")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.weak(format!("For {}", crate::jobs::items(picker.targets.len())));
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
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut picker.new_color);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut picker.new_name)
                        .hint_text("New tag")
                        .desired_width(160.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button("Create").clicked() || enter) && !picker.new_name.trim().is_empty() {
                    let [r, g, b] = picker.new_color;
                    out.push(lib(LibCmd::CreateTag {
                        name: std::mem::take(&mut picker.new_name),
                        color: format!("#{r:02x}{g:02x}{b:02x}"),
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
    egui::Window::new("Duplicate finder")
        .open(&mut open)
        .default_size([620.0, 420.0])
        .show(ctx, |ui| match &finder.groups {
            None => {
                ui.spinner();
                ui.weak("Finding duplicates (only hashed files are compared)…");
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            Some(Ok(groups)) if groups.is_empty() => {
                ui.label("No duplicates among the hashed files.");
            }
            Some(Ok(groups)) => {
                ui.strong(format!(
                    "{} groups, {} reclaimable",
                    count(groups.len() as u64),
                    format_size(reclaimable(groups), DECIMAL)
                ));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (g, group) in groups.iter().enumerate() {
                        let title = format!(
                            "{} × {}  —  {}",
                            group.records.len(),
                            format_size(group.size, DECIMAL),
                            group.records[0].path.name()
                        );
                        egui::CollapsingHeader::new(title)
                            .id_salt(("dup", g))
                            .show(ui, |ui| {
                                for (i, r) in group.records.iter().enumerate() {
                                    ui.horizontal(|ui| {
                                        if ui.small_button("Keep this one").clicked() {
                                            out.push(lib(LibCmd::KeepOne { group: g, keep: i }));
                                        }
                                        if ui.small_button("Open folder").clicked() {
                                            if let Some(dir) = r.path.parent() {
                                                out.push(Action::Navigate(dir));
                                            }
                                        }
                                        ui.weak(&r.source);
                                        if r.offline {
                                            ui.colored_label(ui.visuals().warn_fg_color, "offline");
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
        .checkbox(&mut enabled, "Library (index sources, tags, duplicates)")
        .changed()
    {
        l.pending.push(LibCmd::Enable(enabled));
    }
    ui.add_space(6.0);
    egui::Grid::new("settings-library")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("Library");
            egui::ComboBox::from_id_salt("settings-library-name")
                .selected_text(lib_settings.name.as_str())
                .show_ui(ui, |ui| {
                    for summary in &l.libraries {
                        let label = format!("{} ({} sources)", summary.name, summary.sources);
                        if ui
                            .selectable_label(summary.name == lib_settings.name, label)
                            .clicked()
                            && summary.name != lib_settings.name
                        {
                            l.pending.push(LibCmd::Switch(summary.name.clone()));
                        }
                    }
                });
            ui.end_row();
            ui.label("New library");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut l.new_name).desired_width(140.0));
                if ui
                    .add_enabled(!l.new_name.trim().is_empty(), egui::Button::new("Create"))
                    .clicked()
                {
                    l.pending
                        .push(LibCmd::Switch(std::mem::take(&mut l.new_name)));
                }
            });
            ui.end_row();
            ui.label("Hashing");
            ui.vertical(|ui| {
                for (value, text, tip) in [
                    (
                        Hashing::IdleOnly,
                        "Only while idle",
                        "Pauses while you use Keel and on battery",
                    ),
                    (
                        Hashing::PauseOnBattery,
                        "Pause on battery",
                        "Runs while you work too",
                    ),
                    (
                        Hashing::Off,
                        "Off",
                        "No content ids: no duplicate finder or last-copy warnings",
                    ),
                ] {
                    ui.radio_value(&mut lib_settings.hashing, value, text)
                        .on_hover_text(tip);
                }
            });
            ui.end_row();
            ui.label("Rescan remote sources");
            ui.add(
                egui::Slider::new(&mut lib_settings.rescan_minutes, 5..=1440)
                    .logarithmic(true)
                    .suffix(" min"),
            )
            .on_hover_text("Local sources are watched live (from the next start for this value)");
            ui.end_row();
            ui.label("Details view");
            ui.checkbox(&mut lib_settings.tags_column, "Tags column");
            ui.end_row();
            // --- Task 33 ---
            ui.label("Integrity check");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("settings-library-integrity")
                    .selected_text(match lib_settings.integrity_days {
                        0 => "Off".to_owned(),
                        1 => "Daily".to_owned(),
                        7 => "Weekly".to_owned(),
                        30 => "Monthly".to_owned(),
                        d => format!("Every {d} days"),
                    })
                    .show_ui(ui, |ui| {
                        for (days, text) in
                            [(0, "Off"), (1, "Daily"), (7, "Weekly"), (30, "Monthly")]
                        {
                            ui.selectable_value(&mut lib_settings.integrity_days, days, text);
                        }
                    });
                ui.add(
                    egui::Slider::new(&mut lib_settings.integrity_pct, 0.1..=10.0)
                        .logarithmic(true)
                        .suffix(" % of files"),
                )
                .on_hover_text(
                    "Each check re-hashes this share of every source's hashed files, at idle \
                     priority, and marks files whose bytes changed under the same size and times",
                );
            });
            ui.end_row();
            ui.label("Index");
            if ui
                .add_enabled(l.is_open(), egui::Button::new("Rebuild index"))
                .on_hover_text("Walks every source again")
                .clicked()
            {
                l.pending.push(LibCmd::RebuildIndex);
            }
            ui.end_row();
        });
    if let Some(dir) = keel_core::data_dir() {
        ui.weak(format!("Stored in {}", dir.join("library").display()));
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
