//! Raw NTFS MFT `FILE` records: the update sequence (fixups), the attributes the index
//! keeps (`$STANDARD_INFORMATION`, every `$FILE_NAME`, the unnamed and named `$DATA`
//! streams, `$REPARSE_POINT`, `$ATTRIBUTE_LIST`) and the data runs of a non-resident
//! attribute, plus [`Scan`], which merges base and extension records into one
//! [`Entry`] per file.
//!
//! Pure byte parsing, built on every platform so it can be fuzzed anywhere
//! (`fuzz::mft_record`, docs/fuzzing.md). Every length and offset is checked against
//! the record, so hostile bytes give an error, never a panic, an out-of-bounds read or
//! an allocation the bytes do not back.

// The parser reads every field of the format it meets (the fuzz target and the tests
// check them all); the index keeps a subset, and only Windows reads volumes.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;

/// NTFS protects every 512 bytes of a record with the update sequence, whatever the
/// sector size.
const STRIDE: usize = 512;
/// Largest record accepted (NTFS uses 1 KiB, or 4 KiB on some 4K-sector volumes).
pub(crate) const MAX_RECORD: usize = 64 * 1024;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
pub(crate) const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_IN_USE: u16 = 0x0001;
const RECORD_IS_DIRECTORY: u16 = 0x0002;

/// Attribute header flags: the compression method (low byte), encrypted, sparse.
pub(crate) const ATTR_COMPRESSED: u16 = 0x0001;
const ATTR_COMPRESSION_MASK: u16 = 0x00FF;
pub(crate) const ATTR_SPARSE: u16 = 0x8000;

pub(crate) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// In a `$FILE_NAME`'s flags: the file has a reparse point, whose tag the name keeps.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// The reparse tag of a file compressed by the Windows Overlay Filter (`compact
/// /exe`, CompactOS): its unnamed stream is an empty sparse placeholder and the
/// compressed contents are in the named stream [`WOF_STREAM`].
pub(crate) const IO_REPARSE_TAG_WOF: u32 = 0x8000_0017;
const WOF_STREAM: &str = "WofCompressedData";

/// `$FILE_NAME` namespaces: 0 POSIX, 1 Win32, 2 DOS (an 8.3 alias), 3 Win32 and DOS.
pub(crate) const NAMESPACE_DOS: u8 = 2;

/// Low 48 bits of a file reference: the record number. The high 16 bits are the
/// record's sequence number, which changes each time the record is reused.
pub(crate) const RECORD_MASK: u64 = 0xFFFF_FFFF_FFFF;

/// Why a record was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MftError {
    /// Not a whole number of 512-byte blocks up to [`MAX_RECORD`].
    Size,
    /// Marked "BAAD": chkdsk found a torn write.
    Baad,
    /// Neither "FILE" nor "BAAD": an unused or never-written record.
    Signature,
    /// The update sequence does not match: a torn or damaged record.
    Fixup,
    /// A header field points outside the record.
    Header,
    /// An attribute's length or offsets run outside the record.
    Attribute,
    /// A data run list that overflows or runs past its attribute.
    Runs,
}

impl fmt::Display for MftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MftError::Size => "MFT record of an impossible size",
            MftError::Baad => "MFT record marked BAAD (torn write)",
            MftError::Signature => "not an MFT FILE record",
            MftError::Fixup => "MFT record update sequence mismatch",
            MftError::Header => "MFT record header out of range",
            MftError::Attribute => "MFT attribute out of range",
            MftError::Runs => "MFT data runs out of range",
        })
    }
}

impl std::error::Error for MftError {}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// `$STANDARD_INFORMATION`: the dates Explorer shows (FILETIME: 100 ns since 1601) and
/// the file attributes (without the directory bit, which NTFS keeps in the header).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Standard {
    pub created: u64,
    pub modified: u64,
    /// When the MFT record itself last changed.
    pub changed: u64,
    pub accessed: u64,
    pub attributes: u32,
}

/// One `$FILE_NAME`: a name of the file in a folder (one per hard link, plus an 8.3
/// alias for some).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Name {
    /// The folder's file reference (record number and sequence number).
    pub parent: u64,
    pub name: String,
    pub namespace: u8,
}

/// Sizes of a stream, from its first extent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sizes {
    pub size: u64,
    /// Bytes on disk: the allocated size, or the clusters actually used when the
    /// stream is compressed or sparse; 0 when the data sits in the record (resident).
    pub allocated: u64,
    /// The attribute header flags ([`ATTR_COMPRESSED`], encrypted 0x4000,
    /// [`ATTR_SPARSE`]).
    pub flags: u16,
    /// The data sits in the record itself.
    pub resident: bool,
}

/// One `$ATTRIBUTE_LIST` entry: which record holds (part of) an attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListEntry {
    pub kind: u32,
    pub start_vcn: u64,
    /// File reference of the record holding the attribute.
    pub record: u64,
    pub named: bool,
}

/// A parsed `FILE` record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileRecord {
    pub sequence: u16,
    /// The header's count of names in folders.
    pub link_count: u16,
    /// The base record's file reference; 0 for a base record.
    pub base: u64,
    pub in_use: bool,
    pub is_dir: bool,
    pub standard: Option<Standard>,
    pub names: Vec<Name>,
    /// The unnamed `$DATA` stream (the file's contents), from its first extent.
    pub data: Option<Sizes>,
    /// Named `$DATA` streams (alternate data streams), each counted once.
    pub streams: u16,
    /// The reparse tag, 0 when none.
    pub reparse: u32,
    /// The entries of a resident `$ATTRIBUTE_LIST`.
    pub list: Vec<ListEntry>,
    /// The `$ATTRIBUTE_LIST` is non-resident: its entries are on disk, not here.
    pub list_outside: bool,
    /// Bytes on disk of the `WofCompressedData` stream (not counted in `streams`).
    pub wof: Option<u64>,
}

