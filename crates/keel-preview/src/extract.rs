//! Text inside documents for the library's content search (Task 62): [`extract_text`]
//! reads a file's text with the preview's own readers, bounded by `keel_core::TextCaps`
//! (input size, characters kept, time).

use crate::office::{Package, TOO_BIG, TOO_DEEP, TRUNCATED};
use crate::{docx, odf, pdf, pptx, table, text, DocBlock};
use keel_core::{TextCaps, TextExtractor, TextOutcome};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// The extensions `extract_text` reads (lowercase, without the dot).
pub const TEXT_EXTENSIONS: &[&str] = &[
    // Plain text and code (`text.rs`), Markdown, CSV.
    "rs", "c", "h", "cpp", "hpp", "cs", "go", "java", "js", "jsx", "ts", "tsx", "py", "rb", "php",
    "swift", "kt", "kts", "sh", "ps1", "psm1", "bat", "cmd", "html", "htm", "css", "scss", "xml",
    "json", "toml", "yaml", "yml", "ini", "cfg", "log", "txt", "md", "csv", "tsv",
    // Documents (not .xls: calamine reads a whole legacy workbook into memory when it
    // opens it, with nothing to bound first).
    "pdf", "docx", "xlsx", "xlsb", "ods", "pptx", "odp", "odt",
];

/// A file's text and whether it was cut (the character cap, the time cap, or a reader's
/// own cap on pages or slides).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    pub text: String,
    pub truncated: bool,
}

/// Why there is no text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextError {
    /// No reader for this kind of file, not text after all (a binary `.txt`), or no pdfium.
    Kind,
    /// Over the size cap (the file's size).
    TooLarge(u64),
    /// A damaged file, or one the reader refuses (a password-protected PDF). The reason is
    /// one of a few fixed phrases: a reader's own message can quote the file (an entry
    /// name, a cell's error text), and the reason is logged.
    Failed(String),
}

/// Whether `extract_text` has a reader for `ext`.
pub fn extracts(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    TEXT_EXTENSIONS.contains(&ext.as_str())
}

/// Where extracted text goes: at most `max` characters, until the deadline.
pub(crate) struct Sink {
    text: String,
    chars: usize,
    max: usize,
    deadline: Instant,
    truncated: bool,
}

impl Sink {
    fn new(cap: &TextCaps) -> Sink {
        Sink {
            text: String::new(),
            chars: 0,
            max: cap.max_chars,
            deadline: Instant::now() + cap.time,
            truncated: false,
        }
    }

    /// Full or out of time (the text is then truncated).
    fn done(&mut self) -> bool {
        if !self.truncated && Instant::now() >= self.deadline {
            self.truncated = true;
        }
        self.truncated
    }

    /// Appends `s` and a line break; false once the sink takes no more.
    pub(crate) fn line(&mut self, s: &str) -> bool {
        let s = s.trim_end();
        if s.is_empty() {
            return !self.done();
        }
        if self.done() {
            return false;
        }
        let room = self.max - self.chars;
        let n = s.chars().count();
        if n < room {
            self.text.push_str(s);
            self.text.push('\n');
            self.chars += n + 1;
        } else {
            self.text.extend(s.chars().take(room));
            self.chars = self.max;
            self.truncated = true;
        }
        !self.done()
    }
}

/// The text of the file at `path` (`ext`: its extension) within `cap`.
pub fn extract_text(path: &Path, ext: &str, cap: &TextCaps) -> Result<Extracted, TextError> {
    let caught = keel_vfs::quiet_panic::quiet(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner(path, ext, cap)))
    });
    caught.unwrap_or_else(|_| Err(TextError::Failed("the reader crashed".into())))
}

