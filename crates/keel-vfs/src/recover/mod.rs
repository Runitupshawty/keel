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
//! keep in memory grows with the bytes actually read, never with a size a header declares,
//! and is capped as a whole ([`MAX_HELD_BYTES`]): each folder is named once in
//! [`Scan::folders`] and candidates point at it, so a deep folder holding many entries
//! costs no more than a shallow one. [`restore`] refuses a destination on the source
//! volume ([`VolumeId`]), or on a virtual disk or loop device backed by a file there, and
//! creates its file through the folder handle it checked.

mod alloc;
mod dest;
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
/// Most bytes one scan keeps for its list (names, folders, extents, resident data and the
/// folder records it needs to name paths): past it the scan stops with a note.
pub const MAX_HELD_BYTES: u64 = 512 << 20;
/// Most bytes of an allocation map or FAT read: past it the clusters are rated unknown and
/// chains end (16 TiB at 4 KiB clusters).
pub(crate) const MAX_MAP_BYTES: u64 = 512 << 20;
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

/// A folder named in a scan: its name and its parent's index in [`Scan::folders`]. The
/// root is index 0 (no parent, no name); a parent always comes before its children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub name: String,
    pub parent: Option<u32>,
}

/// A deleted file found on the volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub name: String,
    /// The folder it was in, an index in [`Scan::folders`] ([`Scan::path`] names it); None
    /// when its parent folder is gone.
    pub folder: Option<u32>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scan {
    pub fs: FsKind,
    pub cluster_size: u64,
    /// The volume's size by its boot sector.
    pub volume_size: u64,
    pub candidates: Vec<Candidate>,
    /// The folders the candidates are in, the root first.
    pub folders: Vec<Folder>,
    /// What was skipped and why (shown under the list).
    pub notes: Vec<String>,
}

impl Scan {
    /// The folder's path from the root (`a/b`, empty for the root); None for an index the
    /// table does not hold or one that does not lead to the root.
    pub fn folder_path(&self, folder: u32) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = folder;
        loop {
            let f = self.folders.get(cur as usize)?;
            match f.parent {
                None => break,
                // A parent comes before its children, so the walk always ends.
                Some(p) if p < cur => {
                    parts.push(&f.name);
                    cur = p;
                }
                Some(_) => return None,
            }
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    /// `folder/name`, or `?/name` when the folder is unknown.
    pub fn path(&self, c: &Candidate) -> String {
        match c.folder.and_then(|f| self.folder_path(f)) {
            Some(f) if f.is_empty() => c.name.clone(),
            Some(f) => format!("{f}/{}", c.name),
            None => format!("?/{}", c.name),
        }
    }
}

/// The root's index in [`Scan::folders`].
pub(crate) const ROOT_FOLDER: u32 = 0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Full {
    Count,
    Bytes,
}

/// A scan's list as it is built: the candidates, the folder table and the bytes they hold,
/// capped by [`MAX_CANDIDATES`] and a byte limit.
pub(crate) struct List {
    candidates: Vec<Candidate>,
    folders: Vec<Folder>,
    held: u64,
    limit: u64,
    full: Option<Full>,
}

impl List {
    pub fn new(limit: u64) -> Self {
        Self {
            candidates: Vec::new(),
            folders: vec![Folder {
                name: String::new(),
                parent: None,
            }],
            held: 0,
            limit,
            full: None,
        }
    }

    /// No more candidates fit: the scan stops.
    pub fn full(&self) -> bool {
        self.full.is_some()
    }

    /// Counts `bytes` more as held; false (and the list full) past the limit. Folders can
    /// still be named once the candidate count is reached.
    pub fn charge(&mut self, bytes: usize) -> bool {
        if self.full == Some(Full::Bytes) {
            return false;
        }
        match self
            .held
            .checked_add(bytes as u64)
            .filter(|&n| n <= self.limit)
        {
            Some(n) => {
                self.held = n;
                true
            }
            None => {
                self.full = Some(Full::Bytes);
                false
            }
        }
    }