/// Checks and removes the update sequence in place: the last two bytes of every
/// 512-byte block must hold the sequence number, and get back the bytes saved in the
/// update sequence array.
pub(crate) fn fixup(record: &mut [u8]) -> Result<(), MftError> {
    let len = record.len();
    if len == 0 || len > MAX_RECORD || !len.is_multiple_of(STRIDE) {
        return Err(MftError::Size);
    }
    match record.get(..4) {
        Some(b"FILE") => {}
        Some(b"BAAD") => return Err(MftError::Baad),
        _ => return Err(MftError::Signature),
    }
    let usa = usize::from(u16_at(record, 4).ok_or(MftError::Header)?);
    let count = usize::from(u16_at(record, 6).ok_or(MftError::Header)?);
    let blocks = len / STRIDE;
    // The array (sequence number, then one saved pair per block) sits in the header,
    // before the first block's protected last two bytes.
    if count != blocks + 1 || usa < 0x28 || usa + 2 * count > STRIDE - 2 {
        return Err(MftError::Header);
    }
    let check = [record[usa], record[usa + 1]];
    for block in 0..blocks {
        let end = (block + 1) * STRIDE - 2;
        if record[end..end + 2] != check {
            return Err(MftError::Fixup);
        }
        let saved = usa + 2 * (block + 1);
        let (lo, hi) = (record[saved], record[saved + 1]);
        record[end] = lo;
        record[end + 1] = hi;
    }
    Ok(())
}

/// Fixups, then [`parse_fixed`].
pub(crate) fn parse(record: &mut [u8]) -> Result<FileRecord, MftError> {
    fixup(record)?;
    parse_fixed(record)
}

/// Blanks, in a fixed-up record, every byte the index never reads, so a record can
/// leave the elevated helper without carrying file contents: the value of every
/// resident attribute but `$STANDARD_INFORMATION`, `$ATTRIBUTE_LIST` and `$FILE_NAME`
/// (a small file's data, an alternate stream's, extended attributes, security
/// descriptors, a small folder's index), a reparse point's data after its tag, and the
/// slack after the record's last attribute. [`parse_fixed`] gives the same result on
/// the blanked record. Returns false, leaving the record untouched, for a free record;
/// errors on one whose attributes do not walk.
pub(crate) fn blank_fixed(r: &mut [u8]) -> Result<bool, MftError> {
    let header = MftError::Header;
    let bad = MftError::Attribute;
    if u16_at(r, 0x16).ok_or(header)? & RECORD_IN_USE == 0 {
        return Ok(false);
    }
    let first = usize::from(u16_at(r, 0x14).ok_or(header)?);
    let used = u32_at(r, 0x18).ok_or(header)? as usize;
    if used > r.len() || first < 0x18 || first > used {
        return Err(header);
    }
    // First find every range to clear, then clear: a bad attribute leaves no change.
    let mut clear: Vec<(usize, usize)> = Vec::new();
    let mut at = first;
    loop {
        let kind = u32_at(&r[..used], at).ok_or(bad)?;
        if kind == ATTR_END {
            break;
        }
        let len = u32_at(&r[..used], at + 4).ok_or(bad)? as usize;
        if len < 0x18 || !len.is_multiple_of(8) {
            return Err(bad);
        }
        let end = at.checked_add(len).filter(|&e| e <= used).ok_or(bad)?;
        let raw = &r[at..end];
        if *raw.get(8).ok_or(bad)? == 0 {
            let name_chars = usize::from(*raw.get(9).ok_or(bad)?);
            let name_at = usize::from(u16_at(raw, 0x0A).ok_or(bad)?);
            let value_len = u32_at(raw, 0x10).ok_or(bad)? as usize;
            let value_at = usize::from(u16_at(raw, 0x14).ok_or(bad)?);
            let value_end = value_at.checked_add(value_len).ok_or(bad)?;
            let name_end = if name_chars > 0 {
                name_at + 2 * name_chars
            } else {
                0
            };
            // A value that overlaps the attribute's header or name is not NTFS's.
            if value_end > len || (value_len > 0 && value_at < name_end.max(0x18)) {
                return Err(bad);
            }
            let keep = match kind {
                ATTR_STANDARD_INFORMATION | ATTR_ATTRIBUTE_LIST | ATTR_FILE_NAME => value_len,
                ATTR_REPARSE_POINT => value_len.min(4),
                _ => 0,
            };
            clear.push((at + value_at + keep, at + value_end));
        }
        at = end;
    }
    // The slack, but never the header fields `parse_fixed` reads.
    clear.push((used.max(0x30), r.len()));
    for (from, to) in clear {
        if from < to {
            r[from..to].fill(0);
        }
    }
    Ok(true)
}

/// One attribute's body.
enum Body<'a> {
    Resident(&'a [u8]),
    NonResident {
        start_vcn: u64,
        size: u64,
        /// Allocated size, or the compressed/sparse total when there is one.
        on_disk: u64,
        runs: &'a [u8],
    },
}

struct Attr<'a> {
    kind: u32,
    named: bool,
    /// The name, UTF-16LE.
    name: &'a [u8],
    flags: u16,
    body: Body<'a>,
}

impl Attr<'_> {
    /// The first (or only) extent of the attribute: where its sizes are kept.
    fn first(&self) -> bool {
        match self.body {
            Body::Resident(_) => true,
            Body::NonResident { start_vcn, .. } => start_vcn == 0,
        }
    }
}

fn attr(kind: u32, raw: &[u8]) -> Result<Attr<'_>, MftError> {
    let bad = MftError::Attribute;
    let non_resident = *raw.get(8).ok_or(bad)? != 0;
    let name_chars = usize::from(*raw.get(9).ok_or(bad)?);
    let name_at = usize::from(u16_at(raw, 0x0A).ok_or(bad)?);
    if name_chars > 0 && raw.get(name_at..name_at + 2 * name_chars).is_none() {
        return Err(bad);
    }
    let flags = u16_at(raw, 0x0C).ok_or(bad)?;
    let body = if non_resident {
        let field = |at| u64_at(raw, at).ok_or(bad);
        let runs_at = usize::from(u16_at(raw, 0x20).ok_or(bad)?);
        let allocated = field(0x28)?;
        let size = field(0x30)?;
        // Compressed and sparse streams carry the clusters really in use at 0x40.
        let on_disk = if flags & (ATTR_COMPRESSION_MASK | ATTR_SPARSE) != 0 {
            u64_at(raw, 0x40).unwrap_or(allocated)
        } else {
            allocated
        };
        if runs_at < 0x40 {
            return Err(bad);
        }
        Body::NonResident {
            start_vcn: field(0x10)?,
            size,
            on_disk,
            runs: raw.get(runs_at..).ok_or(bad)?,
        }
    } else {
        let len = u32_at(raw, 0x10).ok_or(bad)? as usize;
        let at = usize::from(u16_at(raw, 0x14).ok_or(bad)?);
        Body::Resident(raw.get(at..at.checked_add(len).ok_or(bad)?).ok_or(bad)?)
    };
    Ok(Attr {
        kind,
        named: name_chars > 0,
        name: raw
            .get(name_at..name_at + 2 * name_chars)
            .unwrap_or_default(),
        flags,
        body,
    })
}

