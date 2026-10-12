//! File lists: Everything's EFU files (UTF-8 CSV with a header: `Filename` required,
//! `Size`, `Date Modified`, `Date Created`, `Attributes` and any other property column;
//! dates as FILETIME numbers or ISO 8601), plain `.txt` / `.m3u` lists (one path per line)
//! and `.csv` / `.tsv` tables. Reading is bounded ([`Limits`]: rows and line length) and
//! never touches the paths a list names. A list can be searched like an index
//! ([`FileListSearcher`]), written in other formats ([`export`]) and made from a folder
//! ([`create`]).

mod create;
mod csv;
mod export;
mod search;
mod time;

pub use create::{create, split_wildcards, CreateOptions};
pub use export::{export, write_efu, ExportOptions, Format};
pub use search::FileListSearcher;
pub use time::{filetime_from_system, format_iso, parse_date, system_from_filetime};

use std::io::BufRead;
use std::path::Path;

use anyhow::{bail, Context};

/// Windows file attribute bits used here (EFU's `Attributes` column).
pub const ATTR_READONLY: u32 = 0x1;
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_DIRECTORY: u32 = 0x10;

/// How much of a file list is read: more rows, or a longer line (a quoted field's line
/// breaks included), is an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_rows: usize,
    pub max_line: usize,
}

impl Default for Limits {
    /// Five million entries; 256 KiB per line (Windows paths reach 32,767 UTF-16 units,
    /// up to about 96 KiB of UTF-8).
    fn default() -> Self {
        Self {
            max_rows: 5_000_000,
            max_line: 256 * 1024,
        }
    }
}

/// One listed file or folder. Dates are FILETIME values (100 ns ticks since 1601 UTC).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The full path, as the list has it.
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub created: Option<u64>,
    pub attributes: Option<u32>,
    /// Values of [`FileList::columns`], in that order ("" when the row has none).
    pub extra: Vec<String>,
}

impl Entry {
    /// The last component of `path` (either separator), or the path itself for a root.
    pub fn name(&self) -> &str {
        let trimmed = self.path.trim_end_matches(['/', '\\']);
        match trimmed.rsplit(['/', '\\']).next() {
            Some(name) if !name.is_empty() => name,
            _ => &self.path,
        }
    }
}

/// A file list: its entries and the names of its extra property columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileList {
    /// Property columns beyond the five EFU ones (`Filename`, `Size`, `Date Modified`,
    /// `Date Created`, `Attributes`), as the header names them.
    pub columns: Vec<String>,
    pub entries: Vec<Entry>,
}

/// What a file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    /// EFU: CSV whose header has `Filename`.
    Efu,
    /// CSV: a header with `Filename`, `Full Path` / `Path`, or `Name` and `Path`
    /// (Everything's own CSV export); else the first column of every row is the path.
    Csv,
    /// As CSV, tab-separated.
    Tsv,
    /// One path per line.
    Txt,
    /// One path per line; `#` lines are comments.
    M3u,
}

impl ListKind {
    /// By extension: `.efu`, `.csv`, `.tsv`, `.txt`, `.m3u`, `.m3u8`.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "efu" => Self::Efu,
            "csv" => Self::Csv,
            "tsv" => Self::Tsv,
            "txt" => Self::Txt,
            "m3u" | "m3u8" => Self::M3u,
            _ => return None,
        })
    }
}

/// Reads the file list at `path` (its kind by extension; any other file is read as EFU
/// when its first line names a `Filename` column, else as one path per line). Only `path`
/// itself is opened.
pub fn read_file(path: &Path, limits: &Limits) -> anyhow::Result<FileList> {
    let file = std::fs::File::open(path).with_context(|| format!("{}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("{} is not a file", path.display());
    }
    let mut r = std::io::BufReader::new(file);
    let kind = match ListKind::from_path(path) {
        Some(kind) => kind,
        None => {
            let head = r.fill_buf()?;
            let line = head.split(|&b| b == b'\n').next().unwrap_or_default();
            let line = String::from_utf8_lossy(line).to_ascii_lowercase();
            match line.split(',').any(|c| c.trim().trim_matches('"') == "filename") {
                true => ListKind::Efu,
                false => ListKind::Txt,
            }
        }
    };
    read(r, kind, limits).with_context(|| format!("{}", path.display()))
}

/// Reads a file list of `kind` from `r`.
pub fn read(r: impl BufRead, kind: ListKind, limits: &Limits) -> anyhow::Result<FileList> {
    match kind {
        ListKind::Efu => read_table(r, b',', true, limits),
        ListKind::Csv => read_table(r, b',', false, limits),
        ListKind::Tsv => read_table(r, b'\t', false, limits),
        ListKind::Txt => read_lines(r, false, limits),
        ListKind::M3u => read_lines(r, true, limits),
    }
}

/// Where each known column is in a table's rows.
#[derive(Default)]
struct Layout {
    path: Option<usize>,
    /// `Name` and `Path` (the folder) when there is no full-path column.
    name_in: Option<(usize, usize)>,
    size: Option<usize>,
    modified: Option<usize>,
    created: Option<usize>,
    attributes: Option<usize>,
    /// (column, index in the row) of the extra columns.
    extra: Vec<(String, usize)>,
}

fn layout(header: &[String], efu: bool) -> Layout {
    let mut l = Layout::default();
    let (mut full, mut folder, mut name) = (None, None, None);
    for (i, raw) in header.iter().enumerate() {
        let col = raw.trim();
        let slot = match col.to_ascii_lowercase().as_str() {
            "filename" => &mut l.path,
            "full path" | "fullpath" | "full path name" if !efu => &mut full,
            "path" if !efu => &mut folder,
            "name" if !efu => &mut name,
            "size" => &mut l.size,
            "date modified" => &mut l.modified,
            "date created" => &mut l.created,
            "attributes" => &mut l.attributes,
            _ => {
                l.extra.push((col.to_owned(), i));
                continue;
            }
        };
        if slot.is_none() {
            *slot = Some(i);
        } else {
            l.extra.push((col.to_owned(), i));
        }
    }
    l.path = l.path.or(full);
    match (l.path, name, folder) {
        (None, Some(n), Some(f)) => l.name_in = Some((n, f)),
        (None, None, Some(f)) => l.path = Some(f),
        // A column a full path was found without: an ordinary property.
        (Some(_), n, f) => l.extra.extend(
            [("Name", n), ("Path", f)]
                .into_iter()
                .filter_map(|(c, i)| Some((c.to_owned(), i?))),
        ),
        _ => {}
    }
    l.extra.sort_by_key(|&(_, i)| i);
    l
}

/// `folder` joined with `name` (the folder's own separator).
fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        return name.to_owned();
    }
    let sep = if folder.contains('\\') { '\\' } else { '/' };
    match folder.ends_with(['/', '\\']) {
        true => format!("{folder}{name}"),
        false => format!("{folder}{sep}{name}"),
    }
}

