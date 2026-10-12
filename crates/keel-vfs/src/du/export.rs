//! Exports of a scan (Phase 17 Task 86): the whole tree, the file types (one row per
//! extension) and the largest files, as CSV or JSON.
//!
//! - **CSV** is UTF-8 (a byte order mark on request, for spreadsheet programs that want
//!   one), RFC 4180: every field quoted, `"` doubled, CRLF line ends, numbers without
//!   grouping or locale. A text field starting with `=`, `+`, `-`, `@`, a tab or a carriage
//!   return gets a leading `'` so a spreadsheet does not run a file name as a formula.
//! - **JSON** of the tree is the scan file format ([`super::save_to`], uncompressed), so
//!   `keel du --load-from` reads an exported tree back; the file types and largest files
//!   are JSON arrays of objects.
//!
//! Hard-link names counted elsewhere ([`flag::HARDLINK_DUP`]) are left out of the file
//! types and largest files, as they add nothing to any total.

use super::{flag, NodeId, NodeKind, Tree, NONE, UNKNOWN_TIME};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;

/// CSV or JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
}

impl Format {
    /// By the file's extension: `.csv` or `.json` (any case).
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("csv") => Ok(Self::Csv),
            Some("json") => Ok(Self::Json),
            _ => bail!("{}: an export file ends in .csv or .json", path.display()),
        }
    }
}

/// What to export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    /// Every item (files, folders, links, free and unknown space).
    Tree,
    /// One row per extension: files, bytes, bytes on disk, share.
    Types,
    /// The biggest files, at most [`Options::top`].
    Largest,
}

/// How to export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Start a CSV file with a UTF-8 byte order mark (never JSON).
    pub bom: bool,
    /// At most this many largest files (and top folders and file types in a report);
    /// 0: all.
    pub top: usize,
    /// Rank by bytes on disk instead of bytes (both are exported).
    pub size_on_disk: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bom: false,
            top: 100,
            size_on_disk: false,
        }
    }
}

/// One item of the tree or of the largest files.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    /// The item's id in this export (the tree's parent references use it).
    pub id: NodeId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    /// Full path (absent for the top item and free and unknown space).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub name: String,
    /// top, root, dir, file, link, free or unknown.
    pub kind: &'static str,
    /// Lowercase, without the dot; "" for none (and for anything but a file).
    pub extension: String,
    pub bytes: u64,
    /// Bytes on disk.
    pub allocated: u64,
    /// Files, folders and items below (a folder's subtree; 0 for a file).
    pub files: u64,
    pub folders: u64,
    pub items: u64,
    /// The item's last write, RFC 3339 UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// The newest last write in a folder's subtree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
    /// Windows attribute bits (synthesized elsewhere).
    pub attributes: u32,
    /// What is special about it: hard link, unreadable, cloud placeholder, ...
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<&'static str>,
}

