//! Writing a file list (or a result set made into one) as EFU, CSV, TSV, TXT, M3U, M3U8 or
//! JSON.

use std::io::Write;

use super::csv::push_field;
use super::time::format_iso;
use super::{FileList, ATTR_DIRECTORY};

/// An export format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// Everything's file list: CSV, `Filename` quoted, dates as FILETIME numbers, CRLF.
    Efu,
    /// CSV with the EFU columns, dates as ISO 8601 (UTC), CRLF.
    Csv,
    /// As CSV, tab-separated (a field with a tab, quote or line break is quoted).
    Tsv,
    /// One full path per line.
    Txt,
    /// A playlist: one full path per line, `#EXTM3U` first when the header is on.
    M3u,
    /// As M3U (the `.m3u8` name says UTF-8; both are written as UTF-8).
    M3u8,
    /// An array of objects: `filename`, `size`, `date_modified`, `date_created`,
    /// `attributes`, then the extra columns by name; missing values are left out.
    Json,
}

impl Format {
    pub const ALL: [Format; 7] = [
        Format::Efu,
        Format::Csv,
        Format::Tsv,
        Format::Txt,
        Format::M3u,
        Format::M3u8,
        Format::Json,
    ];

    /// The file extension (no dot).
    pub fn extension(self) -> &'static str {
        match self {
            Format::Efu => "efu",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Txt => "txt",
            Format::M3u => "m3u",
            Format::M3u8 => "m3u8",
            Format::Json => "json",
        }
    }

    /// A short name for menus ("EFU file list", ...).
    pub fn label(self) -> &'static str {
        match self {
            Format::Efu => "EFU file list",
            Format::Csv => "CSV",
            Format::Tsv => "TSV (tab-separated)",
            Format::Txt => "Text (one path per line)",
            Format::M3u => "M3U playlist",
            Format::M3u8 => "M3U8 playlist (UTF-8)",
            Format::Json => "JSON",
        }
    }

    /// By name or extension, any case (`efu`, `.csv`, `m3u8`).
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim().trim_start_matches('.').to_ascii_lowercase();
        Self::ALL.into_iter().find(|f| f.extension() == t)
    }
}

/// How to export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportOptions {
    pub format: Format,
    /// CSV and TSV: the column names first; M3U / M3U8: `#EXTM3U` first. EFU always has its
    /// header (`Filename` names the path column); TXT and JSON have none.
    pub header: bool,
    /// Start with a UTF-8 byte order mark.
    pub bom: bool,
}

impl ExportOptions {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            header: true,
            bom: false,
        }
    }
}

const EOL: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// Writes `list` to `w` as `opts` asks.
pub fn export(list: &FileList, opts: &ExportOptions, w: &mut dyn Write) -> std::io::Result<()> {
    let mut w = std::io::BufWriter::new(w);
    if opts.bom {
        w.write_all(b"\xEF\xBB\xBF")?;
    }
    match opts.format {
        Format::Efu => table(list, &mut w, ',', true, true),
        Format::Csv => table(list, &mut w, ',', opts.header, false),
        Format::Tsv => table(list, &mut w, '\t', opts.header, false),
        Format::Txt | Format::M3u | Format::M3u8 => {
            if opts.header && opts.format != Format::Txt {
                write!(w, "#EXTM3U{EOL}")?;
            }
            for e in &list.entries {
                // A path holding a line break cannot be one line: it is left out.
                if !e.path.contains(['\r', '\n']) {
                    write!(w, "{}{EOL}", e.path)?;
                }
            }
            Ok(())
        }
        Format::Json => json(list, &mut w),
    }?;
    w.flush()
}

/// Writes `list` as an EFU file list.
pub fn write_efu(list: &FileList, w: &mut dyn Write) -> std::io::Result<()> {
    export(list, &ExportOptions::new(Format::Efu), w)
}

fn attributes(e: &super::Entry) -> Option<u32> {
    match (e.attributes, e.is_dir) {
        (Some(a), true) => Some(a | ATTR_DIRECTORY),
        (Some(a), false) => Some(a),
        (None, true) => Some(ATTR_DIRECTORY),
        (None, false) => None,
    }
}

fn table(
    list: &FileList,
    w: &mut impl Write,
    delim: char,
    header: bool,
    efu: bool,
) -> std::io::Result<()> {
    let mut line = String::new();
    if header {
        let names = ["Filename", "Size", "Date Modified", "Date Created", "Attributes"];
        for (i, name) in names.iter().copied().chain(list.columns.iter().map(String::as_str)).enumerate() {
            if i > 0 {
                line.push(delim);
            }
            push_field(&mut line, name, delim, false);
        }
        line.push_str("\r\n");
        w.write_all(line.as_bytes())?;
    }
    let date = |t: Option<u64>| match t {
        Some(t) if efu => t.to_string(),
        Some(t) => format_iso(t),
        None => String::new(),
    };
    for e in &list.entries {
        line.clear();
        // Everything quotes every Filename; a TSV path is quoted only when it must be.
        push_field(&mut line, &e.path, delim, delim == ',');
        for value in [
            e.size.map(|s| s.to_string()).unwrap_or_default(),
            date(e.modified),
            date(e.created),
            attributes(e).map(|a| a.to_string()).unwrap_or_default(),
        ] {
            line.push(delim);
            line.push_str(&value);
        }
        for i in 0..list.columns.len() {
            line.push(delim);
            push_field(&mut line, e.extra.get(i).map_or("", String::as_str), delim, false);
        }
        line.push_str("\r\n");
        w.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// `text` as a JSON string.
fn json_str(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json(list: &FileList, w: &mut impl Write) -> std::io::Result<()> {
    w.write_all(b"[")?;
    let mut line = String::new();
    for (n, e) in list.entries.iter().enumerate() {
        line.clear();
        line.push_str(if n == 0 { "\n{" } else { ",\n{" });
        line.push_str("\"filename\":");
        json_str(&mut line, &e.path);
        if let Some(size) = e.size {
            line.push_str(&format!(",\"size\":{size}"));
        }
        for (key, t) in [("date_modified", e.modified), ("date_created", e.created)] {
            if let Some(t) = t {
                line.push_str(&format!(",\"{key}\":\"{}\"", format_iso(t)));
            }
        }
        if let Some(a) = attributes(e) {
            line.push_str(&format!(",\"attributes\":{a}"));
        }
        for (i, column) in list.columns.iter().enumerate() {
            let value = e.extra.get(i).map_or("", String::as_str);
            if value.is_empty() {
                continue;
            }
            line.push(',');
            json_str(&mut line, column);
            line.push(':');
            json_str(&mut line, value);
        }
        line.push('}');
        w.write_all(line.as_bytes())?;
    }
    w.write_all(b"\n]\n")
}
