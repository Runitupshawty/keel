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

/// A BIFF record of a legacy Excel workbook: type, size, data.
pub fn biff(typ: u16, data: &[u8]) -> Vec<u8> {
    [
        &typ.to_le_bytes()[..],
        &(data.len() as u16).to_le_bytes(),
        data,
    ]
    .concat()
}

/// A BIFF8 Number record: `value` at (`row`, `col`).
pub fn biff_number(row: u16, col: u16, value: f64) -> Vec<u8> {
    let data = [
        &row.to_le_bytes()[..],
        &col.to_le_bytes(),
        &[0, 0],
        &value.to_le_bytes(),
    ]
    .concat();
    biff(0x0203, &data)
}

/// A BIFF8 DIMENSIONS record: rows `first..last` (exclusive, u32 as BIFF8 has them) and
/// columns `first..last`.
pub fn biff_dimensions(rows: (u32, u32), cols: (u16, u16)) -> Vec<u8> {
    let data = [
        &rows.0.to_le_bytes()[..],
        &rows.1.to_le_bytes(),
        &cols.0.to_le_bytes(),
        &cols.1.to_le_bytes(),
        &[0, 0],
    ]
    .concat();
    biff(0x0200, &data)
}

/// A legacy Excel workbook (`.xls`, BIFF8 in a compound file) with one sheet, "Sheet1",
/// whose records are `sheet` (after its BOF, before its EOF). `big` pads the workbook
/// stream past 4 KiB, into the file's own sectors; else it lies in the mini stream.
pub fn xls(dir: &Path, name: &str, sheet: &[u8], big: bool) -> PathBuf {
    let bof = |dt: u8| biff(0x0809, &[&[0x00, 0x06, dt, 0x00][..], &[0; 12]].concat());
    let bound = |pos: u32| {
        let data = [&pos.to_le_bytes()[..], &[0, 0, 6, 0], b"Sheet1"].concat();
        biff(0x0085, &data)
    };
    let eof = biff(0x000A, &[]);
    let globals_len = (bof(5).len() + bound(0).len() + eof.len()) as u32;
    let mut stream = [bof(5), bound(globals_len), eof.clone(), bof(0x10)].concat();
    stream.extend_from_slice(sheet);
    stream.extend_from_slice(&eof);
    if big {
        stream.resize(stream.len().max(5000), 0);
    }
    let path = dir.join(name);
    std::fs::write(&path, compound_file("Workbook", &stream)).unwrap();
    path
}

/// A compound file (version 3, 512-byte sectors) holding one stream, `name`. Small enough
/// for one FAT sector: up to 60 KiB.
fn compound_file(name: &str, stream: &[u8]) -> Vec<u8> {
    const FREE: u32 = 0xFFFF_FFFF;
    const END: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    let sectors = |len: usize, size: usize| len.div_ceil(size).max(1);
    let mini = stream.len() < 4096;
    // Sector 0 the FAT, 1 the directory, then (mini) the mini FAT and the mini stream, or
    // the stream itself.
    let mut fat = vec![FATSECT, END];
    let chain = |fat: &mut Vec<u32>, n: usize| {
        let first = fat.len() as u32;
        for i in 1..n {
            fat.push(first + i as u32);
        }
        fat.push(END);
        first
    };
    let mut body = Vec::new();
    let (root_start, root_len, start, mini_fat_start) = if mini {
        let minis = sectors(stream.len(), 64);
        let mini_fat_start = chain(&mut fat, 1);
        let mut mini_fat: Vec<u32> = (1..minis as u32).collect();
        mini_fat.push(END);
        mini_fat.resize(128, FREE);
        body.extend(mini_fat.iter().flat_map(|v| v.to_le_bytes()));
        let mini_stream_len = minis * 64;
        let root_start = chain(&mut fat, sectors(mini_stream_len, 512));
        let mut data = stream.to_vec();
        data.resize(sectors(mini_stream_len, 512) * 512, 0);
        body.extend(data);
        (root_start, mini_stream_len as u32, 0, mini_fat_start)
    } else {
        let start = chain(&mut fat, sectors(stream.len(), 512));
        let mut data = stream.to_vec();
        data.resize(sectors(stream.len(), 512) * 512, 0);
        body.extend(data);
        (END, 0, start, END)
    };
    assert!(fat.len() <= 128, "one FAT sector");
    fat.resize(128, FREE);
    let entry = |name: &str, typ: u8, child: u32, start: u32, len: u32| {
        let mut e = vec![0u8; 128];
        let units: Vec<u8> = name
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        e[..units.len()].copy_from_slice(&units);
        e[64..66].copy_from_slice(&(units.len() as u16).to_le_bytes());
        e[66] = typ;
        e[67] = 1;
        e[68..72].copy_from_slice(&FREE.to_le_bytes());
        e[72..76].copy_from_slice(&FREE.to_le_bytes());
        e[76..80].copy_from_slice(&child.to_le_bytes());
        e[116..120].copy_from_slice(&start.to_le_bytes());
        e[120..124].copy_from_slice(&len.to_le_bytes());
        e
    };
    let dir = [
        entry("Root Entry", 5, 1, root_start, root_len),
        entry(name, 2, FREE, start, stream.len() as u32),
        entry("", 0, FREE, 0, 0),
        entry("", 0, FREE, 0, 0),
    ]
    .concat();
    let mut header = vec![0u8; 512];
    header[..8].copy_from_slice(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1");
    for (at, v) in [(24, 0x003Eu16), (26, 3), (28, 0xFFFE), (30, 9), (32, 6)] {
        header[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    let mini_fats = if mini { 1 } else { 0 };
    for (at, v) in [
        (44, 1u32),
        (48, 1),
        (56, 4096),
        (60, mini_fat_start),
        (64, mini_fats),
        (68, END),
        (72, 0),
    ] {
        header[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut difat = vec![0u32];
    difat.resize(109, FREE);
    header[76..].copy_from_slice(
        &difat
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    );
    [
        header,
        fat.iter().flat_map(|v| v.to_le_bytes()).collect(),
        dir,
        body,
    ]
    .concat()
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
