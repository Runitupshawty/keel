//! The folder a restore writes into, opened once: its volume is checked on that handle
//! (with the volumes behind it when it is a loop device or a virtual disk), and files are
//! created relative to the handle, so no path is looked up again between the check and the
//! create. A junction or symlink swapped in for the folder or any folder above it after the
//! check changes nothing: the handle still names the folder that was checked, and on
//! Windows it is held without delete sharing, so that folder cannot be renamed away.

use super::source::volume_of_file;
use super::{RecoverError, Result, VolumeId};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct DestDir {
    path: PathBuf,
    handle: File,
}

impl DestDir {
    /// Opens the folder `dir`; refused with [`RecoverError::SameVolume`] when it is on
    /// `source`, or on a loop device or virtual disk whose backing file is there.
    pub fn open(dir: &Path, source: VolumeId) -> Result<Self> {
        let handle = sys::open_dir(dir)?;
        if volume_of_file(&handle)? == source || sys::backed_on(&handle, source) {
            return Err(RecoverError::SameVolume);
        }
        Ok(Self {
            path: dir.to_owned(),
            handle,
        })
    }

    /// A new file `name` in the folder, or `name (2).ext` and so on (never over a file):
    /// its name, its path and the file.
    pub fn create_free(&self, name: &str) -> Result<(String, PathBuf, File)> {
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        for n in 1..10_000 {
            let free = match n {
                1 => name.to_owned(),
                n => format!("{stem} ({n}){ext}"),
            };
            match sys::create_new(&self.handle, &free) {
                Ok(f) => return Ok((free.clone(), self.path.join(&free), f)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(RecoverError::Unrecoverable(format!(
            "no free name for {name} in {}",
            self.path.display()
        )))
    }

    /// Removes `file`, created here as `name` (a restore that failed).
    pub fn remove(&self, name: &str, file: File) {
        sys::remove(&self.handle, name, file);
    }
}

#[cfg(unix)]
mod sys {
    use super::VolumeId;
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::path::Path;

    pub fn open_dir(dir: &Path) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(dir)
    }

    fn c_name(name: &str) -> io::Result<CString> {
        CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))
    }

    pub fn create_new(dir: &File, name: &str) -> io::Result<File> {
        let c = c_name(name)?;
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: a directory descriptor owned by `dir` and a NUL-terminated name, both
        // alive for the call; the descriptor returned is owned by the File from here on.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, 0o644 as libc::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a new descriptor nothing else owns.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    pub fn remove(dir: &File, name: &str, file: File) {
        drop(file);
        if let Ok(c) = c_name(name) {
            // SAFETY: as in `create_new`.
            unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) };
        }
    }

    /// Whether the folder's volume is a loop device whose backing file (or that file's
    /// own loop device, and so on) is on `source` (Linux).
    #[cfg(target_os = "linux")]
    pub fn backed_on(dir: &File, source: VolumeId) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(mut dev) = dir.metadata().map(|m| m.dev()) else {
            return false;
        };
        for _ in 0..8 {
            let Some(file) = super::loop_backing(dev) else {
                return false;
            };
            let Ok(m) = std::fs::metadata(&file) else {
                return false;
            };
            if VolumeId(m.dev()) == source {
                return true;
            }
            dev = m.dev();
        }
        false
    }

    #[cfg(not(target_os = "linux"))]
    pub fn backed_on(_: &File, _: VolumeId) -> bool {
        false
    }
}

/// The backing file of the loop device `dev` (or of the loop device a partition `dev`
/// belongs to), from sysfs.
#[cfg(target_os = "linux")]
fn loop_backing(dev: u64) -> Option<PathBuf> {
    let (major, minor) = dev_parts(dev);
    let base = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
    [base.join("loop"), base.join("../loop")]
        .iter()
        .find_map(|d| std::fs::read_to_string(d.join("backing_file")).ok())
        .and_then(|text| backing_path(&text))
}

/// Linux's `dev_t` split into major and minor numbers (glibc's encoding).
#[cfg(any(target_os = "linux", test))]
fn dev_parts(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xFFF) | ((dev >> 32) & !0xFFF);
    let minor = (dev & 0xFF) | ((dev >> 12) & 0xFFFF_FF00);
    (major, minor)
}

