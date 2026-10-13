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
        /// A folder (else a file).
        dir: bool,
        text: String,
        error: Option<String>,
        focus: bool,
    },
    /// Task 60: a folder operation (`folder_ops`).
    FolderOp(Box<crate::folder_ops::FolderDialog>),
    /// Palette "Save session as…": the name; `taken` are the profile's sessions.
    SessionName {
        text: String,
        taken: Vec<String>,
        focus: bool,
    },
    /// Task 85: Delete permanently, hard-link dedup, a system cleanup (`system_cleanup`).
    System(Box<crate::system_cleanup::SysDialog>),
    /// Task 85: Compress (NTFS)…
    Compress(Box<crate::compress_ui::CompressDialog>),
    /// Task 85: a user-defined cleanup's expanded commands, before they run.
    Cleanup(Box<crate::cleanups::Prepared>),
    /// A new collection's name (`add`: with the selection in it), or a collection's new
    /// name (`id`).
    CollectionName {
        id: Option<keel_core::CollectionId>,
        add: bool,
        text: String,
        focus: bool,
    },
    ConvertImage(crate::image_convert::ConvertDialog),
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
                    let no = ui.button(crate::i18n::tr("dialogs.cancel"));
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
                    1 => crate::i18n::tr("dialogs.exists_one").to_owned(),
                    n => crate::i18n::tr("dialogs.exists_n").replace("{n}", &n.to_string()),
                });
                for name in names.iter().take(5) {
                    ui.label(format!("  {name}"));
                }
                if names.len() > 5 {
                    ui.weak(
                        crate::i18n::tr("dialogs.and_more")
                            .replace("{n}", &(names.len() - 5).to_string()),
                    );
                }
                ui.label(
                    crate::i18n::tr("dialogs.in_path")
                        .replace("{path}", &op.dst.display().to_string()),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (key, conflict) in [
                        ("dialogs.skip", Conflict::Skip),
                        ("dialogs.overwrite", Conflict::Overwrite),
                        ("dialogs.keep_both", Conflict::RenameNew),
                    ] {
                        let b = ui.button(crate::i18n::tr(key));
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
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
                });
            }
            Dialog::NewItem {
                dir,
                folder,
                text,
                focus,
            } => {
                ui.label(if *folder {
                    crate::i18n::tr("dialogs.new_folder_name")
                } else {
                    crate::i18n::tr("dialogs.new_file_name")
                });
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                if std::mem::take(focus) {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if (ui.button(crate::i18n::tr("dialogs.create")).clicked() || enter)
                        && !text.trim().is_empty()
                    {
                        out = Some(Action::Create {
                            dir: dir.clone(),
                            name: text.trim().to_owned(),
                            folder: *folder,
                        });
                    }
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
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
                ui.label(
                    crate::i18n::tr("dialogs.compress_to")
                        .replace("{n}", &crate::jobs::items(src.len())),
                );
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
                    if (ui.button(crate::i18n::tr("dialogs.compress")).clicked() || enter)
                        && !text.trim().is_empty()
                    {
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
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
                });
            }
            Dialog::CollectionName {
                id,
                add,
                text,
                focus,
            } => {
                ui.label(match id {
                    Some(_) => crate::i18n::tr("dialogs.rename_collection"),
                    None => crate::i18n::tr("dialogs.new_collection_name"),
                });
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                let r = crate::a11y::Named::named(r, crate::i18n::tr("dialogs.collection_name"));
                if std::mem::take(focus) {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let verb = if id.is_some() {
                        crate::i18n::tr("menu.rename")
                    } else {
                        crate::i18n::tr("dialogs.create")
                    };
                    if (ui.button(verb).clicked() || enter) && !text.trim().is_empty() {
                        let name = text.trim().to_owned();
                        use crate::library::LibCmd;
                        out = Some(Action::Library(match *id {
                            Some(id) => LibCmd::SetCollectionName { id, name },
                            None => LibCmd::CreateCollection { name, add: *add },
                        }));
                    }
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
                });
            }
            Dialog::RestoreLibrary {
                file,
                about,
                name,
                focus,
            } => {
                let backup = file.file_name().unwrap_or_default().to_string_lossy();
                ui.strong(crate::i18n::tr("dialogs.restore_named").replace("{name}", &backup));
                ui.label(about.as_str());
                ui.add_space(6.0);
                ui.label(crate::i18n::tr("dialogs.new_library_name"));
                let r = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                let r = crate::a11y::Named::named(r, crate::i18n::tr("dialogs.new_library_name"));
                if std::mem::take(focus) {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if (ui.button(crate::i18n::tr("dialogs.restore")).clicked() || enter)
                        && !name.trim().is_empty()
                    {
                        out = Some(Action::Library(crate::library::LibCmd::RestoreAs {
                            file: file.clone(),
                            name: name.trim().to_owned(),
                        }));
                    }
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
                });
            }
            Dialog::FolderIcon {
                path,
                dir,
                text,
                error,
                focus,
            } => {
                ui.strong(if *dir {
                    crate::i18n::tr("dialogs.folder_icon_for").replace("{name}", path.name())
                } else {
                    crate::i18n::tr("dialogs.file_icon_for").replace("{name}", path.name())
                });
                ui.label(crate::i18n::tr("dialogs.emoji_hint"));
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                let r = crate::a11y::Named::named(r, crate::i18n::tr("dialogs.emoji"));
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
                    if ui.button(crate::i18n::tr("dialogs.set_icon")).clicked() || enter {
                        let icon = text.trim();
                        let names = crate::folder_style::theme_names(*dir);
                        *error = crate::folder_style::icon_error(icon, &names, *dir);
                        if error.is_none() {
                            out = Some(Action::Library(crate::library::LibCmd::FolderStyle {
                                path: path.clone(),
                                color: None,
                                icon: Some(Some(icon.to_owned())),
                            }));
                        }
                    }
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
                });
            }
            Dialog::SessionName { text, taken, focus } => {
                ui.label(crate::i18n::tr("dialogs.save_session_label"));
                let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                let r = crate::a11y::Named::named(r, crate::i18n::tr("dialogs.session_name"));
                if std::mem::take(focus) {
                    r.request_focus();
                }
                let name = text.trim();
                let ok = crate::profiles::valid_name(name);
                if !name.is_empty() && !ok {
                    ui.weak(crate::profiles::NAME_RULE);
                } else if taken.iter().any(|t| t == name) {
                    ui.weak(crate::i18n::tr("dialogs.replaces_session").replace("{name}", name));
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if (ui
                        .add_enabled(ok, egui::Button::new(crate::i18n::tr("dialogs.save")))
                        .clicked()
                        || enter)
                        && ok
                    {
                        out = Some(Action::Session(crate::profiles::SessionCmd::Save(
                            name.to_owned(),
                        )));
                    }
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
                });
            }
            Dialog::Mount(m) => out = crate::mount_ui::dialog_ui(ui, m, &mut cancel),
            Dialog::ExportList(d) => out = crate::filelist_ui::dialog_ui(ui, d, &mut cancel),
            Dialog::ConvertImage(d) => out = crate::image_convert::ui(ui, d, &mut cancel),
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
                    1 => crate::i18n::tr("dialogs.open_named_with").replace("{name}", &name),
                    n => {
                        crate::i18n::tr("dialogs.open_n_files_with").replace("{n}", &n.to_string())
                    }
                });
                ui.add_space(4.0);
                let ext = paths[0]
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase());
                ui.checkbox(
                    remember,
                    match &ext {
                        Some(e) => crate::i18n::tr("dialogs.remember_for_ext").replace("{e}", e),
                        None => crate::i18n::tr("dialogs.remember_no_ext").to_owned(),
                    },
                );
                ui.add_space(4.0);
                let shown: Vec<(String, String)> = (recent.iter())
                    .map(|a| (crate::platform::app_label(a), a.clone()))
                    .chain(apps.iter().filter(|(_, a)| !recent.contains(a)).cloned())
                    .collect();
                if shown.is_empty() {
                    ui.weak(crate::i18n::tr("dialogs.no_apps"));
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
                    let browse = ui.button(crate::i18n::tr("dialogs.browse"));
                    crate::a11y::first_focus(&browse);
                    if browse.clicked() {
                        out = Some(Action::OpenWithBrowse {
                            paths: paths.clone(),
                            remember: *remember,
                        });
                    }
                    if cfg!(windows)
                        && ui
                            .button(crate::i18n::tr("dialogs.system_chooser"))
                            .clicked()
                    {
                        out = Some(Action::OpenWithSystem(paths[0].clone()));
                    }
                    cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
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
        return Some(crate::i18n::tr("dialogs.not_valid_name").replace("{name}", name));
    }
    let bad: &[char] = if windows {
        &['<', '>', ':', '"', '/', '\\', '|', '?', '*']
    } else {
        &['/']
    };
    if let Some(c) = name.chars().find(|c| bad.contains(c) || *c == '\0') {
        return Some(crate::i18n::tr("dialogs.cannot_contain").replace("{c}", &format!("{c:?}")));
    }
    if windows {
        if name.chars().any(char::is_control) {
            return Some(crate::i18n::tr("dialogs.no_control_chars").to_owned());
        }
        if name.ends_with(['.', ' ']) {
            return Some(crate::i18n::tr("dialogs.no_end_dot_space").to_owned());
        }
        let stem = name.split('.').next().unwrap_or(name).trim_end();
        let reserved = keel_vfs::path::is_windows_device(name);
        if reserved {
            return Some(crate::i18n::tr("dialogs.reserved_name").replace("{stem}", stem));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_console_and_superscript_device_names() {
        for name in ["CONIN$", "conout$.txt", "COM¹", "com².log", "LPT³.txt"] {
            assert!(invalid_name_for(name, true).is_some(), "{name}");
            assert_eq!(invalid_name_for(name, false), None);
        }
        for name in ["COM⁴", "LPT¹0", "CONIN$x"] {
            assert_eq!(invalid_name_for(name, true), None);
        }
    }

    #[test]
    fn names_are_checked_per_os() {
        for bad in [
            "a<b", "a:b", "x\"y", "a/b", "a\\b", "a|b", "q?", "s*", "CON", "con.txt", "Lpt9",
            "COM1.log", "nul ", "dot.", "..",
        ] {
            assert!(invalid_name_for(bad, true).is_some(), "{bad} on Windows");
        }
        for ok in ["COM10", "console.txt", "a b.txt", "LPT", ".hidden"] {
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
