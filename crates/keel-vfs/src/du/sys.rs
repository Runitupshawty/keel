//! What a scan reads from the filesystem: one directory listing per folder (Windows: the
//! `FileIdExtdDirectoryInfo` enumeration, which carries sizes, allocation, attributes,
//! reparse tags and file ids without opening any file), the target of a followed link,
//! cluster sizes and drive space.

use super::{attr, LinkKind, UNKNOWN_TIME};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One directory entry, as listed (links not followed).
#[derive(Clone, Debug)]
pub(super) struct Raw {
    pub name: OsString,
    pub dir: bool,
    /// [`attr`] bits.
    pub attrs: u32,
    /// A symlink, junction or mount point (not a cloud placeholder's reparse point).
    pub link: Option<LinkKind>,
    pub size: u64,
    /// On-disk bytes as listed (Windows: the allocation size; Unix: blocks × 512).
    pub alloc: Option<u64>,
    pub mtime: i64,
    /// Volume + file id.
    pub id: Option<u128>,
    /// Hard links (0: not known, as on Windows listings).
    pub nlink: u64,
}

impl Raw {
    pub(super) fn cloud(&self) -> bool {
        self.attrs & attr::CLOUD != 0
    }
}

pub(super) struct Listing {
    /// The folder's own identity (volume + file id).
    pub id: Option<u128>,
    pub entries: Vec<Raw>,
}

/// What a path leads to (links followed).
#[derive(Clone, Debug)]
pub(super) struct Target {
    pub dir: bool,
    pub attrs: u32,
    pub size: u64,
    pub alloc: Option<u64>,
    pub mtime: i64,
    pub id: Option<u128>,
}

