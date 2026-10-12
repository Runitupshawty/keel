//! **Largest files**: the top N files of the scan, in a size range.

use super::{changed, error_line, file_table, size_text, Outcome, SizeFields};
use crate::a11y::Named;
use keel_vfs::du::analysis::FileRow;
use keel_vfs::du::{largest::largest, Tree};
use std::sync::Arc;

pub struct State {
    /// How many to list.
    pub top: usize,
    pub size: SizeFields,
    seen: Option<Arc<Tree>>,
    rows: Vec<FileRow>,
    error: String,
}

impl Default for State {
    fn default() -> Self {
        State {
            top: 100,
            size: SizeFields::default(),
            seen: None,
            rows: Vec::new(),
            error: String::new(),
        }
    }
}

impl State {
    #[cfg(test)]
    pub fn rows(&self) -> &[FileRow] {
        &self.rows
    }

    /// Lists again for `tree` with the current settings.
    pub fn apply(&mut self, tree: &Tree) {
        match self.size.range() {
            Ok(range) => {
                self.rows = largest(tree, self.top, range);
                self.error.clear();
            }
            Err(e) => self.error = e,
        }
    }
}

pub fn ui(ui: &mut egui::Ui, tree: &Arc<Tree>, state: &mut State) -> Option<Outcome> {
    let mut apply = changed(&mut state.seen, tree);
    ui.horizontal_wrapped(|ui| {
        ui.label("Top");
        let r = ui.add(egui::DragValue::new(&mut state.top).range(1..=100_000));
        apply |= r.named("Number of files").changed();
        let (edited, _) = state.size.ui(ui);
        apply |= edited;
    });
    if apply {
        state.apply(tree);
    }
    error_line(ui, &state.error);
    let total: u64 = state.rows.iter().map(|r| r.bytes).sum();
    ui.label(format!(
        "{}, {}",
        crate::library::counted(state.rows.len() as u64, "file"),
        size_text(total)
    ));
    file_table(ui, "du-largest", &state.rows, None).map(Outcome::Select)
}
