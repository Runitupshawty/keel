//! The folder operation dialogs (Merge into…, Flatten, Organize by…, Attributes…, Create
//! link…): each picks what the operation needs and asks for its library plan, whose preview
//! dialog lists every change before anything runs (`LibCmd::Plan`). Set folder dates has
//! nothing to pick and goes straight to its preview.

use crate::a11y::Named as _;
use crate::keys::Action;
use crate::library::LibCmd;
use egui::Key;
use keel_core::{FolderOp, OnConflict, Op, OrganizeBy};
use keel_vfs::{Attributes, LinkKind, VPath};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Merge,
    Flatten,
    Organize,
    Attributes,
    Link,
    /// Remove empty folders (Task 85).
    RemoveEmpty,
    /// Set folder dates to the newest change below.
    FolderDates,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Merge => "Merge into…",
            Kind::Flatten => "Flatten…",
            Kind::Organize => "Organize by…",
            Kind::Attributes => "Attributes…",
            Kind::Link => "Create link…",
            Kind::RemoveEmpty => "Remove empty folders…",
            Kind::FolderDates => FOLDER_DATES,
        }
    }

    /// Whether it opens a dialog first (else its preview opens at once).
    pub fn asks(self) -> bool {
        self != Kind::FolderDates
    }
}

/// The menu and palette entry of Set folder dates.
pub const FOLDER_DATES: &str = "Set folder dates to the newest change below…";

pub struct FolderDialog {
    pub kind: Kind,
    /// Real paths: the selected folders (merge), the folder (flatten, organize), the items
    /// (attributes), the link's target (link).
    pub targets: Vec<VPath>,
    /// Merge: the target folder among `targets`.
    pub into: usize,
    pub by: OrganizeBy,
    pub conflict: OnConflict,
    /// Attributes as read when the dialog opened (None: they differ between the items),
    /// and as the checkboxes show them now.
    pub initial: Attributes,
    pub attrs: Attributes,
    /// Unix: the mode in octal.
    pub mode: String,
    pub link_kind: LinkKind,
    pub link_name: String,
    pub target_is_dir: bool,
    /// Windows: symbolic links can be made (Developer Mode or elevation).
    pub symlinks_ok: bool,
    pub error: Option<String>,
    /// Shown for the first time this frame: the Enter that opened it is not an answer.
    pub fresh: bool,
}

impl FolderDialog {
    /// The dialog for `targets` (real paths; their kinds in `dirs`), or why it cannot open.
    pub fn new(kind: Kind, targets: Vec<VPath>, dirs: &[bool]) -> Result<Self, String> {
        let locals = targets.iter().all(|t| t.to_local_path().is_some());
        let folders = dirs.iter().all(|d| *d);
        match kind {
            Kind::Merge if targets.len() < 2 || !folders => {
                return Err("Select two or more folders to merge".into())
            }
            Kind::Flatten | Kind::Organize | Kind::RemoveEmpty | Kind::FolderDates
                if targets.len() != 1 || !folders =>
            {
                return Err(format!(
                    "{} works on one folder",
                    kind.title().trim_end_matches('…')
                ))
            }
            Kind::FolderDates if !locals => {
                return Err("Folder dates are set on this computer's folders only".into())
            }
            Kind::Link if targets.len() != 1 => {
                return Err("Select one file or folder to link to".into())
            }
            Kind::Attributes | Kind::Link if !locals => {
                return Err("Attributes and links are set on this computer's files only".into())
            }
            _ => {}
        }
        let initial = match kind {
            Kind::Attributes => common_attributes(&targets),
            _ => Attributes::default(),
        };
        let target_is_dir = dirs.first().copied().unwrap_or(false);
        let link_kind = match (cfg!(windows), target_is_dir) {
            (false, _) => LinkKind::Symlink,
            (true, true) => LinkKind::Junction,
            (true, false) => LinkKind::Hardlink,
        };
        let link_name = targets
            .first()
            .map(|t| link_name(t.name(), target_is_dir))
            .unwrap_or_default();
        Ok(Self {
            kind,
            into: 0,
            by: OrganizeBy::Type,
            conflict: OnConflict::Skip,
            mode: initial.mode.map(|m| format!("{m:03o}")).unwrap_or_default(),
            attrs: initial,
            initial,
            link_kind,
            link_name,
            target_is_dir,
            symlinks_ok: kind != Kind::Link || keel_vfs::fsattr::symlinks_allowed(),
            error: None,
            fresh: true,
            targets,
        })
    }