/// Calls `each` for every attribute of a fixed-up record, in record order.
fn each_attribute<'a>(r: &'a [u8], mut each: impl FnMut(Attr<'a>)) -> Result<(), MftError> {
    let first = usize::from(u16_at(r, 0x14).ok_or(MftError::Header)?);
    let used = u32_at(r, 0x18).ok_or(MftError::Header)? as usize;
    if used > r.len() || first < 0x18 || first > used {
        return Err(MftError::Header);
    }
    let r = &r[..used];
    let mut at = first;
    loop {
        let kind = u32_at(r, at).ok_or(MftError::Attribute)?;
        if kind == ATTR_END {
            return Ok(());
        }
        let len = u32_at(r, at + 4).ok_or(MftError::Attribute)? as usize;
        // The smallest attribute (resident, no name, no value) is 0x18 bytes.
        if len < 0x18 || !len.is_multiple_of(8) {
            return Err(MftError::Attribute);
        }
        let raw = r.get(at..at + len).ok_or(MftError::Attribute)?;
        each(attr(kind, raw)?);
        at += len;
    }
}

fn standard(v: &[u8]) -> Option<Standard> {
    Some(Standard {
        created: u64_at(v, 0)?,
        modified: u64_at(v, 0x08)?,
        changed: u64_at(v, 0x10)?,
        accessed: u64_at(v, 0x18)?,
        attributes: u32_at(v, 0x20)?,
    })
}

/// A `$FILE_NAME` value and the reparse tag it carries (0 when none).
fn file_name(v: &[u8]) -> Option<(Name, u32)> {
    let parent = u64_at(v, 0)?;
    let tag = if u32_at(v, 0x38)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        u32_at(v, 0x3C)?
    } else {
        0
    };
    let chars = usize::from(*v.get(0x40)?);
    let namespace = *v.get(0x41)?;
    let wide: Vec<u16> = v
        .get(0x42..0x42 + 2 * chars)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let name = Name {
        parent,
        name: String::from_utf16_lossy(&wide),
        namespace,
    };
    Some((name, tag))
}

fn list_entry(e: &[u8]) -> Option<ListEntry> {
    Some(ListEntry {
        kind: u32_at(e, 0)?,
        named: *e.get(6)? != 0,
        start_vcn: u64_at(e, 0x08)?,
        record: u64_at(e, 0x10)?,
    })
}

/// The entries of a resident `$ATTRIBUTE_LIST`; a damaged entry ends the list.
fn list_entries(v: &[u8]) -> Vec<ListEntry> {
    let mut entries = Vec::new();
    let mut at = 0;
    while let Some(len) = u16_at(v, at + 4) {
        let len = usize::from(len);
        // The fixed part of an entry is 0x1A bytes.
        if len < 0x1A {
            break;
        }
        let Some(entry) = v.get(at..at + len).and_then(list_entry) else {
            break;
        };
        entries.push(entry);
        at += len;
    }
    entries
}

/// Parses a record whose update sequence [`fixup`] already removed.
pub(crate) fn parse_fixed(r: &[u8]) -> Result<FileRecord, MftError> {
    let header = MftError::Header;
    let flags = u16_at(r, 0x16).ok_or(header)?;
    let mut rec = FileRecord {
        sequence: u16_at(r, 0x10).ok_or(header)?,
        link_count: u16_at(r, 0x12).ok_or(header)?,
        base: u64_at(r, 0x20).ok_or(header)?,
        in_use: flags & RECORD_IN_USE != 0,
        is_dir: flags & RECORD_IS_DIRECTORY != 0,
        ..FileRecord::default()
    };
    let mut name_tag = 0;
    each_attribute(r, |a| match (a.kind, &a.body) {
        (ATTR_STANDARD_INFORMATION, Body::Resident(v)) => {
            if rec.standard.is_none() {
                rec.standard = standard(v);
            }
        }
        (ATTR_FILE_NAME, Body::Resident(v)) => {
            if let Some((name, tag)) = file_name(v) {
                rec.names.push(name);
                if name_tag == 0 {
                    name_tag = tag;
                }
            }
        }
        (ATTR_DATA, body) if a.first() => {
            let wof = a.named
                && a.name.as_chunks::<2>().0.len() == WOF_STREAM.len()
                && a.name
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .eq(WOF_STREAM.encode_utf16());
            if wof {
                rec.wof = Some(match body {
                    Body::Resident(v) => (v.len() as u64).next_multiple_of(8),
                    Body::NonResident { on_disk, .. } => *on_disk,
                });
            } else if a.named {
                rec.streams = rec.streams.saturating_add(1);
            } else if rec.data.is_none() {
                let (size, allocated, resident) = match body {
                    Body::Resident(v) => (v.len() as u64, 0, true),
                    Body::NonResident { size, on_disk, .. } => (*size, *on_disk, false),
                };
                rec.data = Some(Sizes {
                    size,
                    allocated,
                    flags: a.flags,
                    resident,
                });
            }
        }
        (ATTR_REPARSE_POINT, Body::Resident(v)) => {
            if let Some(tag) = u32_at(v, 0) {
                rec.reparse = tag;
            }
        }
        (ATTR_ATTRIBUTE_LIST, Body::Resident(v)) => rec.list = list_entries(v),
        (ATTR_ATTRIBUTE_LIST, Body::NonResident { .. }) => rec.list_outside = true,
        _ => {}
    })?;
    // A non-resident reparse point's tag is still in its names.
    if rec.reparse == 0 {
        rec.reparse = name_tag;
    }
    Ok(rec)
}

/// One extent of a non-resident attribute: `clusters` clusters at `lcn`, or a sparse
/// hole when `lcn` is None.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

