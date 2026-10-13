//! Modal dialogs: confirm (trash), name conflicts, new folder / new file, zip name. Rename is inline
//! in the views. Each dialog answers with an `Action` for `AppState::run`.

pub mod properties;

use crate::jobs::Transfer;
use crate::keys::Action;
use crate::settings::ArchiveFormat;
use egui::{Id, Key, Modal};
use keel_vfs::{Conflict, VPath};
use std::path::PathBuf;

pub enum Dialog {
    Confirm {
        text: String,
        /// The button that answers yes ("Move to trash", "Remove", ...).
        yes: &'static str,
        on_yes: Action,
    },
    /// Asked once per transfer; the answer applies to every clash (Phase 1).
    Conflict {
        names: Vec<String>,
        op: Transfer,
        from_clipboard: bool,
    },
    NewItem {
        dir: VPath,
        folder: bool,
        text: String,
        focus: bool,
    },
    /// Compress to zip…: the zip's name (in `dir`) for `src`.
    ZipName {
        dir: PathBuf,
        src: Vec<PathBuf>,
        text: String,
        focus: bool,
        format: ArchiveFormat,
        /// The user typed in the name field: a format change leaves it alone.
        edited: bool,
    },
    /// Bulk rename (Ctrl+F2).
    BulkRename(Box<crate::bulk_rename::BulkRename>),
    /// Open with…: `recent` apps for the extension, then the system's `(name, app)` list.
    OpenWith {
        paths: Vec<PathBuf>,
        recent: Vec<String>,
        apps: Vec<(String, String)>,
        remember: bool,
    },
    /// Mount… on a library source (`mount_ui.rs`).
    Mount(Box<crate::mount_ui::MountDialog>),
    /// Task 77: Export results (`filelist_ui.rs`).
    ExportList(Box<crate::filelist_ui::ExportDialog>),
    /// Task 56: Folder icon → Type an emoji…: one emoji (or `theme:<name>`) for `path`.
    FolderIcon {
        path: VPath,
        text: String,
        error: Option<String>,
        focus: bool,
    },
    /// Task 60: a folder operation (`folder_ops`).
    FolderOp(Box<crate::folder_ops::FolderDialog>),
    /// Task 85: Delete permanently, hard-link dedup, a system cleanup (`system_cleanup`).
    System(Box<crate::system_cleanup::SysDialog>),
    /// Task 85: Compress (NTFS)…
    Compress(Box<crate::compress_ui::CompressDialog>),
    /// Task 85: a user-defined cleanup's expanded commands, before they run.
    Cleanup(Box<crate::cleanups::Prepared>),
    /// Task 53: Restore library…: what the backup holds and the new library's name.
    RestoreLibrary {
        file: PathBuf,
        about: String,
        name: String,
        focus: bool,
    },
}

