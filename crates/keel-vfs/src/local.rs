use crate::{Caps, Entry, Kind, Provider, VPath};
use anyhow::{Context, Result};
use notify::Watcher;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalProvider;

/// Absolute extended-length (`\\?\`, `\\?\UNC\`) path that keeps trailing dots and spaces.
/// `/` is normalized to `\` first.
#[cfg(windows)]
pub fn long(p: &Path) -> Result<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let p = PathBuf::from(p.as_os_str().to_string_lossy().replace('/', "\\"));
    let absolute = if p.is_absolute() {
        p
    } else {
        // Joining manually: GetFullPathName-style normalization would trim trailing dots/spaces.
        anyhow::ensure!(
            !p.has_root()
                && !matches!(p.components().next(), Some(std::path::Component::Prefix(_))),
            "ambiguous drive-relative path: {}",
            p.display()
        );
        std::env::current_dir()?.join(p)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let units: Vec<_> = normalized.as_os_str().encode_wide().collect();
    let prefix: Vec<_> = r"\\?\".encode_utf16().collect();
    if units.starts_with(&prefix) {
        return Ok(normalized);
    }
    let extended = if units.starts_with(&[92, 92]) {
        r"\\?\UNC\"
            .encode_utf16()
            .chain(units[2..].iter().copied())
            .collect::<Vec<_>>()
    } else {
        prefix.into_iter().chain(units).collect::<Vec<_>>()
    };
    Ok(std::ffi::OsString::from_wide(&extended).into())
}

#[cfg(not(windows))]
pub fn long(p: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(p)?)
}