/// Unix nanoseconds as RFC 3339 UTC (seconds), None when unknown or out of range.
pub fn time(ns: i64) -> Option<String> {
    if ns == UNKNOWN_TIME {
        return None;
    }
    let t = chrono::DateTime::from_timestamp(ns.div_euclid(1_000_000_000), 0)?;
    Some(t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// The row of item `id`, with `parent` as given.
fn row_with(tree: &Tree, id: NodeId, out_id: NodeId, parent: Option<NodeId>) -> Row {
    let n = tree.node(id);
    let newest = match n.is_container() {
        true => time(n.newest),
        false => None,
    };
    Row {
        id: out_id,
        parent,
        path: tree.path(id).map(|p| p.to_string_lossy().into_owned()),
        name: n.name.to_string(),
        kind: n.kind.as_str(),
        extension: tree.ext(id).to_owned(),
        bytes: n.size,
        allocated: n.allocated,
        files: n.files,
        folders: n.folders,
        items: n.items,
        modified: time(n.modified),
        newest,
        attributes: n.attributes,
        notes: super::notes(n.flags),
    }
}

/// The row of item `id` (ids as in the tree).
pub fn row(tree: &Tree, id: NodeId) -> Row {
    let parent = tree.node(id).parent;
    row_with(tree, id, id, (parent != NONE).then_some(parent))
}

/// The tree's rows, parents first and each folder's children together, numbered from 0
/// in that order (refreshed-away items left out).
pub fn tree_rows(tree: &Tree) -> impl Iterator<Item = Row> + '_ {
    let (order, remap) = tree.live_order();
    order.into_iter().enumerate().map(move |(i, id)| {
        let parent = tree.node(id).parent;
        let parent = (parent != NONE).then(|| remap[parent as usize]);
        row_with(tree, id, i as NodeId, parent)
    })
}

/// One extension's files.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FileType {
    /// Lowercase, without the dot; "" for files without one.
    pub extension: String,
    pub files: u64,
    pub bytes: u64,
    /// Bytes on disk.
    pub allocated: u64,
    /// Share of all files' bytes (bytes on disk when ranked by them), 0 to 100.
    pub percent: f64,
}

/// The files counted by totals: live files, hard-link duplicates left out.
fn counted_files(tree: &Tree) -> impl Iterator<Item = (NodeId, &super::Node)> {
    tree.nodes()
        .filter(|(_, n)| n.kind == NodeKind::File && !n.has(flag::HARDLINK_DUP))
}

/// Every extension's files, the biggest first (then by extension).
pub fn file_types(tree: &Tree, size_on_disk: bool) -> Vec<FileType> {
    let mut by_ext: HashMap<u32, FileType> = HashMap::new();
    for (id, n) in counted_files(tree) {
        let t = by_ext.entry(n.ext).or_insert_with(|| FileType {
            extension: tree.ext(id).to_owned(),
            ..FileType::default()
        });
        t.files += 1;
        t.bytes = t.bytes.saturating_add(n.size);
        t.allocated = t.allocated.saturating_add(n.allocated);
    }
    let mut types: Vec<FileType> = by_ext.into_values().collect();
    let total: u64 = (types.iter())
        .map(|t| metric(t.bytes, t.allocated, size_on_disk))
        .fold(0, u64::saturating_add);
    for t in &mut types {
        let m = metric(t.bytes, t.allocated, size_on_disk);
        t.percent = match total {
            0 => 0.0,
            _ => (m as f64 / total as f64 * 10_000.0).round() / 100.0,
        };
    }
    types.sort_by(|a, b| {
        (metric(b.bytes, b.allocated, size_on_disk))
            .cmp(&metric(a.bytes, a.allocated, size_on_disk))
            .then_with(|| a.extension.cmp(&b.extension))
    });
    types
}

/// Bytes, or bytes on disk.
pub fn metric(bytes: u64, allocated: u64, size_on_disk: bool) -> u64 {
    match size_on_disk {
        true => allocated,
        false => bytes,
    }
}

/// The biggest files, at most `options.top` (0: all), biggest first.
pub fn largest(tree: &Tree, options: Options) -> Vec<Row> {
    let mut ids: Vec<NodeId> = counted_files(tree).map(|(id, _)| id).collect();
    rank(tree, &mut ids, options);
    ids.into_iter().map(|id| row(tree, id)).collect()
}

/// Keeps the `options.top` biggest of `ids` (0: all), biggest first, then by name and id.
/// Only the kept ones are sorted.
pub fn rank(tree: &Tree, ids: &mut Vec<NodeId>, options: Options) {
    let size = |id: NodeId| {
        let n = tree.node(id);
        metric(n.size, n.allocated, options.size_on_disk)
    };
    let compare = |a: &NodeId, b: &NodeId| {
        (size(*b).cmp(&size(*a)))
            .then_with(|| tree.node(*a).name.cmp(&tree.node(*b).name))
            .then(a.cmp(b))
    };
    if options.top > 0 && options.top < ids.len() {
        ids.select_nth_unstable_by(options.top, compare);
        ids.truncate(options.top);
    }
    ids.sort_by(compare);
}

