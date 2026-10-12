//! Deleted-file recovery (Phase 17 Task 85): read-only parsers over a volume or a volume
//! image (NTFS, exFAT, FAT12/16/32) that list the deleted files still described on disk,
//! each with how likely its content is intact, and a restore that writes only to another
//! volume.
//!
//! - **NTFS** ([`scan_ntfs`]): every `$MFT` record with the in-use flag clear that still has
//!   a `$FILE_NAME` and an unnamed `$DATA` (resident, or non-resident with its data runs).
//! - **exFAT** ([`scan_exfat`]): directory entry sets (file, stream extension, file names)
//!   with the in-use bit clear; contiguous (`NoFatChain`) and FAT-chained data.
//! - **FAT12/16/32** ([`scan_fat`]): `0xE5` entries with their long names reassembled; a
//!   name without a long name has its first character unknown (`_`); the data is taken as
//!   the contiguous clusters from the first one (the chain is erased on delete).
//! - **Recycle Bin** ([`recycle`]): `$I` files with the original path, size and date.
//!
//! The parsers take `Read + Seek` and never write. Every one is fuzzed (`fuzz::recover_*`,
//! `fuzz::recycle_bin`) and returns an error, never a panic, on hostile bytes; what they
//! keep in memory grows with the bytes actually read, never with a size a header declares.
//! [`restore`] refuses a destination on the source volume ([`VolumeId`]).

mod alloc;
mod exfat;
mod fat;
mod ntfs;
pub mod recycle;
mod source;
/// Volume image builders for tests (this crate's and other crates' with `test-util`).
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub mod testimg;
#[cfg(test)]
mod tests;

pub use source::{drive_letter, volume_of, BlockReader, Source, VolumeId};

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Most candidates one scan lists.
pub const MAX_CANDIDATES: usize = 1_000_000;
/// Deepest folder nesting followed (FAT, exFAT) or named (NTFS paths).
pub(crate) const MAX_DEPTH: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error(
        "not an NTFS, exFAT or FAT volume (pass a volume or a volume image, not a whole disk)"
    )]
    UnknownFs,
    #[error("damaged {0} volume: {1}")]
    Corrupt(&'static str, String),
    /// Opening a drive for raw reading was refused.
    #[error("{0}")]
    NeedsAdmin(&'static str),
    #[error("the destination is on the volume being recovered from; choose a folder on another drive (writing there could overwrite the deleted data)")]
    SameVolume,
    #[error("{0}")]
    Unrecoverable(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T, E = RecoverError> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsKind {
    Ntfs,
    Exfat,
    Fat12,
    Fat16,
    Fat32,
}

impl FsKind {
    pub fn label(self) -> &'static str {
        match self {
            FsKind::Ntfs => "NTFS",
            FsKind::Exfat => "exFAT",
            FsKind::Fat12 => "FAT12",
            FsKind::Fat16 => "FAT16",
            FsKind::Fat32 => "FAT32",
        }
    }
}

/// How likely a candidate's content is still the deleted file's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Every cluster of the data is still free (or the data sits in the record itself).
    Good,
    /// Some of its clusters were reused since: parts are other files' data.
    Partial,
    /// Every cluster was reused: what comes back is other data.
    Overwritten,
    /// The allocation map does not cover its clusters.
    Unknown,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::Good => "good",
            Confidence::Partial => "partial",
            Confidence::Overwritten => "overwritten",
            Confidence::Unknown => "unknown",
        }
    }
}

/// A run of the file's bytes on the volume; `offset` None is a sparse run (zeros).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub offset: Option<u64>,
    pub len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Data {
    /// The content held in the record itself (NTFS resident data).
    Resident(Vec<u8>),
    /// Runs of bytes on the volume in file order; past `initialized` the file reads zeros.
    Extents {
        extents: Vec<Extent>,
        initialized: u64,
    },
}

