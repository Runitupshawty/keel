//! RAR through libunrar's C API (`unrar_sys`). The safe `unrar` wrapper hides each entry's
//! redirect type and can only extract with the extraction root cleared, which made
//! libunrar resolve WinRAR "file reference" and hard-link entries against Keel's working
//! folder and skip its symlink checks. Here every redirect entry (symlink, junction, hard
//! link, file reference) is left out of listings and never extracted, and a body is
//! extracted with its extraction root set to a private temp folder, then accepted only as
//! a plain file with a single link.
use super::{ArchiveEntry, ArchiveReader};
use anyhow::{bail, Result};
use std::{
    fs::File,
    io::Read,
    os::raw::{c_char, c_int, c_uint},
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    time::{Duration, UNIX_EPOCH},
};
use unrar_sys as ffi;

const RHDF_ENCRYPTED: c_uint = 0x04;
const RHDF_DIRECTORY: c_uint = 0x20;

pub(super) struct Reader {
    path: PathBuf,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        Archive::open(path, ffi::RAR_OM_LIST)?;
        Ok(Self { path: path.into() })
    }
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        let mut archive = Archive::open(&self.path, ffi::RAR_OM_LIST)?;
        let mut out = Vec::new();
        while let Some(header) = archive.next()? {
            archive.process(ffi::RAR_SKIP, None)?;
            if !header.redirect {
                out.push(header.entry);
            }
        }
        Ok(out)
    }
    fn local(&self) -> &Path {
        &self.path
    }
    fn each_file(
        &mut self,
        want: &dyn Fn(&str) -> bool,
        each: &mut dyn FnMut(&str, u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let mut archive = Archive::open(&self.path, ffi::RAR_OM_EXTRACT)?;
        while let Some(header) = archive.next()? {
            let entry = &header.entry;
            if entry.is_dir || header.redirect || !want(&entry.inner) {
                archive.process(ffi::RAR_SKIP, None)?;
                continue;
            }
            let temp = tempfile::tempdir()?;
            let file = temp.path().join("entry");
            archive.process(ffi::RAR_EXTRACT, Some((temp.path(), &file)))?;
            // Checked before opening, so a link or special file is never followed.
            let plain = std::fs::symlink_metadata(&file)?.is_file();
            anyhow::ensure!(plain, "rar entry is not a plain file: {}", entry.inner);
            let mut body = File::open(&file)?;
            anyhow::ensure!(
                links(&body)? == 1,
                "rar entry is a hard link: {}",
                entry.inner
            );
            each(&entry.inner, entry.size, &mut body)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn links(file: &File) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink())
}
#[cfg(windows)]
fn links(file: &File) -> std::io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{Foundation::HANDLE, Storage::FileSystem};
    let mut info = FileSystem::BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open for the duration of the call; `info` is a valid out-param.
    unsafe { FileSystem::GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
    Ok(info.nNumberOfLinks.into())
}

/// libunrar's `RARHeaderDataEx`. `unrar_sys` declares it `repr(C)`, but dll.hpp wraps it in
/// `#pragma pack(1)`, so every field after `FileAttr` (redirect type and times included) sits
/// 4 bytes off there. Read into this instead.
#[repr(C, packed)]
struct HeaderData {
    arc_name: [c_char; 1024],
    arc_name_w: [ffi::WCHAR; 1024],
    file_name: [c_char; 1024],
    file_name_w: [ffi::WCHAR; 1024],
    flags: c_uint,
    pack_size: c_uint,
    pack_size_high: c_uint,
    unp_size: c_uint,
    unp_size_high: c_uint,
    host_os: c_uint,
    file_crc: c_uint,
    file_time: c_uint,
    unp_ver: c_uint,
    method: c_uint,
    file_attr: c_uint,
    cmt_buf: *mut c_char,
    cmt_buf_size: c_uint,
    cmt_size: c_uint,
    cmt_state: c_uint,
    dict_size: c_uint,
    hash_type: c_uint,
    hash: [c_char; 32],
    redir_type: c_uint,
    redir_name: *mut ffi::WCHAR,
    redir_name_size: c_uint,
    dir_target: c_uint,
    mtime_low: c_uint,
    mtime_high: c_uint,
    ctime_low: c_uint,
    ctime_high: c_uint,
    atime_low: c_uint,
    atime_high: c_uint,
    arc_name_ex: *mut ffi::WCHAR,
    arc_name_ex_size: c_uint,
    file_name_ex: *mut ffi::WCHAR,
    file_name_ex_size: c_uint,
    reserved: [c_uint; 982],
}

struct Header {
    entry: ArchiveEntry,
    /// Symlink, junction, hard link or file reference.
    redirect: bool,
}

/// An open libunrar handle, closed on drop.
struct Archive(NonNull<ffi::Handle>);
impl Drop for Archive {
    fn drop(&mut self) {
        // SAFETY: the handle came from RAROpenArchiveEx and is closed exactly once.
        unsafe { ffi::RARCloseArchive(self.0.as_ptr()) };
    }
}

// libunrar takes narrow paths on Linux (its wide-to-locale conversion breaks there; the
// `unrar` crate does the same) and wide paths elsewhere.
#[cfg(any(target_os = "linux", target_os = "netbsd"))]
fn native(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(path.as_os_str().as_bytes())?)
}
#[cfg(windows)]
fn native(path: &Path) -> Result<Vec<ffi::WCHAR>> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<ffi::WCHAR> = path.as_os_str().encode_wide().chain([0]).collect();
    anyhow::ensure!(!wide[..wide.len() - 1].contains(&0), "NUL in path");
    Ok(wide)
}
#[cfg(not(any(windows, target_os = "linux", target_os = "netbsd")))]
fn native(path: &Path) -> Result<Vec<ffi::WCHAR>> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?;
    anyhow::ensure!(!text.contains('\0'), "NUL in path");
    Ok(text
        .chars()
        .map(|c| c as u32 as ffi::WCHAR)
        .chain([0])
        .collect())
}