fn le_unsigned(b: &[u8]) -> u64 {
    b.iter().rev().fold(0, |v, &x| v << 8 | u64::from(x))
}

fn le_signed(b: &[u8]) -> i64 {
    let bits = 8 * b.len() as u32;
    let v = le_unsigned(b);
    if bits == 0 || bits >= 64 {
        v as i64
    } else {
        ((v << (64 - bits)) as i64) >> (64 - bits)
    }
}

/// Decodes a mapping-pairs array: each run is a header byte (low nibble: bytes of the
/// length, high nibble: bytes of the signed LCN delta, 0 for a hole), the length, the
/// delta; a 0 header ends it.
pub(crate) fn decode_runs(bytes: &[u8]) -> Result<Vec<Run>, MftError> {
    let bad = MftError::Runs;
    let mut runs = Vec::new();
    let mut at = 0;
    let mut lcn: i64 = 0;
    loop {
        let head = *bytes.get(at).ok_or(bad)?;
        if head == 0 {
            return Ok(runs);
        }
        let len_size = usize::from(head & 0x0F);
        let delta_size = usize::from(head >> 4);
        if len_size == 0 || len_size > 8 || delta_size > 8 {
            return Err(bad);
        }
        let clusters = le_unsigned(bytes.get(at + 1..at + 1 + len_size).ok_or(bad)?);
        if clusters == 0 || clusters > i64::MAX as u64 {
            return Err(bad);
        }
        let delta = bytes
            .get(at + 1 + len_size..at + 1 + len_size + delta_size)
            .ok_or(bad)?;
        let run_lcn = if delta_size == 0 {
            None
        } else {
            lcn = lcn.checked_add(le_signed(delta)).ok_or(bad)?;
            Some(u64::try_from(lcn).map_err(|_| bad)?)
        };
        runs.push(Run {
            lcn: run_lcn,
            clusters,
        });
        at += 1 + len_size + delta_size;
    }
}

/// The unnamed `$DATA` stream's size and data runs in a fixed-up record, all its
/// extents in this record in order.
pub(crate) fn data_runs(r: &[u8]) -> Result<(u64, Vec<Run>), MftError> {
    let (size, extents) = data_extents(r)?;
    let runs = extents.into_iter().flat_map(|(_, runs)| runs).collect();
    Ok((size.ok_or(MftError::Runs)?, runs))
}

/// Extents of a non-resident attribute: (start VCN, runs).
pub(crate) type Extents = Vec<(u64, Vec<Run>)>;

/// The extents of the unnamed `$DATA` stream that are in this fixed-up record, in
/// record order, and the stream's size when its first extent (VCN 0) is here. Record
/// 0 (the `$MFT` itself) and the extension records its `$ATTRIBUTE_LIST` names say
/// where the MFT lies on the volume.
pub(crate) fn data_extents(r: &[u8]) -> Result<(Option<u64>, Extents), MftError> {
    let mut size = None;
    let mut extents = Vec::new();
    let mut failed = None;
    each_attribute(r, |a| {
        if a.kind != ATTR_DATA || a.named || failed.is_some() {
            return;
        }
        if let Body::NonResident {
            start_vcn,
            size: s,
            runs: bytes,
            ..
        } = a.body
        {
            if start_vcn == 0 && size.is_none() {
                size = Some(s);
            }
            match decode_runs(bytes) {
                Ok(runs) => extents.push((start_vcn, runs)),
                Err(e) => failed = Some(e),
            }
        }
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    Ok((size, extents))
}

/// One file, merged from its base record and any extension records: what the index
/// stores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Entry {
    /// File reference: the record number, its sequence number in the high 16 bits.
    pub frn: u64,
    pub is_dir: bool,
    /// Every name in a folder (one per hard link), 8.3 aliases left out; the first is
    /// the one indexed.
    pub names: Vec<Name>,
    pub standard: Standard,
    /// The unnamed `$DATA` stream (zero for a folder).
    pub data: Sizes,
    /// Alternate data streams.
    pub streams: u16,
    pub reparse: u32,
    /// Bytes on disk of a WOF-compressed file's compressed contents, 0 when none.
    pub wof: u64,
}

impl Entry {
    /// Win32 attributes: `$STANDARD_INFORMATION`'s plus the directory bit.
    pub fn attributes(&self) -> u32 {
        let dir = if self.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            0
        };
        self.standard.attributes | dir
    }

    /// Hard links: names in folders, an 8.3 alias not counted.
    pub fn links(&self) -> u16 {
        u16::try_from(self.names.len()).unwrap_or(u16::MAX)
    }
}

/// Merges records read in any order into [`Entry`]s. A base record without an
/// attribute list is complete on its own; one with a list waits for its extension
/// records until [`Scan::finish`].
#[derive(Default)]
pub(crate) struct Scan {
    /// Base records with an attribute list, by record number.
    waiting: HashMap<u64, FileRecord>,
    /// Extension records, by their base's record number.
    extensions: HashMap<u64, Vec<FileRecord>>,
}

impl Scan {
    /// Parses record `number` (fixups applied in place). Returns its entry when the
    /// record is a complete file; `Ok(None)` for a free record, an extension record or
    /// a base that waits for its extensions.
    pub fn add(&mut self, number: u64, record: &mut [u8]) -> Result<Option<Entry>, MftError> {
        // Most of a big MFT can be free records: skip them before the fixups.
        if record.get(..4) == Some(&b"FILE"[..])
            && u16_at(record, 0x16).is_some_and(|flags| flags & RECORD_IN_USE == 0)
        {
            return Ok(None);
        }
        let rec = parse(record)?;
        Ok(self.add_parsed(number, rec))
    }

    pub fn add_parsed(&mut self, number: u64, rec: FileRecord) -> Option<Entry> {
        if !rec.in_use {
            return None;
        }
        let number = number & RECORD_MASK;
        if rec.base != 0 {
            let base = rec.base & RECORD_MASK;
            // A record naming itself as its base is damaged.
            if base != number {
                self.extensions.entry(base).or_default().push(rec);
            }
            return None;
        }
        if !rec.list.is_empty() || rec.list_outside {
            self.waiting.insert(number, rec);
            return None;
        }
        entry(number, rec, Vec::new())
    }

