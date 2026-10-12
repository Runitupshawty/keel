//! File preview dispatch and rendering types for Keel.

mod audio;
mod docx;
mod extract; // Task 62: text inside documents
mod font;
/// Fuzz entry points (`fuzz/` and the in-tree runs; docs/fuzzing.md).
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;
mod hex;
mod image;
mod odf;
mod office;
mod pdf;
mod pptx;
#[cfg(feature = "probes")]
pub mod probes;
mod table;
mod text;
mod video;
mod xls; // the bound calamine needs before it opens an .xls

pub use extract::{extract_text, extracts, text_extractor, Extracted, TextError, TEXT_EXTENSIONS};
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
    /// An audio file (ffprobe and ffmpeg); the app plays it.
    Audio {
        /// The embedded cover art.
        cover: Option<Rgba>,
        /// The waveform, when it could be drawn (files up to 20 minutes).
        wave: Option<Rgba>,
        /// 0 when unknown.
        duration_s: f64,
        /// (label, value) for title, artist and album, those the file has.
        tags: Vec<(String, String)>,
        /// "flac · 44.1 kHz · stereo · 912 kb/s".
        meta: String,
    },
    /// A font file: a sample sentence drawn in it.
    Font {
        sample: Rgba,
        /// Family and style ("Ubuntu Light").
        name: String,
        /// "TrueType · 1,234 glyphs · first of 4 fonts".
        info: String,
    },
    Hex {
        head: Vec<u8>,
        size: u64,
    },
    TooLarge(u64),
    Unsupported,
    /// A helper this file type needs is not installed, or Keel cannot read this kind of
    /// file; the text says what to do or why.
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

/// Formats whose renderer reads only what it needs (a PDF page, one video frame, a RAW
/// file's embedded preview, an audio file's tags), so the `MAX_PREVIEW_BYTES` cap does not
/// apply to them.
pub fn streams(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    pdf::accepts(&ext) || video::accepts(&ext) || image::is_raw(&ext) || audio::accepts(&ext)
}

/// Audio files: the app offers playback for their previews.
pub fn is_audio(ext: &str) -> bool {
    audio::accepts(&ext.trim_start_matches('.').to_ascii_lowercase())
}

pub fn accepts(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    text::accepts(&ext)
        || image::accepts(&ext)
        || table::accepts(&ext)
        || pdf::accepts(&ext)
        || docx::accepts(&ext)
        || pptx::accepts(&ext)
        || odf::accepts(&ext)
        || video::accepts(&ext)
        || audio::accepts(&ext)
        || font::accepts(&ext)
}

pub fn init_pdfium(dll_dir: &Path) {
    pdf::init(dll_dir);
}

pub fn preview(req: &Request) -> Preview {
    let caught = keel_vfs::quiet_panic::quiet(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| preview_inner(req)))
    });
    match caught {
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

pub(crate) fn preview_inner(req: &Request) -> Preview {
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
    if odf::accepts(&ext) {
        return odf::render(req, &ext);
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
    if pptx::accepts(&ext) {
        return pptx::render(req);
    }
    if video::accepts(&ext) {
        return video::render(req);
    }
    if audio::accepts(&ext) {
        return audio::render(req);
    }
    if font::accepts(&ext) {
        return font::render(req);
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
    fn only_pdf_video_raw_and_audio_skip_the_size_cap() {
        for ext in [
            "pdf", "PDF", "mp4", ".mkv", "dng", "NEF", "cr3", "flac", "WAV",
        ] {
            assert!(streams(ext), "{ext}");
        }
        for ext in ["txt", "png", "csv", "docx", ""] {
            assert!(!streams(ext), "{ext}");
        }
    }
}
