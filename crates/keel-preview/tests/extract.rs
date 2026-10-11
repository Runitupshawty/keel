//! Task 62: `extract_text`, one tiny synthetic file per format (made here, "Keel Test
//! Document" wording), the caps, damaged files.

use keel_core::{TextCaps, TextOutcome, TEXT_CAPS};
use keel_preview::probes::{lie_about_sizes, nested_tables, replace_entry, sparse_xlsb, word};
use keel_preview::{
    extract_text, extracts, init_pdfium, preview, text_extractor, Preview, Request, TextError,
};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn zip_file(dir: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(name);
    let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (entry, body) in entries {
        zip.start_file(*entry, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

fn text_of(path: &Path) -> String {
    let ext = path.extension().unwrap().to_str().unwrap();
    match extract_text(path, ext, &TEXT_CAPS) {
        Ok(e) => {
            assert!(!e.truncated, "{}", path.display());
            e.text
        }
        Err(e) => panic!("{}: {e:?}", path.display()),
    }
}

/// A one-page PDF showing `line` in Helvetica (offsets computed, as pdfium reads them).
fn pdf(path: &Path, line: &str) {
    let stream = format!("BT /F1 12 Tf 20 100 Td ({line}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{obj}\nendobj\n", i + 1));
    }
    let xref = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for o in offsets {
        out.push_str(&format!("{o:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    std::fs::write(path, out).unwrap();
}

fn pdfium_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deps");
    pdfium_render::prelude::Pdfium::pdfium_platform_library_name_at_path(&dir)
        .exists()
        .then_some(dir)
}

const ODF_NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0""#;

fn odf(dir: &Path, name: &str, body: &str) -> PathBuf {
    let xml = format!(
        "<office:document-content {ODF_NS}><office:body>{body}</office:body></office:document-content>"
    );
    zip_file(dir, name, &[("content.xml", &xml)])
}

const XLSX_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;

fn sheet(rows: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
    )
}

fn xlsx(dir: &Path) -> PathBuf {
    let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    let workbook = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Budget" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
    let wb_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>"#;
    let one = sheet(
        r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Keel</t></is></c><c r="B1" t="inlineStr"><is><t>Test</t></is></c></row>
<row r="2"><c r="A2" t="inlineStr"><is><t>ledger</t></is></c><c r="B2"><v>42</v></c></row>"#,
    );
    let two = sheet(r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Document</t></is></c></row>"#);
    zip_file(
        dir,
        "book.xlsx",
        &[
            ("[Content_Types].xml", XLSX_TYPES),
            ("_rels/.rels", rels),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", wb_rels),
            ("xl/worksheets/sheet1.xml", &one),
            ("xl/worksheets/sheet2.xml", &two),
        ],
    )
}

#[test]
fn every_format_reads_its_text() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let write = |name: &str, body: &[u8]| {
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        p
    };

    let txt = write("notes.txt", b"Keel Test Document\r\nsecond line\n");
    assert_eq!(text_of(&txt), "Keel Test Document\nsecond line\n");
    // UTF-16 with a BOM and Windows-1252 decode as in the preview.
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend("Keel café".encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(text_of(&write("wide.txt", &utf16)), "Keel café\n");
    assert_eq!(text_of(&write("ansi.log", b"caf\xe9 Keel")), "café Keel\n");
    let code = write("main.rs", b"// Keel Test Document\nfn main() {}\n");
    assert!(text_of(&code).contains("fn main() {}"));
    let md = write(
        "readme.md",
        b"# Keel *Test* Document\n\nA [link text](https://example.invalid) and `code`.\n\n- item one\n",
    );
    assert_eq!(
        text_of(&md),
        "Keel Test Document\nA link text and code.\nitem one\n"
    );
    let csv = write("table.csv", b"name,value\nKeel,1\n");
    assert!(text_of(&csv).contains("Keel,1"));

    // docx (docx-rs writes it).
    {
        use docx_rs::*;
        let path = d.join("letter.docx");
        Docx::new()
            .add_paragraph(
                Paragraph::new()
                    .style("Heading1")
                    .add_run(Run::new().add_text("Keel Test Document")),
            )
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Dear reader")))
            .add_table(Table::new(vec![TableRow::new(vec![
                TableCell::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text("c1"))),
                TableCell::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text("c2"))),
            ])]))
            .build()
            .pack(File::create(&path).unwrap())
            .unwrap();
        assert_eq!(text_of(&path), "Keel Test Document\nDear reader\nc1\tc2\n");
    }

    // xlsx: every sheet, by name.
    assert_eq!(
        text_of(&xlsx(d)),
        "Budget\nKeel\tTest\nledger\t42\nNotes\nDocument\n"
    );

    // pptx: slide text without the previewer's "Slide n" headings or slide numbers.
    let slide = |title: &str, body: &str| {
        format!(
            r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>7</a:t></a:r></a:p></p:txBody></p:sp>
</p:spTree></p:cSld></p:sld>"#
        )
    };
    let (s1, s2) = (slide("Keel", "Test"), slide("Document", "quarterly"));
    let deck = zip_file(
        d,
        "deck.pptx",
        &[
            ("ppt/slides/slide2.xml", &s2),
            ("ppt/slides/slide1.xml", &s1),
        ],
    );
    assert_eq!(text_of(&deck), "Keel\nTest\nDocument\nquarterly\n");

    // OpenDocument text, spreadsheet and presentation.
    let odt = odf(
        d,
        "a.odt",
        r#"<office:text><text:h text:outline-level="1">Keel Test Document</text:h><text:p>Hello <text:span>there</text:span></text:p></office:text>"#,
    );
    assert_eq!(text_of(&odt), "Keel Test Document\nHello there\n");
    let ods = odf(
        d,
        "a.ods",
        r#"<office:spreadsheet><table:table table:name="Sheet1"><table:table-row><table:table-cell><text:p>Keel</text:p></table:table-cell><table:table-cell><text:p>Test</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet>"#,
    );
    assert_eq!(text_of(&ods), "Sheet1\nKeel\tTest\n");
    let odp = odf(
        d,
        "a.odp",
        r#"<office:presentation><draw:page><draw:frame><draw:text-box><text:p>Keel Test Document</text:p></draw:text-box></draw:frame></draw:page></office:presentation>"#,
    );
    assert_eq!(text_of(&odp), "Keel Test Document\n");

    // PDF, through pdfium when it is there (scripts/fetch-deps).
    let p = d.join("scan.pdf");
    pdf(&p, "Keel Test Document");
    match pdfium_dir() {
        Some(dir) => {
            init_pdfium(&dir);
            assert!(
                text_of(&p).contains("Keel Test Document"),
                "{}",
                text_of(&p)
            );
            // A damaged PDF fails.
            let bad = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corrupt.pdf");
            assert!(matches!(
                extract_text(&bad, "pdf", &TEXT_CAPS),
                Err(TextError::Failed(_))
            ));
        }
        None => eprintln!("skipping the PDF: pdfium library missing from target/deps"),
    }
}

