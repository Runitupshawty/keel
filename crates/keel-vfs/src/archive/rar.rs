//! RAR through libunrar's C API (`unrar_sys`). The safe `unrar` wrapper hides each entry's
//! redirect type and can only extract with the extraction root cleared, which made
//! libunrar resolve WinRAR "file reference" and hard-link entries against Keel's working
//! folder and skip its symlink checks. Here symlinks and junctions are left out of
//! listings and never extracted. Hard links and file references are listed as plain files
//! and materialised by Keel itself: libunrar only skips past them, and each gets a fresh
//! copy of the earlier entry it names, extracted in the same pass; one that names no
//! earlier plain file fails the whole extraction up front. Every body is extracted with its
//! extraction root set to a private temp folder under the archive cache (checked for space
//! first), then accepted only as a plain file with a single link.
use super::{ArchiveEntry, ArchiveReader};
use anyhow::{bail, Context, Result};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::raw::{c_char, c_int, c_uint},
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    time::{Duration, UNIX_EPOCH},
};
use unrar_sys as ffi;

const RHDF_ENCRYPTED: c_uint = 0x04;
const RHDF_DIRECTORY: c_uint = 0x20;
const FSREDIR_HARDLINK: c_uint = 4;
const FSREDIR_FILECOPY: c_uint = 5;

pub(super) struct Reader {
    path: PathBuf,
}
impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        Archive::open(path, ffi::RAR_OM_LIST)?;
        // libunrar reports a clean end both after the end-of-archive header and for a file
        // cut off exactly on a header boundary.
        anyhow::ensure!(has_end(path)?, "truncated archive (no end marker)");
        Ok(Self { path: path.into() })
    }
}
impl ArchiveReader for Reader {
    fn entries(&mut self) -> Result<Vec<ArchiveEntry>> {
        let mut archive = Archive::open(&self.path, ffi::RAR_OM_LIST)?;
        let mut out = Vec::new();
        while let Some(header) = archive.next()? {
            archive.process(ffi::RAR_SKIP, None)?;
            if !header.link {
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
        // Headers first: which earlier entries the wanted copies repeat, and whether every
        // one of them can be materialised.
        let (mut sources, mut seen, mut broken) = (HashSet::new(), HashSet::new(), Vec::new());
        let mut list = Archive::open(&self.path, ffi::RAR_OM_LIST)?;
        while let Some(header) = list.next()? {
            list.process(ffi::RAR_SKIP, None)?;
            let name = header.entry.inner;
            match header.copy_of {
                Some(source) if want(&name) => {
                    if seen.contains(&source) {
                        sources.insert(source);
                    } else {
                        broken.push(name);
                    }
                }
                None if !header.link && !header.entry.is_dir => {
                    seen.insert(name);
                }
                _ => {}
            }
        }
        anyhow::ensure!(
            broken.is_empty(),
            "{} link entries cannot be extracted: {}",
            broken.len(),
            broken.join(", ")
        );
        let root = temp_root()?;
        let mut kept: HashMap<String, PathBuf> = HashMap::new();
        let mut archive = Archive::open(&self.path, ffi::RAR_OM_EXTRACT)?;
        let mut n = 0u64;
        while let Some(header) = archive.next()? {
            let entry = &header.entry;
            if let Some(source) = &header.copy_of {
                archive.process(ffi::RAR_SKIP, None)?;
                if want(&entry.inner) {
                    let copy = kept.get(source).context("link source was not kept")?;
                    each(&entry.inner, entry.size, &mut File::open(copy)?)?;
                }
                continue;
            }
            let keep = sources.contains(&entry.inner);
            if entry.is_dir || header.link || !(keep || want(&entry.inner)) {
                archive.process(ffi::RAR_SKIP, None)?;
                continue;
            }
            let free = fs4::available_space(root.path())?;
            anyhow::ensure!(
                entry.size <= free,
                "not enough temp space in {} for {} ({} bytes, {free} free)",
                root.path().display(),
                entry.inner,
                entry.size
            );
            n += 1;
            let file = root.path().join(n.to_string());
            archive.process(ffi::RAR_EXTRACT, Some((root.path(), &file)))?;
            // Checked before opening, so a link or special file is never followed.
            let plain = std::fs::symlink_metadata(&file)?.is_file();
            anyhow::ensure!(plain, "rar entry is not a plain file: {}", entry.inner);
            let mut body = File::open(&file)?;
            anyhow::ensure!(
                links(&body)? == 1,
                "rar entry is a hard link: {}",
                entry.inner
            );
            if want(&entry.inner) {
                each(&entry.inner, entry.size, &mut body)?;
            }
            drop(body);
            if keep {
                kept.insert(entry.inner.clone(), file);
            } else {
                std::fs::remove_file(&file)?;
            }
        }
        Ok(())
    }
}

/// A private folder for extracted bodies, on the archive cache's volume rather than %TEMP%.
/// Day-old leftovers of a crashed run are swept first.
fn temp_root() -> Result<tempfile::TempDir> {
    let parent = super::cache::default_root().join("rar-temp");
    std::fs::create_dir_all(&parent)?;
    for item in std::fs::read_dir(&parent)?.flatten() {
        let old = item
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 3600));
        if old {
            let _ = std::fs::remove_dir_all(item.path());
        }
    }
    Ok(tempfile::tempdir_in(parent)?)
}