/// Shows the open dialog; closes it on an answer, Esc or a click outside.
pub fn show(ctx: &egui::Context, dialog: &mut Option<Dialog>) -> Option<Action> {
    let d = dialog.as_mut()?;
    let mut out = None;
    let mut cancel = false;
    let modal = Modal::new(Id::new("keel-dialog")).show(ctx, |ui| {
        ui.set_width(380.0);
        match d {
            Dialog::Confirm { text, yes, on_yes } => {
                ui.label(text.as_str());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // Enter / Space press the focused button (egui), which starts on "yes";
                    // Tab moves to Cancel. No window-wide Enter, so Enter on Cancel cancels.
                    let yes = ui.button(*yes);
                    let no = ui.button("Cancel");
                    crate::a11y::first_focus(&yes);
                    cancel |= no.clicked();
                    if !cancel && yes.clicked() {
                        out = Some(on_yes.clone());
                    }
                });
            }
            Dialog::Conflict {
                names,
                op,
                from_clipboard,
            } => {
                ui.strong(match names.len() {
                    1 => "An item with this name already exists:".to_owned(),
                    n => format!("{n} items with these names already exist:"),
                });
                for name in names.iter().take(5) {
                    ui.label(format!("  {name}"));
                }
                if names.len() > 5 {
                    ui.weak(format!("  and {} more", names.len() - 5));
                }
                ui.label(format!("in {}", op.dst.display()));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (text, conflict) in [
                        ("Skip", Conflict::Skip),
                        ("Overwrite", Conflict::Overwrite),
                        ("Keep both", Conflict::RenameNew),
                    ] {
                        let b = ui.button(text);
                        // Enter keeps both (nothing is lost), Tab reaches the others.
                        if conflict == Conflict::RenameNew {
                            crate::a11y::first_focus(&b);
                        }
                        if b.clicked() {
                            out = Some(Action::StartTransfer {
                                op: op.clone(),
                                conflict,
                                from_clipboard: *from_clipboard,
                            });
                        }
                    }
                    cancel |= ui.button("Cancel").clicked();
                });
            }
            Dialog::NewItem {
                dir,
                folder,
                text,
                focus,
            } => {
                ui.label(if *folder {
                    "New folder name"
                } else {
                    "New file name"
                });
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                if std::mem::take(focus) {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if (ui.button("Create").clicked() || enter) && !text.trim().is_empty() {
                        out = Some(Action::Create {
                            dir: dir.clone(),
                            name: text.trim().to_owned(),
                            folder: *folder,
                        });
                    }
                    cancel |= ui.button("Cancel").clicked();
                });
            }
            Dialog::ZipName {
                dir,
                src,
                text,
                focus,
                format,
                edited,
            } => {
                ui.label(format!("Compress {} to", crate::jobs::items(src.len())));
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                *edited |= r.changed();
                if std::mem::take(focus) {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.horizontal(|ui| {
                    for f in ArchiveFormat::ALL {
                        if ui.selectable_label(*format == f, f.label()).clicked() {
                            pick_format(text, format, *edited, f);
                        }
                    }
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if (ui.button("Compress").clicked() || enter) && !text.trim().is_empty() {
                        let name = text.trim();
                        let (name, format) = match ArchiveFormat::of_name(name) {
                            Some(f) => (name.to_owned(), f),
                            None => (format!("{name}{}", format.ext()), *format),
                        };
                        out = Some(Action::CompressTo {
                            zip: dir.join(name),
                            src: src.clone(),
                            format,
                        });
                    }
                    cancel |= ui.button("Cancel").clicked();
                });
            }
            Dialog::RestoreLibrary {
                file,
                about,
                name,
                focus,
            } => {
                ui.strong(format!(
                    "Restore {}",
                    file.file_name().unwrap_or_default().to_string_lossy()
                ));
                ui.label(about.as_str());
                ui.add_space(6.0);
                ui.label("New library name");
                let r = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                let r = crate::a11y::Named::named(r, "New library name");
                if std::mem::take(focus) {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if (ui.button("Restore").clicked() || enter) && !name.trim().is_empty() {
                        out = Some(Action::Library(crate::library::LibCmd::RestoreAs {
                            file: file.clone(),
                            name: name.trim().to_owned(),
                        }));
                    }
                    cancel |= ui.button("Cancel").clicked();
                });
            }
            Dialog::FolderIcon {
                path,
                text,
                error,
                focus,
            } => {
                ui.strong(format!("Folder icon for {}", path.name()));
                ui.label("Type or paste one emoji (Win+. or Ctrl+Cmd+Space opens the emoji panel)");
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                let r = crate::a11y::Named::named(r, "Emoji");
                if std::mem::take(focus) {
                    r.request_focus();
                }
                if r.changed() {
                    *error = None;
                }
                if let Some(e) = error.as_deref() {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Set icon").clicked() || enter {
                        let icon = text.trim();
                        let names = crate::icons::theme_folder_names();
                        *error = crate::folder_style::icon_error(icon, &names);
                        if error.is_none() {
                            out = Some(Action::Library(crate::library::LibCmd::FolderStyle {
                                path: path.clone(),
                                color: None,
                                icon: Some(Some(icon.to_owned())),
                            }));
                        }
                    }
                    cancel |= ui.button("Cancel").clicked();
                });
            }
            Dialog::Mount(m) => out = crate::mount_ui::dialog_ui(ui, m, &mut cancel),
            Dialog::ExportList(d) => out = crate::filelist_ui::dialog_ui(ui, d, &mut cancel),
            Dialog::FolderOp(d) => out = crate::folder_ops::ui(ui, d, &mut cancel),
            Dialog::System(d) => out = crate::system_cleanup::dialog_ui(ui, d, &mut cancel),
            Dialog::Compress(d) => out = crate::compress_ui::ui(ui, d, &mut cancel),
            Dialog::Cleanup(p) => out = crate::cleanups::confirm_ui(ui, p, &mut cancel),
            Dialog::BulkRename(m) => {
                if let Some(renames) = crate::bulk_rename::ui(ui, m, &mut cancel) {
                    out = Some(Action::BulkRenameApply { renames });
                }
            }
            Dialog::OpenWith {
                paths,
                recent,
                apps,
                remember,
            } => {
                let name = paths[0].file_name().unwrap_or_default().to_string_lossy();
                ui.label(match paths.len() {
                    1 => format!("Open \"{name}\" with"),
                    n => format!("Open {n} files with"),
                });
                ui.add_space(4.0);
                let ext = paths[0]
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase());
                ui.checkbox(
                    remember,
                    match &ext {
                        Some(e) => format!("Remember for .{e} files"),
                        None => "Remember for files without an extension".to_owned(),
                    },
                );
                ui.add_space(4.0);
                let shown: Vec<(String, String)> = (recent.iter())
                    .map(|a| (crate::platform::app_label(a), a.clone()))
                    .chain(apps.iter().filter(|(_, a)| !recent.contains(a)).cloned())
                    .collect();
                if shown.is_empty() {
                    ui.weak("No applications are known for this file type");
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (label, app) in &shown {
                            let button = egui::Button::new(label.as_str())
                                .min_size(egui::vec2(ui.available_width(), 0.0));
                            let r = ui.add(button).on_hover_text(app.as_str());
                            crate::a11y::first_focus(&r);
                            if r.clicked() {
                                out = Some(Action::OpenWithApp {
                                    paths: paths.clone(),
                                    app: app.clone(),
                                    remember: *remember,
                                });
                            }
                        }
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let browse = ui.button("Browse…");
                    crate::a11y::first_focus(&browse);
                    if browse.clicked() {
                        out = Some(Action::OpenWithBrowse {
                            paths: paths.clone(),
                            remember: *remember,
                        });
                    }
                    if cfg!(windows) && ui.button("System chooser…").clicked() {
                        out = Some(Action::OpenWithSystem(paths[0].clone()));
                    }
                    cancel |= ui.button("Cancel").clicked();
                });
            }
        }
    });
    if out.is_some() || cancel || modal.should_close() {
        *dialog = None;
    }
    out
}

