//! Local directory listing with stable file identity: volume serial + file id on Windows
//! (one `FileIdExtdDirectoryInfo` enumeration per directory, no per-file opens), dev + inode
//! elsewhere. `fs_id` is None where the filesystem has no stable ids (FAT, some shares).
//! Times are unix nanoseconds; `ctime` is the change time (inode change on unix, ChangeTime
//! on Windows), which any write moves even when the mtime is put back.

use std::{io, path::Path};

pub(crate) const FILE: i64 = 0;
pub(crate) const DIR: i64 = 1;
/// A link whose target cannot be read.
pub(crate) const DANGLING: i64 = 2;

#[derive(Clone, Debug, Default)]
pub(crate) struct Item {
    pub name: String,
    pub kind: i64,
    pub size: i64,
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
    pub hidden: bool,
    pub link: bool,
    pub fs_id: Option<String>,
    /// Kept with the record when its metadata could not be read.
    pub error: Option<String>,
    /// The content id a device source's host sent with its listing.
    pub cas: Option<[u8; 32]>,
}

/// Unix nanoseconds (negative before 1970; saturating outside 1678..2262).
pub fn unix_ns(t: std::time::SystemTime) -> i64 {
    let ns = |d: std::time::Duration| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => ns(d),
        Err(e) => -ns(e.duration()),
    }
}

fn nanos(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    t.ok().map(unix_ns)
}

/// From `std` metadata (not followed); used where the native enumeration is unavailable.
fn from_metadata(name: String, path: &Path, md: io::Result<std::fs::Metadata>) -> Item {
    let md = match md {
        Ok(md) => md,
        Err(e) => {
            return Item {
                name,
                error: Some(e.to_string()),
                ..Item::default()
            }
        }
    };
    let link = md.file_type().is_symlink();
    let kind = match (link, md.is_dir()) {
        (true, _) => match std::fs::metadata(path) {
            Ok(target) if target.is_dir() => DIR,
            Ok(_) => FILE,
            Err(_) => DANGLING,
        },
        (false, true) => DIR,
        (false, false) => FILE,
    };
    #[cfg(windows)]
    let hidden = {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & 0x2 != 0
    };
    #[cfg(not(windows))]
    let hidden = name.starts_with('.');
    #[cfg(unix)]
    let (fs_id, ctime) = {
        use std::os::unix::fs::MetadataExt;
        (
            Some(format!("{:x}:{:x}", md.dev(), md.ino())),
            md.ctime()
                .checked_mul(1_000_000_000)
                .and_then(|s| s.checked_add(md.ctime_nsec())),
        )
    };
    // Std has no change time on Windows: `win::stat` reads it from the handle.
    #[cfg(not(unix))]
    let (fs_id, ctime) = (None, None);
    Item {
        size: if kind == FILE { md.len() as i64 } else { 0 },
        mtime: nanos(md.modified()),
        ctime,
        hidden,
        link,
        kind,
        fs_id,
        name,
        error: None,
        cas: None,
    }
}

fn std_list(dir: &Path) -> io::Result<Vec<Item>> {
    std::fs::read_dir(dir)?
        .map(|e| {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            Ok(from_metadata(name, &e.path(), e.metadata()))
        })
        .collect()
}

#[cfg(unix)]
pub(crate) fn list(dir: &Path) -> io::Result<Vec<Item>> {
    std_list(dir)
}

#[cfg(unix)]
pub(crate) fn stat(path: &Path) -> io::Result<Item> {
    let md = std::fs::symlink_metadata(path)?;
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Ok(from_metadata(name, path, Ok(md)))
}

/// `stat` through a link: a source root that is a junction or symlink stands for its
/// target (identity, times; not flagged as a link).
#[cfg(unix)]
pub(crate) fn stat_root(path: &Path) -> io::Result<Item> {
    let md = std::fs::metadata(path)?;
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Ok(from_metadata(name, path, Ok(md)))
}

/// The identities a source root may have been stored under: its target's, and (for a root
/// that is itself a link, as indexed before 0.7.0) the link's own.
pub(crate) fn root_ids(path: &Path) -> Vec<String> {
    let mut ids: Vec<String> = [stat_root(path), stat(path)]
        .into_iter()
        .filter_map(|i| i.ok()?.fs_id)
        .collect();
    ids.dedup();
    ids
}

#[cfg(windows)]
pub(crate) use win::{list, stat, stat_root};

