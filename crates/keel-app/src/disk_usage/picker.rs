//! The **Select drives** dialog: all local drives, some drives and portable-device storages
//! (each with its used and free space; double-click scans that one), or one or more folders.

use crate::i18n::tr;
use crate::sidebar::Drive;
use humansize::{format_size, DECIMAL};
use keel_vfs::du::{Root, ScannableKind, ScannableRoot, Space};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::Duration;

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
    /// `(root, name shown, size and free space, ticked)`: phones' and cameras' storages.
    devices: Vec<(Root, String, Option<Space>, bool)>,
    pub folders: Vec<PathBuf>,
    /// The device list, read on a worker (it talks to every device).
    listing_devices: Option<Receiver<Vec<ScannableRoot>>>,
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
        let mut picker = Self::with_drives(drives);
        // Portable devices are a Windows thing (`Root::Mtp`); elsewhere the list is empty.
        if cfg!(windows) {
            picker.list_devices();
        }
        picker
    }

    /// The dialog for `drives`, without looking for devices.
    fn with_drives(drives: &[Drive]) -> Self {
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
            devices: Vec::new(),
            folders: Vec::new(),
            listing_devices: None,
            browsing: None,
        }
    }

    /// Lists the connected devices' storages on a worker (`ui` picks the list up).
    fn list_devices(&mut self) {
        let (tx, rx) = channel();
        let started = crate::worker::spawn("keel-du-devices", move || {
            let devices = (keel_vfs::du::list_scannable_roots().into_iter())
                .filter(|r| r.kind == ScannableKind::Device)
                .collect();
            let _ = tx.send(devices);
        });
        self.listing_devices = started.then_some(rx);
    }

    /// The names shown for the listed device storages (for the scan's progress line).
    pub fn device_labels(&self) -> Vec<(Root, String)> {
        (self.devices.iter())
            .map(|(root, label, ..)| (root.clone(), label.clone()))
            .collect()
    }

    /// The roots the dialog's state asks for (empty: nothing chosen yet).
    pub fn roots(&self) -> Vec<Root> {
        match self.choice {
            Choice::AllLocal => vec![Root::AllLocalDrives],
            Choice::Drives => self
                .drives
                .iter()
                .filter(|d| d.4)
                .map(|d| Root::Path(d.0.clone()))
                .chain(self.devices.iter().filter(|d| d.3).map(|d| d.0.clone()))
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
            let picked = crate::platform::file_dialog(tr("du_picker.dialog_title"), |d| d.pick_folders())
                .unwrap_or_default();
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.browsing = Some(rx);
    }

    /// Draws the dialog (a modal); Some once it is answered.
    pub fn ui(&mut self, ctx: &egui::Context, accent: egui::Color32) -> Option<Answer> {
        if let Some(rx) = &self.listing_devices {
            match rx.try_recv() {
                Ok(found) => {
                    self.devices = (found.into_iter())
                        .map(|r| (r.root, r.label, r.space, false))
                        .collect();
                    self.listing_devices = None;
                }
                // The worker has no context to wake the UI with: look again shortly.
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
                Err(TryRecvError::Disconnected) => self.listing_devices = None,
            }
        }
        if let Some(rx) = &self.browsing {
            if let Ok(picked) = rx.try_recv() {
                self.browsing = None;
                self.add_folders(picked);
            }
        }
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("keel-du-picker")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(tr("du_picker.title"));
            ui.add_space(4.0);
            ui.radio_value(&mut self.choice, Choice::AllLocal, tr("du_picker.all_local"));
            ui.radio_value(&mut self.choice, Choice::Drives, tr("du_picker.individual"));
            let drives_on = self.choice == Choice::Drives;
            ui.indent("keel-du-drives", |ui| {
                if self.drives.is_empty() {
                    ui.weak(tr("du_picker.no_drives"));
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
                        space_bar(ui, *free, *total, accent);
                    })
                    .response
                    .on_hover_text(scan_tip(&path.display().to_string()));
                }
                if let Some(path) = scan_one {
                    answer = Some(Answer::Scan(vec![Root::Path(path)]));
                }
                if self.listing_devices.is_some() || !self.devices.is_empty() {
                    ui.add_space(6.0);
                    ui.strong(tr("disk_usage.phones_and_cameras"));
                    if self.listing_devices.is_some() {
                        ui.horizontal(|ui| {
                            crate::anim::spinner(ui);
                            ui.weak(tr("disk_usage.looking_for_devices"));
                        });
                    }
                    let mut scan_one = None;
                    for (root, label, space, ticked) in self.devices.iter_mut() {
                        ui.horizontal(|ui| {
                            let r = ui.checkbox(ticked, label.as_str());
                            if r.changed() && *ticked {
                                self.choice = Choice::Drives;
                            }
                            if r.double_clicked() {
                                scan_one = Some(root.clone());
                            }
                            if let Some(s) = space {
                                space_bar(ui, s.free, s.total, accent);
                            }
                        })
                        .response
                        .on_hover_text(scan_tip(label));
                    }
                    if let Some(root) = scan_one {
                        answer = Some(Answer::Scan(vec![root]));
                    }
                }
                if !drives_on
                    && (self.drives.iter().any(|d| d.4) || self.devices.iter().any(|d| d.3))
                {
                    ui.weak(tr("du_picker.individual_tip"));
                }
            });
            ui.radio_value(&mut self.choice, Choice::Folders, tr("du_picker.folders"));
            ui.indent("keel-du-folders", |ui| {
                let mut remove = None;
                for (i, f) in self.folders.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("×")
                            .on_hover_text(tr("du_picker.remove_tip"))
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
                        .add_enabled(idle, egui::Button::new(tr("du_picker.add_folders")))
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
                let scan = ui.add_enabled(!roots.is_empty(), egui::Button::new(tr("du_picker.scan")));
                crate::a11y::first_focus(&scan);
                if scan.clicked() {
                    answer = Some(Answer::Scan(roots));
                }
                if ui.button(tr("du_picker.cancel")).clicked() {
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

/// A drive's or device storage's used space as a bar, and "… free of …" (nothing when the
/// size is unknown).
fn space_bar(ui: &mut egui::Ui, free: u64, total: u64, accent: egui::Color32) {
    if total == 0 {
        return;
    }
    let used = 1.0 - free as f32 / total as f32;
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
        format_size(free, DECIMAL),
        format_size(total, DECIMAL)
    ));
}

fn scan_tip(name: &str) -> String {
    tr("disk_usage.double_click_to_scan").replace("{name}", name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Individual drives scans the ticked drives, then the ticked device storages.
    #[test]
    fn roots_include_only_ticked_drives_and_devices() {
        let mut picker = Picker::with_drives(&[
            ("C:".to_owned(), String::new(), 0, 0),
            ("D:".to_owned(), String::new(), 0, 0),
        ]);
        picker.choice = Choice::Drives;
        picker.drives[0].4 = true;
        let drive = Root::Path(picker.drives[0].0.clone());
        let phone = Root::Mtp {
            device_id: "phone".to_owned(),
            storage_id: "internal".to_owned(),
        };
        let camera = Root::Mtp {
            device_id: "camera".to_owned(),
            storage_id: "card".to_owned(),
        };
        picker.devices = vec![
            (phone.clone(), "Phone: Internal".to_owned(), None, true),
            (camera, "Camera: Card".to_owned(), None, false),
        ];

        assert_eq!(picker.roots(), vec![drive, phone.clone()]);
        assert_eq!(picker.device_labels()[0], (phone, "Phone: Internal".into()));
        picker.choice = Choice::AllLocal;
        assert_eq!(picker.roots(), vec![Root::AllLocalDrives]);
    }

    /// The dialog shows "Looking for devices…" until the worker's list arrives, then takes
    /// its storages unticked.
    #[test]
    fn the_device_list_arrives_from_the_worker() {
        let mut picker = Picker::with_drives(&[]);
        let (tx, rx) = channel();
        picker.listing_devices = Some(rx);
        let ctx = egui::Context::default();
        let frame = |picker: &mut Picker| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                let _ = picker.ui(ctx, egui::Color32::WHITE);
            });
        };
        frame(&mut picker);
        assert!(picker.listing_devices.is_some() && picker.devices.is_empty());
        let phone = Root::Mtp {
            device_id: "phone".to_owned(),
            storage_id: "internal".to_owned(),
        };
        let space = Space {
            total: 100,
            free: 40,
        };
        tx.send(vec![ScannableRoot {
            root: phone.clone(),
            kind: ScannableKind::Device,
            label: "Phone: Internal".to_owned(),
            space: Some(space),
        }])
        .unwrap();
        frame(&mut picker);
        assert!(picker.listing_devices.is_none());
        assert_eq!(
            picker.devices,
            vec![(phone, "Phone: Internal".to_owned(), Some(space), false)]
        );
        picker.choice = Choice::Drives;
        assert!(picker.roots().is_empty(), "nothing ticked yet");
    }
}