/// The Compress dialog's format choice: swaps the name's extension unless the user edited it.
pub fn pick_format(
    text: &mut String,
    current: &mut ArchiveFormat,
    edited: bool,
    new: ArchiveFormat,
) {
    if !edited {
        *text = new.apply(text, *current);
    }
    *current = new;
}

/// Why `name` cannot be a file name here, if it cannot.
pub fn invalid_name(name: &str) -> Option<String> {
    invalid_name_for(name, cfg!(windows))
}

pub(crate) fn invalid_name_for(name: &str, windows: bool) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return Some(format!("\"{name}\" is not a valid name"));
    }
    let bad: &[char] = if windows {
        &['<', '>', ':', '"', '/', '\\', '|', '?', '*']
    } else {
        &['/']
    };
    if let Some(c) = name.chars().find(|c| bad.contains(c) || *c == '\0') {
        return Some(format!("A name cannot contain {c:?}"));
    }
    if windows {
        if name.chars().any(char::is_control) {
            return Some("A name cannot contain control characters".into());
        }
        if name.ends_with(['.', ' ']) {
            return Some("A name cannot end with a dot or a space".into());
        }
        let stem = name.split('.').next().unwrap_or(name).trim_end();
        let upper = stem.to_ascii_uppercase();
        let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((upper.starts_with("COM") || upper.starts_with("LPT"))
                && upper.len() == 4
                && matches!(upper.as_bytes()[3], b'1'..=b'9'));
        if reserved {
            return Some(format!("\"{stem}\" is a reserved name on Windows"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_per_os() {
        for bad in [
            "a<b", "a:b", "x\"y", "a/b", "a\\b", "a|b", "q?", "s*", "CON", "con.txt", "Lpt9",
            "COM1.log", "nul ", "dot.", "..",
        ] {
            assert!(invalid_name_for(bad, true).is_some(), "{bad} on Windows");
        }
        for ok in ["COM0", "COM10", "console.txt", "a b.txt", "LPT", ".hidden"] {
            assert_eq!(invalid_name_for(ok, true), None, "{ok} on Windows");
        }
        assert!(invalid_name_for("a/b", false).is_some());
        for ok in ["a:b", "CON", "q?", "dot."] {
            assert_eq!(invalid_name_for(ok, false), None, "{ok} on Unix");
        }
    }

    #[test]
    fn format_choice_rewrites_the_extension_only_when_untouched() {
        use ArchiveFormat::*;
        let (mut text, mut fmt) = ("photos.zip".to_string(), Zip);
        pick_format(&mut text, &mut fmt, false, TarGz);
        assert_eq!((text.as_str(), fmt), ("photos.tar.gz", TarGz));
        pick_format(&mut text, &mut fmt, false, SevenZ);
        assert_eq!((text.as_str(), fmt), ("photos.7z", SevenZ));
        // Edited: the name stays, the format still changes.
        let mut text = "mine.zip".to_string();
        let mut fmt = Zip;
        pick_format(&mut text, &mut fmt, true, Tar);
        assert_eq!((text.as_str(), fmt), ("mine.zip", Tar));
        // An extension-less name gets the new one; a dotted stem keeps its dots.
        assert_eq!(Tar.apply("a.b", Zip), "a.b.tar");
        assert_eq!(Zip.apply("v1.2.TAR.GZ", TarGz), "v1.2.zip");
    }
}
