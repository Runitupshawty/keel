//! **Search**: items of the scan by name (substring or regular expression), size, type,
//! extension and owner, with the results' totals.

use super::{changed, error_line, file_table, size_text, Outcome, SizeFields, Worker};
use crate::a11y::Named;
use keel_vfs::du::analysis::FileRow;
use keel_vfs::du::search::{search, ItemType, Query, Results};
use keel_vfs::du::{permissions, NodeId, Tree};
use std::collections::HashMap;
use std::sync::Arc;

/// Rows the table shows at most (the totals cover every result).
pub const SHOWN: usize = 10_000;

#[derive(Default)]
pub struct State {
    pub name: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub kind: ItemType,
    /// "jpg, png".
    pub extensions: String,
    pub owner: String,
    pub size: SizeFields,
    seen: Option<Arc<Tree>>,
    owners: HashMap<NodeId, String>,
    owner_job: Option<Worker<permissions::Report>>,
    results: Results,
    rows: Vec<FileRow>,
    error: String,
}

impl State {
    /// Files of one extension (the file types list's double-click).
    pub fn for_extension(ext: &str) -> Self {
        State {
            kind: ItemType::File,
            extensions: ext.to_owned(),
            ..State::default()
        }
    }

    pub fn results(&self) -> &Results {
        &self.results
    }

    fn query(&self) -> Result<Query, String> {
        Ok(Query {
            name: self.name.clone(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            size: self.size.range()?,
            kind: self.kind,
            extensions: Query::parse_extensions(&self.extensions),
            owner: self.owner.clone(),
        })
    }

    /// Searches `tree` with the current fields.
    pub fn apply(&mut self, tree: &Tree) {
        let result = (self.query())
            .and_then(|q| search(tree, &q, &self.owners).map_err(|e| format!("{e:#}")));
        match result {
            Ok(r) => {
                self.rows = (r.nodes.iter().take(SHOWN))
                    .filter_map(|&id| FileRow::from_tree(tree, id))
                    .collect();
                self.results = r;
                self.error.clear();
            }
            Err(e) => {
                self.results = Results::default();
                self.rows.clear();
                self.error = e;
            }
        }
    }
}

pub fn ui(ui: &mut egui::Ui, tree: &Arc<Tree>, state: &mut State) -> Option<Outcome> {
    let mut apply = false;
    if changed(&mut state.seen, tree) {
        // Owners belong to the scan they were read for.
        state.owners.clear();
        state.owner_job = None;
        apply = true;
    }
    let enter =
        |r: &egui::Response| r.lost_focus() && r.ctx.input(|i| i.key_pressed(egui::Key::Enter));
    ui.horizontal_wrapped(|ui| {
        ui.label("Name");
        let hint = if state.regex {
            "regular expression"
        } else {
            "part of the name"
        };
        let r = ui.add(
            egui::TextEdit::singleline(&mut state.name)
                .desired_width(200.0)
                .hint_text(hint),
        );
        apply |= enter(&r.named("Name"));
        apply |= ui
            .checkbox(&mut state.regex, "Regular expression")
            .changed();
        apply |= ui
            .checkbox(&mut state.case_sensitive, "Match case")
            .changed();
    });
    ui.horizontal_wrapped(|ui| {
        for (kind, label) in [
            (ItemType::Any, "Everything"),
            (ItemType::File, "Files"),
            (ItemType::Folder, "Folders"),
            (ItemType::Link, "Links"),
        ] {
            apply |= ui.selectable_value(&mut state.kind, kind, label).changed();
        }
        ui.separator();
        ui.label("Extensions");
        let r = ui.add(
            egui::TextEdit::singleline(&mut state.extensions)
                .desired_width(90.0)
                .hint_text("jpg, png"),
        );
        apply |= enter(&r.named("Extensions"));
    });
    ui.horizontal_wrapped(|ui| {
        let (_, submitted) = state.size.ui(ui);
        apply |= submitted;
        ui.separator();
        ui.label("Owner");
        let r = ui.add(
            egui::TextEdit::singleline(&mut state.owner)
                .desired_width(120.0)
                .hint_text("name or SID"),
        );
        apply |= enter(&r.named("Owner"));
        let loading = state.owner_job.is_some();
        let label = if state.owners.is_empty() {
            "Read owners"
        } else {
            "Read owners again"
        };
        let b = ui.add_enabled(!loading, egui::Button::new(label));
        let b = b
            .on_hover_text("Reads the owner of every item of the scan (needed to search by owner)");
        if b.clicked() {
            let tree = tree.clone();
            state.owner_job = Some(Worker::spawn(ui.ctx(), "keel-du-owners", move |h| {
                Ok(permissions::scan(&tree, true, &h))
            }));
        }
        if ui.button("Search").clicked() {
            apply = true;
        }
    });
    if let Some(job) = &state.owner_job {
        match job.poll() {
            None => job.progress(ui, "items read"),
            Some(r) => {
                match r {
                    Ok(report) => {
                        state.owners = report.owners();
                        apply = true;
                    }
                    Err(e) => state.error = format!("{e:#}"),
                }
                state.owner_job = None;
            }
        }
    }
    if apply {
        state.apply(tree);
    }
    error_line(ui, &state.error);
    let r = &state.results;
    let mut summary = format!(
        "{} · {} · {} · {} ({} on disk)",
        crate::library::counted(r.nodes.len() as u64, "result"),
        crate::library::counted(r.files, "file"),
        crate::library::counted(r.folders, "folder"),
        size_text(r.bytes),
        size_text(r.allocated),
    );
    if r.owners_unknown > 0 {
        summary += &format!(
            " · {} without a known owner",
            crate::library::count(r.owners_unknown)
        );
    }
    ui.label(summary);
    if r.nodes.len() > SHOWN {
        ui.weak(format!(
            "Showing the first {}",
            crate::library::count(SHOWN as u64)
        ));
    }
    file_table(ui, "du-search", &state.rows, None).map(Outcome::Select)
}
