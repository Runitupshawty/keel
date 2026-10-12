//! Task 85: replace confirmed duplicates with hard links. The duplicate finder's groups
//! (library BLAKE3 content ids) are planned on a worker: local, online copies only, and
//! within a group only the copies on the same volume as the one kept (the first such
//! copy); copies that already are that file (one hard link) are left out. The preview
//! lists every replacement; the job opens both files (on Windows refusing writers until the
//! duplicate is replaced), compares them byte by byte, makes the link under a temporary
//! name next to the duplicate, checks that the link is the file compared and that neither
//! changed, and renames it over the duplicate (the same volume, so the duplicate is never
//! missing). A link takes the kept copy's permissions and attributes and drops the
//! duplicate's alternate data streams, so pairs where those differ, and read-only copies,
//! are left alone. Undo (Ctrl+Z, the toast) gives each path its own copy again with its old
//! modified time.

use crate::keys::Action;
use crate::library::DupGroup;
use crate::system_cleanup::SysCmd;
use humansize::{format_size, DECIMAL};
use keel_vfs::ops::Stamp;
use keel_vfs::VPath;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

/// One replacement: `dup` becomes a hard link to `keep`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub keep: PathBuf,
    pub dup: PathBuf,
    pub size: u64,
}

/// Why a copy is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// On a remote host, a cloud or another device.
    NotLocal,
    /// Its source is offline.
    Offline,
    /// Not on the volume of the copy kept: a hard link cannot cross volumes.
    OtherVolume,
    /// Already a hard link to the copy kept.
    AlreadyLinked,
    /// Gone, not a plain file, or its size is not the group's any more.
    Changed,
    /// Its permissions, owner or attributes are not the copy kept's (a link would take
    /// the kept copy's).
    Permissions,
    /// It has alternate data streams (a link would drop them).
    #[cfg_attr(not(windows), allow(dead_code))]
Streams,
}

impl Skip {
    pub fn text(self) -> &'static str {
        match self {
            Skip::NotLocal => "not on this computer",
            Skip::Offline => "offline",
            Skip::OtherVolume => "on another volume",
            Skip::AlreadyLinked => "already hard links",
            Skip::Changed => "changed or gone",
            Skip::Permissions => "other permissions or attributes",
            Skip::Streams => "has alternate data streams",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DedupPlan {
    pub pairs: Vec<Pair>,
    pub skipped: Vec<(VPath, Skip)>,
}

impl DedupPlan {
    /// Bytes freed when every pair is linked.
    pub fn bytes(&self) -> u64 {
        self.pairs.iter().map(|p| p.size).sum()
    }

    /// "5 duplicates (1.2 GB) become hard links; 2 left alone (on another volume)".
    pub fn summary(&self) -> String {
        let n = match self.pairs.len() {
            1 => "1 duplicate".to_owned(),
            n => format!("{n} duplicates"),
        };
        let mut text = format!(
            "{n} ({}) become hard links to the copy kept",
            format_size(self.bytes(), DECIMAL)
        );
        if !self.skipped.is_empty() {
            let mut why: Vec<&str> = self.skipped.iter().map(|(_, s)| s.text()).collect();
            why.sort_unstable();
            why.dedup();
            text += &format!("; {} left alone ({})", self.skipped.len(), why.join(", "));
        }
        text
    }
}

/// The volume of a local path (see `jobs::volume_key`); None when it cannot be told.
fn volume(path: &Path) -> Option<String> {
    crate::jobs::volume_key(&VPath::local(path))
}

/// Whether `a` and `b` are on one volume (both readable).
pub fn same_volume(a: &Path, b: &Path) -> bool {
    matches!((volume(a), volume(b)), (Some(x), Some(y)) if x == y)
}

/// The file's identity (volume and file index or device and inode), None when unreadable.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    Stamp::new(0, None).with_file_id(path).id
}

/// Whether `a` and `b` are one file already (hard links to each other).
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((file_id(a), file_id(b)), (Some(x), Some(y)) if x == y)
}

/// A plain file of `size` bytes (not a link, not a folder).
fn plain_file(path: &Path, size: u64) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file() && m.len() == size)
}

/// The replacements for `groups` (worker: reads the disk).
pub fn plan(groups: &[DupGroup]) -> DedupPlan {
    plan_by(groups, volume)
}

