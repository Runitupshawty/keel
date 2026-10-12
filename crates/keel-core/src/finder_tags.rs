//! Finder tags (Task 67): the eight Finder tag colors, the `com.apple.metadata:_kMDItemUserTags`
//! extended attribute macOS keeps a file's tags in, and how those native tags meet the
//! library's.
//!
//! The attribute holds a binary property list (`bplist00`): an array of strings, each a tag
//! name followed by a newline and the color's Finder label number (`"Red\n6"`, `"Work\n0"`).
//! Keel reads and writes that one shape with the small codec below (no plist crate in the
//! build for every platform); the reader is bounds-checked everywhere and fuzzed
//! (`fuzz::finder_tags`). Reading and writing the attribute itself is macOS only
//! ([`read_native`], [`write_native`]); elsewhere tags live in the library alone.

use anyhow::Result;

/// The attribute macOS stores a file's tags in.
pub const XATTR: &str = "com.apple.metadata:_kMDItemUserTags";
/// Most tags read from one attribute (Finder offers far fewer).
const MAX_TAGS: usize = 4096;
/// Largest attribute read.
pub const MAX_XATTR: usize = 1 << 20;

/// A Finder tag color; the discriminant is Finder's label number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FinderColor {
    None = 0,
    Gray = 1,
    Green = 2,
    Purple = 3,
    Blue = 4,
    Yellow = 5,
    Red = 6,
    Orange = 7,
}

impl FinderColor {
    /// The order Finder's menus show them in.
    pub const MENU: [FinderColor; 8] = [
        FinderColor::Red,
        FinderColor::Orange,
        FinderColor::Yellow,
        FinderColor::Green,
        FinderColor::Blue,
        FinderColor::Purple,
        FinderColor::Gray,
        FinderColor::None,
    ];

    /// The seven colors with a dot (the context menu's row).
    pub fn colored() -> impl Iterator<Item = FinderColor> {
        Self::MENU.into_iter().filter(|c| *c != FinderColor::None)
    }

    pub fn name(self) -> &'static str {
        match self {
            FinderColor::None => "None",
            FinderColor::Gray => "Gray",
            FinderColor::Green => "Green",
            FinderColor::Purple => "Purple",
            FinderColor::Blue => "Blue",
            FinderColor::Yellow => "Yellow",
            FinderColor::Red => "Red",
            FinderColor::Orange => "Orange",
        }
    }

    /// The color as the library stores it (`#rrggbb`); None for no color.
    pub fn hex(self) -> Option<&'static str> {
        Some(match self {
            FinderColor::None => return None,
            FinderColor::Gray => "#8e8e93",
            FinderColor::Green => "#34c759",
            FinderColor::Purple => "#af52de",
            FinderColor::Blue => "#007aff",
            FinderColor::Yellow => "#ffcc00",
            FinderColor::Red => "#ff3b30",
            FinderColor::Orange => "#ff9500",
        })
    }

    /// Finder's label number (0 is no color).
    pub fn label(self) -> u8 {
        self as u8
    }

    pub fn from_label(n: u8) -> Option<FinderColor> {
        Some(match n {
            0 => FinderColor::None,
            1 => FinderColor::Gray,
            2 => FinderColor::Green,
            3 => FinderColor::Purple,
            4 => FinderColor::Blue,
            5 => FinderColor::Yellow,
            6 => FinderColor::Red,
            7 => FinderColor::Orange,
            _ => return None,
        })
    }

    /// The Finder color stored as exactly `hex` (either case).
    pub fn of_hex(hex: &str) -> Option<FinderColor> {
        Self::colored().find(|c| c.hex().is_some_and(|h| h.eq_ignore_ascii_case(hex)))
    }

    /// The Finder color closest to a library color (`#rrggbb`; anything else, or none, is
    /// no color): a tag colored before Finder colors existed still gets a dot in Finder.
    pub fn nearest(color: Option<&str>) -> FinderColor {
        let Some(rgb) = color.and_then(rgb) else {
            return FinderColor::None;
        };
        if let Some(c) = color.and_then(Self::of_hex) {
            return c;
        }
        let [r, g, b] = rgb.map(i32::from);
        // Grays (little saturation) are gray, whatever their brightness.
        if r.max(g).max(b) - r.min(g).min(b) < 24 {
            return FinderColor::Gray;
        }
        let dist = |c: FinderColor| {
            let [cr, cg, cb] = c.hex().and_then(rgb).unwrap_or_default().map(i32::from);
            (r - cr).pow(2) + (g - cg).pow(2) + (b - cb).pow(2)
        };
        Self::colored()
            .filter(|c| *c != FinderColor::Gray)
            .min_by_key(|c| dist(*c))
            .unwrap_or(FinderColor::None)
    }
}