fn inner(path: &Path, ext: &str, cap: &TextCaps) -> Result<Extracted, TextError> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    if !extracts(&ext) {
        return Err(TextError::Kind);
    }
    let failed = |e: std::io::Error| TextError::Failed(e.to_string());
    let meta = std::fs::metadata(path).map_err(failed)?;
    if !meta.is_file() {
        return Err(TextError::Kind);
    }
    let limit = if ext == "pdf" {
        cap.max_pdf_bytes
    } else {
        cap.max_bytes
    };
    if meta.len() > limit {
        return Err(TextError::TooLarge(meta.len()));
    }
    let mut out = Sink::new(cap);
    match ext.as_str() {
        "pdf" => pdf::text(path, &mut out)?,
        "docx" => blocks(
            docx::walk(&docx::open(path).map_err(damaged("Word document"))?, false),
            &mut out,
        ),
        "pptx" => blocks(
            pptx::build(path).map_err(damaged("PowerPoint presentation"))?,
            &mut out,
        ),
        "odt" | "ods" | "odp" => {
            let xml = Package::open(path)
                .and_then(|mut p| p.read("content.xml"))
                .map_err(damaged("OpenDocument file"))?
                .ok_or_else(|| TextError::Failed("content.xml is missing".into()))?;
            let parsed = odf::parse(&xml, &ext)
                .map_err(|_| TextError::Failed("not a valid OpenDocument file".into()))?;
            blocks(parsed, &mut out);
        }
        "xlsx" | "xlsb" => {
            table::workbook_text(path, &ext, &mut out).map_err(damaged("spreadsheet"))?
        }
        _ => {
            let bytes = std::fs::read(path).map_err(|e| TextError::Failed(e.to_string()))?;
            let source = text::decode(&bytes).ok_or(TextError::Kind)?;
            if ext == "md" {
                markdown(&source, &mut out);
            } else {
                for line in source.lines() {
                    if !out.line(line) {
                        break;
                    }
                }
            }
        }
    }
    Ok(Extracted {
        text: out.text,
        truncated: out.truncated,
    })
}

/// A reader's error as a fixed reason (`TextError::Failed`).
fn damaged(what: &'static str) -> impl Fn(String) -> TextError {
    move |e| {
        TextError::Failed(if e.ends_with(TOO_BIG) {
            TOO_BIG.into()
        } else if e.ends_with(TOO_DEEP) {
            TOO_DEEP.into()
        } else {
            format!("not a valid {what}")
        })
    }
}

/// A previewer's blocks as lines: headings and paragraphs, table rows with tab-separated
/// cells. Slide titles the previewer adds ("Slide 3") are not the document's text.
fn blocks(blocks: Vec<DocBlock>, out: &mut Sink) {
    for b in blocks {
        let more = match b {
            DocBlock::Heading(2, s) if s.strip_prefix("Slide ").is_some_and(numeric) => true,
            DocBlock::Para(s) if s == TRUNCATED => {
                out.truncated = true;
                false
            }
            DocBlock::Heading(_, s) | DocBlock::Para(s) => out.line(&s),
            DocBlock::Table(rows) => rows.iter().all(|r| out.line(&r.join("\t"))),
            DocBlock::Image(_) => true,
        };
        if !more {
            return;
        }
    }
}

fn numeric(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Markdown's text without its markup: text, code and link text, a line per block.
fn markdown(source: &str, out: &mut Sink) {
    use pulldown_cmark::{Event, Parser, TagEnd};
    let mut line = String::new();
    for event in Parser::new(source) {
        match event {
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                line.push_str(&t)
            }
            Event::SoftBreak | Event::HardBreak => line.push(' '),
            // The line is written (and taken) in the guard; full: stop.
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::CodeBlock
                | TagEnd::TableCell
                | TagEnd::BlockQuote(_),
            ) if !out.line(&std::mem::take(&mut line)) => return,
            _ => {}
        }
    }
    out.line(&line);
}

/// `extract_text` as the library's text reader (`Library::set_text_extractor`).
pub fn text_extractor() -> TextExtractor {
    TextExtractor {
        extensions: TEXT_EXTENSIONS,
        extract: Arc::new(|path, ext, cap| match extract_text(path, ext, cap) {
            Ok(e) => TextOutcome::Done {
                text: e.text,
                truncated: e.truncated,
            },
            Err(TextError::Kind) => TextOutcome::SkippedKind,
            Err(TextError::TooLarge(_)) => TextOutcome::SkippedSize,
            Err(TextError::Failed(why)) => TextOutcome::Failed(why),
        }),
    }
}