/// The path in a sysfs `backing_file`: one line, ` (deleted)` once the file is gone.
#[cfg(any(target_os = "linux", test))]
fn backing_path(text: &str) -> Option<PathBuf> {
    let line = text.trim_end_matches(['\n', '\r']);
    let line = line.strip_suffix(" (deleted)").unwrap_or(line);
    (!line.is_empty()).then(|| PathBuf::from(line))
}

#[cfg(windows)]
mod sys {
    use super::VolumeId;
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};
    use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows::Wdk::Storage::FileSystem::{
        NtCreateFile, FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_SYNCHRONOUS_IO_NONALERT,
        NTCREATEFILE_CREATE_OPTIONS,
    };
    use windows::Win32::Foundation::{
        RtlNtStatusToDosError, BOOLEAN, ERROR_INSUFFICIENT_BUFFER, HANDLE,
        STATUS_OBJECT_NAME_COLLISION, UNICODE_STRING,
    };
    use windows::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, SetFileInformationByHandle, FileDispositionInfo, DELETE,
        FILE_ADD_FILE, FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TRAVERSE, SYNCHRONIZE, VOLUME_NAME_GUID,
    };
    use windows::Win32::System::IO::IO_STATUS_BLOCK;
    use windows::Win32::Storage::Vhd::{
        GetStorageDependencyInformation, GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
        STORAGE_DEPENDENCY_INFO, STORAGE_DEPENDENCY_INFO_TYPE_2,
        STORAGE_DEPENDENCY_INFO_VERSION_2,
    };

    /// The folder, held without delete sharing so it cannot be renamed or replaced while
    /// the restore runs.
    pub fn open_dir(dir: &Path) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .access_mode((FILE_TRAVERSE | FILE_ADD_FILE | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(dir)
    }

    /// Creates `name` in the folder `dir` names by handle (NtCreateFile relative to it).
    pub fn create_new(dir: &File, name: &str) -> io::Result<File> {
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        let bytes = u16::try_from(wide.len() * 2)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
        let object = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: windows::core::PWSTR(wide.as_mut_ptr()),
        };
        let attrs = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: HANDLE(dir.as_raw_handle()),
            ObjectName: &object,
            // OBJ_CASE_INSENSITIVE, as Win32 opens names.
            Attributes: 0x40,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = HANDLE::default();
        let mut status_block = IO_STATUS_BLOCK::default();
        let options =
            NTCREATEFILE_CREATE_OPTIONS(FILE_NON_DIRECTORY_FILE.0 | FILE_SYNCHRONOUS_IO_NONALERT.0);
        // SAFETY: every pointer is to a local that outlives the call (`wide` backs
        // `object`); the handle written is owned by the File below.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_GENERIC_WRITE | DELETE,
                &attrs,
                &mut status_block,
                None,
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ,
                FILE_CREATE,
                options,
                None,
                0,
            )
        };
        if status == STATUS_OBJECT_NAME_COLLISION {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        if status.0 < 0 {
            // SAFETY: a plain status conversion.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        // SAFETY: a new handle nothing else owns.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }

    /// Deletes the file through its own handle (opened with delete access).
    pub fn remove(_dir: &File, _name: &str, file: File) {
        let info = FILE_DISPOSITION_INFO {
            DeleteFile: BOOLEAN(1),
        };
        // SAFETY: a handle owned by `file` and a local of the size given.
        let _ = unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
    }

    /// Whether the folder's volume is a virtual disk (VHD, VHDX) whose file, or the file of
    /// a disk it sits in, is on `source`.
    pub fn backed_on(dir: &File, source: VolumeId) -> bool {
        let Some(root) = volume_root(dir) else {
            return false;
        };
        let Ok(root) = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(root)
        else {
            return false;
        };
        host_volumes(&root)
            .iter()
            .any(|host| crate::recover::volume_of(host).is_ok_and(|v| v == source))
    }

    /// `\\?\Volume{...}\` of the volume the handle is on.
    fn volume_root(dir: &File) -> Option<PathBuf> {
        let mut buf = vec![0u16; 32 * 1024];
        // SAFETY: a handle owned by `dir` and a buffer of the length passed.
        let n = unsafe {
            GetFinalPathNameByHandleW(HANDLE(dir.as_raw_handle()), &mut buf, VOLUME_NAME_GUID)
        } as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        let path = String::from_utf16(&buf[..n]).ok()?;
        let end = path.find("}\\")? + 2;
        Some(PathBuf::from(&path[..end]))
    }

    /// The host volumes of a virtual disk volume (every level), by its root's handle;
    /// empty for a volume that is not a virtual disk.
    fn host_volumes(root: &File) -> Vec<PathBuf> {
        // u64 words keep the buffer aligned for the structures.
        let mut buf: Vec<u64> = vec![0; 512];
        for _ in 0..2 {
            let size = (buf.len() * 8) as u32;
            let info = buf.as_mut_ptr().cast::<STORAGE_DEPENDENCY_INFO>();
            let mut used = 0u32;
            // SAFETY: `info` points into `buf`, `size` bytes long and aligned; the version
            // is set before the call as it asks.
            let err = unsafe {
                (*info).Version = STORAGE_DEPENDENCY_INFO_VERSION_2;
                GetStorageDependencyInformation(
                    HANDLE(root.as_raw_handle()),
                    GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
                    size,
                    info,
                    Some(&mut used),
                )
            };
            if err == ERROR_INSUFFICIENT_BUFFER && used > size {
                buf = vec![0; (used as usize).div_ceil(8)];
                continue;
            }
            if err.0 != 0 {
                return Vec::new();
            }
            // SAFETY: the call filled the header; entries are read only as far as `buf`
            // holds them.
            unsafe {
                let n = (*info).NumberEntries as usize;
                let first = std::ptr::addr_of!((*info).Anonymous.Version2Entries)
                    .cast::<STORAGE_DEPENDENCY_INFO_TYPE_2>();
                let offset = first.cast::<u8>().offset_from(info.cast::<u8>()) as usize;
                let room = (buf.len() * 8).saturating_sub(offset)
                    / std::mem::size_of::<STORAGE_DEPENDENCY_INFO_TYPE_2>();
                return std::slice::from_raw_parts(first, n.min(room))
                    .iter()
                    .filter_map(|e| {
                        if e.HostVolumeName.is_null() {
                            return None;
                        }
                        let host = e.HostVolumeName.to_string().ok()?;
                        (!host.is_empty()).then(|| PathBuf::from(host))
                    })
                    .collect();
            }
        }
        Vec::new()
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::VolumeId;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "restoring is not available on this platform",
        )
    }

    pub fn open_dir(_: &Path) -> io::Result<File> {
        Err(unsupported())
    }

    pub fn create_new(_: &File, _: &str) -> io::Result<File> {
        Err(unsupported())
    }

    pub fn remove(_: &File, _: &str, _: File) {}

    pub fn backed_on(_: &File, _: VolumeId) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_device_numbers_and_backing_files() {
        // glibc's makedev.
        let makedev = |major: u64, minor: u64| {
            ((major & 0xFFF) << 8)
                | ((major & !0xFFF) << 32)
                | (minor & 0xFF)
                | ((minor & !0xFF) << 12)
        };
        for (major, minor) in [(7, 3), (259, 70_000), (4100, 1 << 20)] {
            assert_eq!(dev_parts(makedev(major, minor)), (major, minor));
        }
        assert_eq!(
            backing_path("/srv/images/vol.img\n"),
            Some(PathBuf::from("/srv/images/vol.img"))
        );
        assert_eq!(
            backing_path("/tmp/x.img (deleted)\n"),
            Some(PathBuf::from("/tmp/x.img"))
        );
        assert_eq!(backing_path("\n"), None);
    }

    #[test]
    fn create_free_picks_a_new_name_and_remove_takes_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let d = DestDir::open(dir.path(), VolumeId(u64::MAX)).unwrap();
        let (name, path, file) = d.create_free("a.txt").unwrap();
        assert_eq!((name.as_str(), path.clone()), ("a.txt", dir.path().join("a.txt")));
        drop(file);
        let (name2, path2, file2) = d.create_free("a.txt").unwrap();
        assert_eq!(name2, "a (2).txt");
        assert!(path2.exists());
        d.remove(&name2, file2);
        assert!(!path2.exists());
        assert!(path.exists());
        // The folder's own volume is refused.
        let own = crate::recover::volume_of(dir.path()).unwrap();
        assert!(matches!(
            DestDir::open(dir.path(), own),
            Err(RecoverError::SameVolume)
        ));
    }
}
