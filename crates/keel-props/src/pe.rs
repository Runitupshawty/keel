//! PE version resources (`VS_VERSIONINFO`): FileVersion, ProductVersion, ProductName,
//! CompanyName, LegalCopyright, LegalTrademarks, FileDescription and the rest of the
//! StringFileInfo table, plus the fixed (numeric) file and product versions.
//!
//! Bounded: only the headers, the resource directory path to `RT_VERSION` and the version
//! resource itself are read (at most [`MAX_VERSION_BYTES`]), never the whole image.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};

/// Largest version resource read (real ones are 1 to 4 KiB).
pub const MAX_VERSION_BYTES: usize = 64 * 1024;
const MAX_SECTIONS: usize = 96;
const MAX_DIR_ENTRIES: usize = 4096;
const RT_VERSION: u32 = 16;

/// Why a file's version resource could not be read.
#[derive(Debug)]
pub enum PeError {
    Io(io::Error),
    /// Not a PE image.
    NotPe,
    /// A PE image without a version resource.
    NoVersion,
    /// A header or resource contradicts itself or runs past the end of the file.
    Malformed(&'static str),
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::Io(e) => write!(f, "{e}"),
            PeError::NotPe => f.write_str("not a PE image"),
            PeError::NoVersion => f.write_str("no version resource"),
            PeError::Malformed(w) => write!(f, "malformed PE image: {w}"),
        }
    }
}

impl std::error::Error for PeError {}

impl From<io::Error> for PeError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            PeError::Malformed("truncated")
        } else {
            PeError::Io(e)
        }
    }
}

/// A version resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionInfo {
    /// `VS_FIXEDFILEINFO`'s file version, `a.b.c.d`.
    pub fixed_file_version: Option<String>,
    /// `VS_FIXEDFILEINFO`'s product version, `a.b.c.d`.
    pub fixed_product_version: Option<String>,
    /// StringFileInfo tables: (language and code page, e.g. `040904b0`; entries).
    pub tables: Vec<(String, Vec<(String, String)>)>,
}

