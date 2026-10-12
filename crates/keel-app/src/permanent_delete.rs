//! Task 85: **Delete permanently** for local files and folders. Never on a key: the
//! palette and the context menu's "More delete options" submenu only. The targets are
//! measured on a worker first (files, folders and bytes, links not followed), then a
//! dialog names the count and the bytes and asks to type `delete <count>`; the job
//! measures again, deletes nothing when the targets changed since, and then deletes exactly
//! the items it walked, deepest first, one at a time (never a recursive delete). A cloud
//! placeholder (a OneDrive-style folder, or a file offline or fetched on access) and any
//! other folder that is a reparse point and not a link is never entered: the measure fails
//! and nothing is deleted. Nothing goes to the Recycle Bin or Trash and the undo journal
//! says it cannot be undone.

use crate::jobs::items;
use crate::keys::Action;
use crate::system_cleanup::SysCmd;
use humansize::{format_size, DECIMAL};
use keel_vfs::VPath;
use std::path::{Path, PathBuf};
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

/// What one walked item is, which says how it is deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    /// A symbolic link or junction (`dir`: a Windows link to a folder): the link goes,
    /// never what it points to.
    Link {
        dir: bool,
    },
    /// An ordinary folder (never a reparse point that is not a link): what it holds is
    /// walked and counted.
    Folder,
}

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// What an item with Windows attributes `attrs` is. `link`: a name-surrogate reparse point
/// (symbolic link, junction: Rust's `FileType::is_symlink`), which is one item and never
/// followed. Any other cloud placeholder (OneDrive and other sync apps: a folder that is a
/// reparse point, or a file or folder offline or fetched on access) is `Err(())`: deleting
/// it deletes it from the cloud, and a folder of that kind lists what the cloud holds, not
/// what is here. A plain file with a reparse point (compressed by the system, say) is an
/// ordinary file.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn classify(attrs: u32, link: bool) -> Result<Kind, ()> {
    let dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    if link {
        return Ok(Kind::Link { dir });
    }
    let recalled = attrs
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0;
    if recalled || (dir && attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0) {
        return Err(());
    }
    Ok(if dir { Kind::Folder } else { Kind::File })
}

#[cfg(windows)]
fn kind_of(meta: &std::fs::Metadata) -> Result<Kind, ()> {
    use std::os::windows::fs::MetadataExt;
    classify(meta.file_attributes(), meta.file_type().is_symlink())
}

#[cfg(not(windows))]
fn kind_of(meta: &std::fs::Metadata) -> Result<Kind, ()> {
    let ft = meta.file_type();
    Ok(if ft.is_symlink() {
        Kind::Link { dir: false }
    } else if ft.is_dir() {
        Kind::Folder
    } else {
        Kind::File
    })
}

/// Counts what `paths` hold (worker: walks the disk). Links and junctions count as one
/// item and are never followed; every other folder is listed, and one that cannot be
/// listed fails the measure (its content would be deleted unseen). A cloud placeholder or
/// other reparse-point folder fails it too, before anything is listed below it.
pub fn measure(paths: &[VPath], cancel: &AtomicBool) -> anyhow::Result<Measured> {
    Ok(scan(paths, cancel)?.0)
}

/// Every item under each target (the target first, each folder before what it holds).
type Walked = Vec<Vec<(PathBuf, Kind)>>;

/// [`measure`], also returning what was walked: exactly what [`run`] deletes.
fn scan(paths: &[VPath], cancel: &AtomicBool) -> anyhow::Result<(Measured, Walked)> {
    let mut m = Measured {
        paths: paths.to_vec(),
        ..Measured::default()
    };
    let mut walked = Vec::with_capacity(paths.len());
    for p in paths {
        let local = p
            .to_local_path()
            .ok_or_else(|| anyhow::anyhow!("{} is not on this computer", p.display()))?;
        walked.push(walk(local, &mut m, cancel)?);
    }
    Ok((m, walked))
}

/// Walks `root` with a stack of its own (a tree nested thousands of folders deep fails
/// nothing but memory).
fn walk(
    root: PathBuf,
    m: &mut Measured,
    cancel: &AtomicBool,
) -> anyhow::Result<Vec<(PathBuf, Kind)>> {
    let mut items = Vec::new();
    let mut stack = vec![root];
    while let Some(path) = stack.pop() {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let Ok(kind) = kind_of(&meta) else {
            anyhow::bail!(
                "{} is a cloud placeholder or reparse point: Delete permanently never goes \
                 into those (use Delete, or remove it in the app that syncs it)",
                path.display()
            );
        };
        match kind {
            Kind::Folder => {
                m.folders += 1;
                let list = std::fs::read_dir(&path)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                for entry in list {
                    let entry = entry.map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                    stack.push(entry.path());
                }
            }
            Kind::Link { .. } => m.files += 1,
            Kind::File => {
                m.files += 1;
                m.bytes += meta.len();
            }
        }
        items.push((path, kind));
    }
    Ok(items)
}