/// On-disk bytes of a file entry (`path` is called only when the file must be asked):
/// nothing for a cloud placeholder (never opened);
/// Windows compressed and sparse files by `GetCompressedFileSizeW`, rounded up to the
/// cluster; else the listed allocation, or the length rounded up to the cluster.
pub(super) fn allocated(
    attrs: u32,
    size: u64,
    alloc: Option<u64>,
    path: impl FnOnce() -> PathBuf,
    cluster: u64,
) -> u64 {
    if attrs & attr::CLOUD != 0 {
        return 0;
    }
    #[cfg(windows)]
    if attrs & (attr::COMPRESSED | attr::SPARSE) != 0 {
        if let Some(c) = imp::compressed_size(&path()) {
            return round_up(c, cluster);
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    match alloc {
        // Unix counts blocks, so no block at all is a (sparse) file with nothing on disk;
        // a Windows listing reporting 0 for a file with data did not say.
        Some(a) if a > 0 || size == 0 || cfg!(unix) => a,
        _ => round_up(size, cluster),
    }
}

pub(super) fn round_up(n: u64, cluster: u64) -> u64 {
    if cluster <= 1 {
        return n;
    }
    n.div_ceil(cluster).saturating_mul(cluster)
}

pub(super) use imp::{cluster_size, is_drive_root, list, local_drives, target};

/// Total and free bytes of the volume holding `path`.
pub(super) fn space(path: &Path) -> Option<super::Space> {
    Some(super::Space {
        total: fs4::total_space(path).ok()?,
        free: fs4::free_space(path).ok()?,
    })
}

/// Identity from a volume or device number and a file id or inode.
fn key(volume: u64, id: u128) -> u128 {
    (u128::from(volume) << 64) ^ id
}

#[cfg(windows)]
mod imp {
    use super::{key, Listing, Raw, Target, UNKNOWN_TIME};
    use crate::du::{attr, LinkKind};
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::{io, mem};
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_NO_MORE_FILES, HANDLE, NO_ERROR,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdExtdDirectoryInfo,
        FileIdExtdDirectoryRestartInfo, FileIdInfo, GetCompressedFileSizeW, GetDiskFreeSpaceW,
        GetDriveTypeW, GetFileInformationByHandleEx, GetLogicalDrives,
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FULL_DIR_INFO, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;

    const TAG_NAME_SURROGATE: u32 = 0x2000_0000;
    const TAG_MOUNT_POINT: u32 = 0xA000_0003;

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain([0]).collect()
    }

    /// Opens `path` (following links) for `access`.
    fn open(path: &Path, access: u32) -> io::Result<Handle> {
        let long = crate::local::long(path).map_err(io::Error::other)?;
        let w = wide(&long);
        // SAFETY: `w` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                HANDLE::default(),
            )
        }?;
        Ok(Handle(handle))
    }

    fn file_id(h: &Handle) -> Option<u128> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: `info` is a FILE_ID_INFO of the size passed.
        unsafe {
            GetFileInformationByHandleEx(
                h.0,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                mem::size_of::<FILE_ID_INFO>() as u32,
            )
        }
        .ok()?;
        Some(key(
            info.VolumeSerialNumber,
            u128::from_le_bytes(info.FileId.Identifier),
        ))
    }

    /// FILETIME (100 ns since 1601) to unix nanoseconds.
    fn nanos(t: i64) -> i64 {
        if t == 0 {
            UNKNOWN_TIME
        } else {
            (t - 116_444_736_000_000_000).saturating_mul(100)
        }
    }

    /// Calls `each` with every record of the enumeration `restart`/`next` of `h`.
    /// Ok(false): the filesystem does not have this information class.
    fn enumerate(
        h: &Handle,
        restart: FILE_INFO_BY_HANDLE_CLASS,
        next: FILE_INFO_BY_HANDLE_CLASS,
        each: &mut dyn FnMut(*const u8),
    ) -> io::Result<bool> {
        // 64 KiB, 8-byte aligned as the records require.
        let mut buf = vec![0u64; 8192];
        let mut class = restart;
        loop {
            // SAFETY: `buf` is writable for the byte length passed.
            let r = unsafe {
                GetFileInformationByHandleEx(
                    h.0,
                    class,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                )
            };
            if let Err(e) = r {
                if e.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                    return Ok(true);
                }
                if class == restart {
                    return Ok(false);
                }
                return Err(e.into());
            }
            class = next;
            let base = buf.as_ptr().cast::<u8>();
            let mut off = 0usize;
            loop {
                // SAFETY: the system wrote whole records into `buf`, each at the 8-aligned
                // offset the previous NextEntryOffset gives; NextEntryOffset is the first
                // field of both record types.
                let next_off = unsafe { std::ptr::read_unaligned(base.add(off).cast::<u32>()) };
                // SAFETY: as above, `off` starts a record inside `buf`.
                each(unsafe { base.add(off) });
                if next_off == 0 {
                    break;
                }
                off += next_off as usize;
                if off >= buf.len() * 8 {
                    break;
                }
            }
        }
    }

    /// Reads a record's name: `len` bytes of UTF-16 at `at`.
    ///
    /// SAFETY: `at` points at `len` bytes the system wrote.
    unsafe fn name_at(at: *const u8, len: u32) -> Vec<u16> {
        let n = len as usize / 2;
        let mut v = vec![0u16; n];
        std::ptr::copy_nonoverlapping(at, v.as_mut_ptr().cast::<u8>(), n * 2);
        v
    }

    #[allow(clippy::too_many_arguments)]
    fn raw(
        dir: &Path,
        name: Vec<u16>,
        attrs: u32,
        tag: u32,
        eof: i64,
        alloc: i64,
        write: i64,
        id: Option<u128>,
    ) -> Option<Raw> {
        if name == [46] || name == [46, 46] {
            return None;
        }
        let name = OsString::from_wide(&name);
        let is_dir = attrs & attr::DIRECTORY != 0;
        let link = (attrs & attr::REPARSE_POINT != 0 && tag & TAG_NAME_SURROGATE != 0).then(|| {
            if tag == TAG_MOUNT_POINT {
                if is_volume_mount(&dir.join(&name)) {
                    LinkKind::MountPoint
                } else {
                    LinkKind::Junction
                }
            } else {
                LinkKind::Symlink
            }
        });
        Some(Raw {
            name,
            dir: is_dir,
            attrs,
            link,
            size: if is_dir { 0 } else { eof.max(0) as u64 },
            alloc: Some(alloc.max(0) as u64),
            mtime: nanos(write),
            id,
            nlink: 0,
        })
    }

    pub(crate) fn list(dir: &Path) -> io::Result<Listing> {
        let h = open(dir, FILE_LIST_DIRECTORY.0)?;
        super::super::priority::hint_handle(h.0);
        let id = file_id(&h);
        let volume = id.map(|i| (i >> 64) as u64);
        let mut entries = Vec::new();
        let full = match volume {
            None => true,
            Some(serial) => {
                let name_at_off = mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
                let ok = enumerate(
                    &h,
                    FileIdExtdDirectoryRestartInfo,
                    FileIdExtdDirectoryInfo,
                    &mut |p| {
                        // SAFETY: `p` starts a FILE_ID_EXTD_DIR_INFO record whose name has
                        // FileNameLength bytes after the fixed part.
                        let (info, name) = unsafe {
                            let info = std::ptr::read_unaligned(p.cast::<FILE_ID_EXTD_DIR_INFO>());
                            (info, name_at(p.add(name_at_off), info.FileNameLength))
                        };
                        let fid = u128::from_le_bytes(info.FileId.Identifier);
                        entries.extend(raw(
                            dir,
                            name,
                            info.FileAttributes,
                            info.ReparsePointTag,
                            info.EndOfFile,
                            info.AllocationSize,
                            info.LastWriteTime,
                            Some(key(serial, fid)),
                        ));
                    },
                )?;
                !ok
            }
        };
        if full {
            // FAT and some redirectors: no 128-bit ids (and no hard links).
            entries.clear();
            let name_at_off = mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
            let ok = enumerate(
                &h,
                FileFullDirectoryRestartInfo,
                FileFullDirectoryInfo,
                &mut |p| {
                    // SAFETY: `p` starts a FILE_FULL_DIR_INFO record whose name has
                    // FileNameLength bytes after the fixed part.
                    let (info, name) = unsafe {
                        let info = std::ptr::read_unaligned(p.cast::<FILE_FULL_DIR_INFO>());
                        (info, name_at(p.add(name_at_off), info.FileNameLength))
                    };
                    // EaSize holds the reparse tag of a reparse point.
                    entries.extend(raw(
                        dir,
                        name,
                        info.FileAttributes,
                        info.EaSize,
                        info.EndOfFile,
                        info.AllocationSize,
                        info.LastWriteTime,
                        None,
                    ));
                },
            )?;
            if !ok {
                return Err(io::Error::other("the folder cannot be enumerated"));
            }
        }
        Ok(Listing { id, entries })
    }

    pub(crate) fn target(path: &Path) -> io::Result<Target> {
        let long = crate::local::long(path).map_err(io::Error::other)?;
        let md = std::fs::metadata(&long)?;
        let attrs = md.file_attributes();
        let dir = md.is_dir();
        let id = open(path, FILE_READ_ATTRIBUTES.0)
            .ok()
            .and_then(|h| file_id(&h));
        let alloc = (!dir && attrs & attr::CLOUD == 0)
            .then(|| compressed_size(path))
            .flatten();
        Ok(Target {
            dir,
            attrs,
            size: if dir { 0 } else { md.len() },
            alloc,
            mtime: nanos(md.last_write_time() as i64),
            id,
        })
    }

    pub(crate) fn compressed_size(path: &Path) -> Option<u64> {
        let long = crate::local::long(path).ok()?;
        let w = wide(&long);
        let mut high = 0u32;
        // SAFETY: `w` is NUL-terminated; `high` is a u32 the call writes.
        let low = unsafe {
            SetLastError(NO_ERROR);
            GetCompressedFileSizeW(PCWSTR(w.as_ptr()), Some(&mut high))
        };
        // SAFETY: reads this thread's last error.
        if low == u32::MAX && unsafe { GetLastError() } != NO_ERROR {
            return None;
        }
        Some((u64::from(high) << 32) | u64::from(low))
    }

    /// The mount point or drive root `path` is on (`C:\`, `C:\mnt\vol\`, `\\server\share\`).
    fn volume_root(path: &Path) -> Option<Vec<u16>> {
        let w = wide(path);
        let mut root = vec![0u16; 1024];
        // SAFETY: `w` is NUL-terminated; `root` is writable for its length.
        unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut root) }.ok()?;
        let n = root.iter().position(|&c| c == 0).unwrap_or(root.len());
        root.truncate(n);
        Some(root)
    }

    /// Bytes per cluster of the volume holding `path` (4096 when it cannot be told).
    pub(crate) fn cluster_size(path: &Path) -> u64 {
        let Some(mut root) = volume_root(path) else {
            return 4096;
        };
        root.push(0);
        let (mut spc, mut bps) = (0u32, 0u32);
        // SAFETY: `root` is NUL-terminated; the outputs are u32s the call writes.
        let ok = unsafe {
            GetDiskFreeSpaceW(
                PCWSTR(root.as_ptr()),
                Some(&mut spc),
                Some(&mut bps),
                None,
                None,
            )
        };
        match ok {
            Ok(()) if spc > 0 && bps > 0 => u64::from(spc) * u64::from(bps),
            _ => 4096,
        }
    }

    /// `C:\`, a share's root or a folder a volume is mounted on.
    pub(crate) fn is_drive_root(path: &Path) -> bool {
        let Some(root) = volume_root(path) else {
            return false;
        };
        let norm = |s: String| s.trim_end_matches('\\').to_lowercase();
        let path = path.to_string_lossy().replace('/', "\\");
        norm(String::from_utf16_lossy(&root)) == norm(path)
    }

    /// A folder that a volume is mounted on (a junction is not).
    fn is_volume_mount(path: &Path) -> bool {
        let mut s = path.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC\\") {
                s = rest.to_owned();
            }
        }
        if !s.ends_with('\\') {
            s.push('\\');
        }
        let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
        let mut name = [0u16; 64];
        // SAFETY: `w` is NUL-terminated; `name` is writable for its length.
        unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(w.as_ptr()), &mut name) }.is_ok()
    }

    pub(crate) fn local_drives() -> Vec<PathBuf> {
        // SAFETY: no arguments.
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| PathBuf::from(format!("{}:\\", char::from(b'A' + bit))))
            .filter(|root| {
                let w = wide(root);
                // SAFETY: `w` is NUL-terminated and outlives the call.
                unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) == DRIVE_FIXED }
            })
            .collect()
    }
}

