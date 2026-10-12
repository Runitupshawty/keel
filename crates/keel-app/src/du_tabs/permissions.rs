//! **Permissions**: each folder's owner and access entries (Windows ACLs; owner, group and
//! mode on Unix), with explicit entries, broken inheritance and world-writable folders
//! flagged, and a CSV export.

use super::{changed, error_line, Export, Outcome, Worker};
use crate::a11y::Named;
use egui_extras::{Column, TableBuilder};
use keel_vfs::du::permissions::{self, Report, Row};
use keel_vfs::du::Tree;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Show {
    #[default]
    All,
    /// World-writable, broken inheritance, explicit entries or errors.
    Flagged,
}

#[derive(Default)]
pub struct State {
    pub show: Show,
    /// Files too, not only folders.
    pub files: bool,
    seen: Option<Arc<Tree>>,
    job: Option<Worker<Report>>,
    report: Option<Report>,
    selected: Option<usize>,
    export: Export,
    error: String,
}

impl State {
    pub fn report(&self) -> Option<&Report> {
        self.report.as_ref()
    }
}

/// A row worth a look: everyone may write, it does not inherit, it has its own entries, or
/// it could not be read.
pub fn flagged(r: &Row) -> bool {
    match &r.permissions {
        Some(p) => p.world_writable || p.protected || p.explicit() > 0 || !p.fully_decoded,
        None => true,
    }
}

pub fn ui(ui: &mut egui::Ui, tree: &Arc<Tree>, state: &mut State) -> Option<Outcome> {
    if changed(&mut state.seen, tree) {
        state.job = None;
        state.report = None;
        state.selected = None;
    }
    ui.horizontal_wrapped(|ui| {
        let read = ui.add_enabled(state.job.is_none(), egui::Button::new("Read permissions"));
        if read.clicked() {
            let (tree, files) = (tree.clone(), state.files);
            state.error.clear();
            state.selected = None;
            state.job = Some(Worker::spawn(ui.ctx(), "keel-du-permissions", move |h| {
                Ok(permissions::scan(&tree, files, &h))
            }));
        }
        ui.checkbox(&mut state.files, "Files too");
        ui.separator();
        ui.selectable_value(&mut state.show, Show::All, "All");
        ui.selectable_value(&mut state.show, Show::Flagged, "Flagged");
    });
    if let Some(job) = &state.job {
        match job.poll() {
            None => job.progress(ui, "items read"),
            Some(r) => {
                match r {
                    Ok(report) => state.report = Some(report),
                    Err(e) => state.error = format!("{e:#}"),
                }
                state.job = None;
            }
        }
    }
    error_line(ui, &state.error);
    let report = state.report.as_ref()?;
    let c = report.counts();
    ui.horizontal_wrapped(|ui| {
        let count = |n: u64| crate::library::count(n);
        let mut text = format!(
            "{} read · {} writable by everyone · {} not inheriting · {} with explicit entries",
            count(report.rows.len() as u64),
            count(c.world_writable),
            count(c.protected),
            count(c.explicit),
        );
        if c.errors > 0 {
            text += &format!(" · {} not read", count(c.errors));
        }
        if report.cancelled {
            text += " · cancelled: incomplete";
        }
        ui.label(text);
        if ui.add_enabled(!state.export.busy(), egui::Button::new("Save CSV…")).clicked() {
            let r = report.clone();
            state.export.start(ui.ctx(), "permissions.csv".into(), "CSV", "csv", move |w| {
                Ok(permissions::write_csv(&r, w)?)
            });
        }
    });
    state.export.ui(ui);
    let shown: Vec<usize> = (0..report.rows.len())
        .filter(|&i| state.show == Show::All || flagged(&report.rows[i]))
        .collect();
    let mut out = None;
    let warn = ui.visuals().warn_fg_color;
    let half = (ui.available_height() * 0.6).max(120.0);
    ui.push_id("du-perm-rows", |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(half)
            .sense(egui::Sense::click())
            .column(Column::initial(260.0).at_least(80.0).clip(true))
            .column(Column::initial(160.0).clip(true))
            .column(Column::initial(90.0))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(20.0, |mut h| {
                for t in ["Path", "Owner", "Inheritance", "Explicit", "Notes"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, shown.len(), |mut row| {
                    let i = shown[row.index()];
                    let r = &report.rows[i];
                    row.set_selected(state.selected == Some(i));
                    row.col(|ui| {
                        ui.add(egui::Label::new(r.path.display().to_string()).truncate());
                    });
                    match &r.permissions {
                        Some(p) => {
                            row.col(|ui| {
                                ui.add(egui::Label::new(p.owner.as_str()).truncate())
                                    .on_hover_text(&p.owner_id);
                            });
                            row.col(|ui| {
                                if let Some(mode) = p.mode {
                                    ui.monospace(format!("{mode:04o}"));
                                } else if p.protected {
                                    ui.colored_label(warn, "Broken");
                                } else {
                                    ui.label("Inherits");
                                }
                            });
                            row.col(|ui| {
                                ui.label(p.explicit().to_string());
                            });
                            row.col(|ui| {
                                let mut notes = Vec::new();
                                if p.world_writable {
                                    notes.push("Writable by everyone");
                                }
                                if !p.fully_decoded {
                                    notes.push("Some entries not decoded");
                                }
                                if notes.is_empty() {
                                    ui.label("");
                                } else {
                                    ui.colored_label(warn, notes.join(" · "));
                                }
                            });
                        }
                        None => {
                            for _ in 0..3 {
                                row.col(|ui| {
                                    ui.label("");
                                });
                            }
                            row.col(|ui| {
                                let e = r.error.as_deref().unwrap_or("not read");
                                ui.colored_label(ui.visuals().error_fg_color, e);
                            });
                        }
                    }
                    let resp = row.response();
                    if resp.named(&r.path.display().to_string()).clicked() {
                        state.selected = Some(i);
                        out = Some(Outcome::Select(r.node));
                    }
                });
            });
    });
    // The selected item's entries.
    if let Some(p) = (state.selected)
        .and_then(|i| report.rows.get(i))
        .and_then(|r| r.permissions.as_ref())
    {
        ui.separator();
        let mut head = format!("Owner {} ({})", p.owner, p.owner_id);
        if let Some(g) = &p.group {
            head += &format!(" · group {g}");
        }
        ui.strong(head);
        if p.entries.is_empty() && p.mode.is_none() {
            ui.label("No access entries: nobody but the owner and administrators has access.");
        }
        ui.push_id("du-perm-entries", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::initial(200.0).clip(true))
                .column(Column::initial(60.0))
                .column(Column::initial(130.0))
                .column(Column::initial(80.0))
                .column(Column::remainder().clip(true))
                .header(20.0, |mut h| {
                    for t in ["Who", "Type", "Access", "Source", "Applies to"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|mut body| {
                    for e in &p.entries {
                        body.row(20.0, |mut row| {
                            row.col(|ui| {
                                ui.add(egui::Label::new(e.trustee.as_str()).truncate())
                                    .on_hover_text(&e.sid);
                            });
                            row.col(|ui| {
                                ui.label(e.kind.as_str());
                            });
                            row.col(|ui| {
                                ui.label(e.access.as_str()).on_hover_text(format!("{:#x}", e.mask));
                            });
                            row.col(|ui| {
                                ui.label(if e.inherited { "Inherited" } else { "Explicit" });
                            });
                            row.col(|ui| {
                                ui.label(e.applies_to.as_str());
                            });
                        });
                    }
                });
        });
    }
    out
}
