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
            self.permanent
                .iter()
                .chain(&self.temporary)
                .cloned()
                .collect()
        })
    }

    /// Same omitted paths, toggle and omission mode (each window's dialog state aside).
    pub fn same_lists(&self, other: &Self) -> bool {
        self.permanent == other.permanent
            && self.temporary == other.temporary
            && self.show_omitted == other.show_omitted
            && self.mode == other.mode
    }

    /// Takes `other`'s lists, toggle and mode; keeps this window's dialog open or closed.
    pub fn copy_lists(&mut self, other: &Self) {
        *self = Self {
            manage: self.manage,
            ..other.clone()
        };
    }

    pub fn add(&mut self, paths: Vec<VPath>, temporary: bool) {
        let list = if temporary {
            &mut self.temporary
        } else {
            &mut self.permanent
        };
        for path in paths {
            if !list.contains(&path) {
                list.push(path);
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> bool {
        let mut open = self.manage;
        let mut changed = false;
        if open {
            egui::Window::new("Manage omissions")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Exact result paths only. Files and folders are never deleted.");
                    changed |= ui
                        .checkbox(&mut self.show_omitted, "Show omitted results")
                        .changed();
                    ui.checkbox(
                        &mut self.mode,
                        "Omission mode: Delete omits results temporarily",
                    );
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for (label, list) in [
                                ("Saved in this profile", &mut self.permanent),
                                ("Temporary (this process)", &mut self.temporary),
                            ] {
                                ui.heading(label);
                                let mut remove = None;
                                for (i, path) in list.iter().enumerate() {
                                    ui.horizontal(|ui| {
                                        if ui.small_button("Restore").clicked() {
                                            remove = Some(i);
                                        }
                                        ui.label(path.display());
                                    });
                                }
                                if let Some(i) = remove {
                                    list.remove(i);
                                    changed = true;
                                }
                                if !list.is_empty() && ui.button(format!("Clear {label}")).clicked()
                                {
                                    list.clear();
                                    changed = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str) -> Hit {
        Hit {
            path: VPath::local(path),
            is_dir: false,
            size: 0,
            modified: None,
            snippet: None,
        }
    }

    fn paths(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| h.path.display()).collect()
    }

    #[test]
    fn filter_drops_permanent_and_temporary_paths_only() {
        let mut o = Omissions::default();
        o.add(vec![VPath::local("/x/a.txt")], false);
        o.add(vec![VPath::local("/x/b.txt")], true);
        let hits = vec![
            hit("/x/a.txt"),
            hit("/x/b.txt"),
            hit("/x/c.txt"),
            hit("/x/a.txt.bak"),
        ];
        assert_eq!(
            paths(&o.filter().apply(hits.clone())),
            paths(&[hit("/x/c.txt"), hit("/x/a.txt.bak")])
        );
        // Show omitted: nothing is dropped, the lists are kept.
        o.show_omitted = true;
        assert_eq!(o.filter().apply(hits.clone()).len(), 4);
        assert_eq!((o.permanent.len(), o.temporary.len()), (1, 1));
        // An empty filter keeps everything.
        assert_eq!(Filter::default().apply(hits).len(), 4);
    }

    #[test]
    fn windows_share_lists_but_not_the_dialog() {
        let mut main = Omissions {
            manage: true,
            ..Default::default()
        };
        let mut quick = Omissions::default();
        assert!(quick.same_lists(&main), "the open dialog is not a difference");
        main.add(vec![VPath::local("/x/a.txt")], true);
        assert!(!quick.same_lists(&main));
        quick.copy_lists(&main);
        assert!(quick.same_lists(&main));
        assert!(!quick.manage, "the quick window's dialog stays closed");
    }

    #[test]
    fn add_skips_duplicates_per_list() {
        let mut o = Omissions::default();
        let a = VPath::local("/x/a.txt");
        o.add(vec![a.clone(), a.clone()], false);
        o.add(vec![a.clone()], false);
        o.add(vec![a.clone()], true);
        assert_eq!(o.permanent.as_slice(), std::slice::from_ref(&a));
        assert_eq!(o.temporary, [a]);
    }

    #[test]
    fn only_permanent_omissions_and_the_toggle_are_saved() {
        let mut o = Omissions {
            show_omitted: true,
            mode: true,
            manage: true,
            ..Default::default()
        };
        o.add(vec![VPath::local("/x/a.txt")], false);
        o.add(vec![VPath::local("/x/b.txt")], true);
        let back: Omissions = toml::from_str(&toml::to_string(&o).unwrap()).unwrap();
        assert_eq!(back.permanent, o.permanent);
        assert!(back.show_omitted);
        assert!(back.temporary.is_empty() && !back.mode && !back.manage);
    }
}
