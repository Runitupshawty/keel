//! File attributes (hidden, read-only, Unix mode bits) and links (symbolic links, Windows
//! junctions, hard links) of local paths: what the attribute and link plans change.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

/// Attributes to set (None: left as it is), or as read (None: not a thing on this system).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Attributes {
    /// Windows: the hidden attribute; macOS: the hidden flag (`chflags hidden`). On Linux a
    /// name starting with a dot is hidden: read, never set.
    pub hidden: Option<bool>,
    /// Windows: the read-only attribute; Unix: no write bit (setting it clears every write
    /// bit, clearing it gives the owner write access).
    pub readonly: Option<bool>,
    /// Unix permission bits for owner, group and other (`0o755`); Windows has none.
    pub mode: Option<u32>,
}

impl Attributes {
    /// Nothing to change.
    pub fn is_empty(&self) -> bool {
        self.hidden.is_none() && self.readonly.is_none() && self.mode.is_none()
    }

    /// Refuses what this system cannot set: a mode on Windows, hidden on Linux, a mode
    /// outside `0o777`.
    pub fn check(&self) -> Result<()> {
        anyhow::ensure!(!self.is_empty(), "no attribute to set");
        if let Some(mode) = self.mode {
            anyhow::ensure!(
                !cfg!(windows),
                "Windows files have no Unix mode bits: set hidden or read-only"
            );
            anyhow::ensure!(mode <= 0o777, "mode {mode:o} is not owner/group/other bits");
        }
        anyhow::ensure!(
            self.hidden.is_none() || cfg!(any(windows, target_os = "macos")),
            "on Linux a hidden file is one whose name starts with a dot: rename it instead"
        );
        Ok(())
    }
}

/// The attributes of `path` itself (a link, not its target).
pub fn attributes(path: &Path) -> Result<Attributes> {
    let m = fs::symlink_metadata(path).with_context(|| path.display().to_string())?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let a = m.file_attributes();
        Ok(Attributes {
            hidden: Some(a & HIDDEN != 0),
            readonly: Some(a & READONLY != 0),
            mode: None,
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = m.mode() & 0o777;
        #[cfg(target_os = "macos")]
        let hidden = {
            use std::os::macos::fs::MetadataExt as _;
            Some(m.st_flags() & libc::UF_HIDDEN != 0)
        };
        #[cfg(not(target_os = "macos"))]
        let hidden = None;
        Ok(Attributes {
            hidden,
            readonly: Some(mode & 0o222 == 0),
            mode: Some(mode),
        })
    }
}

#[cfg(windows)]
const READONLY: u32 = 0x1;
#[cfg(windows)]
const HIDDEN: u32 = 0x2;

/// Sets the attributes that are Some on `path` itself (never following a link).
pub fn set_attributes(path: &Path, set: &Attributes) -> Result<()> {
    set.check()?;
    let shown = || path.display().to_string();
    let m = fs::symlink_metadata(path).with_context(shown)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::{core::PCWSTR, Win32::Storage::FileSystem};
        // The bits SetFileAttributesW takes; anything else (folder, reparse point) is not ours.
        const SETTABLE: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x1000 | 0x2000;
        let mut a = m.file_attributes() & SETTABLE;
        for (on, bit) in [(set.hidden, HIDDEN), (set.readonly, READONLY)] {
            match on {
                Some(true) => a |= bit,
                Some(false) => a &= !bit,
                None => {}
            }
        }
        let a = if a == 0 { 0x80 } else { a }; // FILE_ATTRIBUTE_NORMAL alone
        let wide = wide(&crate::long(path)?)?;
        // SAFETY: a NUL-terminated wide string that outlives the call.
        unsafe {
            FileSystem::SetFileAttributesW(
                PCWSTR(wide.as_ptr()),
                FileSystem::FILE_FLAGS_AND_ATTRIBUTES(a),
            )
        }
        .map_err(|e| anyhow::anyhow!("{}", e.message()))
        .with_context(shown)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            !m.file_type().is_symlink() || (set.mode.is_none() && set.readonly.is_none()),
            "a link has no permissions of its own: {}",
            path.display()
        );
        #[cfg(target_os = "macos")]
        if let Some(hidden) = set.hidden {
            use std::os::macos::fs::MetadataExt as _;
            use std::os::unix::ffi::OsStrExt;
            let flags = match hidden {
                true => m.st_flags() | libc::UF_HIDDEN,
                false => m.st_flags() & !libc::UF_HIDDEN,
            };
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            // SAFETY: a NUL-terminated path that outlives the call.
            if unsafe { libc::chflags(c.as_ptr(), flags as _) } != 0 {
                return Err(io::Error::last_os_error()).with_context(shown);
            }
        }
        let mut mode = set.mode.unwrap_or(m.permissions().mode() & 0o777);
        match set.readonly {
            Some(true) => mode &= !0o222,
            Some(false) if mode & 0o222 == 0 => mode |= 0o200,
            _ => {}
        }
        if set.mode.is_some() || set.readonly.is_some() {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).with_context(shown)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn wide(p: &Path) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut units: Vec<u16> = p.as_os_str().encode_wide().collect();
    anyhow::ensure!(!units.contains(&0), "path contains NUL");
    units.push(0);
    Ok(units)
}