#[cfg(unix)]
mod imp {
    use super::{key, Listing, Raw, Target, UNKNOWN_TIME};
    use crate::du::{attr, LinkKind};
    use std::fs;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    /// macOS: a File Provider item whose data (or a folder whose listing) is not local.
    #[cfg(target_os = "macos")]
    const SF_DATALESS: u32 = 0x4000_0000;

    fn mtime(md: &fs::Metadata) -> i64 {
        md.mtime()
            .checked_mul(1_000_000_000)
            .and_then(|s| s.checked_add(md.mtime_nsec()))
            .unwrap_or(UNKNOWN_TIME)
    }

    fn attrs(name: &[u8], md: &fs::Metadata, symlink: bool) -> u32 {
        let mut a = 0;
        if md.mode() & 0o222 == 0 {
            a |= attr::READONLY;
        }
        if name.first() == Some(&b'.') {
            a |= attr::HIDDEN;
        }
        if md.is_dir() {
            a |= attr::DIRECTORY;
        }
        if symlink {
            a |= attr::REPARSE_POINT;
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt as _;
            let f = md.st_flags();
            if f & libc::UF_HIDDEN != 0 {
                a |= attr::HIDDEN;
            }
            if f & SF_DATALESS != 0 {
                a |= attr::OFFLINE;
            }
        }
        a
    }

