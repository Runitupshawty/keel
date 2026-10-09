//! Modal dialogs: confirm (trash), name conflicts, new folder / new file. Rename is inline
//! in the views. Each dialog answers with an `Action` for `AppState::run`.

use crate::jobs::Transfer;
use crate::keys::Action;
use egui::{Id, Key, Modal};
use keel_vfs::{Conflict, VPath};

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
                    let yes = ui.button("Move to trash");
                    if yes.clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        out = Some(on_yes.clone());
                    }
                    cancel |= ui.button("Cancel").clicked();
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
        }
    });
    if out.is_some() || cancel || modal.should_close() {
        *dialog = None;
    }
    out
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
}
