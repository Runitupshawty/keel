//! The mounted volumes Keel may index by walking: on Windows every drive letter that
//! is not a fixed NTFS volume (those are read from the MFT) — FAT, exFAT, ReFS,
//! removable and network drives; on Linux the mounts under `/media` and `/run/media`
//! and network mounts; on macOS the volumes in `/Volumes`.

use std::path::{Path, PathBuf};

/// What kind of drive a volume is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKind {
    Fixed,
    Removable,
    Network,
}

/// A mounted (online) volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    /// The root folder (`E:\`, `/media/usb`).
    pub root: PathBuf,
    pub kind: VolumeKind,
    /// The file system's name ("exFAT", "FAT32", "ReFS", "NTFS", "nfs", ...).
    pub fs: String,
    pub label: String,
}

impl VolumeInfo {
    /// True when this volume is indexed by walking rather than by the NTFS index
    /// (which covers fixed NTFS volumes on Windows).
    pub fn needs_walk(&self) -> bool {
        !(cfg!(windows) && self.kind == VolumeKind::Fixed && self.fs.eq_ignore_ascii_case("NTFS"))
    }
}

/// Where the volume list comes from: the system, or a fixed list in tests.
pub trait VolumeSource: Send + Sync {
    /// The volumes mounted now. May block on a slow network drive: worker threads only.
    fn volumes(&self) -> Vec<VolumeInfo>;
}

/// The volumes of this computer. Lists nothing inside a test binary, so tests never
/// walk real drives.
pub struct SystemVolumes;

impl VolumeSource for SystemVolumes {
    fn volumes(&self) -> Vec<VolumeInfo> {
        if in_test_binary() {
            return Vec::new();
        }
        list()
    }
}

fn in_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.file_name().map(|d| d == "deps"))
        .unwrap_or(false)
}

/// The same folder, ignoring case and a trailing separator (`E:\` = `e:`).
pub fn same_root(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| {
        let mut s = p.to_string_lossy().replace('\\', "/").to_lowercase();
        while s.ends_with('/') {
            s.pop();
        }
        s
    };
    n(a) == n(b)
}

#[cfg(windows)]
fn list() -> Vec<VolumeInfo> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let root = format!("{}:\\", char::from(b'A' + bit));
            let wide = HSTRING::from(root.as_str());
            // SAFETY: `wide` is a NUL-terminated wide string that outlives the call.
            let kind = match unsafe { GetDriveTypeW(&wide) } {
                DRIVE_FIXED | DRIVE_RAMDISK => VolumeKind::Fixed,
                DRIVE_REMOVABLE | DRIVE_CDROM => VolumeKind::Removable,
                DRIVE_REMOTE => VolumeKind::Network,
                _ => return None,
            };
            let (mut label, mut fs) = ([0u16; 261], [0u16; 64]);
            // SAFETY: the buffers outlive the call. Fails for an empty card reader or a
            // disconnected share: not mounted.
            unsafe {
                GetVolumeInformationW(&wide, Some(&mut label), None, None, None, Some(&mut fs))
            }
            .ok()?;
            let text = |b: &[u16]| {
                String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())])
            };
            Some(VolumeInfo {
                root: PathBuf::from(root),
                kind,
                fs: text(&fs),
                label: text(&label),
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn list() -> Vec<VolumeInfo> {
    std::fs::read_to_string("/proc/self/mounts")
        .map(|text| parse_mounts(&text))
        .unwrap_or_default()
}

/// Network file systems by their `/proc/mounts` type.
#[cfg(any(target_os = "linux", test))]
const NETWORK_FS: &[&str] = &[
    "nfs", "nfs4", "cifs", "smb3", "smbfs", "fuse.sshfs", "afs", "ncpfs", "davfs", "fuse.rclone",
];

/// `/proc/mounts` lines: removable mounts (under `/media`, `/run/media`) and network
/// mounts anywhere. Octal escapes (`\040` for a space) are decoded.
#[cfg(any(target_os = "linux", test))]
fn parse_mounts(text: &str) -> Vec<VolumeInfo> {
    let unescape = |s: &str| -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            let octal = b.get(i + 1..i + 4).filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
            match (b[i], octal) {
                (b'\\', Some(d)) => {
                    let v = d.iter().fold(0u32, |a, c| a * 8 + u32::from(c - b'0'));
                    out.push(u8::try_from(v).unwrap_or(b'?'));
                    i += 4;
                }
                (c, _) => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    let mut seen: Vec<VolumeInfo> = Vec::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(_dev), Some(dir), Some(fs)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let dir = unescape(dir);
        let network = NETWORK_FS.contains(&fs);
        let removable = ["/media/", "/run/media/"].iter().any(|p| dir.starts_with(p));
        if !(network || removable) || seen.iter().any(|v| v.root == Path::new(&dir)) {
            continue;
        }
        seen.push(VolumeInfo {
            label: Path::new(&dir)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            root: PathBuf::from(dir),
            kind: if network {
                VolumeKind::Network
            } else {
                VolumeKind::Removable
            },
            fs: fs.to_owned(),
        });
    }
    seen
}

#[cfg(target_os = "macos")]
fn list() -> Vec<VolumeInfo> {
    let Ok(dir) = std::fs::read_dir("/Volumes") else {
        return Vec::new();
    };
    dir.flatten()
        // The boot volume is a link to `/`.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| VolumeInfo {
            label: e.file_name().to_string_lossy().into_owned(),
            root: e.path(),
            kind: VolumeKind::Removable,
            fs: String::new(),
        })
        .collect()
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn list() -> Vec<VolumeInfo> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_keep_removable_and_network_volumes() {
        let text = "\
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
proc /proc proc rw 0 0
/dev/sdb1 /media/me/MY\\040USB vfat rw 0 0
/dev/sdc1 /run/media/me/disk exfat rw 0 0
server:/export /mnt/nfs nfs4 rw 0 0
//nas/share /srv/share cifs rw 0 0
/dev/sdb1 /media/me/MY\\040USB vfat rw 0 0
C:\\134 /mnt/c 9p rw 0 0
garbage
";
        let v = parse_mounts(text);
        let roots: Vec<_> = v.iter().map(|v| v.root.to_string_lossy().into_owned()).collect();
        assert_eq!(
            roots,
            ["/media/me/MY USB", "/run/media/me/disk", "/mnt/nfs", "/srv/share"]
        );
        assert_eq!(v[0].kind, VolumeKind::Removable);
        assert_eq!((v[0].fs.as_str(), v[0].label.as_str()), ("vfat", "MY USB"));
        assert_eq!(v[2].kind, VolumeKind::Network);
        // Hostile escapes do not panic.
        let _ = parse_mounts("a /media/\\777\\0 vfat\nb /media/x\\ vfat\n");
    }

    #[test]
    fn walk_volumes_and_root_identity() {
        let vol = |kind, fs: &str| VolumeInfo {
            root: PathBuf::from("E:\\"),
            kind,
            fs: fs.into(),
            label: String::new(),
        };
        assert!(vol(VolumeKind::Fixed, "exFAT").needs_walk());
        assert!(vol(VolumeKind::Fixed, "ReFS").needs_walk());
        assert!(vol(VolumeKind::Removable, "NTFS").needs_walk());
        assert_eq!(vol(VolumeKind::Fixed, "NTFS").needs_walk(), !cfg!(windows));
        assert!(same_root(Path::new("E:\\"), Path::new("e:")));
        assert!(!same_root(Path::new("E:\\"), Path::new("F:\\")));
        assert!(SystemVolumes.volumes().is_empty(), "never real drives in tests");
    }
}