/// A deleted file found on the volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub name: String,
    /// The folder it was in, from the root (`a/b`, empty for the root); None when its
    /// parent folder is gone.
    pub folder: Option<String>,
    /// FAT without a long name: the first character was erased (shown as `_`).
    pub first_char_unknown: bool,
    pub size: u64,
    /// Seconds since 1970; UTC unless `local_times`.
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    /// The times are the wall-clock reading the volume recorded, without a time zone (FAT,
    /// exFAT without a UTC offset).
    pub local_times: bool,
    pub confidence: Confidence,
    pub data: Data,
    /// Where it was found: the MFT record number (NTFS) or the byte offset of its
    /// directory entry (FAT, exFAT).
    pub location: u64,
}

impl Candidate {
    /// `folder/name`, or `?/name` when the folder is unknown.
    pub fn path(&self) -> String {
        match self.folder.as_deref() {
            Some("") => self.name.clone(),
            Some(f) => format!("{f}/{}", self.name),
            None => format!("?/{}", self.name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scan {
    pub fs: FsKind,
    pub cluster_size: u64,
    /// The volume's size by its boot sector.
    pub volume_size: u64,
    pub candidates: Vec<Candidate>,
    /// What was skipped and why (shown under the list).
    pub notes: Vec<String>,
}

/// Reads the boot sector and scans with the matching parser.
pub fn scan<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    let mut boot = [0u8; 512];
    read_at(r, 0, &mut boot).map_err(|_| RecoverError::UnknownFs)?;
    if &boot[3..11] == b"NTFS    " {
        scan_ntfs(r)
    } else if &boot[3..11] == b"EXFAT   " {
        scan_exfat(r)
    } else {
        scan_fat(r)
    }
}

pub fn scan_ntfs<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    ntfs::scan(r)
}

pub fn scan_exfat<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    exfat::scan(r)
}

pub fn scan_fat<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    fat::scan(r)
}

/// Writes `c`'s content (its `size` bytes) to `out` and returns the count. Sparse runs and
/// the part past the initialized size are zeros. Fails when the runs end before the size
/// or the volume before the runs.
pub fn read_data<R: Read + Seek, W: Write>(r: &mut R, c: &Candidate, out: &mut W) -> Result<u64> {
    match &c.data {
        Data::Resident(bytes) => {
            let n = bytes
                .len()
                .min(usize::try_from(c.size).unwrap_or(usize::MAX));
            out.write_all(&bytes[..n])?;
            Ok(n as u64)
        }
        Data::Extents {
            extents,
            initialized,
        } => {
            let mut buf = vec![0u8; 1 << 20];
            let mut done = 0u64;
            for e in extents {
                let mut n = e.len.min(c.size - done);
                if let Some(off) = e.offset {
                    r.seek(SeekFrom::Start(off))?;
                }
                while n > 0 {
                    let k = n.min(buf.len() as u64) as usize;
                    match e.offset {
                        Some(_) => {
                            r.read_exact(&mut buf[..k]).map_err(|e| {
                                RecoverError::Unrecoverable(format!(
                                    "the data could not be read at byte {done}: {e}"
                                ))
                            })?;
                            let valid = initialized.saturating_sub(done).min(k as u64) as usize;
                            buf[valid..k].fill(0);
                        }
                        None => buf[..k].fill(0),
                    }
                    out.write_all(&buf[..k])?;
                    done += k as u64;
                    n -= k as u64;
                }
                if done == c.size {
                    break;
                }
            }
            if done < c.size {
                return Err(RecoverError::Unrecoverable(format!(
                    "its data runs end at byte {done} of {}",
                    c.size
                )));
            }
            Ok(done)
        }
    }
}

/// Fails with [`RecoverError::SameVolume`] when `dest` is on the `source` volume.
pub fn check_destination(source: VolumeId, dest: &Path) -> Result<()> {
    if volume_of(dest)? == source {
        return Err(RecoverError::SameVolume);
    }
    Ok(())
}

