use crate::{Preview, Request};
use calamine::{open_workbook_auto, Reader};

const TABLE_EXTENSIONS: &[&str] = &["csv", "tsv", "xlsx", "xls", "xlsb", "ods"];
type TableData = (Vec<String>, Vec<Vec<String>>, bool);

pub(crate) fn accepts(ext: &str) -> bool {
    TABLE_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request) -> Preview {
    let result = match req.entry.ext.as_str() {
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

fn csv(req: &Request) -> Result<TableData, String> {
    let bytes = std::fs::read(&req.bytes_path).map_err(|error| error.to_string())?;
    let first_line = bytes.split(|byte| *byte == b'\n').next().unwrap_or(&bytes);
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
        .from_reader(bytes.as_slice());
    let headers = reader
        .headers()
        .map_err(|error| error.to_string())?
        .iter()
        .map(str::to_owned)
        .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    for record in reader.records() {
        if rows.len() == 2000 {
            truncated = true;
            break;
        }
        rows.push(
            record
                .map_err(|error| error.to_string())?
                .iter()
                .map(str::to_owned)
                .collect(),
        );
    }
    Ok((headers, rows, truncated))
}

fn workbook(req: &Request) -> Result<TableData, String> {
    let mut workbook = open_workbook_auto(&req.bytes_path).map_err(|error| error.to_string())?;
    let range = workbook
        .worksheet_range_at(0)
        .ok_or_else(|| "workbook contains no sheets".to_owned())?
        .map_err(|error| error.to_string())?;
    let mut source = range.rows();
    let headers = source
        .next()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .unwrap_or_default();
    let mut rows = Vec::new();
    let mut truncated = false;
    for row in source {
        if rows.len() == 2000 {
            truncated = true;
            break;
        }
        rows.push(row.iter().map(ToString::to_string).collect());
    }
    Ok((headers, rows, truncated))
}
