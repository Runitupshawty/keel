//! What the Disk Usage window's analysis tabs share (Phase 17 Task 84): file rows, size
//! ranges, which files a total counts, and CSV cells.

use super::{flag, NodeId, NodeKind, Tree, UNKNOWN_TIME};
use serde::Serialize;
use std::path::PathBuf;

/// An inclusive byte range (`min ..= max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SizeRange {
    pub min: u64,
    pub max: u64,
}

impl Default for SizeRange {
    fn default() -> Self {
        SizeRange {
            min: 0,
            max: u64::MAX,
        }
    }
}

impl SizeRange {
    pub fn contains(self, bytes: u64) -> bool {
        bytes >= self.min && bytes <= self.max
    }

    /// No bound set.
    pub fn is_any(self) -> bool {
        self == Self::default()
    }

    /// From two optional bounds as typed ("" is no bound; see [`parse_size`]).
    pub fn parse(min: &str, max: &str) -> Result<Self, String> {
        let bound = |s: &str, none: u64| {
            if s.trim().is_empty() {
                Ok(none)
            } else {
                parse_size(s).ok_or_else(|| format!("{} is not a size like 10 MB", s.trim()))
            }
        };
        let range = SizeRange {
            min: bound(min, 0)?,
            max: bound(max, u64::MAX)?,
        };
        if range.min > range.max {
            return Err("The minimum size is above the maximum".into());
        }
        Ok(range)
    }
}

/// `"1.5 MB"` → bytes. KB, MB, GB, TB are decimal (as Keel shows sizes); KiB, MiB, GiB,
/// TiB binary; no unit is bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = (s.find(|c: char| !(c.is_ascii_digit() || c == '.'))).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "b" | "bytes" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let v = n * mult;
    (v.is_finite() && v >= 0.0 && v < u64::MAX as f64).then_some(v.round() as u64)
}

/// A file of the scan with its path, for lists and reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileRow {
    /// Valid for the tree the row was made from (until it is compacted).
    pub node: NodeId,
    pub path: PathBuf,
    /// Lowercase, "" for none.
    pub extension: String,
    pub bytes: u64,
    pub allocated: u64,
    /// Last write, unix nanoseconds.
    pub modified_ns: Option<i64>,
}

impl FileRow {
    /// None for the top item and pseudo items (no path).
    pub fn from_tree(tree: &Tree, node: NodeId) -> Option<Self> {
        let n = tree.get(node)?;
        Some(FileRow {
            node,
            path: tree.path(node)?,
            extension: tree.ext(node).into(),
            bytes: n.size,
            allocated: n.allocated,
            modified_ns: (n.modified != UNKNOWN_TIME).then_some(n.modified),
        })
    }
}

/// A file the scan's totals count: a further name of a hard-linked file counted elsewhere
/// is left out (when **Count hard links once** is on), so analytics never count it twice
/// and duplicates never offer it as space to win back.
pub fn counted_file(tree: &Tree, id: NodeId) -> bool {
    let n = tree.node(id);
    n.kind == NodeKind::File
        && !(tree.options.count_hard_links_once && n.has(flag::HARDLINK_DUP))
}

/// One CSV cell (RFC 4180 quoting). File names come from anyone, so a cell a spreadsheet
/// would run as a formula (`=`, `+`, `-`, `@`, or a leading tab or return) gets a `'`
/// first.
pub fn csv_cell(text: &str) -> String {
    let formula = text.starts_with(['=', '+', '-', '@', '\t', '\r', '\n'])
        || text.trim_start().starts_with(['=', '+', '-', '@']);
    let text = if formula {
        format!("'{text}")
    } else {
        text.to_owned()
    };
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// Hex of a digest.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
