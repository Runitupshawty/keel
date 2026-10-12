//! Exact-path result omissions. Never deletes files or changes the index.
use keel_search::Hit;
use keel_vfs::VPath;
use std::collections::HashSet;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Omissions {
    pub permanent: Vec<VPath>,
    pub show_omitted: bool,
    #[serde(skip)]
    pub temporary: Vec<VPath>,
    #[serde(skip)]
    pub mode: bool,
    #[serde(skip)]
    pub manage: bool,
}

impl Omissions {
    pub fn filter(&self) -> Filter {
        Filter(if self.show_omitted {
            HashSet::new()
        } else {
            self.permanent.iter().chain(&self.temporary).cloned().collect()
        })
    }

    pub fn add(&mut self, paths: Vec<VPath>, temporary: bool) {
        let list = if temporary { &mut self.temporary } else { &mut self.permanent };
        for path in paths {
            if !list.contains(&path) { list.push(path); }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> bool {
        let mut open = self.manage;
        let mut changed = false;
        if open {
            egui::Window::new("Manage omissions").open(&mut open).show(ctx, |ui| {
                ui.label("Exact result paths only. Files and folders are never deleted.");
                changed |= ui.checkbox(&mut self.show_omitted, "Show omitted results").changed();
                ui.checkbox(&mut self.mode, "Omission mode: Delete omits results temporarily");
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for (label, list) in [("Saved in this profile", &mut self.permanent), ("Temporary (this process)", &mut self.temporary)] {
                        ui.heading(label);
                        let mut remove = None;
                        for (i, path) in list.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("Restore").clicked() { remove = Some(i); }
                                ui.label(path.display());
                            });
                        }
                        if let Some(i) = remove { list.remove(i); changed = true; }
                        if !list.is_empty() && ui.button(format!("Clear {label}")).clicked() {
                            list.clear(); changed = true;
                        }
                    }
                });
            });
        }
        self.manage = open;
        changed
    }
}

#[derive(Clone, Debug, Default)]
pub struct Filter(HashSet<VPath>);

impl Filter {
    pub fn apply(&self, mut hits: Vec<Hit>) -> Vec<Hit> {
        hits.retain(|hit| !self.0.contains(&hit.path));
        hits
    }
}

impl crate::state::AppState {
    pub fn refresh_omissions(&mut self) {
        for pane in &mut self.panes {
            for tab in &mut pane.tabs {
                if let crate::tab::TabKind::Search { due, req, .. } = &mut tab.kind {
                    // Reject already-running answers from before this change.
                    *req = 0;
                    *due = Some(std::time::Instant::now());
                    tab.set_listing(crate::tab::Listing::new(Vec::new()));
                }
            }
        }
        self.ctx.request_repaint();
    }
}