/// What a link plan makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// A symbolic link to a file or folder (on Windows: needs Developer Mode or elevation).
    Symlink,
    /// Windows: a junction to a folder on this computer (no privilege needed).
    Junction,
    /// A second name for a file on the same volume.
    Hardlink,
}

/// Why a symbolic link cannot be made on Windows without the privilege.
pub const SYMLINK_PRIVILEGE: &str = "Windows makes symbolic links only with Developer Mode on (Settings → System → For developers) or for an elevated program, and Keel never elevates: make a junction (folders) or a hard link (files) instead, which need neither";

/// Whether `a` and `b` are on one volume (a hard link needs it); None when either cannot be
/// read. On Windows by drive or share of the resolved paths (a volume mounted in a folder
/// counts as its drive's).
pub fn same_volume(a: &Path, b: &Path) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(fs::metadata(a).ok()?.dev() == fs::metadata(b).ok()?.dev())
    }
    #[cfg(not(unix))]
    {
        let root = |p: &Path| -> Option<String> {
            let real = fs::canonicalize(p).ok()?;
            let first = real.components().next()?;
            Some(first.as_os_str().to_string_lossy().to_lowercase())
        };
        Some(root(a)? == root(b)?)
    }
}

/// Whether this process can make symbolic links (always on Unix; on Windows with Developer
/// Mode or elevation), probed with a link in Keel's own cache folder.
pub fn symlinks_allowed() -> bool {
    #[cfg(windows)]
    {
        let dir = crate::cache_dir();
        let probe = dir.join(format!(".keel-symlink-probe-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let _ = fs::remove_file(&probe);
        let made = std::os::windows::fs::symlink_file(dir.join("none"), &probe);
        let _ = fs::remove_file(&probe);
        made.is_ok()
    }
    #[cfg(not(windows))]
    true
}

/// Sets the modified time of the folder `dir` itself: a link there is never followed (on
/// Windows its own time is set; on Unix it is refused).
pub fn set_folder_modified(dir: &Path, time: std::time::SystemTime) -> Result<()> {
    let mut open = fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES; FILE_FLAG_BACKUP_SEMANTICS (a folder) and
        // FILE_FLAG_OPEN_REPARSE_POINT (never what a junction points to).
        open.access_mode(0x0100)
            .custom_flags(0x0200_0000 | 0x0020_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    #[cfg(not(any(windows, unix)))]
    open.read(true);
    let f = open
        .open(dir)
        .with_context(|| format!("open {}", dir.display()))?;
    f.set_modified(time)
        .with_context(|| format!("set the date of {}", dir.display()))
}

/// Makes `link` (which must not exist) pointing at `target` (absolute).
pub fn make_link(target: &Path, link: &Path, kind: LinkKind) -> Result<()> {
    anyhow::ensure!(
        fs::symlink_metadata(link).is_err(),
        "{} already exists",
        link.display()
    );
    let meta = fs::metadata(target).with_context(|| target.display().to_string())?;
    let made = match kind {
        LinkKind::Hardlink => {
            anyhow::ensure!(
                meta.is_file(),
                "a hard link is made to a file, not a folder: {}",
                target.display()
            );
            fs::hard_link(target, link)
        }
        LinkKind::Junction => {
            anyhow::ensure!(
                meta.is_dir(),
                "a junction points to a folder: {}",
                target.display()
            );
            junction(target, link)
        }
        LinkKind::Symlink => symlink(target, link, meta.is_dir()),
    };
    made.map_err(|e| match kind {
        LinkKind::Symlink if cfg!(windows) && e.raw_os_error() == Some(1314) => {
            anyhow::anyhow!(SYMLINK_PRIVILEGE)
        }
        LinkKind::Hardlink if e.kind() == io::ErrorKind::CrossesDevices => anyhow::anyhow!(
            "a hard link must be on the same drive as its file: {}",
            target.display()
        ),
        _ => anyhow::Error::from(e),
    })
    .with_context(|| format!("link {} -> {}", link.display(), target.display()))
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path, dir: bool) -> io::Result<()> {
    match dir {
        true => std::os::windows::fs::symlink_dir(target, link),
        false => std::os::windows::fs::symlink_file(target, link),
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path, _dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(windows))]
fn junction(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "junctions are a Windows thing: make a symbolic link",
    ))
}

/// A mount-point reparse point (what `mklink /J` makes): an empty folder at `link`, then
/// the reparse data naming `target`. The folder goes again when that fails.
#[cfg(windows)]
fn junction(target: &Path, link: &Path) -> io::Result<()> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::{Ioctl::FSCTL_SET_REPARSE_POINT, IO::DeviceIoControl},
    };
    let plain = std::path::absolute(target)?.to_string_lossy().into_owned();
    let plain = plain.strip_prefix(r"\\?\").unwrap_or(&plain).to_owned();
    if plain.starts_with(r"\\") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a junction points to a folder on this computer, not on a share",
        ));
    }
    let buf = mount_point_data(&plain)?;
    fs::create_dir(link)?;
    let set = (|| {
        let file = fs::OpenOptions::new()
            .write(true)
            .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).0)
            .open(link)?;
        // SAFETY: `buf` outlives the call and its length is passed; no output buffer.
        unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle()),
                FSCTL_SET_REPARSE_POINT,
                Some(buf.as_ptr().cast()),
                buf.len() as u32,
                None,
                0,
                None,
                None,
            )
        }
        .map_err(|e| io::Error::other(e.message()))
    })();
    if set.is_err() {
        let _ = fs::remove_dir(link);
    }
    set
}