/// Restores `c` from the volume `r` (whose id is `source`) into the existing folder
/// `dest_dir` on another volume: a new file named after it (`name (2).ext` when the name is
/// taken) with its modified time. Never writes to the source volume: a destination there
/// is refused, checked again on the new file before any data is written.
pub fn restore<R: Read + Seek>(
    r: &mut R,
    source: VolumeId,
    c: &Candidate,
    dest_dir: &Path,
) -> Result<PathBuf> {
    check_destination(source, dest_dir)?;
    let (path, file) = create_free(dest_dir, &safe_file_name(&c.name))?;
    if let Err(e) = write_checked(r, source, c, &path, &file) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    if let Some(t) = c.modified.and_then(|t| system_time(t, c.local_times)) {
        let _ = file.set_modified(t);
    }
    Ok(path)
}

fn write_checked<R: Read + Seek>(
    r: &mut R,
    source: VolumeId,
    c: &Candidate,
    path: &Path,
    file: &std::fs::File,
) -> Result<()> {
    // A junction or mount point swapped in since the check would land the file here.
    check_destination(source, path)?;
    let mut out = io::BufWriter::new(file);
    read_data(r, c, &mut out)?;
    out.flush()?;
    Ok(())
}

fn system_time(secs: i64, local: bool) -> Option<std::time::SystemTime> {
    use chrono::TimeZone;
    let utc = if local {
        let naive = chrono::DateTime::from_timestamp(secs, 0)?.naive_utc();
        chrono::Local
            .from_local_datetime(&naive)
            .earliest()?
            .timestamp()
    } else {
        secs
    };
    let since = u64::try_from(utc).ok()?;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(since))
}

/// `dir/name`, or `dir/name (2).ext` and so on, created new (never over a file).
fn create_free(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File)> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..10_000 {
        let path = match n {
            1 => dir.join(name),
            n => dir.join(format!("{stem} ({n}){ext}")),
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(RecoverError::Unrecoverable(format!(
        "no free name for {name} in {}",
        dir.display()
    )))
}

/// A name every platform accepts: separators, `<>:"|?*` and control characters become
/// `_`, trailing dots and spaces go, reserved device names get a `_` in front.
pub fn safe_file_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(240)
        .collect();
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if s.is_empty() {
        return "recovered".into();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}

// --- shared readers ---

pub(crate) fn read_at<R: Read + Seek>(r: &mut R, off: u64, buf: &mut [u8]) -> io::Result<()> {
    r.seek(SeekFrom::Start(off))?;
    r.read_exact(buf)
}

/// Up to `len` bytes at `off`: fewer when the volume ends first. The buffer grows with
/// the bytes read, not with `len`.
pub(crate) fn read_up_to<R: Read + Seek>(r: &mut R, off: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    if r.seek(SeekFrom::Start(off)).is_err() {
        return Ok(out);
    }
    // A read error past the end of a raw volume ends the data like an EOF.
    let _ = r.by_ref().take(len).read_to_end(&mut out);
    Ok(out)
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |s| {
        u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}

/// The UTF-16LE code units of `b` (an odd last byte is dropped).
pub(crate) fn utf16_units(b: &[u8]) -> impl Iterator<Item = u16> + '_ {
    b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c))
}

/// UTF-16LE code units up to the first NUL.
pub(crate) fn utf16_until_nul(b: &[u8]) -> String {
    let units: Vec<u16> = utf16_units(b).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// A Windows FILETIME (100 ns since 1601) as seconds since 1970; 0 is None.
pub(crate) fn filetime(ft: u64) -> Option<i64> {
    if ft == 0 {
        return None;
    }
    i64::try_from(ft / 10_000_000)
        .ok()
        .map(|s| s - 11_644_473_600)
}

/// Seconds since 1970 of a calendar date and time (no zone), if valid.
pub(crate) fn civil(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
    Some(
        chrono::NaiveDate::from_ymd_opt(y, mo, d)?
            .and_hms_opt(h, mi, s)?
            .and_utc()
            .timestamp(),
    )
}
