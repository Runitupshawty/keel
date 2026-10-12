//! **Storage analytics**: the scan's files in user-defined storage tiers by how long ago
//! they changed, with the estimated monthly cost per tier, per extension and per top
//! folder.

use super::{changed, error_line, size_text, Worker};
use crate::a11y::Named;
use egui_extras::{Column, TableBuilder};
use keel_vfs::du::storage::{analyze, Analytics, Tier, Total};
use keel_vfs::du::Tree;
use std::sync::Arc;

/// Rows per breakdown table.
const ROWS: usize = 200;

pub struct State {
    pub tiers: Vec<Tier>,
    seen: Option<Arc<Tree>>,
    job: Option<Worker<Analytics>>,
    result: Option<Analytics>,
    error: String,
}

impl Default for State {
    fn default() -> Self {
        State {
            tiers: Tier::defaults(),
            seen: None,
            job: None,
            result: None,
            error: String::new(),
        }
    }
}

impl State {
    #[cfg(test)]
    pub fn result(&self) -> Option<&Analytics> {
        self.result.as_ref()
    }

    fn start(&mut self, ctx: &egui::Context, tree: &Arc<Tree>) {
        if let Err(e) = Tier::check(&self.tiers) {
            self.error = e;
            self.job = None;
            return;
        }
        let (tree, tiers) = (tree.clone(), self.tiers.clone());
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        self.job = Some(Worker::spawn(ctx, "keel-du-storage", move |_| {
            analyze(&tree, &tiers, now).map_err(anyhow::Error::msg)
        }));
    }
}

pub fn ui(ui: &mut egui::Ui, tree: &Arc<Tree>, state: &mut State) {
    let mut apply = changed(&mut state.seen, tree);
    ui.label("Storage tiers by last change (price per GB per month, any currency)");
    let mut remove = None;
    let last = state.tiers.len().saturating_sub(1);
    for (i, tier) in state.tiers.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut tier.name).desired_width(90.0));
                apply |= r.named("Tier name").lost_focus();
                match &mut tier.younger_than_days {
                    Some(days) => {
                        ui.label("changed in the last");
                        let r =
                            ui.add(egui::DragValue::new(days).range(1..=36_500).suffix(" days"));
                        apply |= r.named("Days").changed();
                    }
                    None => {
                        ui.label("everything older");
                    }
                }
                ui.label("price");
                let r = ui.add(
                    egui::DragValue::new(&mut tier.cost_per_gb)
                        .speed(0.001)
                        .range(0.0..=1e6)
                        .max_decimals(4),
                );
                apply |= r.named("Price per GB per month").changed();
                if i < last && ui.small_button("Remove").named("Remove tier").clicked() {
                    remove = Some(i);
                }
            });
        });
    }
    if let Some(i) = remove {
        state.tiers.remove(i);
        apply = true;
    }
    if ui.button("Add tier").clicked() {
        // Before the last (unbounded) tier, a year past the oldest bound.
        let days = (state.tiers.iter().filter_map(|t| t.younger_than_days).max()).unwrap_or(0);
        let at = state.tiers.len().saturating_sub(1);
        state.tiers.insert(
            at,
            Tier {
                name: format!("Tier {}", at + 1),
                younger_than_days: Some(days.saturating_add(365).min(36_500)),
                cost_per_gb: 0.0,
            },
        );
        apply = true;
    }
    if apply {
        state.error.clear();
        state.start(ui.ctx(), tree);
    }
    if let Some(job) = &state.job {
        match job.poll() {
            None => job.progress(ui, "files"),
            Some(r) => {
                match r {
                    Ok(a) => state.result = Some(a),
                    Err(e) => state.error = format!("{e:#}"),
                }
                state.job = None;
            }
        }
    }
    error_line(ui, &state.error);
    let Some(a) = &state.result else {
        return;
    };
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("du-storage-results")
        .show(ui, |ui| results(ui, &state.tiers, a));
}

fn results(ui: &mut egui::Ui, tiers: &[Tier], a: &Analytics) {
    let all = a.total.bytes.max(1) as f64;
    let mut rows: Vec<(String, &Total)> = (tiers.iter().zip(&a.tiers))
        .map(|(t, total)| (t.name.clone(), total))
        .collect();
    if a.undated.files > 0 {
        rows.push(("Date not known (not priced)".into(), &a.undated));
    }
    rows.push(("Total".into(), &a.total));
    table(ui, "du-storage-tiers", "Tier", &rows, all);
    egui::CollapsingHeader::new("By extension")
        .default_open(true)
        .show(ui, |ui| {
            let rows: Vec<(String, &Total)> = (a.extensions.iter().take(ROWS))
                .map(|(e, t)| {
                    (
                        if e.is_empty() {
                            "(none)".into()
                        } else {
                            format!(".{e}")
                        },
                        t,
                    )
                })
                .collect();
            table(ui, "du-storage-ext", "Extension", &rows, all);
        });
    egui::CollapsingHeader::new("By top folder")
        .default_open(true)
        .show(ui, |ui| {
            let rows: Vec<(String, &Total)> = (a.folders.iter().take(ROWS))
                .map(|(f, t)| (f.clone(), t))
                .collect();
            table(ui, "du-storage-folders", "Folder", &rows, all);
        });
}

/// Name, files, size, share of all bytes, monthly cost.
fn table(ui: &mut egui::Ui, salt: &str, first: &str, rows: &[(String, &Total)], all: f64) {
    TableBuilder::new(ui)
        .id_salt(salt)
        .striped(true)
        .vscroll(false)
        .column(Column::initial(220.0).at_least(80.0).clip(true))
        .columns(Column::initial(90.0), 4)
        .header(20.0, |mut h| {
            for title in [first, "Files", "Size", "Share", "Per month"] {
                h.col(|ui| {
                    ui.strong(title);
                });
            }
        })
        .body(|mut body| {
            for (name, t) in rows {
                body.row(20.0, |mut row| {
                    row.col(|ui| {
                        ui.add(egui::Label::new(name.as_str()).truncate());
                    });
                    row.col(|ui| {
                        ui.label(crate::library::count(t.files));
                    });
                    row.col(|ui| {
                        ui.label(size_text(t.bytes));
                    });
                    row.col(|ui| {
                        ui.label(format!("{:.1} %", t.bytes as f64 / all * 100.0));
                    });
                    row.col(|ui| {
                        ui.label(cost_text(t.monthly_cost));
                    });
                });
            }
        });
}

pub fn cost_text(cost: f64) -> String {
    if cost >= 100.0 {
        format!("{cost:.0}")
    } else if cost >= 0.01 || cost == 0.0 {
        format!("{cost:.2}")
    } else {
        format!("{cost:.4}")
    }
}