/// Whether the file ends in an end-of-archive header.
fn has_end(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len().min(32);
    file.seek(SeekFrom::End(-(len as i64)))?;
    let mut tail = vec![0; len as usize];
    file.read_exact(&mut tail)?;
    Ok(ends_with_end_header(&tail))
}
/// RAR5: `CRC32, size, type 5, flags, end flags`. RAR4: `CRC16, type 0x7b, flags, size`,
/// with any optional fields counted in `size`.
fn ends_with_end_header(tail: &[u8]) -> bool {
    let le = |b: &[u8]| b.iter().rev().fold(0u32, |n, &x| n << 8 | u32::from(x));
    let rar5 = (3..=16).any(|size: usize| {
        let Some(at) = tail.len().checked_sub(size + 5) else {
            return false;
        };
        let header = &tail[at + 4..];
        usize::from(header[0]) == size && header[1] == 5 && crc32(header) == le(&tail[at..at + 4])
    });
    let rar4 = (7..=20).any(|size: usize| {
        let Some(at) = tail.len().checked_sub(size) else {
            return false;
        };
        let block = &tail[at..];
        block[2] == 0x7b
            && le(&block[5..7]) as usize == size
            && crc32(&block[2..]) & 0xffff == le(&block[..2])
    });
    rar5 || rar4
}
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &b| {
        (0..8).fold(crc ^ u32::from(b), |c, _| {
            (c >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(c & 1))
        })
    })
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
    /// Symlink or junction (or an unknown redirect): never listed or extracted.
    link: bool,
    /// Hard link or file reference: the archive name of the entry whose bytes it repeats.
    copy_of: Option<String>,
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
        let mut target: Vec<ffi::WCHAR> = vec![0; 2048];
        raw.redir_name = target.as_mut_ptr();
        raw.redir_name_size = target.len() as c_uint;
        // SAFETY: the handle is open, `raw` is a writable struct of libunrar's layout and
        // `redir_name` points at `target`, which outlives the call.
        let code = unsafe {
            ffi::RARReadHeaderEx(
                self.0.as_ptr(),
                ptr::addr_of_mut!(*raw).cast::<ffi::HeaderDataEx>(),
            )
        };
        match code {
            ffi::ERAR_SUCCESS => {}
            // A file cut on a header boundary also ends here; `Reader::open` checks for
            // the end-of-archive header.
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
            link: !matches!(redirect, 0 | FSREDIR_HARDLINK | FSREDIR_FILECOPY),
            copy_of: matches!(redirect, FSREDIR_HARDLINK | FSREDIR_FILECOPY)
                .then(|| text(&target).replace('\\', "/")),
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

#[cfg(test)]
mod tests {
    use super::ends_with_end_header;
    #[test]
    fn end_headers_of_both_formats_are_recognised() {
        // WinRAR's RAR5 end block, and the classic 7-byte RAR 2.9-4.x one.
        let rar5 = [0x1d, 0x77, 0x56, 0x51, 0x03, 0x05, 0x04, 0x00];
        let rar4 = [0xc4, 0x3d, 0x7b, 0x00, 0x40, 0x07, 0x00];
        for tail in [&rar5[..], &rar4[..]] {
            assert!(ends_with_end_header(&[b"body".as_slice(), tail].concat()));
            assert!(!ends_with_end_header(&tail[..tail.len() - 1]));
            let mut bad = tail.to_vec();
            bad[0] ^= 1;
            assert!(!ends_with_end_header(&bad));
        }
        assert!(!ends_with_end_header(b""));
    }
}
