//! Task 85: **Delete permanently** for local files and folders. Never on a key: the
//! palette and the context menu's "More delete options" submenu only. The targets are
//! measured on a worker first (files, folders and bytes, links not followed), then a
//! dialog names the count and the bytes and asks to type `delete <count>`; the job
//! measures again and deletes nothing when the targets changed since. Nothing goes to the
//! Recycle Bin or Trash and the undo journal says it cannot be undone.

use crate::jobs::items;
use crate::keys::Action;
use crate::system_cleanup::SysCmd;
use humansize::{format_size, DECIMAL};
use keel_vfs::VPath;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// What the targets hold (each top-level item counts: a file is 1 file, a folder 1 folder
/// plus what is below it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Measured {
    pub paths: Vec<VPath>,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

impl Measured {
    /// Everything that goes: files, folders and links.
    pub fn count(&self) -> u64 {
        self.files + self.folders
    }

    /// The same targets holding the same: what the typed confirmation agreed to.
    pub fn same_as(&self, other: &Measured) -> bool {
        self == other
    }
}

/// Why `paths` cannot be deleted permanently here (None: they can). Local files and
/// folders only, never a drive or volume root.
pub fn refusal(paths: &[VPath]) -> Option<String> {
    if paths.is_empty() {
        return Some("Select the files or folders to delete permanently".into());
    }
    for p in paths {
        if p.split_archive().is_some() || p.to_local_path().is_none() {
            return Some(
                "Delete permanently works on files and folders on this computer only (use \
                 Delete elsewhere)"
                    .into(),
            );
        }
        if p.parent().is_none()
            || p.to_local_path()
                .is_some_and(|l| l.parent().is_none() || l.file_name().is_none())
        {
            return Some(format!("{} is a drive; it is never deleted", p.display()));
        }
    }
    None
}

/// Counts what `paths` hold (worker: walks the disk). Links and junctions count as one
/// item and are never followed; a folder that cannot be read fails the measure (its
/// content would be deleted unseen).
pub fn measure(paths: &[VPath], cancel: &AtomicBool) -> anyhow::Result<Measured> {
    let mut m = Measured {
        paths: paths.to_vec(),
        ..Measured::default()
    };
    for p in paths {
        let local = p
            .to_local_path()
            .ok_or_else(|| anyhow::anyhow!("{} is not on this computer", p.display()))?;
        walk(&local, &mut m, cancel)?;
    }
    Ok(m)
}