#[cfg(windows)]
fn text(wide: &[ffi::WCHAR]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}
#[cfg(not(windows))]
fn text(wide: &[ffi::WCHAR]) -> String {
    wide.iter()
        .take_while(|&&c| c != 0)
        .map(|&c| char::from_u32(c as u32).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

impl Archive {
    fn open(path: &Path, mode: c_uint) -> Result<Self> {
        let name = native(path)?;
        let mut data = ffi::OpenArchiveDataEx::new(name.as_ptr(), mode);
        // SAFETY: `data` and the name it points to outlive the call; libunrar writes only
        // `open_result` and `flags` back.
        let handle = unsafe { ffi::RAROpenArchiveEx(ptr::addr_of_mut!(data)) };
        let handle = NonNull::new(handle.cast_mut()).map(Self);
        match (handle, data.open_result as c_int) {
            (Some(archive), ffi::ERAR_SUCCESS) => Ok(archive),
            (_, code) => bail!(
                "cannot open rar archive {} (unrar error {code})",
                path.display()
            ),
        }
    }
    /// The next file header, or `None` at the end of the archive.
    fn next(&mut self) -> Result<Option<Header>> {
        // SAFETY: all-zero is a valid `HeaderData` (integers and null pointers), as the
        // libunrar docs require for the reserved area.
        let mut raw: Box<HeaderData> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: the handle is open and `raw` is a writable struct of libunrar's layout.
        let code = unsafe {
            ffi::RARReadHeaderEx(
                self.0.as_ptr(),
                ptr::addr_of_mut!(*raw).cast::<ffi::HeaderDataEx>(),
            )
        };
        match code {
            ffi::ERAR_SUCCESS => {}
            ffi::ERAR_END_ARCHIVE => return Ok(None),
            code => bail!("corrupt rar archive (unrar error {code})"),
        }
        // Packed fields are copied out, never borrowed.
        let (name, flags, redirect) = (raw.file_name_w, raw.flags, raw.redir_type);
        // Windows FILETIME: 100 ns ticks since 1601.
        let ticks = (u64::from(raw.mtime_high) << 32) | u64::from(raw.mtime_low);
        let modified = ticks
            .checked_sub(116_444_736_000_000_000)
            .and_then(|t| UNIX_EPOCH.checked_add(Duration::from_nanos(t.saturating_mul(100))))
            .filter(|_| ticks != 0);
        Ok(Some(Header {
            entry: ArchiveEntry {
                inner: text(&name).replace('\\', "/"),
                is_dir: flags & RHDF_DIRECTORY != 0,
                size: (u64::from(raw.unp_size_high) << 32) | u64::from(raw.unp_size),
                modified,
                encrypted: flags & RHDF_ENCRYPTED != 0,
            },
            redirect: redirect != 0,
        }))
    }
    /// Skips, or extracts the current entry to `(root, file)`: `root` is the extraction root
    /// libunrar resolves anything relative against, `file` the exact output path.
    fn process(&mut self, operation: c_int, to: Option<(&Path, &Path)>) -> Result<()> {
        let to = match to {
            Some((root, file)) => Some((native(root)?, native(file)?)),
            None => None,
        };
        let (root, file) = to.as_ref().map_or((ptr::null(), ptr::null()), |(r, f)| {
            (r.as_ptr(), f.as_ptr())
        });
        // SAFETY: the handle is open and both paths are NUL-terminated or null.
        #[cfg(any(target_os = "linux", target_os = "netbsd"))]
        let code = unsafe { ffi::RARProcessFile(self.0.as_ptr(), operation, root, file) };
        // SAFETY: as above.
        #[cfg(not(any(target_os = "linux", target_os = "netbsd")))]
        let code = unsafe { ffi::RARProcessFileW(self.0.as_ptr(), operation, root, file) };
        anyhow::ensure!(
            code == ffi::ERAR_SUCCESS,
            "rar entry failed (unrar error {code})"
        );
        Ok(())
    }
}
