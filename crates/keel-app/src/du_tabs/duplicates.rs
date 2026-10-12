//! **Duplicates**: files of the scan with the same bytes (size, then a hash of the first
//! 64 KiB, then of the whole file; files over 64 MiB are sampled when sparse hashing is on
//! and listed as probable duplicates). Works on any scan, in a library or not.
//!
//! **Keep this one** and **Hard-link the others here** first read every file of the group
//! whole again; then the window previews the operations through the library's plan flow
//! (the others go to the trash; with hard links, each is then made again as a hard link to
//! the kept file), each confirmed before it runs.

use super::{changed, error_line, file_table, size_text, Export, Outcome, Worker};
use crate::a11y::Named;
use egui_extras::{Column, TableBuilder};
use keel_core::Op;
use keel_vfs::du::duplicates::{self, Algorithm, HashCache, Report};
use keel_vfs::du::{analysis::parse_size, Tree};
use keel_vfs::{LinkKind, VPath};
use std::path::PathBuf;
use std::sync::Arc;

/// What to do with a group once its files are read again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Delete the others (to the trash).
    Keep,
    /// Replace the others with hard links to the kept file (same volume).
    HardLink,
}

pub struct State {
    pub algorithm: Algorithm,
    pub sparse: bool,
    /// Smallest file compared, as typed ("1 KB").
    pub min_size: String,
    /// The library's BLAKE3 ids of files in library sources, by path (the window fills it
    /// when the library is open); used instead of reading those files.
    pub library_hashes: Arc<HashCache>,
    seen: Option<Arc<Tree>>,
    job: Option<Worker<Report>>,
    report: Option<Report>,
    selected: usize,
    verify: Option<Worker<Vec<Op>>>,
    export: Export,
    error: String,
}

impl Default for State {
    fn default() -> Self {
        State {
            algorithm: Algorithm::Blake3,
            sparse: true,
            min_size: "1 KB".into(),
            library_hashes: Arc::default(),
            seen: None,
            job: None,
            report: None,
            selected: 0,
            verify: None,
            export: Export::default(),
            error: String::new(),
        }
    }
}

impl State {
    pub fn report(&self) -> Option<&Report> {
        self.report.as_ref()
    }

    pub fn busy(&self) -> bool {
        self.job.is_some() || self.verify.is_some()
    }

    /// Starts finding duplicates in `tree` (the Find duplicates button).
    pub fn start(&mut self, ctx: &egui::Context, tree: &Arc<Tree>) {
        let min_size = match self.min_size.trim() {
            "" => 1,
            s => match parse_size(s) {
                Some(n) => n.max(1),
                None => {
                    self.error = format!("{s} is not a size like 1 KB");
                    return;
                }
            },
        };
        let options = duplicates::Options {
            algorithm: self.algorithm,
            sparse: self.sparse,
            min_size,
            ..duplicates::Options::default()
        };
        // Started for this scan (a later change of scan drops it).
        self.seen = Some(tree.clone());
        let (tree, cache) = (tree.clone(), self.library_hashes.clone());
        self.error.clear();
        self.report = None;
        self.selected = 0;
        self.job = Some(Worker::spawn(ctx, "keel-du-duplicates", move |h| {
            Ok(duplicates::find(&tree, &options, &cache, &h))
        }));
    }
}

/// The operations for `action` on a group whose other files are `others`.
pub fn ops(action: Action, kept: &PathBuf, others: &[PathBuf]) -> Vec<Op> {
    let mut ops = vec![Op::Delete {
        paths: others.iter().map(VPath::local).collect(),
    }];
    if action == Action::HardLink {
        ops.extend(others.iter().map(|o| Op::Link {
            target: VPath::local(kept),
            link: VPath::local(o),
            kind: LinkKind::Hardlink,
        }));
    }
    ops
}