#[test]
fn caps_skip_big_files_and_cut_long_text() {
    let dir = tempfile::tempdir().unwrap();
    // 9 MiB of text: over the 8 MiB cap, never read.
    let big = dir.path().join("big.txt");
    File::create(&big).unwrap().set_len(9 << 20).unwrap();
    assert_eq!(
        extract_text(&big, "txt", &TEXT_CAPS),
        Err(TextError::TooLarge(9 << 20))
    );
    // A PDF has room up to 64 MiB: 9 MiB is under its cap (and not a PDF: it fails).
    let fake = dir.path().join("big.pdf");
    File::create(&fake).unwrap().set_len(9 << 20).unwrap();
    assert!(!matches!(
        extract_text(&fake, "pdf", &TEXT_CAPS),
        Err(TextError::TooLarge(_))
    ));
    // 250,000 characters: the first 200,000 are kept.
    let long = dir.path().join("long.txt");
    let line = "é".repeat(99) + "\n";
    std::fs::write(&long, line.repeat(2_500)).unwrap();
    let e = extract_text(&long, "txt", &TEXT_CAPS).unwrap();
    assert!(e.truncated);
    assert_eq!(e.text.chars().count(), 200_000);
    // Out of time: what was read so far, truncated.
    let caps = TextCaps {
        time: Duration::ZERO,
        ..TEXT_CAPS
    };
    let e = extract_text(&long, "txt", &caps).unwrap();
    assert!(e.truncated);
    assert!(e.text.is_empty());
}