/// A text field as a spreadsheet should show it (no formula).
fn text(s: &str) -> String {
    match s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        true => format!("'{s}"),
        false => s.to_owned(),
    }
}

/// One CSV record: every field quoted, CRLF.
fn csv_record<S: AsRef<str>>(w: &mut impl Write, fields: &[S]) -> Result<()> {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        w.write_all(b"\"")?;
        w.write_all(field.as_ref().replace('"', "\"\"").as_bytes())?;
        w.write_all(b"\"")?;
    }
    w.write_all(b"\r\n")?;
    Ok(())
}

const ROW_HEADER: [&str; 15] = [
    "id",
    "parent",
    "path",
    "name",
    "kind",
    "extension",
    "bytes",
    "allocated",
    "files",
    "folders",
    "items",
    "modified",
    "newest",
    "attributes",
    "notes",
];

fn csv_row(w: &mut impl Write, r: Row) -> Result<()> {
    let opt = |v: Option<String>| v.unwrap_or_default();
    csv_record(
        w,
        &[
            r.id.to_string(),
            r.parent.map(|p| p.to_string()).unwrap_or_default(),
            text(&opt(r.path)),
            text(&r.name),
            r.kind.to_owned(),
            text(&r.extension),
            r.bytes.to_string(),
            r.allocated.to_string(),
            r.files.to_string(),
            r.folders.to_string(),
            r.items.to_string(),
            opt(r.modified),
            opt(r.newest),
            r.attributes.to_string(),
            r.notes.join("; "),
        ],
    )
}

/// Writes `list` of `tree` to `w`.
pub fn write(
    tree: &Tree,
    list: List,
    format: Format,
    options: Options,
    w: impl Write,
) -> Result<()> {
    let mut w = w;
    if format == Format::Csv && options.bom {
        w.write_all("\u{feff}".as_bytes())?;
    }
    match (list, format) {
        (List::Tree, Format::Json) => return super::save_to(tree, w, false),
        (List::Tree, Format::Csv) => {
            csv_record(&mut w, &ROW_HEADER)?;
            for r in tree_rows(tree) {
                csv_row(&mut w, r)?;
            }
        }
        (List::Largest, Format::Json) => {
            serde_json::to_writer_pretty(&mut w, &largest(tree, options))?;
            w.write_all(b"\n")?;
        }
        (List::Largest, Format::Csv) => {
            csv_record(&mut w, &ROW_HEADER)?;
            for r in largest(tree, options) {
                csv_row(&mut w, r)?;
            }
        }
        (List::Types, Format::Json) => {
            serde_json::to_writer_pretty(&mut w, &file_types(tree, options.size_on_disk))?;
            w.write_all(b"\n")?;
        }
        (List::Types, Format::Csv) => {
            csv_record(
                &mut w,
                &["extension", "files", "bytes", "allocated", "percent"],
            )?;
            for t in file_types(tree, options.size_on_disk) {
                csv_record(
                    &mut w,
                    &[
                        text(&t.extension),
                        t.files.to_string(),
                        t.bytes.to_string(),
                        t.allocated.to_string(),
                        format!("{:.2}", t.percent),
                    ],
                )?;
            }
        }
    }
    w.flush()?;
    Ok(())
}

/// Writes `path` through a temporary file next to it, replacing it only once everything
/// was written.
pub fn atomic_write(path: &Path, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("cannot write in {}", dir.display()))?;
    {
        let mut w = BufWriter::new(tmp.as_file());
        write(&mut w)?;
        w.flush()?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Writes `list` of `tree` to `path`, CSV or JSON by its extension.
pub fn save(tree: &Tree, list: List, options: Options, path: &Path) -> Result<()> {
    let format = Format::from_path(path)?;
    atomic_write(path, |w| write(tree, list, format, options, w))
}
