//! Left sidebar: Quick access, Drives, Open archives, Remotes (Phase 3), Cloud (Phase 4).

use crate::keys::Action;
use crate::theme::Theme;
use humansize::{format_size, DECIMAL};
use keel_vfs::VPath;
use std::time::{Duration, Instant};

/// `(name, label, free bytes, total bytes)` as returned by `keel_vfs::drives()`.
pub type Drive = (String, String, u64, u64);

pub const DRIVES_REFRESH: Duration = Duration::from_secs(30);
/// A drive list that has not answered after this long (a dead network volume) is shown
/// as not responding; no new request starts until it answers.
pub const DRIVES_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Sidebar {
    pub quick: Vec<(String, VPath)>,
    pub drives: Vec<Drive>,
    /// When the last `drives()` request was sent (None = never).
    pub drives_requested: Option<Instant>,
    /// A `drives()` request is running (since then).
    pub drives_pending: Option<Instant>,
    /// The running request passed `DRIVES_TIMEOUT`.
    pub drives_stuck: bool,
    /// Configured SFTP hosts (`sidebar_remotes`), rebuilt each frame by the state.
    pub remotes: Vec<crate::sidebar_remotes::RemoteRow>,
    /// Configured cloud accounts, rebuilt each frame by the state.
    pub clouds: Vec<crate::sidebar_remotes::CloudRow>,
    /// Paired devices (None while devices are not running), rebuilt each frame.
    pub devices: Option<Vec<crate::devices::DeviceRow>>,
    /// Why devices are not running.
    pub devices_note: Option<String>,
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
            drives_pending: None,
            drives_stuck: false,
            remotes: Vec::new(),
            clouds: Vec::new(),
            devices: None,
            devices_note: None,
        }
    }
}

impl Sidebar {
    /// The drive whose mount point holds `dir` (whole path components: `/media/usb` is
    /// not the drive of `/media/usb2`), the deepest one when mounts nest.
    pub fn drive_of(&self, dir: &VPath) -> Option<&Drive> {
        let fold = |s: String| if cfg!(windows) { s.to_lowercase() } else { s };
        let local = fold(dir.to_local_path()?.to_string_lossy().into_owned());
        let local = std::path::Path::new(&local);
        self.drives
            .iter()
            .filter(|(name, ..)| local.starts_with(fold(name.clone())))
            .max_by_key(|(name, ..)| name.len())
    }

    /// Draws the sidebar; clicks become `Navigate`, middle-clicks `NewTabAt`. `archives`:
    /// the archive each (pane, tab) is browsing; a click shows that tab.
    pub fn ui(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        current: &VPath,
        archives: &[(usize, usize, VPath)],
        library: &crate::library::LibraryUi,
        out: &mut Vec<Action>,
    ) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            // --- Task 29 ---
            crate::library_ui::sidebar(ui, library, theme, current, out);
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
            if self.drives_stuck {
                ui.colored_label(ui.visuals().warn_fg_color, "Drives are not responding")
                    .on_hover_text("A drive (often a disconnected network drive) is not answering");
            } else if self.drives.is_empty() {
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
            if !archives.is_empty() {
                ui.add_space(8.0);
                section(ui, "Open archives", theme);
                for (pane, tab, archive) in archives {
                    let r = ui
                        .add(
                            egui::Button::image_and_text(
                                egui::Image::new(crate::icons::archive())
                                    .fit_to_exact_size([16.0, 16.0].into()),
                                archive.name(),
                            )
                            .frame(false),
                        )
                        .on_hover_text(archive.display());
                    if r.clicked() {
                        out.push(Action::FocusTab {
                            pane: *pane,
                            tab: *tab,
                        });
                    }
                }
            }
            ui.add_space(8.0);
            section(ui, "Remotes", theme);
            crate::sidebar_remotes::ui(ui, &self.remotes, current, out);
            ui.add_space(8.0);
            section(ui, "Cloud", theme);
            crate::sidebar_remotes::cloud_ui(ui, &self.clouds, current, out);
            ui.add_space(8.0);
            section(ui, "Devices", theme);
            crate::devices::sidebar(
                ui,
                self.devices.as_deref(),
                self.devices_note.as_deref(),
                current,
                theme.accent(),
                out,
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_of_matches_whole_path_components() {
        let mut s = Sidebar::default();
        let drive = |n: &str| (n.to_owned(), String::new(), 1, 2);
        if cfg!(windows) {
            s.drives = vec![drive("C:"), drive("D:")];
            let of = |p: &str| s.drive_of(&VPath::local(p)).map(|d| d.0.clone());
            assert_eq!(of(r"c:\Users\x").as_deref(), Some("C:"));
            assert_eq!(of(r"D:\").as_deref(), Some("D:"));
            assert_eq!(of(r"E:\x"), None);
        } else {
            s.drives = vec![drive("/"), drive("/media/usb"), drive("/media/usb2")];
            let of = |p: &str| s.drive_of(&VPath::local(p)).map(|d| d.0.clone());
            assert_eq!(of("/media/usb2/photos").as_deref(), Some("/media/usb2"));
            assert_eq!(of("/media/usb/x").as_deref(), Some("/media/usb"));
            assert_eq!(of("/media/usbx").as_deref(), Some("/"));
        }
        let remote = VPath::parse("sftp://host/media/usb").unwrap();
        assert_eq!(s.drive_of(&remote), None);
    }
}