#[cfg(windows)]
mod win {
    use super::{Item, DIR, FILE};
    use std::{io, os::windows::ffi::OsStrExt, path::Path};
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileBasicInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo,
        FileIdInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    const ATTR_HIDDEN: u32 = 0x2;
    const ATTR_DIRECTORY: u32 = 0x10;
    const ATTR_REPARSE: u32 = 0x400;
    /// Symlinks and junctions are name surrogates; cloud placeholders (OneDrive) are not.
    const TAG_NAME_SURROGATE: u32 = 0x2000_0000;

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Opens `path` itself, or (`follow`) what a link at `path` leads to.
    fn open(path: &Path, access: u32, follow: bool) -> io::Result<Handle> {
        let long = keel_vfs::long(path).map_err(io::Error::other)?;
        let wide: Vec<u16> = long.as_os_str().encode_wide().chain([0]).collect();
        let flags = if follow {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
        };
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                flags,
                HANDLE::default(),
            )
        }?;
        Ok(Handle(handle))
    }

    fn file_id(h: &Handle) -> io::Result<FILE_ID_INFO> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: `info` is a FILE_ID_INFO of the size passed.
        unsafe {
            GetFileInformationByHandleEx(
                h.0,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        }?;
        Ok(info)
    }

    fn fs_id(serial: u64, id: [u8; 16]) -> String {
        let hex: String = id.iter().rev().map(|b| format!("{b:02x}")).collect();
        format!("{serial:x}:{}", hex.trim_start_matches('0'))
    }

    fn basic(h: &Handle) -> io::Result<FILE_BASIC_INFO> {
        let mut info = FILE_BASIC_INFO::default();
        // SAFETY: `info` is a FILE_BASIC_INFO of the size passed.
        unsafe {
            GetFileInformationByHandleEx(
                h.0,
                FileBasicInfo,
                (&mut info as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        }?;
        Ok(info)
    }

    /// FILETIME (100 ns since 1601) to unix nanoseconds; None for 0 (unknown).
    fn nanos(t: i64) -> Option<i64> {
        (t != 0).then(|| (t - 116_444_736_000_000_000).saturating_mul(100))
    }

    /// Lists `dir`; a junction or symlink is followed (the walk never lists link entries,
    /// only a source root that is one).
    pub(crate) fn list(dir: &Path) -> io::Result<Vec<Item>> {
        let handle = open(dir, FILE_LIST_DIRECTORY.0, true)?;
        let Ok(volume) = file_id(&handle) else {
            return super::std_list(dir);
        };
        // 64 KiB, 8-byte aligned as the records require.
        let mut buf = vec![0u64; 8192];
        let mut out = Vec::new();
        let mut class = FileIdExtdDirectoryRestartInfo;
        loop {
            // SAFETY: `buf` is writable for the byte length passed.
            let r = unsafe {
                GetFileInformationByHandleEx(
                    handle.0,
                    class,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                )
            };
            if let Err(e) = r {
                if e.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                    return Ok(out);
                }
                // Filesystems without 128-bit ids (FAT, some redirectors) refuse the class.
                if class == FileIdExtdDirectoryRestartInfo {
                    return super::std_list(dir);
                }
                return Err(e.into());
            }
            class = FileIdExtdDirectoryInfo;
            let base = buf.as_ptr().cast::<u8>();
            let name_at = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
            let mut off = 0usize;
            loop {
                // SAFETY: the system wrote whole records into `buf`, each starting at an
                // 8-aligned offset given by the previous NextEntryOffset; the name follows
                // the fixed part and has FileNameLength bytes.
                let (info, name) = unsafe {
                    let info =
                        std::ptr::read_unaligned(base.add(off).cast::<FILE_ID_EXTD_DIR_INFO>());
                    let name = std::slice::from_raw_parts(
                        base.add(off + name_at).cast::<u16>(),
                        info.FileNameLength as usize / 2,
                    );
                    (info, String::from_utf16_lossy(name))
                };
                if name != "." && name != ".." {
                    let attrs = info.FileAttributes;
                    let link =
                        attrs & ATTR_REPARSE != 0 && info.ReparsePointTag & TAG_NAME_SURROGATE != 0;
                    let kind = if attrs & ATTR_DIRECTORY != 0 {
                        DIR
                    } else {
                        FILE
                    };
                    out.push(Item {
                        size: if kind == FILE { info.EndOfFile } else { 0 },
                        mtime: nanos(info.LastWriteTime),
                        ctime: nanos(info.ChangeTime),
                        hidden: attrs & ATTR_HIDDEN != 0,
                        link,
                        kind,
                        fs_id: Some(fs_id(volume.VolumeSerialNumber, info.FileId.Identifier)),
                        name,
                        error: None,
                        cas: None,
                    });
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                off += info.NextEntryOffset as usize;
            }
        }
    }

    pub(crate) fn stat(path: &Path) -> io::Result<Item> {
        stat_as(path, false)
    }

    /// `stat` through a link (a source root that is a junction or symlink).
    pub(crate) fn stat_root(path: &Path) -> io::Result<Item> {
        stat_as(path, true)
    }

    fn stat_as(path: &Path, follow: bool) -> io::Result<Item> {
        let long = keel_vfs::long(path).map_err(io::Error::other)?;
        let md = if follow {
            std::fs::metadata(long)?
        } else {
            std::fs::symlink_metadata(long)?
        };
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mut item = super::from_metadata(name, path, Ok(md));
        if let Ok(h) = open(path, FILE_READ_ATTRIBUTES.0, follow) {
            item.fs_id = file_id(&h)
                .ok()
                .map(|id| fs_id(id.VolumeSerialNumber, id.FileId.Identifier));
            // The times `list` reads, at the same precision.
            if let Ok(b) = basic(&h) {
                item.mtime = nanos(b.LastWriteTime).or(item.mtime);
                item.ctime = nanos(b.ChangeTime);
            }
        }
        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_and_stat_agree_on_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut items = list(dir.path()).unwrap();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(items.len(), 2);
        assert_eq!(
            (items[0].name.as_str(), items[0].kind, items[0].size),
            ("a.txt", FILE, 5)
        );
        assert_eq!((items[1].name.as_str(), items[1].kind), ("sub", DIR));
        assert!(items[0].mtime.unwrap() > 1_600_000_000_000_000_000);
        assert!(items[0].ctime.is_some(), "change time");
        let a = stat(&dir.path().join("a.txt")).unwrap();
        assert_eq!((a.mtime, a.ctime), (items[0].mtime, items[0].ctime));
        let id = items[0].fs_id.clone().expect("temp dirs have file ids");
        assert_eq!(
            stat(&dir.path().join("a.txt")).unwrap().fs_id,
            Some(id.clone())
        );
        // A rename keeps the id; a new file gets another.
        std::fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
        assert_eq!(
            stat(&dir.path().join("b.txt")).unwrap().fs_id,
            Some(id.clone())
        );
        std::fs::write(dir.path().join("a.txt"), b"new").unwrap();
        assert_ne!(stat(&dir.path().join("a.txt")).unwrap().fs_id, Some(id));
    }
}