fn rgb(hex: &str) -> Option<[u8; 3]> {
    let h = hex.strip_prefix('#').filter(|h| h.len() == 6)?;
    let b = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    Some([b(0)?, b(2)?, b(4)?])
}

/// One native tag: its name and Finder color.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FinderTag {
    pub name: String,
    pub color: FinderColor,
}

impl FinderTag {
    pub fn new(name: impl Into<String>, color: FinderColor) -> Self {
        Self {
            name: name.into(),
            color,
        }
    }

    /// `"Red\n6"`: the name, a newline and the label number (also for no color, so a name
    /// that itself ends in a newline and a digit reads back unchanged).
    fn text(&self) -> String {
        format!("{}\n{}", self.name, self.color.label())
    }

    /// A string of the attribute: `"Name\nN"`, or a bare name (no color).
    fn parse(s: &str) -> Option<FinderTag> {
        let (name, color) = match s.rsplit_once('\n') {
            Some((name, n)) if n.len() == 1 => {
                match n.parse().ok().and_then(FinderColor::from_label) {
                    Some(c) => (name, c),
                    None => (s, FinderColor::None),
                }
            }
            _ => (s, FinderColor::None),
        };
        (!name.is_empty()).then(|| FinderTag::new(name, color))
    }
}

/// The attribute's bytes for `tags`.
pub fn encode(tags: &[FinderTag]) -> Vec<u8> {
    let texts: Vec<String> = tags.iter().map(FinderTag::text).collect();
    bplist::write_strings(&texts)
}

/// The tags in an attribute's bytes (empty names are skipped).
pub fn decode(data: &[u8]) -> Result<Vec<FinderTag>> {
    Ok(bplist::read_strings(data)?
        .iter()
        .filter_map(|s| FinderTag::parse(s))
        .collect())
}

/// Same name, ignoring case (Finder's tag names are case-insensitive).
pub fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// A file's tags as one list: the library's (`(name, color)`, in their order, each with
/// its nearest Finder color), then the native tags whose names the library does not have.
pub fn merge(library: &[(String, Option<String>)], native: &[FinderTag]) -> Vec<FinderTag> {
    let mut out: Vec<FinderTag> = library
        .iter()
        .map(|(name, color)| FinderTag::new(name.clone(), FinderColor::nearest(color.as_deref())))
        .collect();
    for t in native {
        if !out.iter().any(|o| same_name(&o.name, &t.name)) {
            out.push(t.clone());
        }
    }
    out
}

/// `tags` with the tag `name` (any case) set or cleared: an existing one keeps its place
/// and takes `color`; a new one goes last.
pub fn toggled(tags: &[FinderTag], name: &str, color: FinderColor, on: bool) -> Vec<FinderTag> {
    let mut out: Vec<FinderTag> = Vec::with_capacity(tags.len() + 1);
    let mut found = false;
    for t in tags {
        if same_name(&t.name, name) {
            if on && !found {
                out.push(FinderTag::new(t.name.clone(), color));
            }
            found = true;
        } else {
            out.push(t.clone());
        }
    }
    if on && !found {
        out.push(FinderTag::new(name, color));
    }
    out
}