    pub(crate) fn list(dir: &Path) -> io::Result<Listing> {
        use std::os::unix::ffi::OsStrExt;
        let here = fs::metadata(dir)?;
        let dev = here.dev();
        let mut entries = Vec::new();
        for e in fs::read_dir(dir)? {
            let Ok(e) = e else { continue };
            let name = e.file_name();
            // Not followed: a symlink's own metadata.
            let Ok(md) = e.metadata() else { continue };
            let symlink = md.file_type().is_symlink();
            let is_dir = md.is_dir();
            let link = if symlink {
                Some(LinkKind::Symlink)
            } else if is_dir && md.dev() != dev {
                Some(LinkKind::MountPoint)
            } else {
                None
            };
            entries.push(Raw {
                attrs: attrs(name.as_bytes(), &md, symlink),
                dir: is_dir,
                link,
                size: if is_dir { 0 } else { md.len() },
                alloc: Some(md.blocks().saturating_mul(512)),
                mtime: mtime(&md),
                id: Some(key(md.dev(), u128::from(md.ino()))),
                nlink: md.nlink(),
                name,
            });
        }
        Ok(Listing {
            id: Some(key(dev, u128::from(here.ino()))),
            entries,
        })
    }

    pub(crate) fn target(path: &Path) -> io::Result<Target> {
        use std::os::unix::ffi::OsStrExt;
        let md = fs::metadata(path)?;
        let name = path.file_name().map_or(&[][..], |n| n.as_bytes());
        Ok(Target {
            dir: md.is_dir(),
            attrs: attrs(name, &md, false),
            size: if md.is_dir() { 0 } else { md.len() },
            alloc: Some(md.blocks().saturating_mul(512)),
            mtime: mtime(&md),
            id: Some(key(md.dev(), u128::from(md.ino()))),
        })
    }

    /// Unix sizes on disk come from st_blocks; the cluster only rounds missing ones.
    pub(crate) fn cluster_size(_path: &Path) -> u64 {
        1
    }

    /// `/` or a mount point.
    pub(crate) fn is_drive_root(path: &Path) -> bool {
        let Ok(here) = fs::metadata(path) else {
            return false;
        };
        match fs::metadata(path.join("..")) {
            Ok(up) => up.dev() != here.dev() || up.ino() == here.ino(),
            Err(_) => false,
        }
    }

    pub(crate) fn local_drives() -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = crate::local::drives()
            .into_iter()
            .map(|(mount, ..)| PathBuf::from(mount))
            .filter(|p| crate::local::is_fixed_disk(p))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}
