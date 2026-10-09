use keel_preview::{accepts, init_pdfium, preview, DocBlock, Preview, Request, MAX_PREVIEW_BYTES};
use keel_vfs::{Entry, Kind, VPath};
use std::fs::File;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn request(name: &str) -> Request {
    let path = fixture(name);
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    temp_request(&path, &ext, 256)
}

/// Request for any path; `ext` is passed as-is (may be upper case).
fn temp_request(path: &Path, ext: &str, max_px: u32) -> Request {
    Request {
        entry: Entry {
            path: VPath::local(path),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            kind: Kind::File,
            size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            modified: None,
            hidden: false,
            ext: ext.into(),
        },
        bytes_path: path.to_owned(),
        page: 0,
        max_px,
    }
}

#[test]
fn dispatcher_accepts_supported_extensions() {
    assert!(accepts("rs"));
    assert!(accepts("png"));
    assert!(!accepts("xyz"));
}

#[test]
fn oversized_file_is_rejected_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.rs");
    File::create(&path)
        .unwrap()
        .set_len(MAX_PREVIEW_BYTES + 1)
        .unwrap();
    assert!(matches!(
        preview(&temp_request(&path, "rs", 256)),
        Preview::TooLarge(n) if n == MAX_PREVIEW_BYTES + 1
    ));
}

#[test]
fn directory_is_unsupported_not_read() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        preview(&temp_request(dir.path(), "rs", 256)),
        Preview::Unsupported
    ));
}

#[test]
fn rust_source_is_highlighted() {
    let Preview::Text {
        lines, language, ..
    } = preview(&request("sample.rs"))
    else {
        panic!("expected text preview");
    };
    assert_eq!(language, "Rust");
    assert!(lines
        .iter()
        .any(|line| line.iter().any(|(_, text)| text.contains("fn")) && line.len() > 1));
}

fn text_of(preview: Preview) -> (String, String, bool) {
    let Preview::Text {
        lines,
        language,
        truncated,
    } = preview
    else {
        panic!("expected text preview, got {preview:?}");
    };
    let text = lines.iter().flatten().map(|(_, t)| t.as_str()).collect();
    (text, language, truncated)
}

#[test]
fn utf16_with_bom_is_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend("hello wörld\n".encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&path, bytes).unwrap();
    let (text, _, _) = text_of(preview(&temp_request(&path, "txt", 256)));
    assert_eq!(text, "hello wörld\n");
}

#[test]
fn legacy_codepage_text_is_shown_lossily() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cp1252.txt");
    std::fs::write(&path, b"caf\xe9 au lait\nna\xefve\n").unwrap();
    let (text, _, _) = text_of(preview(&temp_request(&path, "txt", 256)));
    assert!(text.starts_with("caf"));
    assert!(text.contains("au lait"));
}

#[test]
fn mostly_control_bytes_fall_back_to_hex() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("weird.txt");
    let bytes: Vec<u8> = (0..1000_usize).map(|i| [1, 2, 3, 0xff][i % 4]).collect();
    std::fs::write(&path, bytes).unwrap();
    assert!(matches!(
        preview(&temp_request(&path, "txt", 256)),
        Preview::Hex { .. }
    ));
}

#[test]
fn huge_single_line_is_capped_by_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bundle.min.js");
    std::fs::write(&path, "var a=1;".repeat(1024 * 1024)).unwrap(); // 8 MiB, one line
    let started = std::time::Instant::now();
    let (text, _, truncated) = text_of(preview(&temp_request(&path, "js", 256)));
    assert!(truncated);
    assert!(text.len() <= 2 * 1024 * 1024);
    // Uncoloured past the highlight budget, so this stays far below the 5 s preview limit.
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn two_face_syntaxes_are_available() {
    let dir = tempfile::tempdir().unwrap();
    for (name, ext, language) in [
        ("Cargo.toml", "toml", "TOML"),
        ("app.ts", "ts", "TypeScript"),
        // PowerShell: two-face ships it for onig only; fancy-regex keeps it out.
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, "x = 1\n").unwrap();
        let (_, found, _) = text_of(preview(&temp_request(&path, ext, 256)));
        assert_eq!(found, language, "{name}");
    }
}

#[test]
fn raster_image_keeps_small_dimensions() {
    let Preview::Image(image) = preview(&request("sample.png")) else {
        panic!("expected image preview");
    };
    assert_eq!(image.w, 10);
    assert_eq!(image.h, 10);
}

#[test]
fn tiny_svg_scales_up_to_fit_box() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("icon.svg");
    std::fs::write(
        &path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="8"><rect width="16" height="8" fill="red"/></svg>"#,
    )
    .unwrap();
    let Preview::Image(image) = preview(&temp_request(&path, "svg", 256)) else {
        panic!("expected image preview");
    };
    assert_eq!((image.w, image.h), (256, 128));
}

#[test]
fn svg_text_renders_with_system_fonts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("label.svg");
    std::fs::write(
        &path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="40"><text x="5" y="30" font-size="24" font-family="sans-serif">Keel</text></svg>"#,
    )
    .unwrap();
    let Preview::Image(image) = preview(&temp_request(&path, "svg", 100)) else {
        panic!("expected image preview");
    };
    let drawn = image
        .data
        .iter()
        .skip(3)
        .step_by(4)
        .filter(|&&a| a > 0)
        .count();
    assert!(drawn > 0, "text drew nothing");
}