/// A local file's native tags (none when it has no tag attribute).
#[cfg(target_os = "macos")]
pub fn read_native(path: &std::path::Path) -> std::io::Result<Vec<FinderTag>> {
    use std::io::{Error, ErrorKind};
    let Some(data) = xattr::get(path)? else {
        return Ok(Vec::new());
    };
    decode(&data).map_err(|e| Error::new(ErrorKind::InvalidData, format!("{e:#}")))
}

/// Replaces a local file's native tags (no tags removes the attribute).
#[cfg(target_os = "macos")]
pub fn write_native(path: &std::path::Path, tags: &[FinderTag]) -> std::io::Result<()> {
    if tags.is_empty() {
        xattr::remove(path)
    } else {
        xattr::set(path, &encode(tags))
    }
}

#[cfg(target_os = "macos")]
mod xattr {
    use std::ffi::CString;
    use std::io::{Error, ErrorKind, Result};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn c_path(path: &Path) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "a path with a NUL byte"))
    }

    fn c_name() -> CString {
        CString::new(super::XATTR).expect("no NUL in the attribute name")
    }

    fn missing(e: &Error) -> bool {
        e.raw_os_error() == Some(libc::ENOATTR)
    }

    pub fn get(path: &Path) -> Result<Option<Vec<u8>>> {
        let (p, n) = (c_path(path)?, c_name());
        let read = |buf: &mut [u8]| {
            let at = match buf.is_empty() {
                true => std::ptr::null_mut(),
                false => buf.as_mut_ptr().cast(),
            };
            // SAFETY: NUL-terminated strings; `at` is null (asking the size) or `buf`.
            unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), at, buf.len(), 0, 0) }
        };
        let len = read(&mut []);
        if len < 0 {
            let e = Error::last_os_error();
            return if missing(&e) { Ok(None) } else { Err(e) };
        }
        let len = len as usize;
        if len > super::MAX_XATTR {
            return Err(Error::new(ErrorKind::InvalidData, "tag attribute too large"));
        }
        let mut buf = vec![0u8; len];
        let got = read(&mut buf);
        if got < 0 {
            let e = Error::last_os_error();
            return if missing(&e) { Ok(None) } else { Err(e) };
        }
        buf.truncate(got as usize);
        Ok(Some(buf))
    }

    pub fn set(path: &Path, data: &[u8]) -> Result<()> {
        let (p, n) = (c_path(path)?, c_name());
        let value = data.as_ptr().cast();
        // SAFETY: NUL-terminated strings; `value` holds `data.len()` bytes.
        let rc = unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), value, data.len(), 0, 0) };
        if rc < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    pub fn remove(path: &Path) -> Result<()> {
        let (p, n) = (c_path(path)?, c_name());
        // SAFETY: both strings are NUL-terminated.
        let rc = unsafe { libc::removexattr(p.as_ptr(), n.as_ptr(), 0) };
        if rc < 0 {
            let e = Error::last_os_error();
            if !missing(&e) {
                return Err(e);
            }
        }
        Ok(())
    }
}

/// The part of Apple's binary property list format (`bplist00`) the tag attribute uses: a
/// top-level array of strings (ASCII or UTF-16).
pub mod bplist {
    use anyhow::{bail, ensure, Context, Result};

    const MAGIC: &[u8] = b"bplist0";
    const TRAILER: usize = 32;

    fn width(v: u64) -> usize {
        match v {
            0..=0xFF => 1,
            0x100..=0xFFFF => 2,
            0x1_0000..=0xFFFF_FFFF => 4,
            _ => 8,
        }
    }

    fn push_uint(out: &mut Vec<u8>, v: u64, size: usize) {
        out.extend_from_slice(&v.to_be_bytes()[8 - size..]);
    }

    /// An object marker with its count (15 and up follow as an int object).
    fn push_marker(out: &mut Vec<u8>, kind: u8, count: usize) {
        if count < 15 {
            out.push((kind << 4) | count as u8);
        } else {
            out.push((kind << 4) | 0xF);
            let size = width(count as u64);
            out.push(0x10 | size.trailing_zeros() as u8);
            push_uint(out, count as u64, size);
        }
    }

