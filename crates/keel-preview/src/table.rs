use crate::office::Package;
use crate::{Preview, Request};
use calamine::{Data, Ods, Reader, Sheets, Xls, Xlsb, Xlsx};
use std::io::{BufRead, Cursor, Read, Seek};
use std::path::Path;

const TABLE_EXTENSIONS: &[&str] = &["csv", "tsv", "xlsx", "xls", "xlsb", "ods"];
const MAX_ROWS: usize = 2000;
/// A stray cell at column XFD must not allocate 16k empty strings per row.
const MAX_COLS: usize = 512;
type TableData = (Vec<String>, Vec<Vec<String>>, bool);

pub(crate) fn accepts(ext: &str) -> bool {
    TABLE_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request, ext: &str) -> Preview {
    let result = match ext {
        "csv" | "tsv" => csv(req),
        _ => workbook(req, ext),
    };
    match result {
        Ok((headers, rows, truncated)) => Preview::Table {
            headers,
            rows,
            truncated,
        },
        Err(error) => Preview::Error(error),
    }
}

fn lossy(record: &csv::ByteRecord) -> Vec<String> {
    record
        .iter()
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

fn csv(req: &Request) -> Result<TableData, String> {
    let file = std::fs::File::open(&req.bytes_path).map_err(|error| error.to_string())?;
    let mut input = std::io::BufReader::with_capacity(64 * 1024, file);
    let head = input.fill_buf().map_err(|error| error.to_string())?;
    let first_line = head.split(|byte| *byte == b'\n').next().unwrap_or(head);
    let delimiter = b",;\t"
        .iter()
        .copied()
        .max_by_key(|candidate| {
            first_line
                .iter()
                .filter(|byte| **byte == *candidate)
                .count()
        })
        .unwrap_or(b',');
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(input);
    let headers = lossy(reader.byte_headers().map_err(|error| error.to_string())?);
    let mut rows = Vec::new();
    let mut record = csv::ByteRecord::new();
    while reader
        .read_byte_record(&mut record)
        .map_err(|error| error.to_string())?
    {
        if rows.len() == MAX_ROWS {
            return Ok((headers, rows, true));
        }
        rows.push(lossy(&record));
    }
    Ok((headers, rows, false))
}

/// The largest .xls previewed: calamine reads all of it, into memory, when it opens it.
const MAX_XLS_BYTES: u64 = 32 << 20;

fn workbook(req: &Request, ext: &str) -> Result<TableData, String> {
    if ext == "xls" {
        return first_sheet(&mut Sheets::Xls(open_xls(&req.bytes_path)?));
    }
    first_sheet(&mut open_zip(&req.bytes_path, ext)?)
}

/// An .xls opened by calamine once its size and what it claims are bounded (`xls::check`):
/// calamine reads the whole workbook into dense sheets, sized by what the file says.
fn open_xls(path: &Path) -> Result<Xls<Mem>, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_XLS_BYTES {
        return Err("too big to preview (an .xls over 32 MiB)".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if !crate::xls::check(&bytes) {
        return Err("damaged or too large to preview".into());
    }
    Xls::new(Cursor::new(bytes)).map_err(|e| e.to_string())
}

type Mem = Cursor<Vec<u8>>;

/// A zip workbook (xlsx, xlsb, ods) read from a bounded copy of its package
/// (`Package::repack`): calamine reads whole parts, trusting the sizes a zip declares, and
/// would sniff a renamed .xls by its content.
fn open_zip(path: &Path, ext: &str) -> Result<Sheets<Mem>, String> {
    let bytes = Cursor::new(Package::open(path)?.repack(false)?);
    Ok(match ext {
        "xlsx" => Sheets::Xlsx(Xlsx::new(bytes).map_err(|e| e.to_string())?),
        "xlsb" => Sheets::Xlsb(Xlsb::new(bytes).map_err(|e| e.to_string())?),
        _ => Sheets::Ods(Ods::new(bytes).map_err(|e| e.to_string())?),
    })
}

fn first_sheet<RS: Read + Seek>(workbook: &mut Sheets<RS>) -> Result<TableData, String> {
    let first = || "workbook contains no sheets".to_owned();
    // xlsx and xlsb stream their cells: a stray cell far from A1 costs nothing.
    macro_rules! stream {
        ($x:expr) => {{
            let name = $x.sheet_names().first().cloned().ok_or_else(first)?;
            let mut cells = $x
                .worksheet_cells_reader(&name)
                .map_err(|e| e.to_string())?;
            let col = cells.dimensions().start.1;
            return streamed(col, || {
                let c = cells.next_cell().map_err(|e| e.to_string())?;
                Ok(c.map(|c| {
                    (
                        c.get_position(),
                        Data::from(c.get_value().clone()).to_string(),
                    )
                }))
            });
        }};
    }
    match workbook {
        Sheets::Xlsx(x) => stream!(x),
        Sheets::Xlsb(x) => stream!(x),
        _ => {}
    }
    // ponytail: ods is read as one dense Range of the first sheet (calamine has no cells
    // reader for it).
    let range = workbook
        .worksheet_range_at(0)
        .ok_or_else(first)?
        .map_err(|error| error.to_string())?;
    let mut source = range.rows();
    let headers = source
        .next()
        .map(|row| row.iter().take(MAX_COLS).map(ToString::to_string).collect())
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = source
        .by_ref()
        .take(MAX_ROWS)
        .map(|row| row.iter().take(MAX_COLS).map(ToString::to_string).collect())
        .collect();
    Ok((headers, rows, source.next().is_some()))
}

/// Reads cells in sheet order and stops after the header plus MAX_ROWS rows.
fn streamed(
    first_col: u32,
    mut next: impl FnMut() -> Result<Option<((u32, u32), String)>, String>,
) -> Result<TableData, String> {
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut first_row = None;
    let mut truncated = false;
    while let Some(((row, col), value)) = next()? {
        let row = row.saturating_sub(*first_row.get_or_insert(row)) as usize;
        if row > MAX_ROWS {
            truncated = true;
            break;
        }
        let col = col.saturating_sub(first_col) as usize;
        if col >= MAX_COLS {
            continue;
        }
        if table.len() <= row {
            table.resize_with(row + 1, Vec::new);
        }
        let line = &mut table[row];
        if line.len() <= col {
            line.resize(col + 1, String::new());
        }
        line[col] = value;
    }
    let mut rows = table.into_iter();
    let headers = rows.next().unwrap_or_default();
    Ok((headers, rows.collect(), truncated))
}

/// Every sheet's cells as text (Task 62), sheet by sheet: the sheet's name, then each row's
/// cells separated by tabs. xlsx and xlsb (`ext`) stream their cells from a bounded copy of
/// the package; .xls is not read (calamine reads it whole when it opens it).
pub(crate) fn workbook_text(
    path: &Path,
    ext: &str,
    out: &mut crate::extract::Sink,
) -> Result<(), String> {
    let mut workbook = open_zip(path, ext)?;
    for name in workbook.sheet_names() {
        if !out.line(&name) {
            return Ok(());
        }
        let mut row = String::new();
        let mut at = None;
        let mut cell = |r: u32, value: String, out: &mut crate::extract::Sink| -> bool {
            if at.is_some_and(|a| a != r) && !out.line(&std::mem::take(&mut row)) {
                return false;
            }
            at = Some(r);
            if !value.is_empty() {
                if !row.is_empty() {
                    row.push('\t');
                }
                row.push_str(&value);
            }
            true
        };
        macro_rules! cells {
            ($reader:expr) => {{
                let mut cells = $reader
                    .worksheet_cells_reader(&name)
                    .map_err(|e| e.to_string())?;
                while let Some(c) = cells.next_cell().map_err(|e| e.to_string())? {
                    let value = Data::from(c.get_value().clone()).to_string();
                    if !cell(c.get_position().0, value, out) {
                        return Ok(());
                    }
                }
            }};
        }
        match &mut workbook {
            Sheets::Xlsx(x) => cells!(x),
            Sheets::Xlsb(x) => cells!(x),
            _ => return Err("not a workbook the text job reads".into()),
        }
        if !out.line(&row) {
            return Ok(());
        }
    }
    Ok(())
}