    /// The files that waited for extension records, by record number. Extension
    /// records whose base never came (or was reused since) are dropped.
    pub fn finish(self) -> Vec<Entry> {
        let Scan {
            waiting,
            mut extensions,
        } = self;
        let mut done: Vec<Entry> = waiting
            .into_iter()
            .filter_map(|(number, base)| {
                let ext = extensions.remove(&number).unwrap_or_default();
                entry(number, base, ext)
            })
            .collect();
        done.sort_by_key(|e| e.frn & RECORD_MASK);
        done
    }
}

/// Builds the entry of base record `number`; None when it has no name in a folder.
fn entry(number: u64, base: FileRecord, extensions: Vec<FileRecord>) -> Option<Entry> {
    let frn = number | u64::from(base.sequence) << 48;
    let mut names = base.names;
    let mut data = base.data;
    let mut streams = base.streams;
    let mut reparse = base.reparse;
    let mut wof = base.wof;
    for ext in extensions {
        // Left by an earlier file that used the same base record.
        if ext.base != frn {
            continue;
        }
        names.extend(ext.names);
        data = data.or(ext.data);
        streams = streams.saturating_add(ext.streams);
        wof = wof.or(ext.wof);
        if reparse == 0 {
            reparse = ext.reparse;
        }
    }
    if names.iter().any(|n| n.namespace != NAMESPACE_DOS) {
        names.retain(|n| n.namespace != NAMESPACE_DOS);
    }
    if names.is_empty() {
        return None;
    }
    Some(Entry {
        frn,
        is_dir: base.is_dir,
        names,
        standard: base.standard.unwrap_or_default(),
        data: data.unwrap_or_default(),
        streams,
        reparse,
        wof: wof.unwrap_or(0),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One attribute for [`record`].
    pub(crate) enum A {
        Standard(Standard),
        Name(u64, &'static str, u8),
        /// Unnamed resident `$DATA` of this many bytes.
        Resident(usize),
        /// Non-resident `$DATA`: name, start VCN, size, allocated, compressed/sparse
        /// total, flags, runs.
        NonResident(&'static str, u64, u64, u64, Option<u64>, u16, Vec<u8>),
        /// Named resident `$DATA` (an alternate data stream).
        Stream(&'static str, usize),
        Reparse(u32),
        List(Vec<ListEntry>),
        /// A non-resident `$ATTRIBUTE_LIST` (one cluster somewhere).
        ListOutside,
    }

    fn wide(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn resident(kind: u32, name: &str, value: &[u8]) -> Vec<u8> {
        let name = wide(name);
        let value_at = (0x18 + name.len()).div_ceil(8) * 8;
        let len = (value_at + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = (name.len() / 2) as u8;
        a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(value_at as u16).to_le_bytes());
        a[0x18..0x18 + name.len()].copy_from_slice(&name);
        a[value_at..value_at + value.len()].copy_from_slice(value);
        a
    }

    #[allow(clippy::too_many_arguments)]
    fn non_resident(
        kind: u32,
        name: &str,
        start_vcn: u64,
        size: u64,
        allocated: u64,
        total: Option<u64>,
        flags: u16,
        runs: &[u8],
    ) -> Vec<u8> {
        let name = wide(name);
        let header = if total.is_some() { 0x48 } else { 0x40 };
        let runs_at = (header + name.len()).div_ceil(8) * 8;
        let len = (runs_at + runs.len() + 1).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[9] = (name.len() / 2) as u8;
        a[0x0A..0x0C].copy_from_slice(&(header as u16).to_le_bytes());
        a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
        a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&(runs_at as u16).to_le_bytes());
        a[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&size.to_le_bytes());
        if let Some(total) = total {
            a[0x22..0x24].copy_from_slice(&4u16.to_le_bytes());
            a[0x40..0x48].copy_from_slice(&total.to_le_bytes());
        }
        a[header..header + name.len()].copy_from_slice(&name);
        a[runs_at..runs_at + runs.len()].copy_from_slice(runs);
        a
    }

    fn encode(a: &A) -> Vec<u8> {
        match a {
            A::Standard(s) => {
                let mut v = vec![0u8; 0x48];
                v[0..8].copy_from_slice(&s.created.to_le_bytes());
                v[8..16].copy_from_slice(&s.modified.to_le_bytes());
                v[16..24].copy_from_slice(&s.changed.to_le_bytes());
                v[24..32].copy_from_slice(&s.accessed.to_le_bytes());
                v[32..36].copy_from_slice(&s.attributes.to_le_bytes());
                resident(ATTR_STANDARD_INFORMATION, "", &v)
            }
            A::Name(parent, name, namespace) => {
                let w = wide(name);
                let mut v = vec![0u8; 0x42 + w.len()];
                v[0..8].copy_from_slice(&parent.to_le_bytes());
                v[0x40] = (w.len() / 2) as u8;
                v[0x41] = *namespace;
                v[0x42..].copy_from_slice(&w);
                resident(ATTR_FILE_NAME, "", &v)
            }
            A::Resident(n) => resident(ATTR_DATA, "", &vec![b'x'; *n]),
            A::NonResident(name, vcn, size, alloc, total, flags, runs) => {
                non_resident(ATTR_DATA, name, *vcn, *size, *alloc, *total, *flags, runs)
            }
            A::Stream(name, n) => resident(ATTR_DATA, name, &vec![b'y'; *n]),
            A::Reparse(tag) => {
                let mut v = tag.to_le_bytes().to_vec();
                v.extend([0u8; 4]);
                resident(ATTR_REPARSE_POINT, "", &v)
            }
            A::List(entries) => {
                let mut v = Vec::new();
                for e in entries {
                    let mut b = vec![0u8; 0x20];
                    b[0..4].copy_from_slice(&e.kind.to_le_bytes());
                    b[4..6].copy_from_slice(&0x20u16.to_le_bytes());
                    b[6] = u8::from(e.named);
                    b[8..16].copy_from_slice(&e.start_vcn.to_le_bytes());
                    b[16..24].copy_from_slice(&e.record.to_le_bytes());
                    v.extend(b);
                }
                resident(ATTR_ATTRIBUTE_LIST, "", &v)
            }
            A::ListOutside => non_resident(
                ATTR_ATTRIBUTE_LIST,
                "",
                0,
                4096,
                4096,
                None,
                0,
                &[0x11, 1, 0x40],
            ),
        }
    }

    /// A 1 KiB record as NTFS writes it: header, attributes, end marker, then the
    /// update sequence applied (each block's last two bytes saved and replaced).
    pub(crate) fn record(seq: u16, base: u64, flags: u16, attrs: &[A]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut at = 0x38;
        for a in attrs {
            let bytes = encode(a);
            r[at..at + bytes.len()].copy_from_slice(&bytes);
            at += bytes.len();
        }
        r[at..at + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        let used = at + 8;
        r[0x18..0x1C].copy_from_slice(&(used as u32).to_le_bytes());
        let usn = [0x2A, 0x00];
        r[0x30..0x32].copy_from_slice(&usn);
        for block in 0..2 {
            let end = (block + 1) * STRIDE - 2;
            let saved = 0x30 + 2 * (block + 1);
            let (lo, hi) = (r[end], r[end + 1]);
            r[saved] = lo;
            r[saved + 1] = hi;
            r[end..end + 2].copy_from_slice(&usn);
        }
        r
    }

    const IN_USE: u16 = RECORD_IN_USE;
    const DIR: u16 = RECORD_IN_USE | RECORD_IS_DIRECTORY;
    const ROOT: u64 = 5 | 5 << 48;

    fn times(n: u64) -> Standard {
        Standard {
            created: n,
            modified: n + 1,
            changed: n + 2,
            accessed: n + 3,
            attributes: 0x20,
        }
    }

    #[test]
    fn a_normal_file() {
        let mut r = record(
            7,
            0,
            IN_USE,
            &[
                A::Standard(times(1000)),
                A::Name(ROOT, "REPORT~1.PDF", NAMESPACE_DOS),
                A::Name(ROOT, "Report for the year.pdf", 1),
                A::NonResident("", 0, 10_000, 12_288, None, 0, vec![0x21, 3, 0x00, 0x10, 0]),
            ],
        );
        // The protected bytes really were replaced.
        assert_eq!(&r[510..512], &[0x2A, 0]);
        let rec = parse(&mut r).unwrap();
        assert!(rec.in_use && !rec.is_dir);
        assert_eq!(rec.sequence, 7);
        assert_eq!(rec.standard, Some(times(1000)));
        assert_eq!(rec.names.len(), 2);
        assert_eq!(
            rec.data,
            Some(Sizes {
                size: 10_000,
                allocated: 12_288,
                flags: 0,
                resident: false,
            })
        );
        let e = entry(40, rec, Vec::new()).unwrap();
        assert_eq!(e.frn, 40 | 7 << 48);
        assert_eq!(e.names.len(), 1, "the 8.3 alias is not a link");
        assert_eq!(e.names[0].name, "Report for the year.pdf");
        assert_eq!(e.names[0].parent, ROOT);
        assert_eq!(e.links(), 1);
        assert_eq!(e.attributes(), 0x20);
        assert_eq!(
            (e.data.size, e.data.allocated, e.streams),
            (10_000, 12_288, 0)
        );
    }

    #[test]
    fn small_files_and_folders() {
        let mut r = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(times(5)),
                A::Name(ROOT, "tiny.txt", 3),
                A::Resident(42),
            ],
        );
        let e = entry(41, parse(&mut r).unwrap(), Vec::new()).unwrap();
        assert_eq!(
            (e.data.size, e.data.allocated),
            (42, 0),
            "resident: 0 on disk"
        );
        assert!(e.data.resident);

        let mut r = record(
            2,
            0,
            DIR,
            &[A::Standard(times(9)), A::Name(ROOT, "Folder", 1)],
        );
        let e = entry(42, parse(&mut r).unwrap(), Vec::new()).unwrap();
        assert!(e.is_dir);
        assert_eq!(e.attributes(), 0x20 | FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(e.data, Sizes::default());
    }

    #[test]
    fn hard_links_are_every_name() {
        let mut r = record(
            3,
            0,
            IN_USE,
            &[
                A::Standard(times(1)),
                A::Name(ROOT, "one.txt", 1),
                A::Name(0x30 | 1 << 48, "two.txt", 0),
                A::Name(0x31 | 1 << 48, "three.txt", 3),
                A::Resident(3),
            ],
        );
        let e = entry(50, parse(&mut r).unwrap(), Vec::new()).unwrap();
        let names: Vec<(&str, u64)> = e
            .names
            .iter()
            .map(|n| (n.name.as_str(), n.parent))
            .collect();
        assert_eq!(
            names,
            [
                ("one.txt", ROOT),
                ("two.txt", 0x30 | 1 << 48),
                ("three.txt", 0x31 | 1 << 48)
            ]
        );
        assert_eq!(e.links(), 3);
    }

    #[test]
    fn alternate_streams_are_counted() {
        let mut r = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(times(1)),
                A::Name(ROOT, "download.zip", 1),
                A::NonResident("", 0, 5_000, 8_192, None, 0, vec![0x11, 2, 0x20, 0]),
                A::Stream("Zone.Identifier", 26),
                A::NonResident(
                    "big",
                    0,
                    1 << 20,
                    1 << 20,
                    None,
                    0,
                    vec![0x21, 0x10, 0, 0x30, 0],
                ),
                // A later extent of the same named stream is not another stream.
                A::NonResident("big", 256, 0, 0, None, 0, vec![0x11, 1, 0x50, 0]),
            ],
        );
        let e = entry(60, parse(&mut r).unwrap(), Vec::new()).unwrap();
        assert_eq!(e.streams, 2);
        assert_eq!(e.wof, 0);
        assert_eq!(
            e.data.size, 5_000,
            "named streams leave the file size alone"
        );
    }

    /// CompactOS / `compact /exe`: an empty sparse unnamed stream, the contents in
    /// `WofCompressedData`, which is no alternate stream of the user's.
    #[test]
    fn wof_compressed_files_keep_their_compressed_size() {
        let mut r = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(times(1)),
                A::Name(ROOT, "app.exe", 1),
                A::NonResident(
                    "",
                    0,
                    1 << 20,
                    1 << 20,
                    Some(0),
                    ATTR_SPARSE,
                    vec![0x01, 0x10, 0],
                ),
                A::NonResident(
                    "WofCompressedData",
                    0,
                    300_000,
                    303_104,
                    None,
                    0,
                    vec![0x21, 0x4A, 0x00, 0x10, 0],
                ),
                A::Reparse(IO_REPARSE_TAG_WOF),
            ],
        );
        let e = entry(61, parse(&mut r).unwrap(), Vec::new()).unwrap();
        assert_eq!((e.data.size, e.data.allocated), (1 << 20, 0));
        assert_eq!(
            (e.wof, e.streams, e.reparse),
            (303_104, 0, IO_REPARSE_TAG_WOF)
        );
    }

    #[test]
    fn sparse_and_compressed_count_clusters_in_use() {
        let mut r = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(Standard {
                    attributes: 0x200,
                    ..times(1)
                }),
                A::Name(ROOT, "sparse.vhd", 1),
                A::NonResident(
                    "",
                    0,
                    1 << 30,
                    1 << 30,
                    Some(65_536),
                    ATTR_SPARSE,
                    vec![0x11, 16, 0x10, 0x01, 0xF0, 0],
                ),
            ],
        );
        let rec = parse(&mut r).unwrap();
        let data = rec.data.unwrap();
        assert_eq!((data.size, data.allocated), (1 << 30, 65_536));
        assert_eq!(data.flags, ATTR_SPARSE);
        assert_eq!(entry(70, rec, Vec::new()).unwrap().attributes(), 0x200);

        let mut r = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(Standard {
                    attributes: 0x800,
                    ..times(1)
                }),
                A::Name(ROOT, "log.txt", 1),
                A::NonResident(
                    "",
                    0,
                    100_000,
                    131_072,
                    Some(40_960),
                    ATTR_COMPRESSED,
                    vec![0x11, 32, 0x10, 0],
                ),
            ],
        );
        let data = parse(&mut r).unwrap().data.unwrap();
        assert_eq!((data.size, data.allocated), (100_000, 40_960));
        assert_eq!(data.flags & ATTR_COMPRESSED, ATTR_COMPRESSED);
    }

    #[test]
    fn reparse_tags_from_the_attribute_or_the_name() {
        let mut r = record(
            1,
            0,
            DIR,
            &[
                A::Standard(times(1)),
                A::Name(ROOT, "link", 1),
                A::Reparse(0xA000_000C),
            ],
        );
        assert_eq!(parse(&mut r).unwrap().reparse, 0xA000_000C);

        // A $FILE_NAME with the reparse flag carries the tag too.
        let mut r = record(
            1,
            0,
            IN_USE,
            &[A::Standard(times(1)), A::Name(ROOT, "x", 1)],
        );
        let at = 0x38 + encode(&A::Standard(times(1))).len() + 0x18;
        r[at + 0x38..at + 0x3C].copy_from_slice(&0x400u32.to_le_bytes());
        r[at + 0x3C..at + 0x40].copy_from_slice(&0x8000_0017u32.to_le_bytes());
        assert_eq!(parse(&mut r).unwrap().reparse, 0x8000_0017);
    }

    #[test]
    fn attribute_lists_merge_extension_records() {
        let base_ref = 80 | 4 << 48;
        let list = vec![
            ListEntry {
                kind: ATTR_FILE_NAME,
                start_vcn: 0,
                record: 81 | 1 << 48,
                named: false,
            },
            ListEntry {
                kind: ATTR_DATA,
                start_vcn: 0,
                record: 82 | 1 << 48,
                named: false,
            },
        ];
        let mut base = record(
            4,
            0,
            IN_USE,
            &[
                A::Standard(times(1)),
                A::List(list.clone()),
                A::Name(ROOT, "fragmented.db", 1),
            ],
        );
        let mut names = record(
            1,
            base_ref,
            IN_USE,
            &[A::Name(0x99 | 1 << 48, "link.db", 1)],
        );
        let mut data = record(
            1,
            base_ref,
            IN_USE,
            &[
                A::NonResident(
                    "",
                    0,
                    9_000_000,
                    9_003_008,
                    None,
                    0,
                    vec![0x21, 0xFF, 0x10, 0x00, 0],
                ),
                A::Stream("meta", 4),
            ],
        );
        // Left behind by an earlier file in record 80 (sequence 3): ignored.
        let mut stale = record(1, 80 | 3 << 48, IN_USE, &[A::Name(ROOT, "old", 1)]);
        let mut scan = Scan::default();
        // The data extension arrives before its base.
        assert_eq!(scan.add(82, &mut data).unwrap(), None);
        assert_eq!(scan.add(80, &mut base).unwrap(), None);
        assert_eq!(scan.add(81, &mut names).unwrap(), None);
        assert_eq!(scan.add(83, &mut stale).unwrap(), None);
        let parsed = parse_fixed(&base).unwrap();
        assert_eq!(parsed.list, list);
        let done = scan.finish();
        assert_eq!(done.len(), 1);
        let e = &done[0];
        assert_eq!(e.frn, base_ref);
        let names: Vec<&str> = e.names.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["fragmented.db", "link.db"]);
        assert_eq!((e.data.size, e.data.allocated), (9_000_000, 9_003_008));
        assert_eq!(e.streams, 1);

        // A non-resident list (its entries are elsewhere on disk) waits the same way.
        let mut base = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(times(1)),
                A::ListOutside,
                A::Name(ROOT, "huge", 1),
            ],
        );
        let mut scan = Scan::default();
        assert_eq!(scan.add(90, &mut base).unwrap(), None);
        assert!(parse_fixed(&base).unwrap().list_outside);
        assert_eq!(scan.finish()[0].names[0].name, "huge");
    }

    #[test]
    fn free_and_nameless_records_give_no_entry() {
        let mut free = record(1, 0, 0, &[A::Standard(times(1)), A::Name(ROOT, "gone", 1)]);
        let mut scan = Scan::default();
        assert_eq!(scan.add(10, &mut free).unwrap(), None);
        let mut nameless = record(1, 0, IN_USE, &[A::Standard(times(1))]);
        assert_eq!(scan.add(11, &mut nameless).unwrap(), None);
        assert!(scan.finish().is_empty());
    }

    #[test]
    fn corrupt_fixups_are_rejected() {
        let good = record(
            1,
            0,
            IN_USE,
            &[A::Standard(times(1)), A::Name(ROOT, "a", 1)],
        );
        // A torn write: the second block's check bytes differ.
        let mut torn = good.clone();
        torn[1022] = 0x2B;
        assert_eq!(parse(&mut torn), Err(MftError::Fixup));
        let mut baad = good.clone();
        baad[0..4].copy_from_slice(b"BAAD");
        assert_eq!(parse(&mut baad), Err(MftError::Baad));
        let mut zero = vec![0u8; 1024];
        assert_eq!(parse(&mut zero), Err(MftError::Signature));
        // An update sequence array that does not fit the record's blocks.
        let mut count = good.clone();
        count[6] = 9;
        assert_eq!(parse(&mut count), Err(MftError::Header));
        let mut usa = good.clone();
        usa[4..6].copy_from_slice(&0x1FCu16.to_le_bytes());
        assert_eq!(parse(&mut usa), Err(MftError::Header));
        let mut ok = good;
        assert!(parse(&mut ok).is_ok());
    }

    #[test]
    fn truncated_records_and_attributes_are_rejected() {
        let good = record(
            1,
            0,
            IN_USE,
            &[A::Standard(times(1)), A::Name(ROOT, "a", 1)],
        );
        assert_eq!(parse(&mut good[..1000].to_vec()), Err(MftError::Size));
        assert_eq!(parse(&mut good[..512].to_vec()), Err(MftError::Header));
        assert_eq!(parse(&mut Vec::new()), Err(MftError::Size));
        assert_eq!(parse(&mut vec![0u8; MAX_RECORD + 512]), Err(MftError::Size));
        let mut fixed = good;
        fixup(&mut fixed).unwrap();
        // Used size past the record.
        let mut r = fixed.clone();
        r[0x18..0x1C].copy_from_slice(&2048u32.to_le_bytes());
        assert_eq!(parse_fixed(&r), Err(MftError::Header));
        // Used size cutting the attributes short.
        let mut r = fixed.clone();
        r[0x18..0x1C].copy_from_slice(&0x60u32.to_le_bytes());
        assert_eq!(parse_fixed(&r), Err(MftError::Attribute));
        // An attribute longer than the record, and one of length 0 (would loop).
        for len in [0x7FFF_FFF8u32, 0, 4] {
            let mut r = fixed.clone();
            r[0x3C..0x40].copy_from_slice(&len.to_le_bytes());
            assert_eq!(parse_fixed(&r), Err(MftError::Attribute), "length {len}");
        }
        // A resident value running past its attribute.
        let mut r = fixed.clone();
        r[0x38 + 0x10..0x38 + 0x14].copy_from_slice(&0x1000u32.to_le_bytes());
        assert_eq!(parse_fixed(&r), Err(MftError::Attribute));
        // A $FILE_NAME whose name length runs past its value: the name is skipped.
        let mut r = fixed;
        let name_at = 0x38 + encode(&A::Standard(times(1))).len();
        let value_at = name_at + 0x18;
        r[value_at + 0x40] = 200;
        assert!(parse_fixed(&r).unwrap().names.is_empty());
    }

    #[test]
    fn data_runs_decode_and_reject_garbage() {
        // 0x31: 1 length byte, 3 delta bytes. Then a backwards jump, then a hole.
        let runs = [
            0x31, 0x10, 0x00, 0x10, 0x00, 0x21, 0x08, 0x00, 0xF0, 0x01, 0x05, 0x00,
        ];
        assert_eq!(
            decode_runs(&runs).unwrap(),
            [
                Run {
                    lcn: Some(0x1000),
                    clusters: 0x10
                },
                Run {
                    lcn: Some(0x1000 - 0x1000),
                    clusters: 0x08
                },
                Run {
                    lcn: None,
                    clusters: 5
                },
            ]
        );
        // Missing terminator, oversized fields, a zero-length run, an LCN below 0.
        for bad in [
            &[0x11, 1, 2][..],
            &[0x19, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
            &[0x91, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
            &[0x11, 0, 5, 0],
            &[0x11, 1, 0xFF, 0],
            &[
                0x81, 1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F, 0x81, 1, 0xFF, 0xFF, 0xFF,
                0xFF, 0xFF, 0xFF, 0xFF, 0x7F, 0,
            ],
        ] {
            assert_eq!(decode_runs(bad), Err(MftError::Runs), "{bad:?}");
        }
        let mut r = record(
            1,
            0,
            IN_USE,
            &[
                A::Standard(times(1)),
                A::Name(ROOT, "$MFT", 3),
                A::NonResident(
                    "",
                    0,
                    1 << 20,
                    1 << 20,
                    None,
                    0,
                    vec![0x21, 0x40, 0x00, 0x0C, 0],
                ),
                A::NonResident("", 0x40, 0, 0, None, 0, vec![0x21, 0x40, 0x00, 0x20, 0]),
            ],
        );
        fixup(&mut r).unwrap();
        let (size, runs) = data_runs(&r).unwrap();
        assert_eq!(size, 1 << 20);
        let lcns: Vec<Option<u64>> = runs.iter().map(|r| r.lcn).collect();
        assert_eq!(lcns, [Some(0xC00), Some(0x2000)]);
    }

    #[test]
    fn hostile_bytes_never_panic() {
        // Every single-byte change of a full-featured record, over each field.
        let base = record(
            9,
            0,
            DIR,
            &[
                A::Standard(times(1)),
                A::List(vec![ListEntry {
                    kind: ATTR_DATA,
                    start_vcn: 0,
                    record: 3 | 1 << 48,
                    named: true,
                }]),
                A::Name(ROOT, "Ünïcødé", 1),
                A::Stream("s", 3),
                A::NonResident("", 0, 9, 9, Some(9), ATTR_SPARSE, vec![0x11, 1, 1, 0]),
                A::Reparse(1),
            ],
        );
        let used = 0x200;
        for at in 0..used {
            for value in [0x00, 0x01, 0x7F, 0x80, 0xFF] {
                let mut r = base.clone();
                r[at] = value;
                let _ = parse(&mut r.clone());
                let mut scan = Scan::default();
                let _ = scan.add(at as u64, &mut r);
                let _ = scan.finish();
            }
        }
    }
}
