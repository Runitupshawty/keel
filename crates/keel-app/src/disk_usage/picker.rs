//! The **Select drives** dialog: all local drives, some drives (each with its used and
//! free space; double-click scans that one), or one or more folders.

use crate::sidebar::Drive;
use humansize::{format_size, DECIMAL};
use keel_vfs::du::Root;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    AllLocal,
    Drives,
    Folders,
}

pub struct Picker {
    pub choice: Choice,
    /// `(path, name shown, free, total, ticked)`.
    pub drives: Vec<(PathBuf, String, u64, u64, bool)>,
    pub folders: Vec<PathBuf>,
    /// The OS folder dialog, open on a worker.
    browsing: Option<Receiver<Vec<PathBuf>>>,
}

/// The dialog's answer.
pub enum Answer {
    Scan(Vec<Root>),
    Cancel,
}

impl Picker {
    pub fn new(drives: &[Drive]) -> Self {
        let drives = (drives.iter())
            .map(|(name, label, free, total)| {
                let path = PathBuf::from(if cfg!(windows) {
                    format!("{name}\\")
                } else {
                    name.clone()
                });
                let text = if label.is_empty() || cfg!(not(windows)) {
                    name.clone()
                } else {
                    format!("{label} ({name})")
                };
                (path, text, *free, *total, false)
            })
            .collect();
        Picker {
            choice: Choice::AllLocal,
            drives,
            folders: Vec::new(),
            browsing: None,
        }
    }

    /// The roots the dialog's state asks for (empty: nothing chosen yet).
    pub fn roots(&self) -> Vec<Root> {
        match self.choice {
            Choice::AllLocal => vec![Root::AllLocalDrives],
            Choice::Drives => (self.drives.iter())
                .filter(|d| d.4)
                .map(|d| Root::Path(d.0.clone()))
                .collect(),
            Choice::Folders => self.folders.iter().cloned().map(Root::Path).collect(),
        }
    }

    /// Adds folders, each once.
    pub fn add_folders(&mut self, folders: Vec<PathBuf>) {
        for f in folders {
            if !self.folders.contains(&f) {
                self.folders.push(f);
            }
        }
        self.choice = Choice::Folders;
    }

    fn browse(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        crate::worker::spawn("keel-du-pick", move || {
            let picked = crate::platform::file_dialog("Folders to scan", |d| d.pick_folders())
                .unwrap_or_default();
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.browsing = Some(rx);
    }

    /// Draws the dialog (a modal); Some once it is answered.
    pub fn ui(&mut self, ctx: &egui::Context, accent: egui::Color32) -> Option<Answer> {
        if let Some(rx) = &self.browsing {
            if let Ok(picked) = rx.try_recv() {
                self.browsing = None;
                self.add_folders(picked);
            }
        }
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("keel-du-picker")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Select drives");
            ui.add_space(4.0);
            ui.radio_value(&mut self.choice, Choice::AllLocal, "All local drives");
            ui.radio_value(&mut self.choice, Choice::Drives, "Individual drives");
            let drives_on = self.choice == Choice::Drives;
            ui.indent("keel-du-drives", |ui| {
                if self.drives.is_empty() {
                    ui.weak("No drives found");
                }
                let mut scan_one = None;
                for (path, text, free, total, ticked) in self.drives.iter_mut() {
                    ui.horizontal(|ui| {
                        let r = ui.checkbox(ticked, text.as_str());
                        if r.changed() && *ticked {
                            self.choice = Choice::Drives;
                        }
                        if r.double_clicked() {
                            scan_one = Some(path.clone());
                        }
                        if *total > 0 {
                            let used = 1.0 - *free as f32 / *total as f32;
                            ui.add(
                                egui::ProgressBar::new(used)
                                    .desired_width(120.0)
                                    .desired_height(8.0)
                                    .fill(if used > 0.9 {
                                        ui.visuals().error_fg_color
                                    } else {
                                        accent
                                    }),
                            );
                            ui.weak(format!(
                                "{} free of {}",
                                format_size(*free, DECIMAL),
                                format_size(*total, DECIMAL)
                            ));
                        }
                    })
                    .response
                    .on_hover_text(format!("Double-click to scan {}", path.display()));
                }
                if let Some(path) = scan_one {
                    answer = Some(Answer::Scan(vec![Root::Path(path)]));
                }
                if !drives_on && self.drives.iter().any(|d| d.4) {
                    ui.weak("Choose Individual drives to scan the ticked ones");
                }
            });
            ui.radio_value(&mut self.choice, Choice::Folders, "Folders");
            ui.indent("keel-du-folders", |ui| {
                let mut remove = None;
                for (i, f) in self.folders.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("×")
                            .on_hover_text("Remove from the list")
                            .clicked()
                        {
                            remove = Some(i);
                        }
                        ui.label(f.display().to_string());
                    });
                }
                if let Some(i) = remove {
                    self.folders.remove(i);
                }
                ui.horizontal(|ui| {
                    let idle = self.browsing.is_none();
                    if ui
                        .add_enabled(idle, egui::Button::new("Add folders…"))
                        .clicked()
                    {
                        self.browse(ui.ctx());
                    }
                    if !idle {
                        crate::anim::spinner(ui);
                    }
                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let roots = self.roots();
                let scan = ui.add_enabled(!roots.is_empty(), egui::Button::new("Scan"));
                crate::a11y::first_focus(&scan);
                if scan.clicked() {
                    answer = Some(Answer::Scan(roots));
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(Answer::Cancel);
                }
            });
        });
        if answer.is_none() && modal.should_close() && self.browsing.is_none() {
            answer = Some(Answer::Cancel);
        }
        answer
    }
}