    /// A new folder under `parent` (charged with the walk's stack item for it); None once
    /// the list is full.
    pub fn folder(&mut self, parent: u32, name: String) -> Option<u32> {
        let at = u32::try_from(self.folders.len()).ok()?;
        if parent >= at || !self.charge(std::mem::size_of::<Folder>() + name.len() + 64) {
            return None;
        }
        self.folders.push(Folder {
            name,
            parent: Some(parent),
        });
        Some(at)
    }

    /// Adds a candidate; false once the list is full (it is dropped).
    pub fn push(&mut self, c: Candidate) -> bool {
        if self.full.is_some() {
            return false;
        }
        let data = match &c.data {
            Data::Resident(b) => b.len(),
            Data::Extents { extents, .. } => extents.len() * std::mem::size_of::<Extent>(),
        };
        if !self.charge(std::mem::size_of::<Candidate>() + c.name.len() + data) {
            return false;
        }
        self.candidates.push(c);
        if self.candidates.len() >= MAX_CANDIDATES {
            self.full = Some(Full::Count);
        }
        true
    }

    pub fn candidates_mut(&mut self) -> &mut [Candidate] {
        &mut self.candidates
    }

    pub fn finish(
        self,
        fs: FsKind,
        cluster_size: u64,
        volume_size: u64,
        mut notes: Vec<String>,
    ) -> Scan {
        match self.full {
            Some(Full::Count) => notes.push(format!("the list stops at {MAX_CANDIDATES} files")),
            Some(Full::Bytes) => notes.push(format!(
                "the scan stopped early: its list reached the {} MiB memory limit after {} files",
                self.limit >> 20,
                self.candidates.len()
            )),
            None => {}
        }
        Scan {
            fs,
            cluster_size,
            volume_size,
            candidates: self.candidates,
            folders: self.folders,
            notes,
        }
    }
}

/// Reads the boot sector and scans with the matching parser.
pub fn scan<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    scan_limited(r, MAX_HELD_BYTES)
}

/// [`scan`] keeping at most `limit` bytes of list.
pub(crate) fn scan_limited<R: Read + Seek>(r: &mut R, limit: u64) -> Result<Scan> {
    let mut boot = [0u8; 512];
    read_at(r, 0, &mut boot).map_err(|_| RecoverError::UnknownFs)?;
    if &boot[3..11] == b"NTFS    " {
        ntfs::scan(r, limit)
    } else if &boot[3..11] == b"EXFAT   " {
        exfat::scan(r, limit)
    } else {
        fat::scan(r, limit)
    }
}

pub fn scan_ntfs<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    ntfs::scan(r, MAX_HELD_BYTES)
}

pub fn scan_exfat<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    exfat::scan(r, MAX_HELD_BYTES)
}

pub fn scan_fat<R: Read + Seek>(r: &mut R) -> Result<Scan> {
    fat::scan(r, MAX_HELD_BYTES)
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

/// Fails with [`RecoverError::SameVolume`] when the folder `dest` is on the `source`
/// volume, or on a loop device or virtual disk whose backing file is there.
pub fn check_destination(source: VolumeId, dest: &Path) -> Result<()> {
    dest::DestDir::open(dest, source).map(drop)
}

/// Restores `c` from the volume `r` (whose id is `source`) into the existing folder
/// `dest_dir` on another volume: a new file named after it (`name (2).ext` when the name is
/// taken) with its modified time. Never writes to the source volume: the folder is opened
/// once, its volume checked on that handle, and the file created relative to it, so a
/// junction or link swapped in after the check cannot redirect the create.
pub fn restore<R: Read + Seek>(
    r: &mut R,
    source: VolumeId,
    c: &Candidate,
    dest_dir: &Path,
) -> Result<PathBuf> {
    let dir = dest::DestDir::open(dest_dir, source)?;
    let (name, path, file) = dir.create_free(&safe_file_name(&c.name))?;
    let written = (|| -> Result<()> {
        let mut out = io::BufWriter::new(&file);
        read_data(r, c, &mut out)?;
        out.flush()?;
        Ok(())
    })();
    if let Err(e) = written {
        dir.remove(&name, file);
        return Err(e);
    }
    if let Some(t) = c.modified.and_then(|t| system_time(t, c.local_times)) {
        let _ = file.set_modified(t);
    }
    Ok(path)
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
