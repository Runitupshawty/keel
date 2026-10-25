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
    if ext == "pdf" {
        if let Some(read) = pdf::text_in_child(path, cap) {
            return read;
        }
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
            } else if matches!(ext.as_str(), "html" | "htm" | "xml") {
                for line in without_markup(&source).lines() {
                    if !out.line(line) {
                        break;
                    }
                }
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

/// HTML and XML without their markup: the text between the tags, with comments and script
/// and style blocks left out and entities decoded (the XML ones, `&nbsp;`, numbers); a tag
/// is a space. An unclosed tag ends the text.
fn without_markup(source: &str) -> String {
    // ASCII lowercase keeps every byte offset.
    let lower = source.to_ascii_lowercase();
    let mut text = String::with_capacity(source.len() / 2);
    let mut at = 0;
    while let Some(open) = source[at..].find('<').map(|i| at + i) {
        text.push_str(&source[at..open]);
        let tail = &lower[open..];
        let close = if tail.starts_with("<!--") {
            tail.find("-->").map(|i| i + 3)
        } else if let Some(block) = ["<script", "<style"].iter().find(|b| tail.starts_with(**b)) {
            let end = format!("</{}", &block[1..]);
            (tail.find(&end)).and_then(|i| tail[i..].find('>').map(|j| i + j + 1))
        } else {
            tail.find('>').map(|i| i + 1)
        };
        let Some(close) = close else {
            return entities(&text);
        };
        text.push(' ');
        at = open + close;
    }
    text.push_str(&source[at..]);
    entities(&text)
}

/// `&amp;`, `&#233;`, `&#xE9;`, `&nbsp;` decoded; any other `&` stays.
fn entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let name = rest[1..]
            .find(';')
            .filter(|&n| n <= 10)
            .map(|n| &rest[1..n + 1]);
        let c = name.and_then(|n| match n.strip_prefix('#') {
            Some(num) => match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse().ok(),
            }
            .and_then(char::from_u32),
            None if n == "nbsp" => Some(' '),
            None => quick_xml::escape::resolve_predefined_entity(n).and_then(|e| e.chars().next()),
        });
        match (c, name) {
            (Some(c), Some(n)) => {
                out.push(c);
                rest = &rest[n.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
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

#[cfg(test)]
mod tests {
    use super::{entities, without_markup, Sink};
    use keel_core::TextCaps;

    #[test]
    fn entities_markup_and_sink_caps() {
        // Entities: predefined, decimal, hex, nbsp; unknown and surrogate kept;
        // multibyte after `&` (and around a trailing `&`) kept as-is.
        assert_eq!(entities("&amp;"), "&");
        assert_eq!(entities("a&#233;b"), "aéb");
        assert_eq!(entities("a&#xE9;b"), "aéb");
        assert_eq!(entities("&nbsp;"), " ");
        assert_eq!(entities("&foo;"), "&foo;");
        assert_eq!(entities("&#xD800;"), "&#xD800;");
        assert_eq!(entities("héllo & wörld"), "héllo & wörld");
        assert_eq!(entities("a&éb"), "a&éb");
        assert_eq!(entities("a&é;"), "a&é;");
        assert_eq!(entities("&#233;&"), "é&");

        // Markup: comment, script and style blocks dropped, a tag becomes a space,
        // an unclosed tag ends the text.
        assert_eq!(without_markup("a<!--c-->b"), "a b");
        assert_eq!(without_markup("x<script>y</script>z"), "x z");
        assert_eq!(without_markup("x<style>y</style>z"), "x z");
        assert_eq!(without_markup("hello<b"), "hello");
        assert_eq!(without_markup("a<b>c"), "a c");

        // Sink::line: a full line truncates (no line break appended) and stops the
        // sink; a partial line fills the room; empty lines add nothing.
        let cap = TextCaps {
            max_chars: 5,
            ..keel_core::TEXT_CAPS
        };
        let mut sink = Sink::new(&cap);
        assert!(sink.line("ab"));
        assert_eq!(
            (sink.text.as_str(), sink.chars, sink.truncated),
            ("ab\n", 3, false)
        );
        assert!(!sink.line("cdef"));
        assert_eq!(
            (sink.text.as_str(), sink.chars, sink.truncated),
            ("ab\ncd", 5, true)
        );
        assert!(!sink.line("x"));
        assert_eq!(sink.text, "ab\ncd");
        assert!(!sink.line(""));
    }
}