/// [`plan`] with the volume of each path from `vol`.
fn plan_by(groups: &[DupGroup], vol: impl Fn(&Path) -> Option<String>) -> DedupPlan {
    let mut out = DedupPlan::default();
    for g in groups {
        // (shown path, local path, volume) of the usable copies, in the finder's order.
        let mut usable = Vec::new();
        for r in &g.records {
            match r.path.to_local_path() {
                _ if r.offline => out.skipped.push((r.path.clone(), Skip::Offline)),
                None => out.skipped.push((r.path.clone(), Skip::NotLocal)),
                Some(l) if !plain_file(&l, g.size) => {
                    out.skipped.push((r.path.clone(), Skip::Changed))
                }
                Some(l) => {
                    let v = vol(&l);
                    usable.push((r.path.clone(), l, v));
                }
            }
        }
        // The volume holding most copies (the first such on a tie); a hard link cannot
        // cross volumes, and one volume per group keeps the preview readable.
        let mut best: Option<(&String, usize)> = None;
        for (_, _, v) in &usable {
            let Some(v) = v else { continue };
            let n = usable
                .iter()
                .filter(|(_, _, u)| u.as_ref() == Some(v))
                .count();
            if best.is_none_or(|(_, m)| n > m) {
                best = Some((v, n));
            }
        }
        let best = best.map(|(v, _)| v.clone());
        let mut on = Vec::new();
        for (shown, local, v) in usable {
            if v.is_some() && v == best {
                on.push(local);
            } else {
                out.skipped.push((shown, Skip::OtherVolume));
            }
        }
        // The first copy on that volume is kept; a lone copy has nothing to link to.
        let Some((keep, dups)) = on.split_first() else {
            continue;
        };
        for dup in dups {
            if same_file(keep, dup) {
                out.skipped.push((VPath::local(dup), Skip::AlreadyLinked));
            } else if let Err(why) = nothing_of_its_own(keep, dup) {
                out.skipped.push((VPath::local(dup), why));
            } else {
                out.pairs.push(Pair {
                    keep: keep.clone(),
                    dup: dup.clone(),
                    size: g.size,
                });
            }
        }
    }
    out
}

/// Whether the two files hold the same bytes (read side by side, 1 MiB at a time).
#[cfg(test)]
pub fn same_content(a: &Path, b: &Path, cancel: &AtomicBool) -> anyhow::Result<bool> {
    same_bytes(&File::open(a)?, &File::open(b)?, cancel)
}