#[cfg(windows)]
fn is_hidden(_name: &str, metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn is_hidden(name: &str, _metadata: &fs::Metadata) -> bool {
    name.starts_with('.')
}

fn local(p: &VPath) -> Result<PathBuf> {
    p.to_local_path()
        .ok_or_else(|| anyhow::anyhow!("not a local path: {}", p.display()))
        .and_then(|p| long(&p))
}

/// `metadata` is the entry's own (not followed); links are described by their target.
fn entry(path: VPath, local: &Path, metadata: fs::Metadata) -> Entry {
    let name = path.name().to_owned();
    let hidden = is_hidden(&name, &metadata);
    let is_link = metadata.is_symlink();
    let (kind, metadata) = match is_link.then(|| fs::metadata(local)) {
        Some(Ok(target)) => (
            if target.is_dir() {
                Kind::Dir
            } else {
                Kind::File
            },
            target,
        ),
        Some(Err(_)) => (Kind::Symlink, metadata),
        None if metadata.is_dir() => (Kind::Dir, metadata),
        None => (Kind::File, metadata),
    };
    let ext = if kind == Kind::Dir {
        String::new()
    } else {
        Path::new(&name)
            .extension()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    Entry {
        hidden,
        is_link,
        encrypted: false,
        path,
        name,
        kind,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        ext,
    }
}

const TRASH_FAILED: &str = "Could not move to trash; nothing deleted";

/// Sends `path` to the OS trash. Never deletes permanently.
fn trash_path(path: &Path) -> Result<()> {
    // The Shell resolves non-`\\?\` names, which drops trailing dots/spaces and could hit a
    // different file. Refuse rather than risk trashing the wrong thing.
    #[cfg(windows)]
    anyhow::ensure!(
        !path.components().any(|c| matches!(c,
            std::path::Component::Normal(n) if n.to_string_lossy().ends_with(['.', ' ']))),
        "name ends with a dot or space"
    );
    #[cfg(windows)]
    anyhow::ensure!(
        is_fixed_disk(path),
        "This location has no Recycle Bin; nothing deleted"
    );
    trash::delete(path)?;
    anyhow::ensure!(!path.try_exists()?, "still present after trash");
    Ok(())
}

/// True only for fixed local drives. The Shell silently deletes *permanently* on UNC shares,
/// mapped network drives, removable media and optical/RAM disks (no Recycle Bin there), so
/// `trash_path` refuses those instead of trusting `trash::delete`. Also tells a deleted
/// folder from an offline share. May block on a dead mapped drive; call off the UI thread.
#[cfg(windows)]
pub fn is_fixed_disk(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
    let letter = match path.components().next() {
        Some(Component::Prefix(pre)) => match pre.kind() {
            Prefix::Disk(l) | Prefix::VerbatimDisk(l) => l,
            _ => return false, // UNC, VerbatimUNC, DeviceNS, Verbatim
        },
        _ => return false,
    };
    let root: Vec<u16> = std::ffi::OsStr::new(&format!("{}:\\", letter as char))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `root` is a valid NUL-terminated UTF-16 string that outlives the call.
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_FIXED }
}

/// Outside Windows: false under the usual mount roots for network shares and removable
/// media (an unmounted share there must not look deleted).
#[cfg(not(windows))]
pub fn is_fixed_disk(path: &Path) -> bool {
    !["/Volumes", "/media", "/run/media", "/mnt", "/net"]
        .iter()
        .any(|root| path.starts_with(root))
}

impl Provider for LocalProvider {
    fn scheme(&self) -> &'static str {
        "file"
    }
    fn caps(&self) -> Caps {
        Caps {
            write: true,
            rename: true,
            delete: true,
            watch: true,
        }
    }
    /// Sorted: folders first, then case-insensitive natural name order.
    /// May block on dead network volumes; call off the UI thread.
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        (|| -> Result<_> {
            // DirEntry::metadata uses the metadata already returned by FindNextFileW on Windows.
            let entries = fs::read_dir(local(dir)?)?
                .map(|item| {
                    let item = item?;
                    let metadata = item.metadata()?;
                    Ok(entry(
                        dir.join(&item.file_name().to_string_lossy()),
                        &item.path(),
                        metadata,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            // Cache case folding once per entry, not once per comparison.
            let mut keyed: Vec<_> = entries
                .into_iter()
                .map(|e| (e.name.to_lowercase(), e))
                .collect();
            keyed.sort_unstable_by(|(ak, a), (bk, b)| {
                (a.kind != Kind::Dir)
                    .cmp(&(b.kind != Kind::Dir))
                    .then(a.hidden.cmp(&b.hidden))
                    .then_with(|| natord::compare(ak, bk))
                    .then_with(|| a.name.cmp(&b.name))
            });
            Ok(keyed.into_iter().map(|(_, e)| e).collect())
        })()
        .with_context(|| format!("list {}", dir.display()))
    }
    /// Never cached or capped.
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        let path = local(p)?;
        fs::symlink_metadata(&path)
            .map(|m| entry(p.clone(), &path, m))
            .with_context(|| format!("stat {}", p.display()))
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(
            fs::File::open(local(p)?).with_context(|| format!("read {}", p.display()))?,
        ))
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(
            fs::File::create(local(p)?).with_context(|| format!("write {}", p.display()))?,
        ))
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(local(p)?)
                .with_context(|| format!("create {}", p.display()))?,
        ))
    }
    fn write_at(&self, p: &VPath, offset: u64) -> Result<Option<Box<dyn Write + Send>>> {
        let file = open_at(&local(p)?, offset).with_context(|| format!("write {}", p.display()))?;
        Ok(Some(Box::new(Durable(file))))
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        fs::create_dir(local(p)?).with_context(|| format!("mkdir {}", p.display()))
    }
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        // Never replaces an existing target, so a rename cannot destroy another file.
        crate::sys::rename_noreplace(&local(from)?, &local(to)?)
            .with_context(|| format!("rename {} to {}", from.display(), to.display()))
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        trash_path(&local(p)?).with_context(|| format!("{TRASH_FAILED}: {}", p.display()))
    }
    fn remove_kind(&self) -> crate::provider::RemoveKind {
        crate::provider::RemoveKind::Trash
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename(from, to)
    }
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        std::fs::rename(local(from)?, local(to)?).with_context(|| format!("place {}", to.display()))
    }
    fn canonicalize(&self, p: &VPath) -> Result<VPath> {
        Ok(VPath::local(std::fs::canonicalize(local(p)?)?))
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        std::fs::remove_dir(local(p)?).with_context(|| p.display())
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        p.to_local_path()
            .ok_or_else(|| anyhow::anyhow!("not a local path: {}", p.display()))
    }
}

/// `path` opened for writing at `offset`: created (or emptied) at 0, else cut to `offset`
/// bytes, which it must have.
pub(crate) fn open_at(path: &Path, offset: u64) -> Result<fs::File> {
    use std::io::{Seek, SeekFrom};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(offset == 0)
        .open(path)?;
    let len = file.metadata()?.len();
    anyhow::ensure!(len >= offset, "only {len} of {offset} bytes are there");
    file.set_len(offset)?;
    file.seek(SeekFrom::Start(offset))?;
    Ok(file)
}

/// A file whose `flush()` syncs its data to disk (`Provider::write_at`).
struct Durable(fs::File);
impl Write for Durable {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.sync_data()
    }
}

/// A trailing-edge 100 ms debounce. Dropping the watcher disconnects and stops the worker.
pub fn watch(dir: &Path, tx: crossbeam_channel::Sender<()>) -> Result<notify::RecommendedWatcher> {
    let (events, rx) = crossbeam_channel::unbounded();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if !matches!(event, Ok(ref e) if matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = events.send(());
        }
    })?;
    watcher
        .watch(&long(dir)?, notify::RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {}", dir.display()))?;
    std::thread::Builder::new()
        .name("keel-watch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(()) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                    }
                }
                // Never block watcher shutdown on a full UI queue. One queued refresh is sufficient.
                if matches!(
                    tx.try_send(()),
                    Err(crossbeam_channel::TrySendError::Disconnected(_))
                ) {
                    break;
                }
            }
        })?;
    Ok(watcher)
}