    /// The operation to preview, or why not.
    pub fn op(&self) -> Result<Op, String> {
        let one = || self.targets[0].clone();
        Ok(match self.kind {
            Kind::Merge => Op::arrange(
                FolderOp::Merge {
                    folders: self.targets.clone(),
                    into: self.targets[self.into.min(self.targets.len() - 1)].clone(),
                },
                self.conflict,
            ),
            Kind::Flatten => Op::arrange(FolderOp::Flatten { folder: one() }, self.conflict),
            Kind::RemoveEmpty => {
                Op::arrange(FolderOp::RemoveEmpty { folder: one() }, OnConflict::Skip)
            }
            Kind::Organize => Op::arrange(
                FolderOp::Organize {
                    folder: one(),
                    by: self.by,
                },
                self.conflict,
            ),
            Kind::Attributes => Op::SetAttributes {
                paths: self.targets.clone(),
                attrs: self.changed()?,
            },
            Kind::Link => {
                let name = self.link_name.trim();
                if let Some(why) = crate::dialogs::invalid_name(name) {
                    return Err(why);
                }
                let dir = one().parent().ok_or("no folder to put the link in")?;
                Op::Link {
                    target: one(),
                    link: dir.join(name),
                    kind: self.link_kind,
                }
            }
            Kind::FolderDates => Op::folder_dates(one()),
        })
    }

    /// The attributes the user changed (what was mixed and left alone stays as it is).
    fn changed(&self) -> Result<Attributes, String> {
        let pick = |now: Option<bool>, was: Option<bool>| now.filter(|_| now != was);
        let mode = match self.mode.trim() {
            "" => None,
            m => Some(
                u32::from_str_radix(m, 8)
                    .ok()
                    .filter(|m| *m <= 0o777)
                    .ok_or_else(|| format!("{m} is not a mode like 755"))?,
            ),
        };
        let a = Attributes {
            hidden: pick(self.attrs.hidden, self.initial.hidden),
            readonly: pick(self.attrs.readonly, self.initial.readonly),
            mode: mode.filter(|m| Some(*m) != self.initial.mode),
        };
        if a.is_empty() {
            return Err("Nothing changed".into());
        }
        Ok(a)
    }
}

/// "Photos - link", "notes - link.txt".
fn link_name(name: &str, dir: bool) -> String {
    match name.rfind('.').filter(|i| *i > 0 && !dir) {
        Some(i) => format!("{} - link{}", &name[..i], &name[i..]),
        None => format!("{name} - link"),
    }
}

/// The attributes all of `paths` share (None where they differ or cannot be read).
fn common_attributes(paths: &[VPath]) -> Attributes {
    let read: Vec<Attributes> = paths
        .iter()
        .map(|p| {
            p.to_local_path()
                .and_then(|l| keel_vfs::fsattr::attributes(&l).ok())
                .unwrap_or_default()
        })
        .collect();
    let same = |f: fn(&Attributes) -> Option<bool>| {
        let first = read.first().and_then(f);
        read.iter()
            .all(|a| f(a) == first)
            .then_some(first)
            .flatten()
    };
    let mode = read.first().and_then(|a| a.mode);
    Attributes {
        hidden: same(|a| a.hidden),
        readonly: same(|a| a.readonly),
        mode: read
            .iter()
            .all(|a| a.mode == mode)
            .then_some(mode)
            .flatten(),
    }
}

/// A checkbox for an attribute that may be mixed (None: shown indeterminate; a click sets
/// it on).
fn tri(ui: &mut egui::Ui, value: &mut Option<bool>, label: &str) {
    let mut on = value.unwrap_or(false);
    let r = ui.add(egui::Checkbox::new(&mut on, label).indeterminate(value.is_none()));
    if r.changed() {
        *value = Some(on);
    }
}

fn conflict_choice(ui: &mut egui::Ui, c: &mut OnConflict) {
    ui.label("When a name is taken:");
    ui.horizontal(|ui| {
        for (text, rule) in [
            ("Skip", OnConflict::Skip),
            ("Keep both", OnConflict::RenameNew),
            ("Overwrite", OnConflict::Overwrite),
        ] {
            ui.radio_value(c, rule, text);
        }
    });
}