#[test]
fn csv_has_headers_and_two_data_rows() {
    let Preview::Table { headers, rows, .. } = preview(&request("sample.csv")) else {
        panic!("expected table preview");
    };
    assert_eq!(headers.len(), 2);
    assert_eq!(rows.len(), 2);
}

#[test]
fn upper_case_extension_still_dispatches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("DATA.CSV");
    std::fs::write(&path, "a,b\n1,2\n").unwrap();
    assert!(matches!(
        preview(&temp_request(&path, "CSV", 256)),
        Preview::Table { .. }
    ));
}

#[test]
fn csv_with_invalid_utf8_still_previews() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("latin1.csv");
    std::fs::write(&path, b"name,city\nJos\xe9,M\xfcnchen\nAnn,Oslo\n").unwrap();
    let Preview::Table { headers, rows, .. } = preview(&temp_request(&path, "csv", 256)) else {
        panic!("expected table preview");
    };
    assert_eq!(headers, ["name", "city"]);
    assert_eq!(rows.len(), 2);
    assert!(rows[0][0].starts_with("Jos"));
}

#[test]
fn csv_stops_at_row_cap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.csv");
    let body: String = std::iter::once("a,b\n".to_owned())
        .chain((0..3000).map(|i| format!("{i},{i}\n")))
        .collect();
    std::fs::write(&path, body).unwrap();
    let Preview::Table {
        rows, truncated, ..
    } = preview(&temp_request(&path, "csv", 256))
    else {
        panic!("expected table preview");
    };
    assert_eq!(rows.len(), 2000);
    assert!(truncated);
}

#[test]
fn xlsx_has_data_rows() {
    let Preview::Table { rows, .. } = preview(&request("sample.xlsx")) else {
        panic!("expected table preview");
    };
    assert!(!rows.is_empty());
}

#[test]
fn docx_heading_and_paragraph_are_extracted() {
    let Preview::Doc { blocks } = preview(&request("sample.docx")) else {
        panic!("expected document preview");
    };
    assert_eq!(blocks.len(), 2);
    assert!(matches!(blocks.first(), Some(DocBlock::Heading(1, _))));
}

#[test]
fn docx_text_inside_hyperlink_insert_and_content_control() {
    use docx_rs::{Docx, Hyperlink, HyperlinkType, Insert, Paragraph, Run, StructuredDataTag};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rich.docx");
    Docx::new()
        .add_paragraph(
            Paragraph::new()
                .add_run(Run::new().add_text("plain "))
                .add_hyperlink(
                    Hyperlink::new("https://example.com", HyperlinkType::External)
                        .add_run(Run::new().add_text("linked ")),
                )
                .add_insert(Insert::new(Run::new().add_text("inserted ")))
                .add_structured_data_tag(
                    StructuredDataTag::new().add_run(Run::new().add_text("control")),
                ),
        )
        .build()
        .pack(File::create(&path).unwrap())
        .unwrap();
    let Preview::Doc { blocks } = preview(&temp_request(&path, "docx", 256)) else {
        panic!("expected document preview");
    };
    let Some(DocBlock::Para(text)) = blocks.first() else {
        panic!("expected a paragraph, got {blocks:?}");
    };
    assert_eq!(text, "plain linked inserted control");
}

#[test]
fn binary_data_uses_bounded_hex_preview() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.bin");
    let bytes: Vec<u8> = (0..8192_usize).map(|i| (i % 256) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let Preview::Hex { head, size } = preview(&temp_request(&path, "bin", 256)) else {
        panic!("expected hex preview");
    };
    assert_eq!(head[..], bytes[..4096]);
    assert_eq!(size, 8192);
}

fn pdfium_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deps");
    pdfium_render::prelude::Pdfium::pdfium_platform_library_name_at_path(&dir)
        .exists()
        .then_some(dir)
}

#[test]
fn pdf_has_one_page_and_fits_box() {
    let Some(dir) = pdfium_dir() else {
        eprintln!("skipping: pdfium library missing from target/deps");
        return;
    };
    init_pdfium(&dir);
    let Preview::Pdf { pages, image, .. } = preview(&request("sample.pdf")) else {
        panic!("expected pdf preview");
    };
    assert_eq!(pages, 1);
    assert!(image.w <= 256 && image.h <= 256, "{}x{}", image.w, image.h);
    assert!(image.w.max(image.h) >= 255, "{}x{}", image.w, image.h);
}

#[test]
fn corrupt_pdf_returns_error() {
    let Some(dir) = pdfium_dir() else {
        eprintln!("skipping: pdfium library missing from target/deps");
        return;
    };
    init_pdfium(&dir);
    assert!(matches!(
        preview(&request("corrupt.pdf")),
        Preview::Error(_)
    ));
}

fn make_clip(path: &Path, duration: &str) {
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc=duration={duration}:size=64x64"))
        .arg("-y")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn video_thumbnail_when_ffmpeg_is_available() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping: ffmpeg is missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.mp4");
    make_clip(&path, "2");
    let Preview::Video { thumb, .. } = preview(&temp_request(&path, "mp4", 64)) else {
        panic!("expected video preview");
    };
    assert_eq!(thumb.w, 64);

    // Under a second: the `-ss 1` attempt yields nothing, the `-ss 0` retry must.
    let short = dir.path().join("short.mp4");
    make_clip(&short, "0.5");
    let result = preview(&temp_request(&short, "mp4", 32));
    let Preview::Video { thumb, .. } = result else {
        panic!("expected video preview for a 0.5 s clip, got {result:?}");
    };
    assert_eq!(thumb.w, 32);
}