pub fn ui(ui: &mut egui::Ui, tree: &Arc<Tree>, state: &mut State) -> Option<Outcome> {
    let mut out = None;
    if changed(&mut state.seen, tree) {
        // Groups belong to the scan they were found in.
        state.job = None;
        state.verify = None;
        state.report = None;
    }
    ui.horizontal_wrapped(|ui| {
        let idle = !state.busy();
        let find = ui.add_enabled(idle, egui::Button::new("Find duplicates"));
        if find.clicked() {
            state.start(ui.ctx(), tree);
        }
        ui.label("Hash");
        egui::ComboBox::from_id_salt("du-dup-algorithm")
            .selected_text(state.algorithm.label())
            .show_ui(ui, |ui| {
                for a in Algorithm::ALL {
                    ui.selectable_value(&mut state.algorithm, a, a.label());
                }
            })
            .response
            .named("Hash algorithm");
        ui.checkbox(&mut state.sparse, "Sample files over 64 MiB")
            .on_hover_text("Faster; such files are listed as probable duplicates and read whole before any action");
        ui.label("Smallest file");
        let r = ui.add(egui::TextEdit::singleline(&mut state.min_size).desired_width(60.0));
        r.named("Smallest file");
    });
    if let Some(job) = &state.job {
        match job.poll() {
            None => job.progress(ui, "files hashed"),
            Some(r) => {
                match r {
                    Ok(report) => state.report = Some(report),
                    Err(e) => state.error = format!("{e:#}"),
                }
                state.job = None;
            }
        }
    }
    if let Some(job) = &state.verify {
        match job.poll() {
            None => job.progress(ui, "files read again"),
            Some(r) => {
                match r {
                    Ok(ops) => out = Some(Outcome::Plans(ops)),
                    Err(e) => state.error = format!("{e:#}"),
                }
                state.verify = None;
            }
        }
    }
    error_line(ui, &state.error);
    let Some(report) = &state.report else {
        return out;
    };
    ui.horizontal_wrapped(|ui| {
        let probable = report.groups.iter().filter(|g| g.probable).count();
        let mut text = format!(
            "{}, {} to win back",
            crate::library::counted(report.groups.len() as u64, "group"),
            size_text(report.reclaimable())
        );
        if probable > 0 {
            text += &format!(" ({probable} probable)");
        }
        if report.cancelled {
            text += " · cancelled: incomplete";
        }
        ui.strong(text);
        if !report.skipped.is_empty() {
            ui.label(format!("· {} not read", crate::library::count(report.skipped.len() as u64)))
                .on_hover_text(skipped_text(report));
        }
        let idle = !state.export.busy();
        if ui.add_enabled(idle, egui::Button::new("Save CSV…")).clicked() {
            let r = report.clone();
            state.export.start(ui.ctx(), "duplicates.csv".into(), "CSV", "csv", move |w| {
                Ok(duplicates::write_csv(&r, w)?)
            });
        }
        if ui.add_enabled(idle, egui::Button::new("Save JSON…")).clicked() {
            let r = report.clone();
            state.export.start(ui.ctx(), "duplicates.json".into(), "JSON", "json", move |w| {
                Ok(duplicates::write_json(&r, w)?)
            });
        }
    });
    state.export.ui(ui);
    if report.groups.is_empty() {
        ui.label("No duplicates.");
        return out;
    }
    state.selected = state.selected.min(report.groups.len() - 1);
    // The groups, then the selected group's files.
    let half = (ui.available_height() / 2.0).max(120.0);
    ui.push_id("du-dup-groups", |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(half)
            .sense(egui::Sense::click())
            .column(Column::initial(60.0))
            .column(Column::initial(90.0))
            .column(Column::initial(100.0))
            .column(Column::initial(70.0))
            .column(Column::initial(140.0))
            .column(Column::remainder().clip(true))
            .header(20.0, |mut h| {
                for t in ["Files", "Each", "To win back", "Type", "Status", "Name"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, report.groups.len(), |mut row| {
                    let i = row.index();
                    let g = &report.groups[i];
                    row.set_selected(i == state.selected);
                    row.col(|ui| {
                        ui.label(g.files.len().to_string());
                    });
                    row.col(|ui| {
                        ui.label(size_text(g.bytes()));
                    });
                    row.col(|ui| {
                        ui.label(size_text(g.reclaimable()));
                    });
                    row.col(|ui| {
                        ui.label(g.files[0].extension.as_str());
                    });
                    row.col(|ui| {
                        if g.probable {
                            ui.colored_label(ui.visuals().warn_fg_color, "Probable duplicates")
                                .on_hover_text("Only samples were compared");
                        } else {
                            ui.label(format!("Same {}", g.algorithm.label()));
                        }
                    });
                    row.col(|ui| {
                        let name = g.files[0].path.file_name().unwrap_or_default().to_string_lossy();
                        ui.add(egui::Label::new(name).truncate());
                    });
                    if row.response().named(&format!("Group {}", i + 1)).clicked() {
                        state.selected = i;
                    }
                });
            });
    });
    ui.separator();
    let group = &report.groups[state.selected];
    let busy = state.busy();
    let mut act = None;
    let mut buttons = |ui: &mut egui::Ui, i: usize| {
        let keep = ui.add_enabled(!busy, egui::Button::new("Keep this one").small());
        if keep.on_hover_text("Delete the others (to the trash), after a preview").clicked() {
            act = Some((i, Action::Keep));
        }
        let link = ui.add_enabled(!busy, egui::Button::new("Hard-link others here").small());
        let tip = "Replace the others with hard links to this file (same volume), after a preview";
        if link.on_hover_text(tip).clicked() {
            act = Some((i, Action::HardLink));
        }
    };
    let picked = file_table(ui, "du-dup-files", &group.files, Some(("Action", &mut buttons)));
    if let Some((keep, action)) = act {
        let g = group.clone();
        state.error.clear();
        state.verify = Some(Worker::spawn(ui.ctx(), "keel-du-verify", move |h| {
            let others = duplicates::verify(&g, keep, action == Action::HardLink, &h)?;
            Ok(ops(action, &g.files[keep].path, &others))
        }));
    }
    out.or(picked.map(Outcome::Select))
}

fn skipped_text(report: &Report) -> String {
    let mut lines: Vec<String> = (report.skipped.iter().take(20))
        .map(|s| format!("{}: {}", s.path.display(), s.reason))
        .collect();
    if report.skipped.len() > 20 {
        lines.push(format!("and {} more", report.skipped.len() - 20));
    }
    lines.join("\n")
}