/// Mounted volumes as `(name, label, free bytes, total bytes)`. `name` is `"C:"` on Windows
/// and the mount point elsewhere, where pseudo, system and read-only image mounts are left
/// out (see `user_mount`).
/// May block on dead network volumes; call off the UI thread.
pub fn drives() -> Vec<(String, String, u64, u64)> {
    sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| {
            cfg!(windows)
                || user_mount(
                    &d.mount_point().to_string_lossy(),
                    &d.file_system().to_string_lossy(),
                    d.is_read_only(),
                )
        })
        .map(|d| {
            let mount = d.mount_point().to_string_lossy();
            let name = if cfg!(windows) {
                mount.trim_end_matches('\\').to_owned()
            } else {
                mount.into_owned()
            };
            let label = d.name().to_string_lossy().into_owned();
            (name, label, d.available_space(), d.total_space())
        })
        .collect()
}

/// macOS/Linux: a mount a user browses (the root, home, data and removable volumes), not
/// a pseudo filesystem, a system/snap/boot mount or a read-only image (dmg, squashfs).
pub fn user_mount(mount: &str, fs: &str, read_only: bool) -> bool {
    const PSEUDO: &[&str] = &[
        "proc",
        "sysfs",
        "devtmpfs",
        "devpts",
        "tmpfs",
        "cgroup",
        "cgroup2",
        "overlay",
        "squashfs",
        "autofs",
        "efivarfs",
        "securityfs",
        "debugfs",
        "tracefs",
        "fusectl",
        "configfs",
        "pstore",
        "bpf",
        "mqueue",
        "hugetlbfs",
        "ramfs",
        "nsfs",
        "devfs",
        "binfmt_misc",
        "fuse.portal",
        "fuse.gvfsd-fuse",
        "nullfs",
    ];
    const SYSTEM: &[&str] = &[
        "/boot",
        "/dev",
        "/proc",
        "/sys",
        "/run",
        "/snap",
        "/var/lib",
        "/System/Volumes",
        "/private/var/vm",
    ];
    let under = |root: &str| mount == root || mount.starts_with(&format!("{root}/"));
    if mount == "/" {
        return true;
    }
    if PSEUDO.contains(&fs) || (read_only && mount != "/") {
        return false;
    }
    // Removable media under /run/media stays.
    under("/run/media") || !SYSTEM.iter().any(|root| under(root))
}

#[cfg(test)]
mod mount_tests {
    use super::user_mount;

    #[test]
    fn pseudo_system_and_read_only_mounts_are_hidden() {
        for (mount, fs, ro) in [
            ("/", "ext4", false),
            ("/", "apfs", true), // the sealed macOS system volume
            ("/home", "ext4", false),
            ("/media/usb", "vfat", false),
            ("/run/media/me/USB", "exfat", false),
            ("/Volumes/Backup", "apfs", false),
            ("/mnt/data", "ntfs3", false),
        ] {
            assert!(user_mount(mount, fs, ro), "{mount} shown");
        }
        for (mount, fs, ro) in [
            ("/proc", "proc", false),
            ("/dev/shm", "tmpfs", false),
            ("/snap/core/1", "squashfs", true),
            ("/boot/efi", "vfat", false),
            ("/System/Volumes/VM", "apfs", false),
            ("/Volumes/Installer", "hfs", true),
            ("/run/user/1000/doc", "fuse.portal", false),
        ] {
            assert!(!user_mount(mount, fs, ro), "{mount} hidden");
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::long;
    use std::path::{Path, PathBuf};

    #[test]
    fn long_normalizes_forward_slashes() {
        assert_eq!(
            long(Path::new("C:/a/b")).unwrap(),
            PathBuf::from(r"\\?\C:\a\b")
        );
        assert_eq!(
            long(Path::new("//server/share/x")).unwrap(),
            PathBuf::from(r"\\?\UNC\server\share\x")
        );
        assert_eq!(
            long(Path::new(r"\\?\C:\a\b.")).unwrap(),
            PathBuf::from(r"\\?\C:\a\b.")
        );
    }
}

#[cfg(all(test, windows))]
mod recycle_bin_tests {
    use super::trash_path;

    #[test]
    fn unc_path_is_refused_and_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("victim.txt");
        std::fs::write(&file, b"keep me").unwrap();
        // Admin-share spelling of the same temp file: \\localhost\C$\Users\...
        let local = file.to_string_lossy().replace(r"\\?\", "");
        let Some(rest) = local.get(2..) else { return };
        let unc = format!(r"\\localhost\{}${}", &local[..1], rest);
        if std::fs::metadata(&unc).is_err() {
            eprintln!("skipping: admin share {unc} not reachable");
            return;
        }
        let err = trash_path(std::path::Path::new(&unc)).unwrap_err();
        assert!(err.to_string().contains("no Recycle Bin"), "{err}");
        assert!(file.exists(), "file must survive a refused delete");
    }
}
