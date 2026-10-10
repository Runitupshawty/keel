use crate::{Preview, Request};
use calamine::{open_workbook_auto, Data, Reader, Sheets};
use std::io::BufRead;

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
        _ => workbook(req),
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

fn workbook(req: &Request) -> Result<TableData, String> {
    let mut workbook = open_workbook_auto(&req.bytes_path).map_err(|error| error.to_string())?;
    if let Sheets::Xlsx(xlsx) = &mut workbook {
        return xlsx_streamed(xlsx);
    }
    // ponytail: xls/xlsb/ods are read as one dense Range of the first sheet (bounded by
    // MAX_PREVIEW_BYTES on disk); calamine 0.36 also has a cells reader for xlsb, stream it
    // like xlsx if large xlsb previews matter (xls/ods still have none).
    let range = workbook
        .worksheet_range_at(0)
        .ok_or_else(|| "workbook contains no sheets".to_owned())?
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
fn xlsx_streamed<R: std::io::Read + std::io::Seek>(
    xlsx: &mut calamine::Xlsx<R>,
) -> Result<TableData, String> {
    let name = xlsx
        .sheet_names()
        .first()
        .cloned()
        .ok_or_else(|| "workbook contains no sheets".to_owned())?;
    let mut cells = xlsx
        .worksheet_cells_reader(&name)
        .map_err(|error| error.to_string())?;
    let first_col = cells.dimensions().start.1;
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut first_row = None;
    let mut truncated = false;
    while let Some(cell) = cells.next_cell().map_err(|error| error.to_string())? {
        let (row, col) = cell.get_position();
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
        line[col] = Data::from(cell.get_value().clone()).to_string();
    }
    let mut rows = table.into_iter();
    let headers = rows.next().unwrap_or_default();
    Ok((headers, rows.collect(), truncated))
}