#[test]
fn damaged_and_unknown_files_are_not_text() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // The fuzz corpus's docx with a corrupt deflate stream (docx-rs panics on it).
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/preview");
    let bytes = std::fs::read(corpus.join("docx-corrupt-deflate")).unwrap();
    let bad = d.join("bad.docx");
    std::fs::write(&bad, &bytes[1..]).unwrap();
    assert!(matches!(
        extract_text(&bad, "docx", &TEXT_CAPS),
        Err(TextError::Failed(_))
    ));
    for ext in ["docx", "pptx", "odt", "xlsx"] {
        let p = d.join(format!("plain.{ext}"));
        std::fs::write(&p, b"this is not a zip").unwrap();
        assert!(
            matches!(extract_text(&p, ext, &TEXT_CAPS), Err(TextError::Failed(_))),
            "{ext}"
        );
    }
    // No reader: an unknown extension, a binary .txt, a folder.
    let p = d.join("photo.xyz");
    std::fs::write(&p, b"Keel").unwrap();
    assert_eq!(extract_text(&p, "xyz", &TEXT_CAPS), Err(TextError::Kind));
    assert!(!extracts("xyz") && extracts("PDF") && extracts(".docx"));
    let p = d.join("blob.txt");
    std::fs::write(&p, b"\x00\x01\x02binary").unwrap();
    assert_eq!(extract_text(&p, "txt", &TEXT_CAPS), Err(TextError::Kind));
    assert_eq!(extract_text(d, "txt", &TEXT_CAPS), Err(TextError::Kind));
    // As the library's text reader.
    let x = text_extractor();
    assert!(x.extensions.contains(&"odp"));
    assert_eq!((x.extract)(&p, "txt", &TEXT_CAPS), TextOutcome::SkippedKind);
    let big = d.join("big.md");
    File::create(&big).unwrap().set_len(9 << 20).unwrap();
    assert_eq!(
        (x.extract)(&big, "md", &TEXT_CAPS),
        TextOutcome::SkippedSize
    );
}

/// The file's preview (the same readers, opened by hand or through the daemon).
fn preview_of(path: &Path, ext: &str) -> Preview {
    preview(&Request {
        entry: keel_vfs::Entry {
            path: keel_vfs::VPath::local(path),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            kind: keel_vfs::Kind::File,
            size: std::fs::metadata(path).unwrap().len(),
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: ext.into(),
        },
        bytes_path: path.to_owned(),
        page: 0,
        max_px: 256,
        fit_width: false,
    })
}

fn failed(path: &Path, ext: &str) -> String {
    match extract_text(path, ext, &TEXT_CAPS) {
        Err(TextError::Failed(why)) => why,
        other => panic!("{}: {other:?}", path.display()),
    }
}

/// Review of 0.22.0, B1: 20,000 nested tables used to overflow the reader's stack (an
/// abort no panic hook sees); past 64 tables (or 256 elements) the file is damaged. 64
/// still read, on a thread with the text job's default stack.
#[test]
fn deeply_nested_documents_fail_without_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_owned();
    let deep = word(&d, "deep.docx", &nested_tables(20_000));
    assert!(std::fs::metadata(&deep).unwrap().len() < 100_000);
    assert_eq!(failed(&deep, "docx"), "nests too deeply");
    match preview_of(&deep, "docx") {
        Preview::Error(e) => assert!(e.ends_with("nests too deeply"), "{e}"),
        other => panic!("{other:?}"),
    }
    let elements = format!("<w:p>{}{}</w:p>", "<w:r>".repeat(300), "</w:r>".repeat(300));
    assert_eq!(
        failed(&word(&d, "runs.docx", &elements), "docx"),
        "nests too deeply"
    );
    // Allowed nesting reads on a thread with the text job's default stack: 64 tables, and
    // 120 content controls one inside another (240 elements).
    let ok = word(&d, "ok.docx", &nested_tables(64));
    let controls = format!(
        "{}<w:p><w:r><w:t>Keel</w:t></w:r></w:p>{}",
        "<w:sdt><w:sdtContent>".repeat(120),
        "</w:sdtContent></w:sdt>".repeat(120)
    );
    let tags = word(&d, "tags.docx", &controls);
    std::thread::spawn(move || {
        extract_text(&ok, "docx", &TEXT_CAPS).unwrap();
        assert!(matches!(preview_of(&ok, "docx"), Preview::Doc { .. }));
        extract_text(&tags, "docx", &TEXT_CAPS).unwrap();
    })
    .join()
    .unwrap();
}