    /// A binary plist whose top object is an array of `items`.
    pub fn write_strings<S: AsRef<str>>(items: &[S]) -> Vec<u8> {
        let objects = items.len() + 1;
        let refs = width(objects as u64);
        let mut out = b"bplist00".to_vec();
        let mut offsets = Vec::with_capacity(objects);
        offsets.push(out.len() as u64);
        push_marker(&mut out, 0xA, items.len());
        for i in 1..objects {
            push_uint(&mut out, i as u64, refs);
        }
        for s in items {
            let s = s.as_ref();
            offsets.push(out.len() as u64);
            if s.is_ascii() {
                push_marker(&mut out, 0x5, s.len());
                out.extend_from_slice(s.as_bytes());
            } else {
                let units: Vec<u16> = s.encode_utf16().collect();
                push_marker(&mut out, 0x6, units.len());
                for u in units {
                    out.extend_from_slice(&u.to_be_bytes());
                }
            }
        }
        let table = out.len() as u64;
        let offset_size = width(table);
        for o in offsets {
            push_uint(&mut out, o, offset_size);
        }
        out.extend_from_slice(&[0; 6]);
        out.push(offset_size as u8);
        out.push(refs as u8);
        out.extend_from_slice(&(objects as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&table.to_be_bytes());
        out
    }

    fn uint(data: &[u8], at: usize, size: usize) -> Result<u64> {
        let end = at.checked_add(size).context("offset overflow")?;
        let bytes = data.get(at..end).context("truncated plist")?;
        Ok(bytes.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b)))
    }

    struct Doc<'a> {
        data: &'a [u8],
        offset_size: usize,
        ref_size: usize,
        objects: u64,
        table: usize,
    }

    impl Doc<'_> {
        /// Where object `i` starts (inside the object area).
        fn offset(&self, i: u64) -> Result<usize> {
            ensure!(i < self.objects, "object {i} out of range");
            let at = (i as usize)
                .checked_mul(self.offset_size)
                .and_then(|o| o.checked_add(self.table))
                .context("offset overflow")?;
            let o = uint(self.data, at, self.offset_size)?;
            ensure!(
                o >= 8 && o < self.table as u64,
                "object offset {o} outside the objects"
            );
            Ok(o as usize)
        }

        /// The count of the object whose marker is at `at`, and where its body starts.
        fn count(&self, at: usize) -> Result<(usize, usize)> {
            let low = self.data[at] & 0xF;
            if low != 0xF {
                return Ok((usize::from(low), at + 1));
            }
            let int = *self.data.get(at + 1).context("truncated count")?;
            ensure!(int >> 4 == 0x1 && int & 0xF <= 3, "bad count marker {int:#x}");
            let size = 1usize << (int & 0xF);
            let n = uint(self.data, at + 2, size)?;
            let n = usize::try_from(n).context("count overflow")?;
            Ok((n, at + 2 + size))
        }

        /// The bytes `[at, at + len)` inside the object area.
        fn body(&self, at: usize, len: usize) -> Result<&[u8]> {
            let end = at.checked_add(len).context("length overflow")?;
            ensure!(end <= self.table, "object runs past the objects");
            Ok(&self.data[at..end])
        }

        fn string(&self, i: u64) -> Result<String> {
            let at = self.offset(i)?;
            let (n, body) = self.count(at)?;
            match self.data[at] >> 4 {
                0x5 => Ok(String::from_utf8_lossy(self.body(body, n)?).into_owned()),
                0x6 => {
                    let bytes = self.body(body, n.checked_mul(2).context("length overflow")?)?;
                    let units: Vec<u16> = bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect();
                    Ok(String::from_utf16_lossy(&units))
                }
                m => bail!("object {i} is not a string (marker {m:#x})"),
            }
        }
    }

    /// The strings of a binary plist whose top object is an array of strings.
    pub fn read_strings(data: &[u8]) -> Result<Vec<String>> {
        ensure!(
            data.len() >= 8 + TRAILER && data.starts_with(MAGIC),
            "not a binary plist"
        );
        let t = &data[data.len() - TRAILER..];
        let offset_size = usize::from(t[6]);
        let ref_size = usize::from(t[7]);
        ensure!(
            (1..=8).contains(&offset_size) && (1..=8).contains(&ref_size),
            "bad plist trailer"
        );
        let objects = uint(t, 8, 8)?;
        let top = uint(t, 16, 8)?;
        let table = uint(t, 24, 8)?;
        let area = (data.len() - TRAILER) as u64;
        ensure!(
            objects >= 1 && top < objects && table >= 9 && table <= area,
            "bad plist trailer"
        );
        let table_len = objects
            .checked_mul(offset_size as u64)
            .context("offset table overflow")?;
        ensure!(
            table.checked_add(table_len).is_some_and(|end| end <= area),
            "offset table runs past the end"
        );
        let doc = Doc {
            data,
            offset_size,
            ref_size,
            objects,
            table: table as usize,
        };
        let at = doc.offset(top)?;
        ensure!(data[at] >> 4 == 0xA, "the tags are not an array");
        let (n, refs) = doc.count(at)?;
        ensure!(n <= MAX_TAGS, "too many tags ({n})");
        let refs_len = n.checked_mul(ref_size).context("array overflow")?;
        doc.body(refs, refs_len)?;
        (0..n)
            .map(|k| doc.string(uint(data, refs + k * ref_size, ref_size)?))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// `plistlib.dumps(["Red\n6", "Work\n0", "Café\n4"], fmt=FMT_BINARY)` (Python).
    const PYTHON: &str = "62706c6973743030a3010203555265640a3656576f726b0a306600430061006600e9\
        000a0034080c12190000000000000101000000000000000400000000000000000000000000000026";

    #[test]
    fn reads_a_plist_another_writer_made() {
        let tags = decode(&hex(PYTHON)).unwrap();
        assert_eq!(
            tags,
            [
                FinderTag::new("Red", FinderColor::Red),
                FinderTag::new("Work", FinderColor::None),
                FinderTag::new("Café", FinderColor::Blue),
            ]
        );
        // The same document as that writer's (same object order, sizes and trailer).
        assert_eq!(encode(&tags), hex(PYTHON));
    }

    #[test]
    fn round_trips_long_lists_and_unicode() {
        for n in [0usize, 1, 14, 15, 16, 300] {
            let tags: Vec<FinderTag> = (0..n)
                .map(|i| {
                    let name = match i % 3 {
                        0 => format!("tag {i}"),
                        1 => format!("étiquette {i} 🏷"),
                        _ => format!("{}{i}", "x".repeat(40)),
                    };
                    FinderTag::new(name, FinderColor::from_label((i % 8) as u8).unwrap())
                })
                .collect();
            assert_eq!(decode(&encode(&tags)).unwrap(), tags, "{n} tags");
        }
    }

    #[test]
    fn a_name_that_looks_like_a_color_survives() {
        let tags = vec![
            FinderTag::new("plan\n5", FinderColor::None),
            FinderTag::new("x\n9", FinderColor::Red),
        ];
        assert_eq!(decode(&encode(&tags)).unwrap(), tags);
        // A bare name (another writer's no-color form) and an empty one.
        let bare = bplist::write_strings(&["Important", "", "\n6"]);
        assert_eq!(
            decode(&bare).unwrap(),
            [FinderTag::new("Important", FinderColor::None)]
        );
    }

    #[test]
    fn broken_plists_are_errors_not_panics() {
        let good = encode(&[FinderTag::new("Red", FinderColor::Red)]);
        assert!(decode(b"").is_err());
        assert!(decode(b"bplist00").is_err());
        for cut in 0..good.len() {
            let _ = decode(&good[..cut]);
        }
        for i in 0..good.len() {
            for v in [0u8, 0x0F, 0x5F, 0x6F, 0xAF, 0xFF] {
                let mut bad = good.clone();
                bad[i] = v;
                let _ = decode(&bad);
            }
        }
        // Not an array of strings.
        let mut not_array = good.clone();
        not_array[8] = 0x51;
        assert!(decode(&not_array).is_err());
    }

    #[test]
    fn finder_colors_map_both_ways() {
        for c in FinderColor::MENU {
            assert_eq!(FinderColor::from_label(c.label()), Some(c));
            match c.hex() {
                Some(h) => {
                    assert_eq!(FinderColor::of_hex(h), Some(c));
                    assert_eq!(FinderColor::of_hex(&h.to_uppercase()), Some(c));
                    assert_eq!(FinderColor::nearest(Some(h)), c);
                    assert!(crate::valid_color(h), "{h}");
                }
                None => assert_eq!(c, FinderColor::None),
            }
        }
        assert_eq!(FinderColor::MENU.len(), 8);
        assert_eq!(FinderColor::colored().count(), 7);
        assert_eq!(FinderColor::from_label(8), None);
        // The palette Keel offered before Finder colors.
        for (hex, want) in [
            ("#e5484d", FinderColor::Red),
            ("#f76b15", FinderColor::Orange),
            ("#ffc53d", FinderColor::Yellow),
            ("#30a46c", FinderColor::Green),
            ("#3b82f6", FinderColor::Blue),
            ("#8e4ec6", FinderColor::Purple),
            ("#8b8d98", FinderColor::Gray),
            ("#000000", FinderColor::Gray),
        ] {
            assert_eq!(FinderColor::nearest(Some(hex)), want, "{hex}");
        }
        assert_eq!(FinderColor::nearest(None), FinderColor::None);
        assert_eq!(FinderColor::nearest(Some("red")), FinderColor::None);
    }

    #[test]
    fn merge_and_toggle() {
        let lib = vec![
            ("Work".to_owned(), Some("#e5484d".to_owned())),
            ("plain".to_owned(), None),
        ];
        let native = vec![
            FinderTag::new("work", FinderColor::Blue),
            FinderTag::new("Finder only", FinderColor::Green),
        ];
        assert_eq!(
            merge(&lib, &native),
            [
                FinderTag::new("Work", FinderColor::Red),
                FinderTag::new("plain", FinderColor::None),
                FinderTag::new("Finder only", FinderColor::Green),
            ]
        );
        let on = toggled(&native, "WORK", FinderColor::Red, true);
        assert_eq!(on[0], FinderTag::new("work", FinderColor::Red));
        assert_eq!(on.len(), 2);
        let off = toggled(&native, "Work", FinderColor::Red, false);
        assert_eq!(off, [FinderTag::new("Finder only", FinderColor::Green)]);
        let added = toggled(&native, "New", FinderColor::None, true);
        assert_eq!(added.last(), Some(&FinderTag::new("New", FinderColor::None)));
    }

    /// The xattr round trip on a real file (macOS CI): written, read back, replaced, removed.
    #[cfg(target_os = "macos")]
    #[test]
    fn native_tags_round_trip_through_the_xattr() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tagged.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(read_native(&file).unwrap(), []);
        let tags = vec![
            FinderTag::new("Red", FinderColor::Red),
            FinderTag::new("Keel test é", FinderColor::None),
        ];
        write_native(&file, &tags).unwrap();
        assert_eq!(read_native(&file).unwrap(), tags);
        let more = toggled(&tags, "Blue", FinderColor::Blue, true);
        write_native(&file, &more).unwrap();
        assert_eq!(read_native(&file).unwrap(), more);
        write_native(&file, &[]).unwrap();
        assert_eq!(read_native(&file).unwrap(), []);
        // Removing what is not there is fine; a missing file is an error.
        write_native(&file, &[]).unwrap();
        assert!(read_native(&dir.path().join("missing")).is_err());
        // A folder takes tags too.
        write_native(dir.path(), &tags[..1]).unwrap();
        assert_eq!(read_native(dir.path()).unwrap(), tags[..1]);
    }
}
