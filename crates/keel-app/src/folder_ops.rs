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
            Kind::Merge => crate::i18n::tr("folder_ops.merge_title"),
            Kind::Flatten => crate::i18n::tr("folder_ops.flatten_title"),
            Kind::Organize => crate::i18n::tr("folder_ops.organize_title"),
            Kind::Attributes => crate::i18n::tr("folder_ops.attributes_title"),
            Kind::Link => crate::i18n::tr("folder_ops.link_title"),
            Kind::RemoveEmpty => crate::i18n::tr("folder_ops.remove_empty_title"),
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
    ui.label(crate::i18n::tr("folder_ops.when_taken"));
    ui.horizontal(|ui| {
        for (key, rule) in [
            ("dialogs.skip", OnConflict::Skip),
            ("dialogs.keep_both", OnConflict::RenameNew),
            ("dialogs.overwrite", OnConflict::Overwrite),
        ] {
            ui.radio_value(c, rule, crate::i18n::tr(key));
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
            ui.label(crate::i18n::tr("folder_ops.merge_into"));
            for (i, t) in d.targets.iter().enumerate() {
                ui.radio_value(&mut d.into, i, t.name());
            }
            ui.weak(crate::i18n::tr("folder_ops.merge_removes"));
            conflict_choice(ui, &mut d.conflict);
        }
        Kind::Flatten => {
            ui.label(crate::i18n::tr("folder_ops.flatten_body").replace("{one}", &one));
            conflict_choice(ui, &mut d.conflict);
        }
        Kind::RemoveEmpty => {
            let local = d
                .targets
                .first()
                .is_some_and(|t| t.to_local_path().is_some());
            ui.label(if local {
                crate::i18n::tr("folder_ops.remove_empty_local")
                    .replace("{one}", &one)
                    .replace("{trash}", keel_vfs::trashbin::label())
            } else {
                crate::i18n::tr("folder_ops.remove_empty_remote").replace("{one}", &one)
            });
        }
        Kind::Organize => {
            ui.label(crate::i18n::tr("folder_ops.organize_body").replace("{one}", &one));
            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut d.by,
                    OrganizeBy::Type,
                    crate::i18n::tr("folder_ops.organize_type"),
                )
                .on_hover_text(crate::i18n::tr("folder_ops.organize_type_tip"));
                ui.radio_value(
                    &mut d.by,
                    OrganizeBy::Year,
                    crate::i18n::tr("folder_ops.organize_year"),
                );
                ui.radio_value(
                    &mut d.by,
                    OrganizeBy::Month,
                    crate::i18n::tr("folder_ops.organize_year_month"),
                );
            });
            conflict_choice(ui, &mut d.conflict);
        }
        Kind::Attributes => {
            ui.label(match d.targets.len() {
                1 => crate::i18n::tr("folder_ops.attributes_one").replace("{one}", &one),
                n => crate::i18n::tr("folder_ops.attributes_n").replace("{n}", &n.to_string()),
            });
            if cfg!(any(windows, target_os = "macos")) {
                tri(
                    ui,
                    &mut d.attrs.hidden,
                    crate::i18n::tr("folder_ops.attributes_hidden"),
                );
            }
            tri(
                ui,
                &mut d.attrs.readonly,
                crate::i18n::tr("folder_ops.attributes_readonly"),
            );
            if cfg!(unix) {
                ui.horizontal(|ui| {
                    let mode = crate::i18n::tr("folder_ops.attributes_mode");
                    ui.label(mode);
                    let r = ui.add(egui::TextEdit::singleline(&mut d.mode).desired_width(60.0));
                    r.named(mode);
                });
            }
        }
        Kind::Link => {
            ui.label(crate::i18n::tr("folder_ops.link_body").replace("{one}", &one));
            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut d.link_kind,
                    LinkKind::Symlink,
                    crate::i18n::tr("folder_ops.link_symlink"),
                );
                if cfg!(windows) && d.target_is_dir {
                    ui.radio_value(
                        &mut d.link_kind,
                        LinkKind::Junction,
                        crate::i18n::tr("folder_ops.link_junction"),
                    );
                }
                if !d.target_is_dir {
                    ui.radio_value(
                        &mut d.link_kind,
                        LinkKind::Hardlink,
                        crate::i18n::tr("folder_ops.link_hardlink"),
                    );
                }
            });
            if d.link_kind == LinkKind::Symlink && !d.symlinks_ok {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    crate::i18n::tr("folder_ops.link_symlink_warning"),
                );
                let (key, kind) = match d.target_is_dir {
                    true => ("folder_ops.link_make_junction", LinkKind::Junction),
                    false => ("folder_ops.link_make_hardlink", LinkKind::Hardlink),
                };
                if ui.button(crate::i18n::tr(key)).clicked() {
                    d.link_kind = kind;
                }
            }
            ui.horizontal(|ui| {
                ui.label(crate::i18n::tr("folder_ops.link_name_label"));
                let r = ui.add(egui::TextEdit::singleline(&mut d.link_name).desired_width(240.0));
                r.named(crate::i18n::tr("folder_ops.link_name_a11y"));
            });
        }
        // Not shown: it previews at once (`Kind::asks`).
        Kind::FolderDates => {
            ui.label(crate::i18n::tr("folder_ops.folder_dates_body").replace("{one}", &one));
        }
    }
    if let Some(e) = &d.error {
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    ui.add_space(8.0);
    let mut out = None;
    ui.horizontal(|ui| {
        let go = ui.button(crate::i18n::tr("folder_ops.preview"));
        crate::a11y::first_focus(&go);
        let enter = !std::mem::take(&mut d.fresh) && ui.input(|i| i.key_pressed(Key::Enter));
        if go.clicked() || enter {
            match d.op() {
                Ok(op) => out = Some(Action::Library(LibCmd::Plan(op))),
                Err(e) => d.error = Some(e),
            }
        }
        *cancel |= ui.button(crate::i18n::tr("dialogs.cancel")).clicked();
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

    #[test]
    fn folder_op_titles_in_spanish() {
        crate::i18n::set("es");
        assert_eq!(Kind::Merge.title(), "Fusionar en…");
        assert_eq!(Kind::Flatten.title(), "Aplanar…");
        assert_eq!(Kind::Organize.title(), "Organizar por…");
        assert_eq!(Kind::Attributes.title(), "Atributos…");
        assert_eq!(Kind::Link.title(), "Crear enlace…");
        assert_eq!(Kind::RemoveEmpty.title(), "Quitar carpetas vacías…");
        assert_eq!(
            crate::i18n::tr("folder_ops.when_taken"),
            "Si el nombre ya está en uso:"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.merge_into"),
            "Mover todo lo de las carpetas seleccionadas a:"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.merge_removes"),
            "Las otras carpetas se eliminan cuando queden vacías."
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.flatten_body").replace("{one}", "docs"),
            "Mover cada archivo de las subcarpetas de docs a esa carpeta y luego eliminar las \
             subcarpetas que queden vacías. La vista previa los cuenta."
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.remove_empty_local")
                .replace("{one}", "docs")
                .replace("{trash}", "Trash"),
            "Mover cada carpeta vacía bajo docs a la Trash: carpetas sin archivos y sin \
             enlaces, a cualquier profundidad. La vista previa las lista."
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.remove_empty_remote").replace("{one}", "docs"),
            "Eliminar cada carpeta vacía bajo docs: carpetas sin archivos y sin enlaces, a \
             cualquier profundidad. Donde no hay papelera (SFTP, FTP, S3, WebDAV) se eliminan \
             para siempre. La vista previa las lista y lo dice."
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.organize_body").replace("{one}", "docs"),
            "Mover los archivos de docs a subcarpetas por:"
        );
        assert_eq!(crate::i18n::tr("folder_ops.organize_type"), "Tipo");
        assert_eq!(
            crate::i18n::tr("folder_ops.organize_type_tip"),
            "Imágenes, vídeos, audio, documentos, archivos, otros"
        );
        assert_eq!(crate::i18n::tr("folder_ops.organize_year"), "Año");
        assert_eq!(
            crate::i18n::tr("folder_ops.organize_year_month"),
            "Año y mes"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.attributes_one").replace("{one}", "docs"),
            "Para docs"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.attributes_n").replace("{n}", "3"),
            "Para 3 elementos"
        );
        assert_eq!(crate::i18n::tr("folder_ops.attributes_hidden"), "Oculto");
        assert_eq!(
            crate::i18n::tr("folder_ops.attributes_readonly"),
            "Solo lectura"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.attributes_mode"),
            "Modo (octal)"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.link_body").replace("{one}", "nota.txt"),
            "Un enlace a nota.txt, en la misma carpeta"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.link_symlink"),
            "Enlace simbólico"
        );
        assert_eq!(crate::i18n::tr("folder_ops.link_junction"), "Unión");
        assert_eq!(crate::i18n::tr("folder_ops.link_hardlink"), "Enlace físico");
        assert_eq!(
            crate::i18n::tr("folder_ops.link_symlink_warning"),
            "Windows solo crea enlaces simbólicos con el Modo desarrollador activado \
             (Configuración → Sistema → Para desarrolladores) o en un programa elevado; Keel \
             nunca se eleva."
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.link_make_junction"),
            "Hacer una unión en su lugar"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.link_make_hardlink"),
            "Hacer un enlace físico en su lugar"
        );
        assert_eq!(crate::i18n::tr("folder_ops.link_name_label"), "Nombre");
        assert_eq!(
            crate::i18n::tr("folder_ops.link_name_a11y"),
            "Nombre del enlace"
        );
        assert_eq!(
            crate::i18n::tr("folder_ops.folder_dates_body").replace("{one}", "docs"),
            "Poner a cada carpeta bajo docs la fecha del cambio más reciente de dentro"
        );
        assert_eq!(crate::i18n::tr("folder_ops.preview"), "Vista previa");
        crate::i18n::set("en");
    }
}