/// Deletes one walked item: a file, a link (itself), or a folder that must be empty by
/// now (one that got something new is not deleted).
fn remove(path: &Path, kind: Kind) -> std::io::Result<()> {
    match kind {
        Kind::Folder | Kind::Link { dir: true } => std::fs::remove_dir(path),
        Kind::Link { dir: false } => std::fs::remove_file(path),
        Kind::File => std::fs::remove_file(path).or_else(|e| {
            // A read-only file (Windows): clear the attribute, then delete it.
            let meta = std::fs::symlink_metadata(path)?;
            if !cfg!(windows) || !meta.file_type().is_file() || !meta.permissions().readonly() {
                return Err(e);
            }
            let mut perm = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            std::fs::set_permissions(path, perm)?;
            std::fs::remove_file(path)
        }),
    }
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
/// confirmation), then deletes what that walk found, each target's items deepest first,
/// one at a time: links are removed themselves and a folder only once it is empty, so
/// nothing that was not counted goes. Stops at the first failure.
pub fn run(
    confirmed: &Measured,
    report: &dyn Fn(keel_vfs::Progress),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    if let Some(why) = refusal(&confirmed.paths) {
        anyhow::bail!(why);
    }
    let (now, walked) = scan(&confirmed.paths, cancel)?;
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
    for (path, items) in confirmed.paths.iter().zip(&walked) {
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
        let gone = items.iter().rev().try_for_each(|(item, kind)| {
            remove(item, *kind).map_err(|e| anyhow::anyhow!("delete {}: {e}", item.display()))
        });
        if let Err(e) = gone {
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

    #[test]
    fn cloud_placeholders_are_refused_and_links_and_plain_reparse_files_are_not() {
        const DIR: u32 = 0x10;
        const REPARSE: u32 = 0x400;
        const NORMAL: u32 = 0x80;
        // A OneDrive placeholder folder (or any folder that is a reparse point but no
        // link): never entered.
        assert_eq!(classify(DIR | REPARSE, false), Err(()));
        assert_eq!(classify(DIR | 0x4_0000, false), Err(()));
        // A junction or folder symbolic link: the link itself, never followed.
        assert_eq!(classify(DIR | REPARSE, true), Ok(Kind::Link { dir: true }));
        assert_eq!(classify(REPARSE, true), Ok(Kind::Link { dir: false }));
        assert_eq!(classify(DIR, false), Ok(Kind::Folder));
        assert_eq!(classify(NORMAL, false), Ok(Kind::File));
        // A file with a reparse point alone (system compression, say) is a plain file.
        assert_eq!(classify(NORMAL | REPARSE, false), Ok(Kind::File));
        // Online-only, recall-on-open and offline files.
        assert_eq!(classify(NORMAL | 0x40_0000, false), Err(()));
        assert_eq!(classify(NORMAL | REPARSE | 0x40_0000, false), Err(()));
        assert_eq!(classify(NORMAL | 0x4_0000, false), Err(()));
        assert_eq!(classify(NORMAL | 0x1000, false), Err(()));
    }

    #[test]
    fn a_deep_tree_is_measured_and_deleted_without_recursion() {
        let d = tmp("deep");
        let mut deepest = d.join("t");
        // macOS paths are at most 1024 bytes long.
        let depth = if cfg!(target_os = "macos") { 400 } else { 1500 };
        for _ in 0..depth {
            deepest.push("a");
        }
        std::fs::create_dir_all(&deepest).unwrap();
        std::fs::write(deepest.join("f.bin"), [0u8; 3]).unwrap();
        let paths = vec![VPath::local(d.join("t"))];
        // A small stack: a walk recursing per folder level would overflow it.
        let m = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let cancel = AtomicBool::new(false);
                let m = measure(&paths, &cancel).unwrap();
                run(&m, &|_| {}, &cancel).unwrap();
                m
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!((m.files, m.folders, m.bytes), (1, 1501, 3));
        assert!(!d.join("t").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_that_got_something_new_mid_delete_stays() {
        let d = tmp("late");
        std::fs::create_dir_all(d.join("t/sub")).unwrap();
        std::fs::write(d.join("t/sub/a.bin"), [0u8; 4]).unwrap();
        let paths = vec![VPath::local(d.join("t"))];
        let cancel = AtomicBool::new(false);
        let m = measure(&paths, &cancel).unwrap();
        let (now, walked) = scan(&paths, &cancel).unwrap();
        assert!(now.same_as(&m));
        // Something new after the last walk: the folders holding it are not deleted.
        std::fs::write(d.join("t/sub/new.bin"), b"new").unwrap();
        let gone = walked[0].iter().rev().try_for_each(|(p, k)| remove(p, *k));
        assert!(gone.is_err());
        assert!(d.join("t/sub/new.bin").exists());
        assert!(!d.join("t/sub/a.bin").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn junctions_are_one_item_and_never_followed_and_read_only_files_go() {
        let d = tmp("junction");
        std::fs::create_dir_all(d.join("outside")).unwrap();
        std::fs::write(d.join("outside/big.bin"), [0u8; 1000]).unwrap();
        std::fs::create_dir_all(d.join("t")).unwrap();
        keel_vfs::fsattr::make_link(
            &d.join("outside"),
            &d.join("t/j"),
            keel_vfs::LinkKind::Junction,
        )
        .unwrap();
        std::fs::write(d.join("t/ro.txt"), b"ro").unwrap();
        let mut perm = std::fs::metadata(d.join("t/ro.txt")).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(d.join("t/ro.txt"), perm).unwrap();
        let paths = vec![VPath::local(d.join("t"))];
        let m = measure(&paths, &AtomicBool::new(false)).unwrap();
        assert_eq!((m.files, m.folders, m.bytes), (2, 1, 2));
        run(&m, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(!d.join("t").exists());
        assert!(
            d.join("outside/big.bin").exists(),
            "the junction's target stays"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A folder carrying a reparse point that is not a link (as OneDrive's placeholders
    /// do; here a tag no driver handles, so its content cannot be listed): never a leaf,
    /// and as it cannot be listed, nothing is deleted.
    #[cfg(windows)]
    #[test]
    fn a_reparse_folder_that_cannot_be_listed_is_never_deleted_unseen() {
        use std::ffi::c_void;
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        #[link(name = "kernel32")]
        extern "system" {
            fn DeviceIoControl(
                device: *mut c_void,
                code: u32,
                input: *const c_void,
                input_len: u32,
                output: *mut c_void,
                output_len: u32,
                returned: *mut u32,
                overlapped: *mut c_void,
            ) -> i32;
        }
        const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
        const FSCTL_DELETE_REPARSE_POINT: u32 = 0x0009_00AC;
        const TAG: u32 = 0x0000_0123; // a third-party tag, not a name surrogate
        let guid = *b"keel-test-guid..";
        let d = tmp("reparse");
        let odd = d.join("t/odd");
        std::fs::create_dir_all(&odd).unwrap();
        std::fs::write(odd.join("inside.txt"), b"unseen").unwrap();
        let ioctl = |code: u32, data: &[u8]| {
            let mut buf = TAG.to_le_bytes().to_vec();
            buf.extend((data.len() as u16).to_le_bytes());
            buf.extend(0u16.to_le_bytes());
            buf.extend(guid);
            buf.extend(data);
            let dir = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(0x0020_0000 | 0x0200_0000) // open reparse point, backup
                .open(&odd)?;
            let mut n = 0u32;
            // SAFETY: `buf` outlives the call and its length is passed; no output buffer.
            let ok = unsafe {
                DeviceIoControl(
                    dir.as_raw_handle(),
                    code,
                    buf.as_ptr().cast(),
                    buf.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        };
        if let Err(e) = ioctl(FSCTL_SET_REPARSE_POINT, b"data") {
            eprintln!("cannot set a reparse point here ({e}); skipped");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        let paths = vec![VPath::local(d.join("t"))];
        let m = measure(&paths, &AtomicBool::new(false));
        ioctl(FSCTL_DELETE_REPARSE_POINT, b"").unwrap();
        let error = m.unwrap_err().to_string();
        assert!(error.contains("placeholder or reparse point"), "{error}");
        assert!(odd.join("inside.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn offline_files_are_refused_before_any_item_is_deleted() {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn SetFileAttributesW(path: *const u16, attrs: u32) -> i32;
        }
        let d = tmp("offline");
        let file = d.join("offline.bin");
        std::fs::write(&file, b"leave me").unwrap();
        let paths = vec![VPath::local(&d)];
        let cancel = AtomicBool::new(false);
        let confirmed = measure(&paths, &cancel).unwrap();
        let wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path in this test's temporary directory.
        assert_ne!(
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_OFFLINE) },
            0
        );
        let measured = measure(&paths, &cancel);
        let deleted = run(&confirmed, &|_| {}, &cancel);
        // Restore the ordinary attribute before asserting or cleaning up the fixture.
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), 0x80) }, 0);
        assert!(measured
            .unwrap_err()
            .to_string()
            .contains("placeholder or reparse point"));
        assert!(deleted.is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"leave me");
        std::fs::remove_dir_all(d).unwrap();
    }
}
