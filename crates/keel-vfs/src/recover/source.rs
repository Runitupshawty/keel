//! The volume being recovered from: a drive opened read-only for raw reading (Windows
//! `\\.\X:`, a block device elsewhere) or a volume image file, its [`VolumeId`], and the
//! aligned, cached reader raw volume handles need.

use super::{RecoverError, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The volume a path lives on: the volume serial number on Windows, the device number
/// elsewhere (a block device's own number for the device itself). A volume image counts
/// as the volume holding the image file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeId(pub(crate) u64);

/// The volume `path` (a file or folder that exists) is on.
pub fn volume_of(path: &Path) -> io::Result<VolumeId> {
    #[cfg(windows)]
    if let Some(letter) = drive_letter(&path.to_string_lossy()) {
        return sys_volume_of(Path::new(&format!("{letter}:\\")));
    }
    sys_volume_of(path)
}

#[cfg(unix)]
fn sys_volume_of(path: &Path) -> io::Result<VolumeId> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let m = std::fs::metadata(path)?;
    Ok(VolumeId(if m.file_type().is_block_device() {
        m.rdev()
    } else {
        m.dev()
    }))
}

#[cfg(windows)]
fn sys_volume_of(path: &Path) -> io::Result<VolumeId> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    // No access asked (attributes only); links and junctions are followed to the volume
    // the data would land on.
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a handle owned by `file`, which outlives the call; `info` is written once.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
    Ok(VolumeId(u64::from(info.dwVolumeSerialNumber)))
}

#[cfg(not(any(unix, windows)))]
fn sys_volume_of(_: &Path) -> io::Result<VolumeId> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "volume ids are not available on this platform",
    ))
}

/// `X` for `X:`, `X:\`, `X:/` and `\\.\X:`.
pub fn drive_letter(spec: &str) -> Option<char> {
    let s = spec
        .strip_prefix(r"\\.\")
        .or_else(|| spec.strip_prefix("//./"))
        .unwrap_or(spec);
    let s = s.strip_suffix(['\\', '/']).unwrap_or(s);
    let mut chars = s.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(c), Some(':'), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

/// A volume opened read-only for recovery.
pub struct Source {
    /// What was opened: `\\.\X:`, a device or an image file.
    pub path: PathBuf,
    /// The volume restores must not write to.
    pub id: VolumeId,
    /// A drive or device (raw reading), not an image file.
    pub device: bool,
    reader: BlockReader<File>,
}

impl Source {
    /// Opens `spec` read-only: a drive letter (`X:`; Windows, needs administrator), a block
    /// device (needs root) or a volume image file.
    pub fn open(spec: &str) -> Result<Source> {
        #[cfg(windows)]
        if let Some(letter) = drive_letter(spec) {
            let path = PathBuf::from(format!(r"\\.\{letter}:"));
            let file = File::open(&path).map_err(|e| match e.kind() {
                io::ErrorKind::PermissionDenied => RecoverError::NeedsAdmin(
                    "reading a drive directly needs administrator rights: run keel from a terminal opened with Run as administrator",
                ),
                _ => RecoverError::Io(io::Error::new(e.kind(), format!("{letter}: {e}"))),
            })?;
            let id = volume_of(Path::new(&format!("{letter}:\\")))?;
            return Ok(Source {
                path,
                id,
                device: true,
                reader: BlockReader::new(file),
            });
        }
        let path = PathBuf::from(spec);
        let meta = std::fs::metadata(&path)?;
        if meta.is_dir() {
            return Err(RecoverError::Unrecoverable(format!(
                "{spec} is a folder: pass a drive, a device or a volume image"
            )));
        }
        let device = !meta.is_file();
        let file = File::open(&path).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied if device => RecoverError::NeedsAdmin(
                "reading a device directly needs root: run keel with sudo",
            ),
            _ => RecoverError::Io(e),
        })?;
        let id = volume_of(&path)?;
        Ok(Source {
            path,
            id,
            device,
            reader: BlockReader::new(file),
        })
    }

    pub fn reader(&mut self) -> &mut BlockReader<File> {
        &mut self.reader
    }
}

/// Reads through 64 KiB chunks at 64 KiB-aligned offsets (raw volume handles accept only
/// sector-aligned reads), the last chunk cached; a chunk that fails is read again as its
/// 4 KiB block (the end of a volume).
pub struct BlockReader<R> {
    inner: R,
    pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
}

const CHUNK: u64 = 64 << 10;
const BLOCK: u64 = 4 << 10;

impl<R: Read + Seek> BlockReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            pos: 0,
            buf: vec![0; CHUNK as usize],
            buf_start: 0,
            buf_len: 0,
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        let start = self.pos - self.pos % CHUNK;
        match self.read_block(start, CHUNK as usize) {
            Ok(n) => {
                self.buf_start = start;
                self.buf_len = n;
                Ok(())
            }
            Err(_) => {
                let start = self.pos - self.pos % BLOCK;
                self.buf_len = 0;
                let n = self.read_block(start, BLOCK as usize)?;
                self.buf_start = start;
                self.buf_len = n;
                Ok(())
            }
        }
    }

    fn read_block(&mut self, start: u64, len: usize) -> io::Result<usize> {
        self.inner.seek(SeekFrom::Start(start))?;
        let mut n = 0;
        while n < len {
            match self.inner.read(&mut self.buf[n..len]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) if n > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }
}

impl<R: Read + Seek> Read for BlockReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let end = self.buf_start + self.buf_len as u64;
        if self.pos < self.buf_start || self.pos >= end {
            self.fill()?;
        }
        let end = self.buf_start + self.buf_len as u64;
        if self.pos < self.buf_start || self.pos >= end {
            return Ok(0);
        }
        let at = (self.pos - self.buf_start) as usize;
        let n = out.len().min(self.buf_len - at);
        out[..n].copy_from_slice(&self.buf[at..at + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for BlockReader<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.pos = match to {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "seek before the start")
            })?,
            SeekFrom::End(d) => {
                let end = self.inner.seek(SeekFrom::End(0))?;
                end.checked_add_signed(d).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "seek before the start")
                })?
            }
        };
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_reader_reads_like_the_file() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
        let mut r = BlockReader::new(io::Cursor::new(data.clone()));
        for (off, len) in [
            (0u64, 10usize),
            (65_530, 20),
            (131_071, 70_000),
            (199_990, 100),
            (5, 0),
        ] {
            r.seek(SeekFrom::Start(off)).unwrap();
            let mut got = Vec::new();
            r.by_ref().take(len as u64).read_to_end(&mut got).unwrap();
            let end = (off as usize + len).min(data.len());
            assert_eq!(got, data[off as usize..end], "at {off}");
        }
        r.seek(SeekFrom::Start(300_000)).unwrap();
        let mut b = [0u8; 4];
        assert_eq!(r.read(&mut b).unwrap(), 0);
        assert_eq!(r.seek(SeekFrom::End(-4)).unwrap(), 199_996);
        r.read_exact(&mut b).unwrap();
        assert_eq!(b, data[199_996..]);
    }

    #[test]
    fn drive_letters() {
        assert_eq!(drive_letter("e:"), Some('E'));
        assert_eq!(drive_letter("E:\\"), Some('E'));
        assert_eq!(drive_letter(r"\\.\F:"), Some('F'));
        assert_eq!(drive_letter("E:\\x"), None);
        assert_eq!(drive_letter("image.img"), None);
        assert_eq!(drive_letter("1:"), None);
    }
}