/// REPARSE_DATA_BUFFER for a mount point to the absolute path `target`: the substitute
/// name `\??\<target>` and the print name `<target>`, each NUL-terminated.
#[cfg(any(windows, test))]
fn mount_point_data(target: &str) -> io::Result<Vec<u8>> {
    let subst: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
    let print: Vec<u16> = target.encode_utf16().collect();
    let names = (subst.len() + print.len() + 2) * 2;
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "junction target too long");
    let len = u16::try_from(8 + names).map_err(|_| too_long())?;
    let mut buf = Vec::with_capacity(8 + len as usize);
    buf.extend(0xA000_0003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
    buf.extend(len.to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    for half in [0, subst.len() * 2, (subst.len() + 1) * 2, print.len() * 2] {
        buf.extend((half as u16).to_le_bytes());
    }
    for unit in subst.iter().chain(&[0]).chain(&print).chain(&[0]) {
        buf.extend(unit.to_le_bytes());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_volume_of_a_file_and_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "f").unwrap();
        assert_eq!(same_volume(&file, dir.path()), Some(true));
        assert_eq!(same_volume(&dir.path().join("missing"), dir.path()), None);
    }

    #[test]
    fn mount_point_data_has_both_names() {
        let buf = mount_point_data(r"C:\a").unwrap();
        let u16_at = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        assert_eq!(&buf[..4], &0xA000_0003u32.to_le_bytes());
        // \??\C:\a = 8 units, C:\a = 4 units, plus two NULs: 14 units after 8 header bytes.
        assert_eq!(u16_at(4) as usize, 8 + 14 * 2);
        assert_eq!((u16_at(8), u16_at(10)), (0, 16));
        assert_eq!((u16_at(12), u16_at(14)), (18, 8));
        assert_eq!(buf.len(), 16 + 14 * 2);
        let units: Vec<u16> = buf[16..].chunks(2).map(u16_at_slice).collect();
        assert_eq!(String::from_utf16(&units[..8]).unwrap(), r"\??\C:\a");
        assert_eq!(String::from_utf16(&units[9..13]).unwrap(), r"C:\a");
        assert!(mount_point_data(&"x".repeat(40_000)).is_err());
    }

    #[test]
    fn a_folders_own_date_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("f.txt"), "f").unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        set_folder_modified(&sub, t).unwrap();
        assert_eq!(fs::metadata(&sub).unwrap().modified().unwrap(), t);
        assert!(set_folder_modified(&dir.path().join("missing"), t).is_err());
    }

    fn u16_at_slice(c: &[u8]) -> u16 {
        u16::from_le_bytes([c[0], c[1]])
    }

    #[test]
    fn checks_refuse_what_this_system_cannot_set() {
        assert!(Attributes::default().check().is_err());
        let mode = Attributes {
            mode: Some(0o640),
            ..Attributes::default()
        };
        assert_eq!(mode.check().is_ok(), !cfg!(windows));
        let big = Attributes {
            mode: Some(0o4755),
            ..Attributes::default()
        };
        assert!(big.check().is_err());
        let hidden = Attributes {
            hidden: Some(true),
            ..Attributes::default()
        };
        assert_eq!(
            hidden.check().is_ok(),
            cfg!(any(windows, target_os = "macos"))
        );
    }

    #[test]
    fn read_only_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        fs::write(&f, b"a").unwrap();
        let before = attributes(&f).unwrap();
        assert_eq!(before.readonly, Some(false));
        let ro = Attributes {
            readonly: Some(true),
            ..Attributes::default()
        };
        set_attributes(&f, &ro).unwrap();
        assert_eq!(attributes(&f).unwrap().readonly, Some(true));
        assert!(fs::write(&f, b"b").is_err() || cfg!(unix) && is_root());
        let back = Attributes {
            mode: before.mode.filter(|_| cfg!(unix)),
            readonly: before.readonly,
            hidden: before
                .hidden
                .filter(|_| cfg!(any(windows, target_os = "macos"))),
        };
        set_attributes(&f, &back).unwrap();
        assert_eq!(attributes(&f).unwrap(), before);
    }

    #[cfg(unix)]
    fn is_root() -> bool {
        // SAFETY: no arguments, no state.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(windows)]
    fn is_root() -> bool {
        false
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn hidden_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("h.txt");
        fs::write(&f, b"h").unwrap();
        let on = Attributes {
            hidden: Some(true),
            ..Attributes::default()
        };
        set_attributes(&f, &on).unwrap();
        assert_eq!(attributes(&f).unwrap().hidden, Some(true));
        set_attributes(
            &f,
            &Attributes {
                hidden: Some(false),
                ..Attributes::default()
            },
        )
        .unwrap();
        assert_eq!(attributes(&f).unwrap().hidden, Some(false));
    }

    #[test]
    fn hard_links_share_the_file_and_need_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let (f, l) = (dir.path().join("f.txt"), dir.path().join("l.txt"));
        fs::write(&f, b"one").unwrap();
        make_link(&f, &l, LinkKind::Hardlink).unwrap();
        fs::write(&l, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        let err = make_link(dir.path(), &dir.path().join("d"), LinkKind::Hardlink).unwrap_err();
        assert!(format!("{err:#}").contains("not a folder"), "{err:#}");
        let err = make_link(&f, &l, LinkKind::Hardlink).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }

    #[cfg(windows)]
    #[test]
    fn junctions_point_at_folders_and_go_without_their_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir_all(target.join("sub")).unwrap();
        fs::write(target.join("sub").join("a.txt"), b"a").unwrap();
        let j = dir.path().join("j");
        make_link(&target, &j, LinkKind::Junction).unwrap();
        assert_eq!(fs::read(j.join("sub").join("a.txt")).unwrap(), b"a");
        assert!(fs::symlink_metadata(&j).unwrap().file_type().is_symlink());
        crate::local::delete_permanently(&crate::VPath::local(&j)).unwrap();
        assert!(target.join("sub").join("a.txt").exists());
        let err = make_link(&target.join("sub").join("a.txt"), &j, LinkKind::Junction);
        assert!(format!("{:#}", err.unwrap_err()).contains("folder"));
        assert!(!j.exists(), "a refused junction leaves nothing");
    }

    /// Without Developer Mode or elevation the error says so and names the alternatives;
    /// with it, the link is made.
    #[cfg(windows)]
    #[test]
    fn a_symlink_without_the_privilege_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, b"f").unwrap();
        let l = dir.path().join("l.txt");
        match make_link(&f, &l, LinkKind::Symlink) {
            Ok(()) => assert!(symlinks_allowed()),
            Err(e) => {
                assert!(!symlinks_allowed());
                let text = format!("{e:#}");
                assert!(text.contains("Developer Mode"), "{text}");
                assert!(text.contains("junction"), "{text}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_made_and_junctions_refused() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, b"f").unwrap();
        make_link(&f, &dir.path().join("l"), LinkKind::Symlink).unwrap();
        assert_eq!(fs::read(dir.path().join("l")).unwrap(), b"f");
        assert!(make_link(dir.path(), &dir.path().join("j"), LinkKind::Junction).is_err());
    }
}