fn walk(path: &Path, m: &mut Measured, cancel: &AtomicBool) -> anyhow::Result<()> {
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let ft = meta.file_type();
    if ft.is_dir() && !ft.is_symlink() && !is_junction(&meta) {
        m.folders += 1;
        let list =
            std::fs::read_dir(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        for entry in list {
            let entry = entry.map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            walk(&entry.path(), m, cancel)?;
        }
    } else {
        m.files += 1;
        if ft.is_file() {
            m.bytes += meta.len();
        }
    }
    Ok(())
}

/// A Windows junction or other directory reparse point (`symlink_metadata` says "dir").
#[cfg(windows)]
fn is_junction(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_junction(_: &std::fs::Metadata) -> bool {
    false
}

/// "3 items (12 files, 4 folders, 1.2 GB)".
pub fn summary(m: &Measured) -> String {
    let files = match m.files {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    };
    let folders = match m.folders {
        0 => String::new(),
        1 => ", 1 folder".to_owned(),
        n => format!(", {n} folders"),
    };
    format!(
        "{} ({files}{folders}, {})",
        items(m.paths.len()),
        format_size(m.bytes, DECIMAL)
    )
}

/// The typed confirmation: shows the count and the bytes; Delete is enabled only once
/// `delete <count>` is typed (any case, spaces around ignored).
#[derive(Debug)]
pub struct Confirm {
    pub measured: Measured,
    pub typed: String,
    focus: bool,
}

impl Confirm {
    pub fn new(measured: Measured) -> Self {
        Self {
            measured,
            typed: String::new(),
            focus: true,
        }
    }

    /// What must be typed.
    pub fn phrase(&self) -> String {
        format!("delete {}", self.measured.count())
    }

    pub fn text(&self) -> String {
        format!(
            "Permanently delete {}? They do not go to the {} and cannot be restored or \
             undone.",
            summary(&self.measured),
            keel_vfs::trashbin::label()
        )
    }

    /// Whether the typed text is the phrase.
    pub fn matches(&self) -> bool {
        let typed: Vec<&str> = self.typed.split_whitespace().collect();
        let phrase = self.phrase();
        let want: Vec<&str> = phrase.split_whitespace().collect();
        typed.len() == want.len()
            && typed
                .iter()
                .zip(&want)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// The Delete button: the action when the phrase was typed, else None.
    pub fn answer(&self) -> Option<Action> {
        self.matches().then(|| {
            Action::System(SysCmd::DeletePermanentlyNow(Box::new(
                self.measured.clone(),
            )))
        })
    }
}

/// The dialog body (inside `dialogs::show`'s modal).
pub fn ui(ui: &mut egui::Ui, c: &mut Confirm, cancel: &mut bool) -> Option<Action> {
    let mut out = None;
    ui.strong("Delete permanently");
    ui.label(c.text());
    for p in c.measured.paths.iter().take(5) {
        ui.add(egui::Label::new(format!("  {}", p.display())).truncate());
    }
    if c.measured.paths.len() > 5 {
        ui.weak(format!("  and {} more", c.measured.paths.len() - 5));
    }
    ui.add_space(6.0);
    let phrase = c.phrase();
    ui.label(format!("Type \"{phrase}\" to confirm:"));
    let r = ui.add(egui::TextEdit::singleline(&mut c.typed).desired_width(f32::INFINITY));
    let r = crate::a11y::Named::named(r, "Type the confirmation");
    if std::mem::take(&mut c.focus) {
        r.request_focus();
    }
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let ok = c.matches();
        let delete = ui.add_enabled(ok, egui::Button::new("Delete permanently"));
        if delete.clicked() || (enter && ok) {
            out = c.answer();
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

/// The job: measures again (nothing is deleted when the targets changed since the
/// confirmation), then deletes each target for good. Stops at the first failure.
pub fn run(
    confirmed: &Measured,
    report: &dyn Fn(keel_vfs::Progress),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    if let Some(why) = refusal(&confirmed.paths) {
        anyhow::bail!(why);
    }
    let now = measure(&confirmed.paths, cancel)?;
    anyhow::ensure!(
        now.same_as(confirmed),
        "Nothing deleted: the items changed since you confirmed ({} now)",
        summary(&now)
    );
    let mut p = keel_vfs::Progress {
        total_items: confirmed.paths.len(),
        total_bytes: confirmed.bytes,
        ..keel_vfs::Progress::default()
    };
    for path in &confirmed.paths {
        if cancel.load(Ordering::Relaxed) {
            anyhow::ensure!(p.done_items > 0, "operation cancelled");
            anyhow::bail!(
                "Cancelled: {} of {} deleted permanently",
                p.done_items,
                confirmed.paths.len()
            );
        }
        p.current = path.display();
        report(p.clone());
        if let Err(e) = keel_vfs::local::delete_permanently(path) {
            if p.done_items == 0 {
                return Err(e);
            }
            anyhow::bail!(
                "{} of {} deleted permanently; stopped at {}: {e:#}",
                p.done_items,
                confirmed.paths.len(),
                path.display()
            );
        }
        p.done_items += 1;
    }
    p.done_bytes = p.total_bytes;
    report(p);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("keel-permdel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn measured(files: u64, folders: u64, bytes: u64) -> Measured {
        Measured {
            paths: vec![VPath::local("/x/a"), VPath::local("/x/b")],
            files,
            folders,
            bytes,
        }
    }

    #[test]
    fn confirmation_names_count_and_bytes_and_needs_the_phrase() {
        let mut c = Confirm::new(measured(12, 3, 4_200_000));
        assert_eq!(c.phrase(), "delete 15");
        let text = c.text();
        assert!(
            text.contains("2 items (12 files, 3 folders, 4.20 MB)"),
            "{text}"
        );
        assert!(text.contains("cannot be restored or undone"), "{text}");
        for wrong in [
            "",
            "delete",
            "delete 14",
            "delete 15 now",
            "remove 15",
            "delete15",
        ] {
            c.typed = wrong.into();
            assert!(!c.matches(), "{wrong:?}");
            assert_eq!(c.answer(), None, "{wrong:?}");
        }
        for right in ["delete 15", "  DELETE   15 ", "Delete 15"] {
            c.typed = right.into();
            assert!(c.matches(), "{right:?}");
        }
        assert_eq!(
            c.answer(),
            Some(Action::System(SysCmd::DeletePermanentlyNow(Box::new(
                measured(12, 3, 4_200_000)
            ))))
        );
        // One file: singular wording, no folder part.
        let one = Confirm::new(Measured {
            paths: vec![VPath::local("/x/a")],
            files: 1,
            folders: 0,
            bytes: 10,
        });
        assert_eq!(one.phrase(), "delete 1");
        assert!(
            one.text().contains("1 item (1 file, 10 B)"),
            "{}",
            one.text()
        );
    }

    #[test]
    fn refuses_remote_archive_and_roots() {
        assert!(refusal(&[]).is_some());
        let remote = VPath::parse("sftp://host/home/a").unwrap();
        assert!(refusal(&[remote]).unwrap().contains("this computer only"));
        let zip = VPath::join_archive(&VPath::local(std::env::temp_dir().join("a.zip")), "x");
        assert!(refusal(&[zip]).is_some());
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        assert!(refusal(&[VPath::local(root)]).unwrap().contains("drive"));
        let fine = VPath::local(std::env::temp_dir().join("x.txt"));
        assert_eq!(refusal(&[fine]), None);
    }

    #[test]
    fn measures_then_deletes_only_what_was_confirmed() {
        let d = tmp("run");
        std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
        std::fs::write(d.join("sub/a.bin"), [0u8; 100]).unwrap();
        std::fs::write(d.join("sub/deeper/b.bin"), [0u8; 50]).unwrap();
        std::fs::write(d.join("c.txt"), b"hello").unwrap();
        std::fs::write(d.join("keep.txt"), b"stays").unwrap();
        let paths = vec![VPath::local(d.join("sub")), VPath::local(d.join("c.txt"))];
        let cancel = AtomicBool::new(false);
        let m = measure(&paths, &cancel).unwrap();
        assert_eq!((m.files, m.folders, m.bytes), (3, 2, 155));
        assert_eq!(Confirm::new(m.clone()).phrase(), "delete 5");

        // Changed since the confirmation: nothing goes.
        std::fs::write(d.join("sub/new.bin"), [1u8; 7]).unwrap();
        let e = run(&m, &|_| {}, &cancel).unwrap_err().to_string();
        assert!(e.starts_with("Nothing deleted"), "{e}");
        assert!(d.join("sub/a.bin").exists() && d.join("c.txt").exists());
        std::fs::remove_file(d.join("sub/new.bin")).unwrap();

        run(&m, &|_| {}, &cancel).unwrap();
        assert!(!d.join("sub").exists() && !d.join("c.txt").exists());
        assert!(d.join("keep.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_counted_once_and_never_followed() {
        let d = tmp("links");
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(d.join("outside/big.bin"), [0u8; 1000]).unwrap();
        std::fs::create_dir_all(d.join("t")).unwrap();
        std::os::unix::fs::symlink(d.join("outside"), d.join("t/link")).unwrap();
        let paths = vec![VPath::local(d.join("t"))];
        let m = measure(&paths, &AtomicBool::new(false)).unwrap();
        assert_eq!((m.files, m.folders, m.bytes), (1, 1, 0));
        run(&m, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(
            d.join("outside/big.bin").exists(),
            "the link's target stays"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