/// The dialog body; Some(action) when it asks for the preview.
pub fn ui(ui: &mut egui::Ui, d: &mut FolderDialog, cancel: &mut bool) -> Option<Action> {
    ui.strong(d.kind.title().trim_end_matches('…'));
    ui.add_space(4.0);
    let one = d.targets[0].display();
    match d.kind {
        Kind::Merge => {
            ui.label("Move everything in the selected folders into:");
            for (i, t) in d.targets.iter().enumerate() {
                ui.radio_value(&mut d.into, i, t.name());
            }
            ui.weak("The other folders are removed once they are empty.");
            conflict_choice(ui, &mut d.conflict);
        }
        Kind::Flatten => {
            ui.label(format!(
                "Move every file in the subfolders of {one} up into it, then remove the \
                 subfolders left empty. The preview counts them."
            ));
            conflict_choice(ui, &mut d.conflict);
        }
        Kind::RemoveEmpty => {
            let local = d
                .targets
                .first()
                .is_some_and(|t| t.to_local_path().is_some());
            ui.label(if local {
                format!(
                    "Move every empty folder below {one} to the {}: folders holding no files                      and no links, at any depth. The preview lists them.",
                    keel_vfs::trashbin::label()
                )
            } else {
                format!(
                    "Remove every empty folder below {one}: folders holding no files and no                      links, at any depth. Where there is no trash (SFTP, FTP, S3, WebDAV) they                      are deleted for good. The preview lists them and says so."
                )
            });
        }
        Kind::Organize => {
            ui.label(format!("Move the files in {one} into subfolders by:"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut d.by, OrganizeBy::Type, "Type")
                    .on_hover_text("Images, Videos, Audio, Documents, Archives, Other");
                ui.radio_value(&mut d.by, OrganizeBy::Year, "Year");
                ui.radio_value(&mut d.by, OrganizeBy::Month, "Year and month");
            });
            conflict_choice(ui, &mut d.conflict);
        }
        Kind::Attributes => {
            ui.label(match d.targets.len() {
                1 => format!("For {one}"),
                n => format!("For {n} items"),
            });
            if cfg!(any(windows, target_os = "macos")) {
                tri(ui, &mut d.attrs.hidden, "Hidden");
            }
            tri(ui, &mut d.attrs.readonly, "Read-only");
            if cfg!(unix) {
                ui.horizontal(|ui| {
                    ui.label("Mode (octal)");
                    let r = ui.add(egui::TextEdit::singleline(&mut d.mode).desired_width(60.0));
                    r.named("Mode (octal)");
                });
            }
        }
        Kind::Link => {
            ui.label(format!("A link to {one}, in the same folder"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut d.link_kind, LinkKind::Symlink, "Symbolic link");
                if cfg!(windows) && d.target_is_dir {
                    ui.radio_value(&mut d.link_kind, LinkKind::Junction, "Junction");
                }
                if !d.target_is_dir {
                    ui.radio_value(&mut d.link_kind, LinkKind::Hardlink, "Hard link");
                }
            });
            if d.link_kind == LinkKind::Symlink && !d.symlinks_ok {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Windows makes symbolic links only with Developer Mode on (Settings → \
                     System → For developers) or for an elevated program; Keel never elevates.",
                );
                let (text, kind) = match d.target_is_dir {
                    true => ("Make a junction instead", LinkKind::Junction),
                    false => ("Make a hard link instead", LinkKind::Hardlink),
                };
                if ui.button(text).clicked() {
                    d.link_kind = kind;
                }
            }
            ui.horizontal(|ui| {
                ui.label("Name");
                let r = ui.add(egui::TextEdit::singleline(&mut d.link_name).desired_width(240.0));
                r.named("Link name");
            });
        }
        // Not shown: it previews at once (`Kind::asks`).
        Kind::FolderDates => {
            ui.label(format!(
                "Set each folder in {one} to the date of the newest file below it"
            ));
        }
    }
    if let Some(e) = &d.error {
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    ui.add_space(8.0);
    let mut out = None;
    ui.horizontal(|ui| {
        let go = ui.button("Preview");
        crate::a11y::first_focus(&go);
        let enter = !std::mem::take(&mut d.fresh) && ui.input(|i| i.key_pressed(Key::Enter));
        if go.clicked() || enter {
            match d.op() {
                Ok(op) => out = Some(Action::Library(LibCmd::Plan(op))),
                Err(e) => d.error = Some(e),
            }
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> VPath {
        VPath::local(s)
    }

    #[test]
    fn dialogs_check_the_selection_and_build_their_plans() {
        let dirs = vec![v("/d/a"), v("/d/b")];
        assert!(FolderDialog::new(Kind::Merge, vec![v("/d/a")], &[true]).is_err());
        assert!(FolderDialog::new(Kind::Merge, dirs.clone(), &[true, false]).is_err());
        let mut d = FolderDialog::new(Kind::Merge, dirs.clone(), &[true, true]).unwrap();
        d.into = 1;
        d.conflict = OnConflict::RenameNew;
        assert_eq!(
            d.op(),
            Ok(Op::arrange(
                FolderOp::Merge {
                    folders: dirs.clone(),
                    into: v("/d/b"),
                },
                OnConflict::RenameNew
            ))
        );
        assert!(FolderDialog::new(Kind::Flatten, dirs, &[true, true]).is_err());
        let mut d = FolderDialog::new(Kind::Organize, vec![v("/d/a")], &[true]).unwrap();
        d.by = OrganizeBy::Month;
        assert!(matches!(
            d.op(),
            Ok(Op::Arrange {
                what: FolderOp::Organize {
                    by: OrganizeBy::Month,
                    ..
                },
                ..
            })
        ));
        assert!(FolderDialog::new(Kind::Link, vec![v("/d/a"), v("/d/b")], &[true, true]).is_err());
        let d = FolderDialog::new(Kind::Link, vec![v("/d/notes.txt")], &[false]).unwrap();
        assert_eq!(d.link_name, "notes - link.txt");
        let Ok(Op::Link { link, kind, .. }) = d.op() else {
            panic!("a link");
        };
        assert_eq!(link, v("/d/notes - link.txt"));
        assert_eq!(
            kind,
            if cfg!(windows) {
                LinkKind::Hardlink
            } else {
                LinkKind::Symlink
            }
        );
        assert!(FolderDialog::new(
            Kind::Attributes,
            vec![VPath::parse("sftp://h/x").unwrap()],
            &[false]
        )
        .is_err());
        let d = FolderDialog::new(Kind::FolderDates, vec![v("/d/a")], &[true]).unwrap();
        assert!(!d.kind.asks());
        assert_eq!(d.op(), Ok(Op::folder_dates(v("/d/a"))));
        let two = FolderDialog::new(Kind::FolderDates, vec![v("/d/a"), v("/d/b")], &[true, true]);
        assert_eq!(
            two.err().as_deref(),
            Some("Set folder dates to the newest change below works on one folder")
        );
        assert!(FolderDialog::new(Kind::FolderDates, vec![v("/d/f.txt")], &[false]).is_err());
        let remote = VPath::parse("sftp://h/x").unwrap();
        assert!(FolderDialog::new(Kind::FolderDates, vec![remote], &[true]).is_err());
    }

    #[test]
    fn attributes_send_only_what_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let ro = Attributes {
            readonly: Some(true),
            ..Attributes::default()
        };
        keel_vfs::fsattr::set_attributes(&b, &ro).unwrap();
        let paths = vec![VPath::local(&a), VPath::local(&b)];
        let mut d = FolderDialog::new(Kind::Attributes, paths, &[false, false]).unwrap();
        assert_eq!(d.initial.readonly, None, "mixed");
        assert_eq!(d.op(), Err("Nothing changed".into()));
        d.attrs.readonly = Some(false);
        let Ok(Op::SetAttributes { attrs, .. }) = d.op() else {
            panic!("attributes");
        };
        assert_eq!(
            attrs,
            Attributes {
                readonly: Some(false),
                ..Attributes::default()
            }
        );
        if cfg!(unix) {
            d.mode = "9".into();
            assert!(d.op().is_err());
        }
        keel_vfs::fsattr::set_attributes(
            &b,
            &Attributes {
                readonly: Some(false),
                ..Attributes::default()
            },
        )
        .unwrap();
    }
}
