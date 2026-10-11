//! Crafted files for tests (the `probes` feature, never in a program people run): each
//! made here from code, so no binary probe is committed. The reviews of 0.22.0 found each
//! one crashing or flooding a reader.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A Word package (docx-rs writes it) whose body is `body`.
pub fn word(dir: &Path, name: &str, body: &str) -> PathBuf {
    use docx_rs::*;
    let base = dir.join(format!("base-{name}"));
    Docx::new()
        .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Keel")))
        .build()
        .pack(File::create(&base).unwrap())
        .unwrap();
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    let path = dir.join(name);
    replace_entry(&base, &path, "word/document.xml", &document);
    std::fs::remove_file(&base).unwrap();
    path
}

/// `from` with the entry `name` (replaced or added) holding `body`, as `to`.
pub fn replace_entry(from: &Path, to: &Path, name: &str, body: &str) {
    let mut zip = zip::ZipArchive::new(File::open(from).unwrap()).unwrap();
    let mut out = zip::ZipWriter::new(File::create(to).unwrap());
    for i in 0..zip.len() {
        if zip.by_index_raw(i).unwrap().name() != name {
            out.raw_copy_file(zip.by_index_raw(i).unwrap()).unwrap();
        }
    }
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    out.start_file(name, options).unwrap();
    out.write_all(body.as_bytes()).unwrap();
    out.finish().unwrap();
}

/// `depth` tables one inside another around one paragraph.
pub fn nested_tables(depth: usize) -> String {
    format!(
        "{}<w:p><w:r><w:t>Keel</w:t></w:r></w:p>{}",
        "<w:tbl><w:tr><w:tc>".repeat(depth),
        "</w:tc></w:tr></w:tbl>".repeat(depth)
    )
}

/// Writes `size` over the uncompressed size of every entry, in the local headers and the
/// central directory.
pub fn lie_about_sizes(path: &Path, size: u32) {
    let mut bytes = std::fs::read(path).unwrap();
    for i in 0..bytes.len().saturating_sub(30) {
        let at = match &bytes[i..i + 4] {
            b"PK\x03\x04" => i + 22,
            b"PK\x01\x02" => i + 24,
            _ => continue,
        };
        bytes[at..at + 4].copy_from_slice(&size.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

/// An xlsb record: its type and size as 7-bit groups, then the data.
fn record(typ: u16, data: &[u8]) -> Vec<u8> {
    let mut out = if typ < 0x80 {
        vec![typ as u8]
    } else {
        vec![(typ & 0x7F) as u8 | 0x80, (typ >> 7) as u8]
    };
    let mut len = data.len();
    loop {
        let b = (len & 0x7F) as u8;
        len >>= 7;
        out.push(if len > 0 { b | 0x80 } else { b });
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(data);
    out
}

fn wide(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out = (units.len() as u32).to_le_bytes().to_vec();
    out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
    out
}

/// An xlsb workbook with one sheet: "Keel" at A1 and 2 at XFD1048576 (the
/// last cell there is).
pub fn sparse_xlsb(dir: &Path) -> PathBuf {
    let mut sheet_entry = [0u32.to_le_bytes(), 1u32.to_le_bytes()].concat();
    sheet_entry.extend(wide("rId1"));
    sheet_entry.extend(wide("Sheet1"));
    // The sheet, the end of the sheets, and a record that follows the defined names.
    let workbook = [
        record(0x009C, &sheet_entry),
        record(0x0090, &[]),
        record(0x009D, &[]),
    ]
    .concat();
    let (last_row, last_col) = (1_048_575u32, 16_383u32);
    let dim: Vec<u8> = [0u32, last_row, 0, last_col]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let cell = |col: u32, rest: &[u8]| [&col.to_le_bytes()[..], &[0; 4], rest].concat();
    let sheet = [
        record(0x0094, &dim),
        record(0x0091, &[]),
        record(0x0000, &0u32.to_le_bytes()),
        record(0x0006, &cell(0, &wide("Keel"))),
        record(0x0000, &last_row.to_le_bytes()),
        record(0x0005, &cell(last_col, &2f64.to_le_bytes())),
        record(0x0092, &[]),
    ]
    .concat();
    let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/>
</Relationships>"#;
    let path = dir.join("sparse.xlsb");
    let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in [
        ("xl/workbook.bin", workbook),
        ("xl/_rels/workbook.bin.rels", rels.as_bytes().to_vec()),
        ("xl/worksheets/sheet1.bin", sheet),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(&body).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// A one-sheet xlsx whose `sheetData` is `rows`.
pub fn xlsx(dir: &Path, name: &str, rows: &str) -> PathBuf {
    let types = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
    let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    let workbook = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    let wb_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
    let sheet = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
    );
    let path = dir.join(name);
    let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (entry, body) in [
        ("[Content_Types].xml", types),
        ("_rels/.rels", rels),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", wb_rels),
        ("xl/worksheets/sheet1.xml", &sheet),
    ] {
        zip.start_file(entry, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// The probes of the review of 0.22.0, in `dir`: 20,000 nested Word tables, an xlsb with
/// a cell at XFD1048576, a docx whose zip says its 65 MiB part holds 1 KiB, and an xlsx
/// whose error cell holds `marker` (with 3,000 more characters). None may crash a reader,
/// and the marker may never reach a log.
pub fn review_files(dir: &Path, marker: &str) -> Vec<PathBuf> {
    let deep = word(dir, "deep.docx", &nested_tables(20_000));
    let text = format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", "a".repeat(65 << 20));
    let liar = word(dir, "liar.docx", &text);
    lie_about_sizes(&liar, 1024);
    let cell = format!(
        r#"<row r="1"><c r="A1" t="e"><v>{marker} {}</v></c></row>"#,
        "x".repeat(3_000)
    );
    vec![deep, sparse_xlsb(dir), liar, xlsx(dir, "error.xlsx", &cell)]
}
