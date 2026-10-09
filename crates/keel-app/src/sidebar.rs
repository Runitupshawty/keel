//! Left sidebar: Quick access, Drives, Remotes (Phase 3), Cloud (Phase 4).

use crate::keys::Action;
use crate::theme::Theme;
use humansize::{format_size, DECIMAL};
use keel_vfs::VPath;
use std::time::{Duration, Instant};

/// `(name, label, free bytes, total bytes)` as returned by `keel_vfs::drives()`.
pub type Drive = (String, String, u64, u64);

pub const DRIVES_REFRESH: Duration = Duration::from_secs(30);

pub struct Sidebar {
    pub quick: Vec<(String, VPath)>,
    pub drives: Vec<Drive>,
    /// When the last `drives()` request was sent (None = never).
    pub drives_requested: Option<Instant>,
    /// Configured SFTP hosts (`sidebar_remotes`), rebuilt each frame by the state.
    pub remotes: Vec<crate::sidebar_remotes::RemoteRow>,
}

impl Default for Sidebar {
    fn default() -> Self {
        let mut quick = Vec::new();
        if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_owned()) {
            quick.push(("Home".to_owned(), VPath::local(home)));
        }
        if let Some(u) = directories::UserDirs::new() {
            let dirs = [
                ("Desktop", u.desktop_dir()),
                ("Documents", u.document_dir()),
                ("Downloads", u.download_dir()),
                ("Pictures", u.picture_dir()),
            ];
            for (label, dir) in dirs {
                if let Some(d) = dir {
                    quick.push((label.to_owned(), VPath::local(d)));
                }
            }
        }
        Self {
            quick,
            drives: Vec::new(),
            drives_requested: None,
            remotes: Vec::new(),
        }
    }
}

impl Sidebar {
    /// The drive whose mount point is the longest prefix of `dir`.
    pub fn drive_of(&self, dir: &VPath) -> Option<&Drive> {
        let shown = dir.display().to_lowercase();
        self.drives
            .iter()
            .filter(|(name, ..)| shown.starts_with(&name.to_lowercase()))
            .max_by_key(|(name, ..)| name.len())
    }

    /// Draws the sidebar; clicks become `Navigate`, middle-clicks `NewTabAt`.
    pub fn ui(&self, ui: &mut egui::Ui, theme: &Theme, current: &VPath, out: &mut Vec<Action>) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            let item = |ui: &mut egui::Ui, path: &VPath, text: &str| {
                let icon = if path == current {
                    crate::icons::folder_open()
                } else {
                    crate::icons::folder()
                };
                ui.add(
                    egui::Button::image_and_text(
                        egui::Image::new(icon).fit_to_exact_size([16.0, 16.0].into()),
                        text,
                    )
                    .frame(false),
                )
            };
            section(ui, "Quick access", theme);
            for (label, path) in &self.quick {
                let r = item(ui, path, label).on_hover_text(path.display());
                clicked(&r, path, out);
            }
            ui.add_space(8.0);
            section(ui, "Drives", theme);
            if self.drives.is_empty() {
                ui.weak("Loading…");
            }
            for (name, label, free, total) in &self.drives {
                let text = if label.is_empty() || cfg!(not(windows)) {
                    name.clone()
                } else {
                    format!("{label} ({name})")
                };
                let path = VPath::local(if cfg!(windows) {
                    format!("{name}\\")
                } else {
                    name.clone()
                });
                let r = item(ui, &path, &text);
                clicked(&r, &path, out);
                if *total > 0 {
                    let used = 1.0 - *free as f32 / *total as f32;
                    ui.add(
                        egui::ProgressBar::new(used)
                            .desired_height(4.0)
                            .fill(if used > 0.9 {
                                ui.visuals().error_fg_color
                            } else {
                                theme.accent()
                            }),
                    )
                    .on_hover_text(format!(
                        "{} free of {}",
                        format_size(*free, DECIMAL),
                        format_size(*total, DECIMAL)
                    ));
                }
            }
            ui.add_space(8.0);
            section(ui, "Remotes", theme);
            crate::sidebar_remotes::ui(ui, &self.remotes, current, out);
            ui.add_space(8.0);
            section(ui, "Cloud", theme);
            ui.weak("None configured");
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str, theme: &Theme) {
    ui.label(
        egui::RichText::new(title)
            .small()
            .strong()
            .color(theme.muted()),
    );
}

fn clicked(r: &egui::Response, path: &VPath, out: &mut Vec<Action>) {
    if r.clicked() {
        out.push(Action::Navigate(path.clone()));
    } else if r.middle_clicked() {
        out.push(Action::NewTabAt(path.clone()));
    }
}
