//! File preview dispatch and rendering types for Keel.

mod docx;
mod hex;
mod image;
mod pdf;
mod table;
mod text;
mod video;

use std::path::{Path, PathBuf};

pub const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Rgba {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum DocBlock {
    Heading(u8, String),
    Para(String),
    Table(Vec<Vec<String>>),
    Image(Rgba),
}

#[derive(Clone, Debug)]
pub enum Preview {
    Text {
        lines: Vec<Vec<([u8; 4], String)>>,
        language: String,
        truncated: bool,
    },
    Markdown(String),
    Image(Rgba),
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        truncated: bool,
    },
    Pdf {
        pages: u32,
        page: u32,
        image: Rgba,
    },
    Doc {
        blocks: Vec<DocBlock>,
    },
    Video {
        thumb: Rgba,
        duration_s: f64,
        meta: String,
    },
    Hex {
        head: Vec<u8>,
        size: u64,
    },
    TooLarge(u64),
    Unsupported,
    /// A helper this file type needs is not installed; the text says what to do.
    Missing(&'static str),
    Error(String),
}

pub struct Request {
    pub entry: keel_vfs::Entry,
    pub bytes_path: PathBuf,
    pub page: u32,
    pub max_px: u32,
    /// PDF pages: `max_px` is the page width (the preview panel) instead of a fit box for
    /// the longer side (thumbnails).
    pub fit_width: bool,
}

/// Formats whose renderer reads only what it needs (a PDF page, one video frame), so the
/// `MAX_PREVIEW_BYTES` cap does not apply to them.
pub fn streams(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    pdf::accepts(&ext) || video::accepts(&ext)
}

pub fn accepts(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    text::accepts(&ext)
        || image::accepts(&ext)
        || table::accepts(&ext)
        || pdf::accepts(&ext)
        || docx::accepts(&ext)
        || video::accepts(&ext)
}

pub fn init_pdfium(dll_dir: &Path) {
    pdf::init(dll_dir);
}

pub fn preview(req: &Request) -> Preview {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| preview_inner(req))) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            Preview::Error(format!("previewer panicked: {message}"))
        }
    }
}

fn preview_inner(req: &Request) -> Preview {
    let metadata = match std::fs::metadata(&req.bytes_path) {
        Ok(metadata) => metadata,
        Err(error) => return Preview::Error(error.to_string()),
    };
    // FIFOs, devices and directories would block or make no sense to read.
    if !metadata.is_file() {
        return Preview::Unsupported;
    }
    let size = metadata.len();
    if size > MAX_PREVIEW_BYTES && !streams(&req.entry.ext) {
        return Preview::TooLarge(size);
    }

    #[cfg(any(test, feature = "test-hooks"))]
    if req.entry.ext == "keel-panic" {
        panic!("preview test hook");
    }

    let ext = req.entry.ext.trim_start_matches('.').to_ascii_lowercase();
    if text::accepts(&ext) {
        return text::render(req, &ext, size);
    }
    if image::accepts(&ext) {
        return image::render(req, &ext);
    }
    if table::accepts(&ext) {
        return table::render(req, &ext);
    }
    if pdf::accepts(&ext) {
        return pdf::render(req);
    }
    if docx::accepts(&ext) {
        return docx::render(req);
    }
    if video::accepts(&ext) {
        return video::render(req);
    }
    hex::render(req, size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::{Entry, Kind, VPath};

    #[test]
    fn panic_is_converted_to_error() {
        let path = std::env::temp_dir().join("keel-preview-panic-hook");
        std::fs::write(&path, b"x").unwrap();
        let req = Request {
            entry: Entry {
                path: VPath::local(&path),
                name: "panic".into(),
                kind: Kind::File,
                size: 1,
                modified: None,
                hidden: false,
                is_link: false,
                encrypted: false,
                ext: "keel-panic".into(),
            },
            bytes_path: path,
            page: 0,
            max_px: 1,
            fit_width: false,
        };
        assert!(matches!(preview(&req), Preview::Error(message) if message.contains("panicked")));
    }

    #[test]
    fn only_pdf_and_video_skip_the_size_cap() {
        for ext in ["pdf", "PDF", "mp4", ".mkv"] {
            assert!(streams(ext), "{ext}");
        }
        for ext in ["txt", "png", "csv", "docx", ""] {
            assert!(!streams(ext), "{ext}");
        }
    }
}
