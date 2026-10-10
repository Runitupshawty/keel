//! Which volume, and which physical disk, a local path is on (Task 33: failure domains for
//! the library's protection model). Touches the disk; call off the UI thread.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeType {
    Fixed,
    /// USB / removable media.
    Removable,
    /// A network share.
    Network,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Stable volume id: `\\?\volume{guid}\` or `\\server\share\` (Windows), `uuid:<fs uuid>`
    /// (Linux), else `dev:<device>` (the device number or the mounted device).
    pub id: String,
    /// The volume label (Windows), else the mount point's name.
    pub label: String,
    pub kind: VolumeType,
    /// The physical disk behind the volume (`disk:<serial>` when the disk reports one, else
    /// its device name), or `net:<server>` for a share; None when unknown.
    pub disk: Option<String>,
    pub used: u64,
    pub total: u64,
}

/// The volume `path` (which must exist) is on. None when the OS cannot tell.
pub fn volume_info(path: &Path) -> Option<VolumeInfo> {
    imp::info(path)
}

#[cfg(windows)]
mod imp {
    use super::{VolumeInfo, VolumeType};
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        BusTypeUsb, CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW,
        FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_GET_DEVICE_NUMBER,
        IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
        STORAGE_PROPERTY_QUERY,
    };
    use windows::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_REMOTE, DRIVE_REMOVABLE};
    use windows::Win32::System::IO::DeviceIoControl;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// A device opened for queries only (no read/write access needed, no admin).
    struct Device(HANDLE);
    impl Drop for Device {
        fn drop(&mut self) {
            // SAFETY: a handle CreateFileW returned, closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
    fn open(name: &str) -> Option<Device> {
        let w = wide(name);
        // SAFETY: `w` is NUL-terminated and outlives the call.
        unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .ok()
        .map(Device)
    }

    /// (serial number, on USB) of physical disk `n`.
    fn disk_facts(n: u32) -> (Option<String>, bool) {
        let Some(dev) = open(&format!(r"\\.\PhysicalDrive{n}")) else {
            return (None, false);
        };
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            ..Default::default()
        };
        let mut buf = vec![0u8; 4096];
        let mut got = 0u32;
        // SAFETY: in/out buffers are valid for the sizes given.
        let ok = unsafe {
            DeviceIoControl(
                dev.0,
                IOCTL_STORAGE_QUERY_PROPERTY,
                Some(&query as *const _ as *const _),
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                Some(buf.as_mut_ptr() as *mut _),
                buf.len() as u32,
                Some(&mut got),
                None,
            )
        };
        if ok.is_err() || (got as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
            return (None, false);
        }
        // SAFETY: the buffer holds at least one descriptor (checked above); unaligned read.
        let d: STORAGE_DEVICE_DESCRIPTOR =
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };
        let at = d.SerialNumberOffset as usize;
        let serial = (at > 0 && at < got as usize)
            .then(|| {
                let end = buf[at..got as usize]
                    .iter()
                    .position(|&b| b == 0)
                    .map_or(got as usize, |e| at + e);
                String::from_utf8_lossy(&buf[at..end]).trim().to_owned()
            })
            .filter(|s| !s.is_empty());
        (serial, d.BusType == BusTypeUsb)
    }

    pub(super) fn info(path: &Path) -> Option<VolumeInfo> {
        let id = crate::desktop::volume_id(path)?;
        let root = wide(&id);
        let mut label = [0u16; 261];
        // SAFETY: `root` is NUL-terminated; the label buffer is writable.
        let named = unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                None,
            )
        };
        let label = if named.is_ok() {
            let n = label.iter().position(|&c| c == 0).unwrap_or(label.len());
            String::from_utf16_lossy(&label[..n])
        } else {
            String::new()
        };
        // SAFETY: as above.
        let drive = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
        let (mut total, mut free) = (0u64, 0u64);
        let p = wide(&path.to_string_lossy());
        // SAFETY: `p` is NUL-terminated; outputs are valid u64s.
        let _ = unsafe {
            GetDiskFreeSpaceExW(PCWSTR(p.as_ptr()), None, Some(&mut total), Some(&mut free))
        };
        let mut kind = match drive {
            DRIVE_REMOTE => VolumeType::Network,
            DRIVE_REMOVABLE | DRIVE_CDROM => VolumeType::Removable,
            _ => VolumeType::Fixed,
        };
        let disk = if let Some(share) = id.strip_prefix(r"\\").filter(|_| !id.starts_with(r"\\?\"))
        {
            kind = VolumeType::Network;
            share
                .split('\\')
                .next()
                .map(|server| format!("net:{server}"))
        } else {
            // `\\?\Volume{…}` without the trailing backslash names the volume device.
            let volume = open(id.trim_end_matches('\\'));
            let number = volume.and_then(|v| {
                let mut n = STORAGE_DEVICE_NUMBER::default();
                let mut got = 0u32;
                // SAFETY: the output is one STORAGE_DEVICE_NUMBER.
                unsafe {
                    DeviceIoControl(
                        v.0,
                        IOCTL_STORAGE_GET_DEVICE_NUMBER,
                        None,
                        0,
                        Some(&mut n as *mut _ as *mut _),
                        std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                        Some(&mut got),
                        None,
                    )
                }
                .ok()
                .map(|()| n.DeviceNumber)
            });
            // Spanned volumes (dynamic disks, Storage Spaces) have no single disk: None.
            number.map(|n| {
                let (serial, usb) = disk_facts(n);
                if usb {
                    kind = VolumeType::Removable;
                }
                // ponytail: a disk without a serial (a VHDX such as a Dev Drive, some RAID
                // controllers) is named by its number, which can change when disks are added;
                // serials cover every SATA/NVMe/USB-bridge disk seen. The drive inventory's
                // failure-domain field overrides it (`Library::set_failure_domain`).
                serial.map_or_else(|| format!("disk:#{n}"), |s| format!("disk:{s}"))
            })
        };
        Some(VolumeInfo {
            id,
            label,
            kind,
            disk,
            used: total.saturating_sub(free),
            total,
        })
    }
}

#[cfg(unix)]
mod imp {
    use super::{VolumeInfo, VolumeType};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub(super) fn info(path: &Path) -> Option<VolumeInfo> {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: zeroed is a valid statvfs; `c` is NUL-terminated.
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } != 0 {
            return None;
        }
        let frsize = vfs.f_frsize as u64;
        let total = vfs.f_blocks as u64 * frsize;
        let used = total.saturating_sub(vfs.f_bfree as u64 * frsize);
        let dev = std::fs::metadata(path).ok()?.dev();
        // The mount point: the highest ancestor on the same device.
        let mount = path
            .ancestors()
            .take_while(|a| std::fs::metadata(a).is_ok_and(|m| m.dev() == dev))
            .last()
            .unwrap_or(path);
        let label = mount
            .file_name()
            .map_or_else(|| "/".to_owned(), |n| n.to_string_lossy().into_owned());
        let mut v = VolumeInfo {
            id: format!("dev:{dev:x}"),
            label,
            kind: VolumeType::Fixed,
            disk: None,
            used,
            total,
        };
        details(&c, dev, path, &mut v);
        Some(v)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn details(c: &std::ffi::CStr, dev: u64, path: &Path, v: &mut VolumeInfo) {
        use super::linux::{disks, mount_of, net_server, parse_mountinfo, SysFs};
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")
            .map(|t| parse_mountinfo(&t))
            .unwrap_or_default();
        let at = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mount = mount_of(
            &mounts,
            (
                libc::major(dev as libc::dev_t),
                libc::minor(dev as libc::dev_t),
            ),
            &at,
        );
        // A share is named by what is mounted (stable across remounts); its server is the
        // failure domain.
        if let Some(m) = mount {
            if let Some(server) = net_server(&m.fstype, &m.source) {
                v.kind = VolumeType::Network;
                v.id = format!("share:{}", m.source);
                v.disk = Some(format!("net:{server}"));
                return;
            }
        }
        // NFS, SMB/CIFS (old and new) superblock magics.
        const NETWORK: &[i64] = &[0x6969, 0x517B, 0xFF53_4D42, 0xFE53_4D42];
        // SAFETY: zeroed is a valid statfs; `c` is NUL-terminated.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut fs) } == 0
            && NETWORK.contains(&(fs.f_type as i64))
        {
            v.kind = VolumeType::Network;
            return;
        }
        // An anonymous device (a btrfs subvolume, …) stands for the mounted block device.
        let block = if libc::major(dev as libc::dev_t) == 0 {
            mount
                .filter(|m| m.source.starts_with("/dev/"))
                .and_then(|m| std::fs::metadata(&m.source).ok())
                .map(|md| md.rdev())
        } else {
            Some(dev)
        };
        let Some(block) = block else { return };
        // The filesystem UUID: the /dev/disk/by-uuid link to this device.
        if let Ok(links) = std::fs::read_dir("/dev/disk/by-uuid") {
            for l in links.flatten() {
                if std::fs::metadata(l.path()).is_ok_and(|m| m.rdev() == block) {
                    v.id = format!("uuid:{}", l.file_name().to_string_lossy());
                    break;
                }
            }
        }
        struct Real;
        impl SysFs for Real {
            fn canonical(&self, p: &str) -> Option<String> {
                Some(
                    std::fs::canonicalize(p)
                        .ok()?
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            fn read(&self, p: &str) -> Option<String> {
                std::fs::read_to_string(p).ok()
            }
            fn list(&self, p: &str) -> Vec<String> {
                std::fs::read_dir(p).map_or_else(
                    |_| Vec::new(),
                    |d| {
                        d.flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect()
                    },
                )
            }
        }
        let found = disks(
            &Real,
            libc::major(block as libc::dev_t),
            libc::minor(block as libc::dev_t),
        );
        if found.iter().any(|d| d.removable) {
            v.kind = VolumeType::Removable;
        }
        if !found.is_empty() {
            let ids: Vec<String> = found.into_iter().map(|d| d.id).collect();
            v.disk = Some(ids.join("+"));
        }
    }

    #[cfg(target_os = "macos")]
    fn details(c: &std::ffi::CStr, _dev: u64, _path: &Path, v: &mut VolumeInfo) {
        // SAFETY: zeroed is a valid statfs; `c` is NUL-terminated.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut fs) } != 0 {
            return;
        }
        let text = |b: &[libc::c_char]| {
            let bytes: Vec<u8> = b
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let (kind, from, on) = (
            text(&fs.f_fstypename),
            text(&fs.f_mntfromname),
            text(&fs.f_mntonname),
        );
        if ["nfs", "smbfs", "afpfs", "webdav"].contains(&kind.as_str()) {
            v.kind = VolumeType::Network;
            v.disk = from
                .trim_start_matches("//")
                .split('/')
                .next()
                .map(|s| format!("net:{}", s.rsplit('@').next().unwrap_or(s)));
            return;
        }
        if on.starts_with("/Volumes/") {
            v.kind = VolumeType::Removable;
        }
        v.id = format!("dev:{from}");
        // /dev/disk3s1 -> /dev/disk3 (APFS volumes share their container's disk).
        // ponytail: device names, not serials; IOKit would give the serial.
        if let Some(rest) = from.strip_prefix("/dev/disk") {
            let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
            v.disk = Some(format!("disk:disk{n}"));
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    fn details(_: &std::ffi::CStr, _: u64, _: &Path, _: &mut VolumeInfo) {}
}

/// Linux mount and disk lookups as pure functions over the text of `/proc/self/mountinfo`
/// and a view of `/sys` (tested on every platform with fixtures).
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
pub(crate) mod linux {
    use std::path::Path;

    /// One line of `/proc/self/mountinfo`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct Mount {
        /// major:minor of the mounted filesystem (0:N for anonymous devices: btrfs
        /// subvolumes, network filesystems).
        pub dev: (u32, u32),
        pub point: String,
        pub fstype: String,
        /// What is mounted: `/dev/sda2`, `server:/export`, `//server/share`.
        pub source: String,
    }

    /// Undoes the octal escapes mountinfo uses for space, tab, newline and backslash.
    fn unescape(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            let code = (b[i] == b'\\' && i + 4 <= b.len())
                .then(|| u8::from_str_radix(s.get(i + 1..i + 4)?, 8).ok())
                .flatten();
            match code {
                Some(c) => {
                    out.push(c);
                    i += 4;
                }
                None => {
                    out.push(b[i]);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// `ID PARENT MAJ:MIN ROOT POINT OPTIONS [OPTIONAL…] - FSTYPE SOURCE SUPER-OPTIONS`.
    pub(crate) fn parse_mountinfo(text: &str) -> Vec<Mount> {
        text.lines()
            .filter_map(|line| {
                let (head, tail) = line.split_once(" - ")?;
                let head: Vec<&str> = head.split(' ').collect();
                let mut tail = tail.split(' ');
                let (major, minor) = head.get(2)?.split_once(':')?;
                Some(Mount {
                    dev: (major.parse().ok()?, minor.parse().ok()?),
                    point: unescape(head.get(4)?),
                    fstype: tail.next()?.to_owned(),
                    source: unescape(tail.next()?),
                })
            })
            .collect()
    }

    /// The mount `path` (resolved) is under on device `dev`: the deepest such mount point.
    pub(crate) fn mount_of<'a>(
        mounts: &'a [Mount],
        dev: (u32, u32),
        path: &Path,
    ) -> Option<&'a Mount> {
        mounts
            .iter()
            .filter(|m| m.dev == dev && path.starts_with(&m.point))
            .max_by_key(|m| m.point.len())
    }

    /// The server of a network filesystem's mount source, lowercased; None for local ones.
    pub(crate) fn net_server(fstype: &str, source: &str) -> Option<String> {
        let host = match fstype {
            // server:/export, [v6]:/export
            "nfs" | "nfs4" => match source.strip_prefix('[') {
                Some(v6) => v6.split_once(']')?.0,
                None => source.split_once(':')?.0,
            },
            // //server/share
            "cifs" | "smb3" | "smbfs" => source.trim_start_matches('/').split('/').next()?,
            // user@host:/path
            "sshfs" | "fuse.sshfs" => source.split_once(':')?.0.rsplit('@').next()?,
            // https://host/path
            "davfs" | "fuse.davfs" | "davfs2" => source
                .split_once("://")?
                .1
                .split('/')
                .next()?
                .rsplit('@')
                .next()?,
            _ => return None,
        };
        (!host.is_empty()).then(|| host.to_lowercase())
    }

    /// The parts of `/sys` the disk lookup reads (paths as text).
    pub(crate) trait SysFs {
        /// The resolved path (sysfs entries are symlinks into `/sys/devices`).
        fn canonical(&self, p: &str) -> Option<String>;
        fn read(&self, p: &str) -> Option<String>;
        /// Entry names in a folder (empty when missing).
        fn list(&self, p: &str) -> Vec<String>;
    }

    /// A physical disk behind a block device.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct Disk {
        /// `disk:<serial>`, else `disk:<wwid>`, else `disk:<kernel name>`.
        pub id: String,
        pub removable: bool,
    }

    /// The physical disks behind block device `major:minor`, sorted: a partition's disk;
    /// the disks under a device-mapper (LVM, LUKS) or md device, through `slaves/`; else
    /// the device itself.
    pub(crate) fn disks(sys: &dyn SysFs, major: u32, minor: u32) -> Vec<Disk> {
        fn walk(sys: &dyn SysFs, dev: &str, depth: u8, out: &mut Vec<Disk>) {
            let slaves = sys.list(&format!("{dev}/slaves"));
            if !slaves.is_empty() && depth < 8 {
                for s in slaves {
                    if let Some(d) = sys.canonical(&format!("{dev}/slaves/{s}")) {
                        walk(sys, &d, depth + 1, out);
                    }
                }
                return;
            }
            let read = |p: String| {
                sys.read(&p)
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            };
            let disk = match read(format!("{dev}/partition")) {
                Some(_) => dev.rsplit_once('/').map_or(dev, |(parent, _)| parent),
                None => dev,
            };
            let name = disk.rsplit('/').next().unwrap_or(disk).to_owned();
            let id = read(format!("{disk}/device/serial"))
                .or_else(|| read(format!("{disk}/device/wwid")))
                .or_else(|| read(format!("{disk}/wwid")))
                .unwrap_or(name);
            out.push(Disk {
                id: format!("disk:{id}"),
                removable: read(format!("{disk}/removable")).as_deref() == Some("1"),
            });
        }
        let mut out = Vec::new();
        if let Some(dev) = sys.canonical(&format!("/sys/dev/block/{major}:{minor}")) {
            walk(sys, &dev, 0, &mut out);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    pub(super) fn info(_: &std::path::Path) -> Option<super::VolumeInfo> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_has_a_volume_and_a_disk() {
        let tmp = std::env::temp_dir();
        let v = volume_info(&tmp).unwrap();
        assert!(!v.id.is_empty());
        assert!(v.total > 0 && v.used <= v.total, "{v:?}");
        // Two folders on one volume: the same id and disk.
        let sub = tempfile::tempdir().unwrap();
        let w = volume_info(sub.path()).unwrap();
        assert_eq!((w.id, w.disk), (v.id.clone(), v.disk.clone()));
        #[cfg(windows)]
        assert!(v.disk.is_some(), "a fixed disk is found: {v:?}");
        eprintln!("{v:?}");
        assert_eq!(volume_info(Path::new("/no/such/keel/path")), None);
    }

    use super::linux::{disks, mount_of, net_server, parse_mountinfo, Disk, SysFs};
    use std::collections::HashMap;

    const MOUNTINFO: &str = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
25 22 253:1 / /home rw,relatime shared:2 - ext4 /dev/mapper/vg-home rw
26 22 0:41 /@data /data rw,relatime shared:3 - btrfs /dev/nvme0n1p3 rw,subvol=/@data
27 22 0:42 /@media /srv/my\\040media rw,relatime shared:4 - btrfs /dev/nvme0n1p3 rw
30 22 0:51 / /mnt/nas rw,relatime shared:5 - nfs4 Server.Example:/export/photos rw,addr=x
31 22 0:52 / /mnt/smb rw,relatime shared:6 master:1 - cifs //server.example/share rw
32 22 0:53 / /mnt/v6 rw - nfs [2001:db8::1]:/x rw
33 22 0:54 / /mnt/ssh rw - fuse.sshfs user@host.example:/home rw
";

    #[test]
    fn mountinfo_names_mounts_and_servers() {
        let m = parse_mountinfo(MOUNTINFO);
        assert_eq!(m.len(), 8);
        assert_eq!(
            (m[2].dev, m[2].fstype.as_str(), m[2].source.as_str()),
            ((0, 41), "btrfs", "/dev/nvme0n1p3")
        );
        // Escaped spaces are decoded.
        assert_eq!(m[3].point, "/srv/my media");
        // The deepest mount of the device the path is on.
        let at = |dev, p: &str| mount_of(&m, dev, Path::new(p)).map(|m| m.point.clone());
        assert_eq!(at((0, 41), "/data/photos/a.jpg").as_deref(), Some("/data"));
        assert_eq!(at((8, 2), "/etc").as_deref(), Some("/"));
        assert_eq!(at((0, 99), "/data"), None);
        let servers: Vec<_> = m.iter().map(|m| net_server(&m.fstype, &m.source)).collect();
        assert_eq!(
            servers,
            [
                None,
                None,
                None,
                None,
                Some("server.example".into()),
                Some("server.example".into()),
                Some("2001:db8::1".into()),
                Some("host.example".into()),
            ]
        );
    }

    /// A /sys made of links (resolved paths), files and folders.
    #[derive(Default)]
    struct Fake {
        links: HashMap<String, String>,
        files: HashMap<String, String>,
    }

    impl SysFs for Fake {
        fn canonical(&self, p: &str) -> Option<String> {
            self.links.get(p).cloned()
        }
        fn read(&self, p: &str) -> Option<String> {
            self.files.get(p).cloned()
        }
        fn list(&self, p: &str) -> Vec<String> {
            let prefix = format!("{p}/");
            let mut out: Vec<String> = self
                .links
                .keys()
                .filter_map(|k| k.strip_prefix(&prefix))
                .filter(|rest| !rest.contains('/'))
                .map(str::to_owned)
                .collect();
            out.sort();
            out
        }
    }

    #[test]
    fn sys_block_resolves_partitions_lvm_and_spans_to_disks() {
        let sata = "/sys/devices/pci0/ata1/block/sda";
        let nvme = "/sys/devices/pci0/nvme/nvme0/nvme0n1";
        let usb = "/sys/devices/pci0/usb1/block/sdb";
        let mut sys = Fake::default();
        let mut link = |from: &str, to: String| sys.links.insert(from.into(), to);
        link("/sys/dev/block/8:2", format!("{sata}/sda2"));
        link("/sys/dev/block/259:3", format!("{nvme}/nvme0n1p3"));
        link("/sys/dev/block/8:17", format!("{usb}/sdb1"));
        // LVM: dm-1 on sda2; dm-2 spans sda2 and nvme0n1p3; dm-3 (LUKS) on dm-1.
        link(
            "/sys/dev/block/253:1",
            "/sys/devices/virtual/block/dm-1".into(),
        );
        link(
            "/sys/dev/block/253:2",
            "/sys/devices/virtual/block/dm-2".into(),
        );
        link(
            "/sys/dev/block/253:3",
            "/sys/devices/virtual/block/dm-3".into(),
        );
        link(
            "/sys/devices/virtual/block/dm-1/slaves/sda2",
            format!("{sata}/sda2"),
        );
        link(
            "/sys/devices/virtual/block/dm-2/slaves/sda2",
            format!("{sata}/sda2"),
        );
        link(
            "/sys/devices/virtual/block/dm-2/slaves/nvme0n1p3",
            format!("{nvme}/nvme0n1p3"),
        );
        link(
            "/sys/devices/virtual/block/dm-3/slaves/dm-1",
            "/sys/devices/virtual/block/dm-1".into(),
        );
        let mut file = |p: String, v: &str| sys.files.insert(p, format!("{v}\n"));
        file(format!("{sata}/sda2/partition"), "2");
        file(format!("{sata}/device/serial"), "WD-123");
        file(format!("{sata}/removable"), "0");
        file(format!("{nvme}/nvme0n1p3/partition"), "3");
        // NVMe: no device/serial on the namespace, a wwid.
        file(format!("{nvme}/wwid"), "eui.0025");
        file(format!("{usb}/sdb1/partition"), "1");
        file(format!("{usb}/removable"), "1");
        let ids = |major, minor| -> Vec<String> {
            disks(&sys, major, minor)
                .into_iter()
                .map(|d| d.id)
                .collect()
        };
        assert_eq!(ids(8, 2), ["disk:WD-123"]);
        assert_eq!(ids(259, 3), ["disk:eui.0025"]);
        // Every logical volume on one disk is that disk, also under LUKS.
        assert_eq!(ids(253, 1), ["disk:WD-123"]);
        assert_eq!(ids(253, 3), ["disk:WD-123"]);
        // A volume spanning two disks names both.
        assert_eq!(ids(253, 2), ["disk:WD-123", "disk:eui.0025"]);
        // No serial: the kernel name; removable flag read from the disk.
        assert_eq!(
            disks(&sys, 8, 17),
            [Disk {
                id: "disk:sdb".into(),
                removable: true
            }]
        );
        assert!(disks(&sys, 9, 9).is_empty());
    }
}
