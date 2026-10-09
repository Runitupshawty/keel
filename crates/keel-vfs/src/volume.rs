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
                // ponytail: a disk without a serial is named by its number, which can change
                // when disks are added; serials cover every SATA/NVMe/USB-bridge disk seen.
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
        details(&c, dev as u64, &mut v);
        Some(v)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn details(c: &std::ffi::CStr, dev: u64, v: &mut VolumeInfo) {
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
        let (major, minor) = (libc::major(dev as _), libc::minor(dev as _));
        // The filesystem UUID: the /dev/disk/by-uuid link to this device.
        if let Ok(links) = std::fs::read_dir("/dev/disk/by-uuid") {
            for l in links.flatten() {
                if std::fs::metadata(l.path()).is_ok_and(|m| m.rdev() == dev) {
                    v.id = format!("uuid:{}", l.file_name().to_string_lossy());
                    break;
                }
            }
        }
        // The disk: /sys/dev/block/M:m is the partition (or the whole disk).
        let Ok(block) = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")) else {
            return;
        };
        let disk = if block.join("partition").exists() {
            block.parent().map(Path::to_path_buf)
        } else {
            Some(block)
        };
        let Some(disk) = disk else { return };
        let read = |f: &str| {
            std::fs::read_to_string(disk.join(f))
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        if read("removable").as_deref() == Some("1") {
            v.kind = VolumeType::Removable;
        }
        let name = disk.file_name().map(|n| n.to_string_lossy().into_owned());
        v.disk = read("device/serial")
            .or_else(|| read("device/wwid"))
            .or(name)
            .map(|s| format!("disk:{s}"));
    }

    #[cfg(target_os = "macos")]
    fn details(c: &std::ffi::CStr, _dev: u64, v: &mut VolumeInfo) {
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
    fn details(_: &std::ffi::CStr, _: u64, _: &mut VolumeInfo) {}
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
}