/// Review of 0.22.0, M1: a zip that declares 1 KiB per part inflates through the 64 MiB
/// budget, not to its end (docx-rs and calamine read a bounded copy).
#[test]
fn zips_that_lie_about_sizes_stay_within_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let text = format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", "a".repeat(65 << 20));
    let docx = word(d, "liar.docx", &text);
    lie_about_sizes(&docx, 1024);
    assert!(std::fs::metadata(&docx).unwrap().len() < (1 << 20));
    let why = failed(&docx, "docx");
    assert!(
        why == "exceeds the 64 MiB decompressed limit" || why == "not a valid Word document",
        "{why}"
    );
    // calamine's shared strings, past the budget.
    let book = xlsx(d);
    let strings = format!(
        r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" uniqueCount="1"><si><t>{}</t></si></sst>"#,
        "b".repeat(65 << 20)
    );
    let liar = d.join("liar.xlsx");
    replace_entry(&book, &liar, "xl/sharedStrings.xml", &strings);
    lie_about_sizes(&liar, 1024);
    let why = failed(&liar, "xlsx");
    assert!(
        why == "exceeds the 64 MiB decompressed limit" || why == "not a valid spreadsheet",
        "{why}"
    );
    // A count of strings no part of its size can hold (calamine reserves room for it).
    let counted = d.join("counted.xlsx");
    replace_entry(
        &book,
        &counted,
        "xl/sharedStrings.xml",
        r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" uniqueCount="99999999999"><si><t>x</t></si></sst>"#,
    );
    assert_eq!(failed(&counted, "xlsx"), "not a valid spreadsheet");
}

/// Review of 0.22.0, B2: one stray cell at XFD1048576 made calamine allocate the whole
/// sheet (549,755,813,888 bytes, an abort); xlsb streams its cells now. .xls is not read.
#[test]
fn a_sparse_workbook_streams_and_xls_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = sparse_xlsb(dir.path());
    assert!(std::fs::metadata(&path).unwrap().len() < 2_000);
    assert_eq!(text_of(&path), "Sheet1\nKeel\n2\n");
    // The preview's first sheet streams too: the header row, then the cut.
    match preview_of(&path, "xlsb") {
        Preview::Table {
            headers,
            rows,
            truncated,
        } => assert_eq!(
            (headers, rows.len(), truncated),
            (vec!["Keel".into()], 0, true)
        ),
        other => panic!("{other:?}"),
    }
    let xls = dir.path().join("old.xls");
    std::fs::write(&xls, b"Keel").unwrap();
    assert_eq!(extract_text(&xls, "xls", &TEXT_CAPS), Err(TextError::Kind));
    assert!(!extracts("xls"));
}

/// Review of 0.22.0, M4: a reader's own message can quote the file (calamine puts an
/// unknown error cell's text in its error); the reason is a fixed phrase.
#[test]
fn failure_reasons_never_quote_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let marker = format!("SECRETMARKER salary 98765 {}", "x".repeat(3_000));
    let bad = dir.path().join("error.xlsx");
    let cell = format!(r#"<row r="1"><c r="A1" t="e"><v>{marker}</v></c></row>"#);
    replace_entry(
        &xlsx(dir.path()),
        &bad,
        "xl/worksheets/sheet1.xml",
        &sheet(&cell),
    );
    assert_eq!(failed(&bad, "xlsx"), "not a valid spreadsheet");
    // An entry name is the file's too.
    let named = zip_file(dir.path(), "named.pptx", &[(&marker[..60], "x")]);
    let why = failed(&named, "pptx");
    assert!(!why.contains("SECRETMARKER"), "{why}");
}