impl VersionInfo {
    /// A StringFileInfo value by key (`CompanyName`), from the US English table when there is
    /// one, else the first table that has it.
    pub fn string(&self, key: &str) -> Option<&str> {
        fn pick<'a>(t: &'a (String, Vec<(String, String)>), key: &str) -> Option<&'a str> {
            t.1.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
                .filter(|v| !v.trim().is_empty())
        }
        self.tables
            .iter()
            .filter(|t| t.0.to_ascii_lowercase().starts_with("0409"))
            .find_map(|t| pick(t, key))
            .or_else(|| self.tables.iter().find_map(|t| pick(t, key)))
    }

    /// The file version: the fixed one, else the `FileVersion` string.
    pub fn file_version(&self) -> Option<String> {
        self.fixed_file_version
            .clone()
            .or_else(|| self.string("FileVersion").map(str::to_owned))
    }

    /// The product version: the `ProductVersion` string (as shown by Explorer), else the
    /// fixed one.
    pub fn product_version(&self) -> Option<String> {
        self.string("ProductVersion")
            .map(str::to_owned)
            .or_else(|| self.fixed_product_version.clone())
    }
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_at<R: Read + Seek>(r: &mut R, len: u64, off: u64, n: usize) -> Result<Vec<u8>, PeError> {
    if off.checked_add(n as u64).is_none_or(|end| end > len) {
        return Err(PeError::Malformed("past the end of the file"));
    }
    r.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

struct Section {
    va: u32,
    vsize: u32,
    raw_size: u32,
    raw_ptr: u32,
}

fn rva_to_offset(sections: &[Section], rva: u32) -> Option<u64> {
    sections.iter().find_map(|s| {
        let span = s.vsize.max(s.raw_size);
        let delta = rva.checked_sub(s.va)?;
        (delta < span && delta < s.raw_size).then(|| u64::from(s.raw_ptr) + u64::from(delta))
    })
}

/// Reads the version resource of the PE image in `r` (`len` bytes long).
pub fn version_info<R: Read + Seek>(r: &mut R, len: u64) -> Result<VersionInfo, PeError> {
    let dos = read_at(r, len, 0, 64).map_err(|_| PeError::NotPe)?;
    if &dos[..2] != b"MZ" {
        return Err(PeError::NotPe);
    }
    let pe_off = u64::from(u32_at(&dos, 0x3c).unwrap_or(0));
    let coff = read_at(r, len, pe_off, 24).map_err(|_| PeError::NotPe)?;
    if &coff[..4] != b"PE\0\0" {
        return Err(PeError::NotPe);
    }
    let nsections = usize::from(u16_at(&coff, 6).unwrap_or(0));
    let opt_size = usize::from(u16_at(&coff, 20).unwrap_or(0));
    if nsections > MAX_SECTIONS {
        return Err(PeError::Malformed("too many sections"));
    }
    let opt = read_at(r, len, pe_off + 24, opt_size)?;
    let dirs_at = match u16_at(&opt, 0) {
        Some(0x10b) => 96,
        Some(0x20b) => 112,
        _ => return Err(PeError::Malformed("optional header magic")),
    };
    let ndirs = u32_at(&opt, dirs_at - 4).unwrap_or(0);
    if ndirs <= 2 {
        return Err(PeError::NoVersion);
    }
    let res_rva = u32_at(&opt, dirs_at + 16).unwrap_or(0);
    if res_rva == 0 {
        return Err(PeError::NoVersion);
    }
    let table = read_at(r, len, pe_off + 24 + opt_size as u64, nsections * 40)?;
    let sections: Vec<Section> = table
        .chunks_exact(40)
        .map(|s| Section {
            vsize: u32_at(s, 8).unwrap_or(0),
            va: u32_at(s, 12).unwrap_or(0),
            raw_size: u32_at(s, 16).unwrap_or(0),
            raw_ptr: u32_at(s, 20).unwrap_or(0),
        })
        .collect();
    let base = rva_to_offset(&sections, res_rva).ok_or(PeError::Malformed("resource RVA"))?;

    // Level 1: the type (RT_VERSION); level 2: the first name; level 3: US English, else the
    // first language. Each level must be a subdirectory, except the last.
    let types = dir_entries(r, len, base, 0)?;
    let (_, by_name) = types
        .into_iter()
        .find(|&(id, off)| id == RT_VERSION && off & 0x8000_0000 != 0)
        .ok_or(PeError::NoVersion)?;
    let names = dir_entries(r, len, base, by_name & 0x7fff_ffff)?;
    let (_, by_lang) = names
        .into_iter()
        .find(|&(_, off)| off & 0x8000_0000 != 0)
        .ok_or(PeError::NoVersion)?;
    let langs = dir_entries(r, len, base, by_lang & 0x7fff_ffff)?;
    let leaves = || langs.iter().filter(|(_, off)| off & 0x8000_0000 == 0);
    let (_, data_entry) = leaves()
        .find(|(id, _)| *id == 0x409)
        .or_else(|| leaves().next())
        .copied()
        .ok_or(PeError::NoVersion)?;
    let entry = read_at(r, len, base + u64::from(data_entry), 16)?;
    let data_rva = u32_at(&entry, 0).unwrap_or(0);
    let size = u32_at(&entry, 4).unwrap_or(0) as usize;
    if size > MAX_VERSION_BYTES {
        return Err(PeError::Malformed("version resource too large"));
    }
    let off = rva_to_offset(&sections, data_rva).ok_or(PeError::Malformed("version RVA"))?;
    let data = read_at(r, len, off, size)?;
    parse_version_resource(&data)
}

/// A resource directory's entries: (name or id, offset to data or subdirectory).
fn dir_entries<R: Read + Seek>(
    r: &mut R,
    len: u64,
    base: u64,
    off: u32,
) -> Result<Vec<(u32, u32)>, PeError> {
    let at = base + u64::from(off);
    let head = read_at(r, len, at, 16)?;
    let n =
        usize::from(u16_at(&head, 12).unwrap_or(0)) + usize::from(u16_at(&head, 14).unwrap_or(0));
    if n > MAX_DIR_ENTRIES {
        return Err(PeError::Malformed("resource directory too large"));
    }
    let raw = read_at(r, len, at + 16, n * 8)?;
    Ok(raw
        .chunks_exact(8)
        .map(|e| (u32_at(e, 0).unwrap_or(0), u32_at(e, 4).unwrap_or(0)))
        .collect())
}

/// One `VS_VERSIONINFO`-style block: key, value bytes, children bytes, total length.
struct Block<'a> {
    key: String,
    value: &'a [u8],
    children: &'a [u8],
    len: usize,
}

fn align4(n: usize) -> usize {
    n.saturating_add(3) & !3
}

