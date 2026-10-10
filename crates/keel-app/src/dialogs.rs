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
    /// Properties of `paths`; `info` arrives from a worker.
    Properties {
        paths: Vec<VPath>,
        info: Option<Result<properties::Props, String>>,
    },
    /// Linux "Open with": `(name, desktop id)` of the applications for `path`.
    OpenWith {
        path: PathBuf,
        apps: Vec<(String, String)>,
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
            Dialog::Confirm { text, on_yes } => {
                ui.label(text.as_str());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // Enter / Space press the focused button (egui), which starts on "yes";
                    // Tab moves to Cancel. No window-wide Enter, so Enter on Cancel cancels.
                    // Drive's delete is a move to its trash (the text says so).
                    let yes = ui.button(match on_yes {
                        Action::DeleteRemote(_) if !text.starts_with("Move ") => "Delete",
                        Action::Cloud { .. } => "Remove",
                        Action::ZipTo { .. } => "Add",
                        _ => "Move to trash",
                    });
                    let no = ui.button("Cancel");
                    if !yes.has_focus() && !no.has_focus() {
                        yes.request_focus();
                    }
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
                        if ui.button(text).clicked() {
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
            Dialog::Properties { paths, info } => {
                properties::ui(ui, paths, info, &mut out);
                ui.add_space(8.0);
                cancel |= ui.button("Close").clicked();
            }
            Dialog::OpenWith { path, apps } => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                ui.label(format!("Open \"{name}\" with"));
                ui.add_space(4.0);
                if apps.is_empty() {
                    ui.weak("No applications are registered for this file type");
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (label, id) in apps.iter() {
                            let button = egui::Button::new(label.as_str())
                                .min_size(egui::vec2(ui.available_width(), 0.0));
                            if ui.add(button).on_hover_text(id.as_str()).clicked() {
                                out = Some(Action::LaunchWith {
                                    id: id.clone(),
                                    path: path.clone(),
                                });
                            }
                        }
                    });
                ui.add_space(8.0);
                cancel |= ui.button("Cancel").clicked();
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

fn invalid_name_for(name: &str, windows: bool) -> Option<String> {
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
