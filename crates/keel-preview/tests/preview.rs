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
    Request {
        entry: Entry {
            path: VPath::local(&path),
            name: name.to_owned(),
            kind: Kind::File,
            size: std::fs::metadata(&path).unwrap().len(),
            modified: None,
            hidden: false,
            ext,
        },
        bytes_path: path,
        page: 0,
        max_px: 256,
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
    let req = Request {
        entry: Entry {
            path: VPath::local(&path),
            name: "huge.rs".into(),
            kind: Kind::File,
            size: MAX_PREVIEW_BYTES + 1,
            modified: None,
            hidden: false,
            ext: "rs".into(),
        },
        bytes_path: path,
        page: 0,
        max_px: 256,
    };
    assert!(matches!(preview(&req), Preview::TooLarge(n) if n == MAX_PREVIEW_BYTES + 1));
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

#[test]
fn raster_image_keeps_small_dimensions() {
    let Preview::Image(image) = preview(&request("sample.png")) else {
        panic!("expected image preview");
    };
    assert_eq!(image.w, 10);
    assert_eq!(image.h, 10);
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
fn binary_data_uses_bounded_hex_preview() {
    let mut req = request("sample.png");
    req.entry.ext = "bin".into();
    let Preview::Hex { head, size } = preview(&req) else {
        panic!("expected hex preview");
    };
    assert!(head.len() <= 4096);
    assert!(size > 0);
}

fn pdfium_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deps");
    dir.join("pdfium.dll").exists().then_some(dir)
}

#[test]
fn pdf_has_one_page() {
    let Some(dir) = pdfium_dir() else {
        eprintln!("skipping: target/deps/pdfium.dll is missing");
        return;
    };
    init_pdfium(&dir);
    assert!(matches!(
        preview(&request("sample.pdf")),
        Preview::Pdf { pages: 1, .. }
    ));
}

#[test]
fn corrupt_pdf_returns_error() {
    let Some(dir) = pdfium_dir() else {
        eprintln!("skipping: target/deps/pdfium.dll is missing");
        return;
    };
    init_pdfium(&dir);
    assert!(matches!(
        preview(&request("corrupt.pdf")),
        Preview::Error(_)
    ));
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
    let status = std::process::Command::new("ffmpeg")
        .args(["-f", "lavfi", "-i", "testsrc=duration=2:size=64x64", "-y"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let req = Request {
        entry: Entry {
            path: VPath::local(&path),
            name: "sample.mp4".into(),
            kind: Kind::File,
            size: std::fs::metadata(&path).unwrap().len(),
            modified: None,
            hidden: false,
            ext: "mp4".into(),
        },
        bytes_path: path,
        page: 0,
        max_px: 64,
    };
    let Preview::Video { thumb, .. } = preview(&req) else {
        panic!("expected video preview");
    };
    assert_eq!(thumb.w, 64);
}