fn block(b: &[u8]) -> Option<Block<'_>> {
    let total = usize::from(u16_at(b, 0)?);
    if total < 6 {
        return None;
    }
    let b = &b[..total.min(b.len())];
    let value_len = usize::from(u16_at(b, 2)?);
    let value_type = u16_at(b, 4)?;
    let mut units = Vec::new();
    let mut pos = 6;
    loop {
        let u = u16_at(b, pos)?;
        pos += 2;
        if u == 0 {
            break;
        }
        units.push(u);
    }
    let key = String::from_utf16_lossy(&units);
    let value_start = align4(pos).min(b.len());
    // Text values count words; some linkers write bytes, so the value runs to the block's
    // end for text and is cut at its NUL when decoded.
    let value_bytes = if value_type == 1 {
        value_len * 2
    } else {
        value_len
    };
    let value_end = value_start.saturating_add(value_bytes).min(b.len());
    let children_start = align4(value_end).min(b.len());
    let value = if value_type == 1 {
        &b[value_start..]
    } else {
        &b[value_start..value_end]
    };
    Some(Block {
        key,
        value,
        children: &b[children_start..],
        len: total,
    })
}

/// The blocks inside `b`, in order.
fn children(b: &[u8]) -> impl Iterator<Item = Block<'_>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let child = block(b.get(pos..)?)?;
        pos = pos.saturating_add(align4(child.len));
        Some(child)
    })
}

fn utf16_value(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units).trim().to_owned()
}

fn version4(ms: u32, ls: u32) -> String {
    format!("{}.{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff)
}

/// Parses a `VS_VERSIONINFO` resource.
pub fn parse_version_resource(b: &[u8]) -> Result<VersionInfo, PeError> {
    let root = block(b).ok_or(PeError::Malformed("version block"))?;
    if root.key != "VS_VERSION_INFO" {
        return Err(PeError::Malformed("version block key"));
    }
    let mut out = VersionInfo::default();
    if u32_at(root.value, 0) == Some(0xfeef_04bd) {
        let v = |o| u32_at(root.value, o).unwrap_or(0);
        out.fixed_file_version = Some(version4(v(8), v(12)));
        out.fixed_product_version = Some(version4(v(16), v(20)));
    }
    for info in children(root.children) {
        if info.key != "StringFileInfo" {
            continue;
        }
        for table in children(info.children) {
            let entries: Vec<(String, String)> = children(table.children)
                .map(|s| (s.key, utf16_value(s.value)))
                .take(256)
                .collect();
            out.tables.push((table.key, entries));
            if out.tables.len() >= 16 {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use std::io::Cursor;

    fn read(b: &[u8]) -> Result<VersionInfo, PeError> {
        version_info(&mut Cursor::new(b), b.len() as u64)
    }

    #[test]
    fn reads_strings_and_fixed_versions() {
        for pe32_plus in [false, true] {
            let exe = fixtures::pe_with_version(pe32_plus, &fixtures::VERSION_STRINGS);
            let v = read(&exe).unwrap();
            assert_eq!(v.fixed_file_version.as_deref(), Some("1.2.3.4"));
            assert_eq!(v.file_version().as_deref(), Some("1.2.3.4"));
            assert_eq!(v.product_version().as_deref(), Some("1.2 beta"));
            assert_eq!(v.string("ProductName"), Some("Keel Test Tool"));
            assert_eq!(v.string("CompanyName"), Some("Example Corp"));
            assert_eq!(v.string("LegalCopyright"), Some("(c) Example Corp"));
            assert_eq!(v.string("LegalTrademarks"), Some("Example is a mark"));
            assert_eq!(v.string("FileDescription"), Some("A test tool"));
            assert_eq!(v.string("companyname"), Some("Example Corp"));
            assert_eq!(v.string("Missing"), None);
        }
    }

    #[test]
    fn not_pe_and_no_version() {
        assert!(matches!(read(b"hello"), Err(PeError::NotPe)));
        let exe = fixtures::pe_with_version(false, &[]);
        // A version block with no strings still has the fixed version.
        assert_eq!(
            read(&exe).unwrap().fixed_product_version.as_deref(),
            Some("1.2.3.4")
        );
        let bare = fixtures::pe_without_resources();
        assert!(matches!(read(&bare), Err(PeError::NoVersion)));
    }

    #[test]
    fn truncations_never_panic() {
        let exe = fixtures::pe_with_version(true, &fixtures::VERSION_STRINGS);
        for n in 0..exe.len() {
            let _ = read(&exe[..n]);
        }
        let mut bad = exe.clone();
        for i in (0..bad.len()).step_by(7) {
            bad[i] ^= 0xa5;
            let _ = read(&bad);
        }
    }
}