/// Whether two open files hold the same bytes (read side by side from where they are, 1
/// MiB at a time).
fn same_bytes(a: &File, b: &File, cancel: &AtomicBool) -> anyhow::Result<bool> {
    let (mut fa, mut fb) = (a, b);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
        let n = read_full(&mut fa, &mut ba)?;
        let m = read_full(&mut fb, &mut bb)?;
        if n != m || ba[..n] != bb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buf` unless the file ends first; the bytes read.
fn read_full(f: &mut &File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// A free name next to `path` for a temporary item (`.<name>.keel-<what>[-n]`).
fn temp_beside(path: &Path, what: &str) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no name", path.display()))?
        .to_string_lossy();
    for n in 0..100 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let tmp = path.with_file_name(format!(".{name}.keel-{what}{suffix}"));
        if fs::symlink_metadata(&tmp).is_err() {
            return Ok(tmp);
        }
    }
    anyhow::bail!("no free temporary name next to {}", path.display())
}

/// What a link replaced: the path and its modified time before (restored by the undo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linked {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
}

/// Opens `path` to read it. On Windows nobody can open it for writing while the handle is
/// open (renaming or deleting it stays possible: the identity checks catch that); elsewhere
/// the size and modified time are checked again instead.
fn open_locked(path: &Path) -> std::io::Result<same_file::Handle> {
    let mut o = File::options();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_DELETE: u32 = 0x4;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        o.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    same_file::Handle::from_file(o.open(path)?)
}

/// The size and modified time of an open file.
fn stamp(h: &same_file::Handle) -> std::io::Result<(u64, Option<SystemTime>)> {
    let m = h.as_file().metadata()?;
    Ok((m.len(), m.modified().ok()))
}

/// Whether `path` is (still) the open file `h`.
fn is_handle(path: &Path, h: &same_file::Handle) -> bool {
    same_file::Handle::from_path(path).is_ok_and(|p| p == *h)
}

/// Why `dup` would lose something of its own as a hard link to `keep`: the link takes the
/// kept copy's permissions, owner and attributes, and `dup`'s alternate data streams are
/// gone with its file record (undo cannot bring either back).
pub fn nothing_of_its_own(keep: &Path, dup: &Path) -> Result<(), Skip> {
    sys::nothing_of_its_own(keep, dup)
}

#[cfg(windows)]
mod sys {
    //! Owner, DACL, attributes and named streams through Win32 (kernel32 / advapi32).
    use super::Skip;
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::path::Path;

    const OWNER_SECURITY_INFORMATION: u32 = 0x1;
    const GROUP_SECURITY_INFORMATION: u32 = 0x2;
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const ERROR_HANDLE_EOF: i32 = 38;
    /// Hidden, system and encrypted: what a link would change about the duplicate (read-only
    /// copies are refused on their own).
    const ATTRIBUTES: u32 = 0x2 | 0x4 | 0x4000;

    #[repr(C)]
    struct FindStreamData {
        size: i64,
        /// MAX_PATH + 36.
        name: [u16; 296],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn FindFirstStreamW(
            file: *const u16,
            level: i32,
            data: *mut FindStreamData,
            flags: u32,
        ) -> *mut c_void;
        fn FindNextStreamW(find: *mut c_void, data: *mut FindStreamData) -> i32;
        fn FindClose(find: *mut c_void) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn GetFileSecurityW(
            file: *const u16,
            info: u32,
            sd: *mut c_void,
            len: u32,
            needed: *mut u32,
        ) -> i32;
    }

    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let long = keel_vfs::long(path).map_err(io::Error::other)?;
        let mut w: Vec<u16> = long.as_os_str().encode_wide().collect();
        if w.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        w.push(0);
        Ok(w)
    }

    /// Whether `path` has a stream besides its unnamed data stream.
    pub fn named_streams(path: &Path) -> io::Result<bool> {
        let name = wide(path)?;
        let mut data = FindStreamData {
            size: 0,
            name: [0; 296],
        };
        // SAFETY: `name` is NUL-terminated, `data` is the FindStreamInfoStandard struct;
        // the search handle is closed below.
        let find = unsafe { FindFirstStreamW(name.as_ptr(), 0, &mut data, 0) };
        if find as isize == -1 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(ERROR_HANDLE_EOF) => Ok(false),
                _ => Err(e),
            };
        }
        let mut named = false;
        loop {
            let n = data.name.iter().position(|&c| c == 0).unwrap_or(296);
            if String::from_utf16_lossy(&data.name[..n]) != "::$DATA" {
                named = true;
                break;
            }
            // SAFETY: `find` is open; `data` as above.
            if unsafe { FindNextStreamW(find, &mut data) } == 0 {
                break;
            }
        }
        // SAFETY: opened above, closed once.
        unsafe { FindClose(find) };
        Ok(named)
    }

    /// The owner, group and DACL (self-relative security descriptor bytes).
    pub fn security(path: &Path) -> io::Result<Vec<u8>> {
        let name = wide(path)?;
        let info =
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut need = 0u32;
        // SAFETY: a size query (no buffer); `need` receives the size.
        unsafe { GetFileSecurityW(name.as_ptr(), info, std::ptr::null_mut(), 0, &mut need) };
        if need == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; need as usize];
        // SAFETY: `buf` holds `need` bytes.
        let ok = unsafe {
            GetFileSecurityW(
                name.as_ptr(),
                info,
                buf.as_mut_ptr().cast(),
                need,
                &mut need,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(need as usize);
        Ok(buf)
    }

    pub fn nothing_of_its_own(keep: &Path, dup: &Path) -> Result<(), Skip> {
        let attrs = |p: &Path| {
            std::fs::symlink_metadata(p)
                .map(|m| m.file_attributes() & ATTRIBUTES)
                .map_err(|_| Skip::Changed)
        };
        if attrs(keep)? != attrs(dup)? {
            return Err(Skip::Permissions);
        }
        match named_streams(dup) {
            Ok(false) => {}
            Ok(true) => return Err(Skip::Streams),
            Err(_) => return Err(Skip::Changed),
        }
        match (security(keep), security(dup)) {
            (Ok(a), Ok(b)) if a == b => Ok(()),
            (Ok(_), Ok(_)) => Err(Skip::Permissions),
            _ => Err(Skip::Changed),
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Skip;
    use std::path::Path;

    /// Mode bits, owner and group (extended attributes are not compared).
    pub fn nothing_of_its_own(keep: &Path, dup: &Path) -> Result<(), Skip> {
        use std::os::unix::fs::MetadataExt;
        let of = |p: &Path| {
            std::fs::symlink_metadata(p)
                .map(|m| (m.mode() & 0o7777, m.uid(), m.gid()))
                .map_err(|_| Skip::Changed)
        };
        if of(keep)? == of(dup)? {
            Ok(())
        } else {
            Err(Skip::Permissions)
        }
    }
}

/// Makes `pair.dup` a hard link to `pair.keep` after checking that both are plain files
/// of the planned size on one volume, not one file already, neither is read-only, the link
/// would not drop anything of the duplicate's own ([`nothing_of_its_own`]), and their
/// bytes are equal. Both stay open from before the compare until the duplicate is replaced
/// (on Windows no one can write either meanwhile); the new link must be the kept file that
/// was compared, and neither file may have changed or been replaced, else nothing changes.
/// The duplicate is replaced in one rename.
pub fn link(pair: &Pair, cancel: &AtomicBool) -> anyhow::Result<Linked> {
    link_with(pair, cancel, &|| {})
}

/// [`link`] with `between` run after the compare, before the link (tests change the files
/// there).
fn link_with(pair: &Pair, cancel: &AtomicBool, between: &dyn Fn()) -> anyhow::Result<Linked> {
    let (keep, dup) = (&pair.keep, &pair.dup);
    anyhow::ensure!(
        plain_file(keep, pair.size) && plain_file(dup, pair.size),
        "{} or {} changed since the preview",
        keep.display(),
        dup.display()
    );
    anyhow::ensure!(
        same_volume(keep, dup),
        "{} is not on the volume of {}",
        dup.display(),
        keep.display()
    );
    anyhow::ensure!(
        !same_file(keep, dup),
        "{} already is a hard link to {}",
        dup.display(),
        keep.display()
    );
    let opened = |p: &Path| open_locked(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()));
    let (kh, dh) = (opened(keep)?, opened(dup)?);
    let (kb, db) = (stamp(&kh)?, stamp(&dh)?);
    anyhow::ensure!(
        is_handle(keep, &kh) && is_handle(dup, &dh) && kb.0 == pair.size && db.0 == pair.size,
        "{} or {} changed since the preview",
        keep.display(),
        dup.display()
    );
    for p in [dup, keep] {
        anyhow::ensure!(
            !fs::symlink_metadata(p)?.permissions().readonly(),
            "{} is read-only",
            p.display()
        );
    }
    if let Err(why) = nothing_of_its_own(keep, dup) {
        anyhow::bail!("{} left alone: {}", dup.display(), why.text());
    }
    anyhow::ensure!(
        same_bytes(kh.as_file(), dh.as_file(), cancel)?,
        "{} and {} are not the same bytes",
        keep.display(),
        dup.display()
    );
    between();
    let tmp = temp_beside(dup, "link")?;
    fs::hard_link(keep, &tmp)
        .map_err(|e| anyhow::anyhow!("linking {} to {}: {e}", dup.display(), keep.display()))?;
    // The link must be the file compared, and neither may have changed since.
    let unchanged = || -> anyhow::Result<()> {
        anyhow::ensure!(
            is_handle(&tmp, &kh) && stamp(&kh)? == kb,
            "{} changed while it was compared",
            keep.display()
        );
        anyhow::ensure!(
            is_handle(dup, &dh) && stamp(&dh)? == db,
            "{} changed while it was compared",
            dup.display()
        );
        Ok(())
    };
    if let Err(e) = unchanged() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, dup) {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("replacing {}: {e}", dup.display());
    }
    Ok(Linked {
        path: dup.clone(),
        mtime: db.1,
    })
}

/// The undo of [`link`]: `path` gets its own copy of the bytes again (copied next to it,
/// its old modified time set, renamed over the link). A read-only file stays read-only.
pub fn unlink(path: &Path, mtime: Option<SystemTime>) -> anyhow::Result<()> {
    let tmp = temp_beside(path, "copy")?;
    let copied = (|| -> anyhow::Result<()> {
        fs::copy(path, &tmp)?;
        let readonly = fs::metadata(&tmp)?.permissions().readonly();
        if let Some(t) = mtime {
            if readonly {
                set_readonly(&tmp, false)?;
            }
            File::options().write(true).open(&tmp)?.set_modified(t)?;
            if readonly {
                set_readonly(&tmp, true)?;
            }
        }
        if fs::symlink_metadata(path)?.permissions().readonly() {
            replace_read_only(&tmp, path)?;
        } else {
            fs::rename(&tmp, path)?;
        }
        Ok(())
    })();
    if let Err(e) = copied {
        if fs::metadata(&tmp).is_ok_and(|m| m.permissions().readonly()) {
            let _ = set_readonly(&tmp, false);
        }
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("copying {} back: {e:#}", path.display());
    }
    Ok(())
}

/// Renames `tmp` over `path` when `path` is read-only (Windows refuses to replace such a
/// file): `path` is moved aside, `tmp` takes its name, and the old name goes. The
/// attribute belongs to the file record the kept copy shares, so it is not cleared unless
/// the system will not delete the name otherwise.
fn replace_read_only(tmp: &Path, path: &Path) -> anyhow::Result<()> {
    let old = temp_beside(path, "old")?;
    fs::rename(path, &old)?;
    if let Err(e) = fs::rename(tmp, path) {
        let _ = fs::rename(&old, path);
        return Err(e.into());
    }
    if fs::remove_file(&old).is_err() {
        set_readonly(&old, false)?;
        fs::remove_file(&old)?;
    }
    Ok(())
}

/// Sets or clears the read-only attribute (Windows) or the write bits (elsewhere; clearing
/// gives back the owner's only).
#[allow(clippy::permissions_set_readonly_false)]
fn set_readonly(path: &Path, on: bool) -> std::io::Result<()> {
    let mut perm = fs::metadata(path)?.permissions();
    #[cfg(unix)]
    if !on {
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(perm.mode() | 0o200);
        return fs::set_permissions(path, perm);
    }
    perm.set_readonly(on);
    fs::set_permissions(path, perm)
}

/// The job: links each pair; pairs that fail a check are skipped and counted. Returns what
/// was linked (also when cancelled partway) and why each skipped pair was.
pub fn run(
    plan: &DedupPlan,
    report: &dyn Fn(keel_vfs::Progress),
    cancel: &AtomicBool,
) -> (Vec<Linked>, Vec<String>) {
    let mut p = keel_vfs::Progress {
        total_items: plan.pairs.len(),
        total_bytes: plan.bytes(),
        ..keel_vfs::Progress::default()
    };
    let (mut linked, mut failed) = (Vec::new(), Vec::new());
    for pair in &plan.pairs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        p.current = pair.dup.display().to_string();
        report(p.clone());
        match link(pair, cancel) {
            Ok(l) => linked.push(l),
            Err(e) => {
                p.skipped += 1;
                failed.push(format!("{e:#}"));
            }
        }
        p.done_items += 1;
        p.done_bytes += pair.size;
    }
    report(p);
    (linked, failed)
}

/// The preview dialog's state.
#[derive(Debug)]
pub struct Preview {
    pub plan: DedupPlan,
}

/// The preview (inside `dialogs::show`'s modal): the summary, the replacements (the first
/// 200), the copies left alone, and the button.
pub fn ui(ui: &mut egui::Ui, d: &mut Preview, cancel: &mut bool) -> Option<Action> {
    let mut out = None;
    ui.strong("Replace duplicates with hard links");
    if d.plan.pairs.is_empty() {
        ui.label(if d.plan.skipped.is_empty() {
            "No duplicates to replace.".to_owned()
        } else {
            format!("Nothing to replace: {}", d.plan.summary())
        });
    } else {
        ui.label(d.plan.summary());
        ui.weak(
            "Each pair is compared byte by byte right before it is linked. Linked files \
             share their bytes: a change to one shows in all of them. Undo gives each its \
             own copy again.",
        );
    }
    egui::ScrollArea::vertical()
        .max_height(220.0)
        .show(ui, |ui| {
            for pair in d.plan.pairs.iter().take(200) {
                ui.add(egui::Label::new(format!("{}", pair.dup.display())).truncate());
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("  → {}", pair.keep.display())).weak(),
                    )
                    .truncate(),
                );
            }
            if d.plan.pairs.len() > 200 {
                ui.weak(format!("and {} more", d.plan.pairs.len() - 200));
            }
            for (path, why) in d.plan.skipped.iter().take(50) {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{} ({})", path.display(), why.text())).weak(),
                    )
                    .truncate(),
                );
            }
        });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let n = d.plan.pairs.len();
        if n > 0 {
            let label = match n {
                1 => "Replace 1 file".to_owned(),
                n => format!("Replace {n} files"),
            };
            let b = ui.button(label);
            crate::a11y::first_focus(&b);
            if b.clicked() {
                out = Some(Action::System(SysCmd::DedupRun(Box::new(d.plan.clone()))));
            }
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::DupRecord;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("keel-dedup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn group(size: u64, paths: &[&Path]) -> DupGroup {
        DupGroup {
            size,
            records: paths
                .iter()
                .map(|p| DupRecord {
                    path: VPath::local(p),
                    source: "src".into(),
                    offline: false,
                })
                .collect(),
        }
    }

    #[test]
    fn plans_same_volume_copies_and_skips_the_rest() {
        let d = tmp("plan");
        let (a, b, c) = (d.join("a.bin"), d.join("b.bin"), d.join("c.bin"));
        for p in [&a, &b] {
            fs::write(p, b"same bytes").unwrap();
        }
        fs::hard_link(&a, &c).unwrap(); // already one file with a
        let mut g = group(10, &[&a, &b, &c]);
        g.records.push(DupRecord {
            path: VPath::parse("sftp://host/x/a.bin").unwrap(),
            source: "host".into(),
            offline: false,
        });
        g.records.push(DupRecord {
            path: VPath::local(d.join("gone.bin")),
            source: "src".into(),
            offline: false,
        });
        g.records.push(DupRecord {
            path: VPath::local(d.join("off.bin")),
            source: "usb".into(),
            offline: true,
        });
        let plan = plan(&[g]);
        assert_eq!(
            plan.pairs,
            [Pair {
                keep: a.clone(),
                dup: b.clone(),
                size: 10
            }]
        );
        let why: Vec<Skip> = plan.skipped.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            why,
            [
                Skip::NotLocal,
                Skip::Changed,
                Skip::Offline,
                Skip::AlreadyLinked
            ]
        );
        assert_eq!(plan.bytes(), 10);
        let s = plan.summary();
        assert!(s.starts_with("1 duplicate (10 B) become hard links"), "{s}");
        assert!(s.contains("4 left alone"), "{s}");
        assert!(same_volume(&a, &b));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn byte_compare_finds_any_difference() {
        let d = tmp("cmp");
        let cancel = AtomicBool::new(false);
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(d.join("x"), &big).unwrap();
        fs::write(d.join("y"), &big).unwrap();
        assert!(same_content(&d.join("x"), &d.join("y"), &cancel).unwrap());
        let mut last = big.clone();
        *last.last_mut().unwrap() ^= 1; // past the first 1 MiB block
        fs::write(d.join("z"), &last).unwrap();
        assert!(!same_content(&d.join("x"), &d.join("z"), &cancel).unwrap());
        fs::write(d.join("short"), &big[..10]).unwrap();
        assert!(!same_content(&d.join("x"), &d.join("short"), &cancel).unwrap());
        fs::write(d.join("e1"), b"").unwrap();
        fs::write(d.join("e2"), b"").unwrap();
        assert!(same_content(&d.join("e1"), &d.join("e2"), &cancel).unwrap());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn links_after_a_byte_compare_and_undo_copies_back() {
        let d = tmp("link");
        let (keep, dup) = (d.join("keep.txt"), d.join("dup.txt"));
        fs::write(&keep, b"identical content").unwrap();
        fs::write(&dup, b"identical content").unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(&dup)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let pair = Pair {
            keep: keep.clone(),
            dup: dup.clone(),
            size: 17,
        };
        let cancel = AtomicBool::new(false);
        let linked = link(&pair, &cancel).unwrap();
        assert_eq!(linked.path, dup);
        assert_eq!(linked.mtime, Some(old));
        assert!(same_file(&keep, &dup), "one file now");
        assert_eq!(fs::read(&dup).unwrap(), b"identical content");
        // No temporary names left behind.
        let names: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        // A second time: already linked.
        assert!(link(&pair, &cancel)
            .unwrap_err()
            .to_string()
            .contains("already"));

        unlink(&dup, linked.mtime).unwrap();
        assert!(!same_file(&keep, &dup), "its own copy again");
        assert_eq!(fs::read(&dup).unwrap(), b"identical content");
        assert_eq!(fs::metadata(&dup).unwrap().modified().unwrap(), old);
        // Writing one no longer changes the other.
        fs::write(&dup, b"changed").unwrap();
        assert_eq!(fs::read(&keep).unwrap(), b"identical content");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn undo_journal_entry_checks_then_copies_back() {
        use crate::undo::{run as undo, stat_fn, Entry, Item, Op};
        let d = tmp("undo");
        let (keep, dup) = (d.join("keep.bin"), d.join("dup.bin"));
        fs::write(&keep, [7u8; 4096]).unwrap();
        fs::write(&dup, [7u8; 4096]).unwrap();
        let cancel = AtomicBool::new(false);
        let pair = Pair {
            keep: keep.clone(),
            dup: dup.clone(),
            size: 4096,
        };
        let linked = link(&pair, &cancel).unwrap();
        let router = keel_vfs::Router::new();
        let now = VPath::local(&dup);
        let entry = Entry {
            id: 1,
            op: Op::Hardlink(vec![(now.clone(), linked.mtime)]),
            items: vec![Item {
                now: now.clone(),
                was: None,
                stamp: stat_fn(&router)(&now),
            }],
        };
        assert!(entry.items[0].stamp.is_some());
        assert_eq!(entry.done_text(), "Replaced 1 item with hard links");
        assert!(entry.preview().starts_with("Undo hard links"));
        assert!(entry.check(stat_fn(&router)).is_ok());
        undo(&entry, &router, &|_| {}, &cancel).unwrap();
        assert!(!same_file(&keep, &dup));
        assert_eq!(fs::read(&dup).unwrap(), [7u8; 4096]);
        // Changed since (its own copy now): a second undo is refused.
        assert!(entry.check(stat_fn(&router)).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn refuses_different_bytes_and_read_only_and_changed_sizes() {
        let d = tmp("refuse");
        let cancel = AtomicBool::new(false);
        let (keep, dup) = (d.join("k"), d.join("d"));
        fs::write(&keep, b"aaaa").unwrap();
        fs::write(&dup, b"aaab").unwrap();
        let pair = Pair {
            keep: keep.clone(),
            dup: dup.clone(),
            size: 4,
        };
        let e = link(&pair, &cancel).unwrap_err().to_string();
        assert!(e.contains("not the same bytes"), "{e}");
        assert_eq!(fs::read(&dup).unwrap(), b"aaab", "left alone");
        // A size other than the plan's.
        fs::write(&dup, b"aaaaa").unwrap();
        let e = link(&pair, &cancel).unwrap_err().to_string();
        assert!(e.contains("changed since the preview"), "{e}");
        // Read-only.
        fs::write(&dup, b"aaaa").unwrap();
        let mut perm = fs::metadata(&dup).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&dup, perm.clone()).unwrap();
        let e = link(&pair, &cancel).unwrap_err().to_string();
        assert!(e.contains("read-only"), "{e}");
        perm.set_readonly(false);
        fs::set_permissions(&dup, perm).unwrap();
        // The job skips and counts failures, links the rest.
        let other = d.join("o");
        fs::write(&other, b"aaaa").unwrap();
        let plan = DedupPlan {
            pairs: vec![
                Pair {
                    keep: keep.clone(),
                    dup: other.clone(),
                    size: 4,
                },
                Pair {
                    keep: keep.clone(),
                    dup: d.join("missing"),
                    size: 4,
                },
            ],
            skipped: Vec::new(),
        };
        let (linked, failed) = run(&plan, &|_| {}, &cancel);
        assert_eq!(linked.len(), 1);
        assert_eq!(failed.len(), 1);
        assert!(same_file(&keep, &other));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copies_on_other_volumes_are_left_alone() {
        let d = tmp("vol");
        for n in ["a", "b", "c", "d"] {
            fs::write(d.join(n), b"x").unwrap();
        }
        // a on one volume, b c d on another: b keeps, c and d link to it.
        let paths: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(|n| d.join(n)).collect();
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let a = paths[0].clone();
        let plan = plan_by(&[group(1, &refs)], |p| {
            Some(if p == a { "vol1" } else { "vol2" }.to_owned())
        });
        let dups: Vec<&PathBuf> = plan.pairs.iter().map(|p| &p.dup).collect();
        assert_eq!(dups, [&paths[2], &paths[3]]);
        assert!(plan.pairs.iter().all(|p| p.keep == paths[1]));
        assert_eq!(plan.skipped, [(VPath::local(&paths[0]), Skip::OtherVolume)]);
        // A volume that cannot be told: never linked.
        let plan = plan_by(&[group(1, &refs)], |_| None);
        assert!(plan.pairs.is_empty());
        assert_eq!(plan.skipped.len(), 4);
        let _ = fs::remove_dir_all(&d);
    }

    fn pair_in(d: &Path, bytes: &[u8]) -> Pair {
        let (keep, dup) = (d.join("keep.bin"), d.join("dup.bin"));
        fs::write(&keep, bytes).unwrap();
        fs::write(&dup, bytes).unwrap();
        Pair {
            keep,
            dup,
            size: bytes.len() as u64,
        }
    }

    /// No `.keep.bin.keel-link` and the like left next to them.
    fn no_temporaries(d: &Path) {
        for e in fs::read_dir(d).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".keel-"), "{name}");
        }
    }

    #[test]
    fn keep_written_between_compare_and_link_is_never_linked() {
        use std::cell::Cell;
        let d = tmp("race-write");
        let pair = pair_in(&d, b"version one");
        let wrote = Cell::new(false);
        let r = link_with(&pair, &AtomicBool::new(false), &|| {
            // An app appending to the kept copy during a long compare.
            let w = File::options()
                .append(true)
                .open(&pair.keep)
                .and_then(|mut f| std::io::Write::write_all(&mut f, b" and more"));
            wrote.set(w.is_ok());
        });
        if wrote.get() {
            let e = r.unwrap_err().to_string();
            assert!(e.contains("changed while it was compared"), "{e}");
            assert!(!same_file(&pair.keep, &pair.dup));
        } else {
            // Windows: the open handle refuses writers until the duplicate is replaced.
            if !cfg!(windows) {
                panic!("only Windows refuses the writer");
            }
            r.unwrap();
        }
        assert_eq!(fs::read(&pair.dup).unwrap(), b"version one");
        no_temporaries(&d);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn keep_or_dup_replaced_between_compare_and_link_is_never_linked() {
        use std::cell::Cell;
        for which in ["keep", "dup"] {
            let d = tmp(&format!("race-swap-{which}"));
            let pair = pair_in(&d, b"version one");
            let target = if which == "keep" {
                &pair.keep
            } else {
                &pair.dup
            };
            let swapped = Cell::new(false);
            let r = link_with(&pair, &AtomicBool::new(false), &|| {
                // An editor saving: write a new file, rename it over the old one.
                let new = d.join("saved.tmp");
                fs::write(&new, b"version two").unwrap();
                swapped.set(fs::rename(&new, target).is_ok());
                let _ = fs::remove_file(&new);
            });
            if swapped.get() {
                let e = r.unwrap_err().to_string();
                assert!(e.contains("changed while it was compared"), "{which}: {e}");
                assert!(!same_file(&pair.keep, &pair.dup), "{which}");
                assert_eq!(fs::read(target).unwrap(), b"version two", "{which}");
                let other = if which == "keep" {
                    &pair.dup
                } else {
                    &pair.keep
                };
                assert_eq!(fs::read(other).unwrap(), b"version one", "{which}");
            } else {
                r.unwrap();
            }
            no_temporaries(&d);
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn a_read_only_keep_is_refused() {
        let d = tmp("ro-keep");
        let pair = pair_in(&d, b"same");
        set_readonly(&pair.keep, true).unwrap();
        let e = link(&pair, &AtomicBool::new(false))
            .unwrap_err()
            .to_string();
        assert!(e.contains("read-only") && e.contains("keep.bin"), "{e}");
        assert!(!same_file(&pair.keep, &pair.dup));
        set_readonly(&pair.keep, false).unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn undo_of_a_link_made_read_only_since_still_works() {
        let d = tmp("ro-undo");
        let pair = pair_in(&d, b"same bytes");
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
        File::options()
            .write(true)
            .open(&pair.dup)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let linked = link(&pair, &AtomicBool::new(false)).unwrap();
        set_readonly(&pair.dup, true).unwrap(); // both names: one file
        unlink(&pair.dup, linked.mtime).unwrap();
        assert!(!same_file(&pair.keep, &pair.dup));
        assert_eq!(fs::metadata(&pair.dup).unwrap().modified().unwrap(), old);
        assert!(fs::metadata(&pair.dup).unwrap().permissions().readonly());
        // The kept copy was not touched: still read-only.
        assert!(fs::metadata(&pair.keep).unwrap().permissions().readonly());
        no_temporaries(&d);
        for p in [&pair.keep, &pair.dup] {
            set_readonly(p, false).unwrap();
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_duplicate_with_its_own_permissions_or_streams_is_left_alone() {
        let d = tmp("own");
        let pair = pair_in(&d, b"same bytes");
        #[cfg(windows)]
        fs::write(format!("{}:meta", pair.dup.display()), b"app data").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&pair.dup, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(&pair.keep, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let want = if cfg!(windows) {
            Skip::Streams
        } else {
            Skip::Permissions
        };
        assert_eq!(nothing_of_its_own(&pair.keep, &pair.dup), Err(want));
        let plan = plan(&[group(10, &[&pair.keep, &pair.dup])]);
        assert!(plan.pairs.is_empty());
        assert_eq!(plan.skipped, [(VPath::local(&pair.dup), want)]);
        let e = link(&pair, &AtomicBool::new(false))
            .unwrap_err()
            .to_string();
        assert!(e.contains(want.text()), "{e}");
        assert!(!same_file(&pair.keep, &pair.dup));
        // Twins with nothing of their own link.
        let d2 = tmp("own-twins");
        let twins = pair_in(&d2, b"same bytes");
        assert_eq!(nothing_of_its_own(&twins.keep, &twins.dup), Ok(()));
        link(&twins, &AtomicBool::new(false)).unwrap();
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }
}