fn parse_attributes(text: &str) -> Option<u32> {
    let t = text.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => t.parse().ok(),
    }
}

/// A path that ends with a separator names a folder: the separator goes (not a root's:
/// `C:\`, `/`).
fn folder_path(path: &mut String) -> bool {
    if !path.ends_with(['/', '\\']) {
        return false;
    }
    let trimmed = path.trim_end_matches(['/', '\\']);
    if !trimmed.is_empty() && !trimmed.ends_with(':') {
        path.truncate(trimmed.len());
    }
    true
}

fn push(list: &mut FileList, entry: Entry, limits: &Limits) -> anyhow::Result<()> {
    if list.entries.len() >= limits.max_rows {
        bail!("more than {} entries", limits.max_rows);
    }
    list.entries.push(entry);
    Ok(())
}

fn read_table(
    r: impl BufRead,
    delim: u8,
    efu: bool,
    limits: &Limits,
) -> anyhow::Result<FileList> {
    let mut records = csv::Records::new(r, delim, limits.max_line);
    let mut list = FileList::default();
    let Some(header) = records.next_record()? else {
        if efu {
            bail!("empty file: an EFU file list starts with a header naming Filename");
        }
        return Ok(list);
    };
    let mut l = layout(&header, efu);
    let mut first = None;
    if l.path.is_none() && l.name_in.is_none() {
        if efu {
            bail!("not an EFU file list: the header has no Filename column");
        }
        // No header: every row's first field is a path, and there are no properties.
        l.extra.clear();
        first = Some(header);
    }
    list.columns = l.extra.iter().map(|(c, _)| c.clone()).collect();
    let field = |row: &[String], i: Option<usize>| -> Option<String> {
        i.and_then(|i| row.get(i)).cloned()
    };
    loop {
        let row = match first.take() {
            Some(row) => row,
            None => match records.next_record()? {
                Some(row) => row,
                None => break,
            },
        };
        let path = match (l.path, l.name_in) {
            (Some(i), _) => field(&row, Some(i)),
            (None, Some((n, f))) => match (field(&row, Some(n)), field(&row, Some(f))) {
                (Some(n), f) if !n.is_empty() => Some(join(&f.unwrap_or_default(), &n)),
                _ => None,
            },
            // Headerless.
            (None, None) => row.first().map(|p| p.trim().to_owned()),
        };
        let Some(mut path) = path.filter(|p| !p.is_empty()) else {
            continue;
        };
        let attributes = field(&row, l.attributes).and_then(|a| parse_attributes(&a));
        let trailing = folder_path(&mut path);
        let entry = Entry {
            is_dir: trailing || attributes.is_some_and(|a| a & ATTR_DIRECTORY != 0),
            path,
            size: field(&row, l.size).and_then(|s| s.trim().parse().ok()),
            modified: field(&row, l.modified).and_then(|d| parse_date(&d)),
            created: field(&row, l.created).and_then(|d| parse_date(&d)),
            attributes,
            extra: (l.extra.iter())
                .map(|&(_, i)| row.get(i).cloned().unwrap_or_default())
                .collect(),
        };
        push(&mut list, entry, limits)?;
    }
    Ok(list)
}

fn read_lines(mut r: impl BufRead, comments: bool, limits: &Limits) -> anyhow::Result<FileList> {
    let mut list = FileList::default();
    let (mut buf, mut line) = (Vec::new(), 0);
    while let Some(end) = csv::read_line(&mut r, &mut buf, limits.max_line, &mut line)? {
        let text = String::from_utf8_lossy(&buf[..end]);
        let text = text.trim();
        if text.is_empty() || (comments && text.starts_with('#')) {
            continue;
        }
        let text = match text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
            Some(inner) if !inner.is_empty() => inner,
            _ => text,
        };
        let mut path = text.to_owned();
        let is_dir = folder_path(&mut path);
        let entry = Entry {
            path,
            is_dir,
            ..Entry::default()
        };
        push(&mut list, entry, limits)?;
    }
    Ok(list)
}

#[cfg(test)]
mod tests;
