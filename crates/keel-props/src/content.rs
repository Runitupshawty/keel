//! Content search over any file: Everything's `content:`, `utf8content:`, `utf16content:`,
//! `utf16becontent:`, `ansicontent:`, and the `binary:` and `hex:` forms, with the `case:`,
//! `whole-words:`, `regex:`, `each-line:`, `multi-line:` and `dot-all:` options.
//!
//! A [`Matcher`] is compiled once per query and asked about many files. Every search is
//! bounded: the bytes read and the time spent per file, and the bytes and time of the whole
//! query through a shared [`Budget`]; a [`Cancel`] stops it between 1 MiB chunks. Cloud
//! placeholders are never read (their content would be downloaded) unless the budget allows
//! recall. A file in a library source can be searched through the text the library already
//! extracted ([`Matcher::match_file`]'s `extracted`), so `content:` finds words in PDFs and
//! Office documents too.
//!
//! The file is read as a stream: the pattern is searched in windows that overlap by the
//! longest possible match (a plain pattern) or by 64 KiB (a regular expression), so a match
//! across two chunks is found, and `^`, `$` and word boundaries only match where the whole
//! text has them.
//!
//! ```
//! use keel_props::content::{Budget, Encoding, Matcher, Outcome, Spec};
//! let m = Matcher::new(Spec::new("needle", Encoding::Auto)).unwrap();
//! let out = m.match_reader(&b"hay needle hay"[..], &Budget::default()).unwrap();
//! assert!(matches!(out, Outcome::Match(_)));
//! ```

use crate::{is_placeholder, Cancel, Error};
use regex::bytes::{Regex, RegexBuilder};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bytes read from the file per step (cancel and time limits are checked between steps).
const CHUNK: usize = 1 << 20;
/// Overlap between windows for a regular expression whose match length has no bound.
pub const REGEX_WINDOW: usize = 64 * 1024;
/// Bytes kept before a window's first search position, so `^` and `\b` see what precedes it.
const CONTEXT: usize = 8;
/// A match ending this close to a window's end waits for the next window (`$`, `\b`).
const LOOKAHEAD: usize = 8;
/// Most bytes carried into the next window.
const MAX_CARRY: usize = 1 << 20;
/// `each-line:` searches longer lines in pieces of this many bytes.
pub const MAX_LINE: usize = 1 << 20;
/// Longest pattern accepted, in bytes.
pub const MAX_PATTERN: usize = 16 * 1024;
/// Compiled regular expression size limit.
const REGEX_SIZE_LIMIT: usize = 4 << 20;

/// How a file's bytes are read as text, which is what the content function names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// `content:`: by byte order mark (UTF-8, UTF-16, UTF-32), else the UTF-16 check on the
    /// first 64 KiB, else UTF-8, or Windows-1252 when the start is not valid UTF-8.
    #[default]
    Auto,
    /// `utf8content:` (invalid bytes never match).
    Utf8,
    /// `utf16content:`, little-endian.
    Utf16Le,
    /// `utf16becontent:`, big-endian.
    Utf16Be,
    /// `ansicontent:`: Windows-1252.
    Ansi,
    /// `binary:`: the raw bytes, against the pattern's UTF-8 bytes (ASCII letters ignore case
    /// unless `case:`).
    Binary,
    /// `hex:`: the raw bytes, against a pattern of hex byte pairs (`50 4b 03 04`).
    Hex,
}

impl Encoding {
    /// The encoding a content function name selects (`content`, `utf8content:`, ...),
    /// case-insensitive, with or without the colon. `binary` and `hex` are accepted too.
    pub fn from_function(name: &str) -> Option<Encoding> {
        let n = name.trim().trim_end_matches(':').to_ascii_lowercase();
        Some(match n.as_str() {
            "content" => Encoding::Auto,
            "utf8content" => Encoding::Utf8,
            "utf16content" => Encoding::Utf16Le,
            "utf16becontent" => Encoding::Utf16Be,
            "ansicontent" => Encoding::Ansi,
            "binary" => Encoding::Binary,
            "hex" => Encoding::Hex,
            _ => return None,
        })
    }

    /// Everything's name for it (`utf16becontent`).
    pub fn function_name(self) -> &'static str {
        match self {
            Encoding::Auto => "content",
            Encoding::Utf8 => "utf8content",
            Encoding::Utf16Le => "utf16content",
            Encoding::Utf16Be => "utf16becontent",
            Encoding::Ansi => "ansicontent",
            Encoding::Binary => "binary",
            Encoding::Hex => "hex",
        }
    }

    fn is_raw(self) -> bool {
        matches!(self, Encoding::Binary | Encoding::Hex)
    }
}

/// How line breaks affect a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lines {
    /// The file is one text: a pattern may span lines, `^` and `$` are its start and end.
    #[default]
    Whole,
    /// `multi-line:`: `^` and `$` also match at line breaks.
    MultiLine,
    /// `each-line:`: every line is matched on its own (`^` and `$` are its start and end; a
    /// match never spans lines). `\r` before `\n` is not part of the line.
    EachLine,
}

/// What to search for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub pattern: String,
    pub encoding: Encoding,
    /// `case:`: match case (default: ignore case).
    pub case: bool,
    /// `whole-words:` (`ww:`): the match starts and ends at word boundaries.
    pub whole_words: bool,
    /// `regex:`: the pattern is a regular expression (Rust `regex` syntax).
    pub regex: bool,
    pub lines: Lines,
    /// `dot-all:`: `.` matches a line break too.
    pub dot_all: bool,
}

impl Spec {
    /// A plain, case-insensitive pattern over the whole text.
    pub fn new(pattern: impl Into<String>, encoding: Encoding) -> Spec {
        Spec {
            pattern: pattern.into(),
            encoding,
            case: false,
            whole_words: false,
            regex: false,
            lines: Lines::Whole,
            dot_all: false,
        }
    }

    /// Reads `[modifier:]...pattern` as the `content-match:` property names it: the
    /// modifiers `case:`, `no-case:`, `whole-words:` (`ww:`), `no-whole-words:`, `regex:`,
    /// `each-line:`, `multi-line:`, `dot-all:`, `binary:` and `hex:` and the content function
    /// names (`utf16content:` ...) in any order, then the pattern. A pattern in double quotes
    /// is taken as is (`"case:x"` searches for `case:x`).
    pub fn parse(text: &str, encoding: Encoding) -> Spec {
        let mut spec = Spec::new(String::new(), encoding);
        let mut rest = text;
        while let Some((word, after)) = rest.split_once(':') {
            match word.to_ascii_lowercase().as_str() {
                "case" => spec.case = true,
                "no-case" => spec.case = false,
                "whole-words" | "ww" | "wholeword" | "wholewords" => spec.whole_words = true,
                "no-whole-words" | "no-ww" => spec.whole_words = false,
                "regex" => spec.regex = true,
                "each-line" => spec.lines = Lines::EachLine,
                "multi-line" => spec.lines = Lines::MultiLine,
                "dot-all" => spec.dot_all = true,
                w => match Encoding::from_function(w) {
                    Some(e) => spec.encoding = e,
                    None => break,
                },
            }
            rest = after;
        }
        let pattern = match rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            Some(inner) => inner,
            None => rest,
        };
        spec.pattern = pattern.to_owned();
        spec
    }
}

/// Why a pattern cannot be searched for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    Empty,
    TooLong,
    /// `hex:` needs whole hex byte pairs.
    BadHex(String),
    /// The regular expression does not compile (syntax, or over the size limit).
    Regex(String),
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternError::Empty => f.write_str("empty content pattern"),
            PatternError::TooLong => write!(f, "content pattern longer than {MAX_PATTERN} bytes"),
            PatternError::BadHex(s) => write!(f, "not hex byte pairs: {s}"),
            PatternError::Regex(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PatternError {}

/// Limits of a content search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes read per file (the rest is not searched; no match in what was read is unknown).
    /// Default 64 MiB.
    pub max_file_bytes: u64,
    /// Time per file. Default 10 s.
    pub max_file_time: Duration,
    /// Bytes read by the whole query (every file a [`Budget`] is used for). Default 4 GiB.
    pub max_query_bytes: u64,
    /// Time of the whole query, from [`Budget::new`]. Default 60 s.
    pub max_query_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 64 << 20,
            max_file_time: Duration::from_secs(10),
            max_query_bytes: 4 << 30,
            max_query_time: Duration::from_secs(60),
        }
    }
}

/// One query's limits, cancel flag and running totals, shared by every file it searches
/// (and every thread: clones share the totals).
#[derive(Debug, Clone)]
pub struct Budget(Arc<BudgetInner>);

#[derive(Debug)]
struct BudgetInner {
    limits: Limits,
    cancel: Cancel,
    allow_recall: bool,
    started: Instant,
    bytes: AtomicU64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(Limits::default(), Cancel::default(), false)
    }
}

impl Budget {
    /// A budget starting now. `allow_recall` reads cloud placeholders (downloading them).
    pub fn new(limits: Limits, cancel: Cancel, allow_recall: bool) -> Budget {
        Budget(Arc::new(BudgetInner {
            limits,
            cancel,
            allow_recall,
            started: Instant::now(),
            bytes: AtomicU64::new(0),
        }))
    }

    pub fn limits(&self) -> &Limits {
        &self.0.limits
    }

    pub fn cancel(&self) -> &Cancel {
        &self.0.cancel
    }

    /// Bytes read so far by the query.
    pub fn bytes_read(&self) -> u64 {
        self.0.bytes.load(Ordering::Relaxed)
    }

    fn add(&self, n: usize) {
        self.0.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// `Err` when cancelled, `Some` when the query is out of time or bytes.
    fn check(&self) -> Result<Option<Why>, Error> {
        let b = &self.0;
        if b.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if b.started.elapsed() >= b.limits.max_query_time {
            return Ok(Some(Why::QueryTimeLimit));
        }
        if self.bytes_read() >= b.limits.max_query_bytes {
            return Ok(Some(Why::QueryByteLimit));
        }
        Ok(None)
    }
}

/// Where a match was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    /// Byte offset of the match in the searched text: the text decoded to UTF-8, or the raw
    /// bytes for `binary:` and `hex:`.
    pub offset: u64,
    /// 1-based line of the match's start.
    pub line: u64,
}

/// Why a file's answer is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// A folder or another non-regular file.
    NotAFile,
    /// A cloud placeholder (reading it would download it).
    Placeholder,
    /// No match in the part read; the file is longer than [`Limits::max_file_bytes`].
    FileTooLarge,
    FileTimeLimit,
    QueryTimeLimit,
    QueryByteLimit,
}

/// A file's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Match(Found),
    NoMatch,
    /// Not searched, or not completely (Everything's `unknown`).
    Unknown(Why),
}

impl Outcome {
    pub fn is_match(&self) -> bool {
        matches!(self, Outcome::Match(_))
    }

    /// As a property value: yes, no or unknown.
    pub fn to_prop(self) -> crate::Prop {
        match self {
            Outcome::Match(_) => crate::Prop::Bool(true),
            Outcome::NoMatch => crate::Prop::Bool(false),
            Outcome::Unknown(_) => crate::Prop::Unknown,
        }
    }
}

/// A compiled content search.
#[derive(Debug, Clone)]
pub struct Matcher {
    spec: Spec,
    re: Regex,
    /// Bytes the next window keeps from the previous one when nothing matched.
    overlap: usize,
    /// Whether a match depends on what follows it (`$`, ``, a regular expression), so a
    /// match at a window's end must wait for the next window.
    lookahead: bool,
}

impl Matcher {
    pub fn new(spec: Spec) -> Result<Matcher, PatternError> {
        if spec.pattern.is_empty() {
            return Err(PatternError::Empty);
        }
        if spec.pattern.len() > MAX_PATTERN {
            return Err(PatternError::TooLong);
        }
        let raw = spec.encoding.is_raw();
        let (body, max_len) = if spec.encoding == Encoding::Hex {
            let bytes = parse_hex(&spec.pattern)?;
            (escape_bytes(&bytes, false), Some(bytes.len()))
        } else if spec.regex {
            (spec.pattern.clone(), None)
        } else if raw {
            let b = spec.pattern.as_bytes();
            (escape_bytes(b, true), Some(b.len()))
        } else {
            // One pattern character matches one text character: at most 4 UTF-8 bytes.
            (
                regex::escape(&spec.pattern),
                Some(spec.pattern.chars().count() * 4),
            )
        };
        let body = if spec.whole_words {
            let boundary = if raw { r"(?-u:\b)" } else { r"\b" };
            // A plain pattern gets a boundary only on a side that is a word character, so
            // `ww:` with `-flag` still matches; a regular expression gets both.
            let (first, last) = if spec.regex || spec.encoding == Encoding::Hex {
                (true, true)
            } else {
                let p = &spec.pattern;
                (
                    p.chars().next().is_some_and(is_word_char),
                    p.chars().last().is_some_and(is_word_char),
                )
            };
            format!(
                "{}(?:{body}){}",
                if first { boundary } else { "" },
                if last { boundary } else { "" }
            )
        } else {
            body
        };
        let re = RegexBuilder::new(&body)
            .case_insensitive(!spec.case && spec.encoding != Encoding::Hex)
            .multi_line(spec.lines == Lines::MultiLine)
            .dot_matches_new_line(spec.dot_all)
            .unicode(!raw)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| PatternError::Regex(e.to_string()))?;
        let overlap = max_len.map_or(REGEX_WINDOW, |n| n.min(REGEX_WINDOW)) + LOOKAHEAD;
        let lookahead = spec.regex || spec.whole_words;
        Ok(Matcher {
            spec,
            re,
            overlap,
            lookahead,
        })
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Searches the file at `path`. With `extracted` (the text a library source extracted
    /// from it) and `content:` (automatic encoding), that text is searched instead of the
    /// file's bytes, also for a cloud placeholder.
    pub fn match_file(
        &self,
        path: &Path,
        extracted: Option<&str>,
        budget: &Budget,
    ) -> Result<Outcome, Error> {
        let meta = fs::metadata(path)?;
        self.match_file_as(path, meta.is_file(), is_placeholder(&meta), extracted, budget)
    }

    fn match_file_as(
        &self,
        path: &Path,
        is_file: bool,
        placeholder: bool,
        extracted: Option<&str>,
        budget: &Budget,
    ) -> Result<Outcome, Error> {
        if let Some(text) = extracted.filter(|_| self.spec.encoding == Encoding::Auto) {
            return self.match_text(text, budget);
        }
        if !is_file {
            return Ok(Outcome::Unknown(Why::NotAFile));
        }
        if placeholder && !budget.0.allow_recall {
            return Ok(Outcome::Unknown(Why::Placeholder));
        }
        if let Some(why) = budget.check()? {
            return Ok(Outcome::Unknown(why));
        }
        self.match_reader(File::open(path)?, budget)
    }

    /// Searches text already in memory (a library's extracted text), as UTF-8, within the
    /// same limits as a file.
    pub fn match_text(&self, text: &str, budget: &Budget) -> Result<Outcome, Error> {
        self.scan(text.as_bytes(), Some(Encoding::Utf8), budget)
    }

    /// Searches the bytes `r` gives, read as [`Spec::encoding`] says.
    pub fn match_reader<R: Read>(&self, r: R, budget: &Budget) -> Result<Outcome, Error> {
        self.scan(r, None, budget)
    }

    fn scan<R: Read>(
        &self,
        mut r: R,
        text_override: Option<Encoding>,
        budget: &Budget,
    ) -> Result<Outcome, Error> {
        let started = Instant::now();
        let limits = budget.0.limits;
        let mut buf = vec![0u8; CHUNK];
        let mut decoder: Option<Decoder> = None;
        let mut sink = Sink::new(self);
        let mut out = Vec::new();
        let mut read_total: u64 = 0;
        loop {
            if let Some(why) = budget.check()? {
                return Ok(Outcome::Unknown(why));
            }
            if started.elapsed() >= limits.max_file_time {
                return Ok(Outcome::Unknown(Why::FileTimeLimit));
            }
            let left = limits.max_file_bytes.saturating_sub(read_total);
            let want = usize::try_from(left).unwrap_or(CHUNK).min(CHUNK);
            let n = if want == 0 {
                0
            } else {
                read_full(&mut r, &mut buf[..want])?
            };
            budget.add(n);
            read_total += n as u64;
            let eof = n < want || (want == 0 && probe_eof(&mut r)?);
            let truncated = !eof && want == 0;
            let data = &buf[..n];
            let dec = decoder.get_or_insert_with(|| {
                Decoder::new(text_override.unwrap_or(self.spec.encoding), data)
            });
            out.clear();
            dec.decode(data, &mut out, eof);
            if let Some(found) = sink.push(&out, eof) {
                return Ok(Outcome::Match(found));
            }
            if eof {
                return Ok(Outcome::NoMatch);
            }
            if truncated {
                return Ok(Outcome::Unknown(Why::FileTooLarge));
            }
        }
    }
}

/// Word characters for whole-word matching (as the regex crate's `\b` sees them).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `(?-u)` escapes of raw bytes; `letters` keeps ASCII letters as letters (so they can
/// ignore case).
fn escape_bytes(b: &[u8], letters: bool) -> String {
    let mut s = String::with_capacity(b.len() * 4);
    for &c in b {
        if letters && c.is_ascii_alphanumeric() {
            s.push(char::from(c));
        } else {
            s.push_str(&format!(r"\x{c:02x}"));
        }
    }
    s
}

/// Hex byte pairs, with spaces, commas, dashes or colons between them and an optional `0x`
/// before each group (`504b0304`, `50 4b 03 04`, `0x50,0x4b`).
pub fn parse_hex(s: &str) -> Result<Vec<u8>, PatternError> {
    let bad = || PatternError::BadHex(s.chars().take(64).collect());
    let mut digits = Vec::with_capacity(s.len());
    for group in s.split(|c: char| c.is_whitespace() || matches!(c, ',' | '-' | ':')) {
        let g = group
            .strip_prefix("0x")
            .or_else(|| group.strip_prefix("0X"))
            .unwrap_or(group);
        if !g.len().is_multiple_of(2) {
            return Err(bad());
        }
        for c in g.chars() {
            digits.push(c.to_digit(16).ok_or_else(bad)? as u8);
        }
    }
    if digits.is_empty() {
        return Err(PatternError::Empty);
    }
    Ok(digits.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

/// Reads until `buf` is full or the end; returns the bytes read.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// True when `r` has nothing more (reads at most one byte).
fn probe_eof<R: Read>(r: &mut R) -> io::Result<bool> {
    let mut one = [0u8; 1];
    Ok(read_full(r, &mut one)? == 0)
}

/// Bytes to UTF-8 text (or the raw bytes for `binary:` and `hex:`), as a stream.
enum Decoder {
    Raw,
    Text(encoding_rs::Decoder),
    Utf32 { le: bool, carry: Vec<u8>, first: bool },
}

impl Decoder {
    /// Picks the decoder; `head` is the first chunk (the automatic encoding looks at it).
    fn new(enc: Encoding, head: &[u8]) -> Decoder {
        use crate::text::Encoding as T;
        use encoding_rs::{UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
        let e = match enc {
            Encoding::Binary | Encoding::Hex => return Decoder::Raw,
            Encoding::Utf8 => UTF_8,
            Encoding::Utf16Le => UTF_16LE,
            Encoding::Utf16Be => UTF_16BE,
            Encoding::Ansi => WINDOWS_1252,
            Encoding::Auto => match crate::text::sniff(&head[..head.len().min(crate::text::SNIFF_LEN)]) {
                T::Utf16Le => UTF_16LE,
                T::Utf16Be => UTF_16BE,
                T::Utf32Le | T::Utf32Be => {
                    return Decoder::Utf32 {
                        le: head.starts_with(b"\xff\xfe"),
                        carry: Vec::new(),
                        first: true,
                    }
                }
                T::Ansi => WINDOWS_1252,
                // ASCII, UTF-8 and binary files: binary content is searched as UTF-8 so the
                // text inside programs and documents is found.
                _ => UTF_8,
            },
        };
        Decoder::Text(e.new_decoder_with_bom_removal())
    }

    fn decode(&mut self, src: &[u8], out: &mut Vec<u8>, last: bool) {
        match self {
            Decoder::Raw => out.extend_from_slice(src),
            Decoder::Text(d) => {
                let need = d
                    .max_utf8_buffer_length(src.len())
                    .unwrap_or(src.len().saturating_mul(3).saturating_add(16));
                let start = out.len();
                out.resize(start + need, 0);
                let (_, _read, written, _) = d.decode_to_utf8(src, &mut out[start..], last);
                out.truncate(start + written);
            }
            Decoder::Utf32 { le, carry, first } => {
                carry.extend_from_slice(src);
                let whole = carry.len() / 4 * 4;
                let mut tmp = [0u8; 4];
                for u in carry[..whole].as_chunks::<4>().0 {
                    let v = if *le {
                        u32::from_le_bytes(*u)
                    } else {
                        u32::from_be_bytes(*u)
                    };
                    let c = char::from_u32(v).unwrap_or('\u{fffd}');
                    if std::mem::take(first) && c == '\u{feff}' {
                        continue;
                    }
                    out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
                carry.drain(..whole);
                if last && !carry.is_empty() {
                    carry.clear();
                    out.extend_from_slice("\u{fffd}".as_bytes());
                }
            }
        }
    }
}

/// Where decoded text goes: the whole-text window search or the line-by-line search.
struct Sink<'m> {
    m: &'m Matcher,
    /// Text not yet searched completely (whole text: the window; each line: the unfinished
    /// line).
    hay: Vec<u8>,
    /// Whole text: where the next search starts in `hay` (bytes before it are context).
    from: usize,
    /// Offset of `hay[0]` in the searched text.
    base: u64,
    /// Line breaks before `hay[0]`.
    lines_before: u64,
    text: bool,
}

impl<'m> Sink<'m> {
    fn new(m: &'m Matcher) -> Sink<'m> {
        Sink {
            m,
            hay: Vec::new(),
            from: 0,
            base: 0,
            lines_before: 0,
            text: !m.spec.encoding.is_raw(),
        }
    }

    fn push(&mut self, data: &[u8], last: bool) -> Option<Found> {
        self.hay.extend_from_slice(data);
        if self.m.spec.lines == Lines::EachLine {
            self.push_lines(last)
        } else {
            self.push_whole(last)
        }
    }

    fn found(&self, at: usize) -> Found {
        Found {
            offset: self.base + at as u64,
            line: self.lines_before + count_newlines(&self.hay[..at]) + 1,
        }
    }

    fn push_whole(&mut self, last: bool) -> Option<Found> {
        let len = self.hay.len();
        let keep = match self.m.re.find_at(&self.hay, self.from) {
            Some(mm) if last || !self.m.lookahead || mm.end() + LOOKAHEAD <= len => {
                return Some(self.found(mm.start()))
            }
            // Ends near the window's end: `$` or `\b` may only hold because the window ends
            // there. Search again from its start once more text is in.
            Some(mm) => mm.start(),
            None if last => return None,
            None => len.saturating_sub(self.m.overlap),
        };
        let keep = keep.max(self.from).max(len.saturating_sub(MAX_CARRY));
        let keep = self.char_start(keep);
        let ctx = self.char_start(keep.saturating_sub(CONTEXT));
        self.lines_before += count_newlines(&self.hay[..ctx]);
        self.base += ctx as u64;
        self.hay.drain(..ctx);
        self.from = keep - ctx;
        None
    }

    fn push_lines(&mut self, last: bool) -> Option<Found> {
        let mut start = 0;
        while let Some(nl) = self.hay[start..].iter().position(|&b| b == b'\n') {
            let end = start + nl;
            if let Some(f) = self.line(start, end) {
                return Some(f);
            }
            self.lines_before += 1;
            start = end + 1;
        }
        // A line longer than MAX_LINE is searched in pieces.
        while self.hay.len() - start > MAX_LINE {
            let end = self.char_start(start + MAX_LINE);
            let end = if end <= start { start + MAX_LINE } else { end };
            if let Some(f) = self.line(start, end) {
                return Some(f);
            }
            start = end;
        }
        if last && start < self.hay.len() {
            if let Some(f) = self.line(start, self.hay.len()) {
                return Some(f);
            }
            start = self.hay.len();
        }
        self.base += start as u64;
        self.hay.drain(..start);
        None
    }

    /// Searches `hay[start..end]` as one line (without a trailing `\r`).
    fn line(&self, start: usize, end: usize) -> Option<Found> {
        let mut line = &self.hay[start..end];
        if let Some(l) = line.strip_suffix(b"\r") {
            line = l;
        }
        self.m.re.find(line).map(|mm| Found {
            offset: self.base + (start + mm.start()) as u64,
            line: self.lines_before + 1,
        })
    }

    /// `at`, moved back to the start of a UTF-8 character in text mode.
    fn char_start(&self, mut at: usize) -> usize {
        if self.text {
            while at > 0 && at < self.hay.len() && (self.hay[at] & 0xc0) == 0x80 {
                at -= 1;
            }
        }
        at
    }
}

fn count_newlines(b: &[u8]) -> u64 {
    b.iter().filter(|&&c| c == b'\n').count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{encode, FxEncoding};

    fn spec(p: &str, e: Encoding) -> Spec {
        Spec::new(p, e)
    }

    fn run_with(s: Spec, data: &[u8], budget: &Budget) -> Outcome {
        Matcher::new(s).unwrap().match_reader(data, budget).unwrap()
    }

    fn run(s: Spec, data: &[u8]) -> Outcome {
        run_with(s, data, &Budget::default())
    }

    fn hit(s: Spec, data: &[u8]) -> bool {
        run(s, data).is_match()
    }

    const TEXT: &str = "Caf\u{e9} au lait\nna\u{ef}ve r\u{e9}sum\u{e9}\n";

    #[test]
    fn auto_reads_every_encoding() {
        for enc in FxEncoding::ALL {
            let data = encode(TEXT, enc);
            for p in ["caf\u{e9}", "R\u{c9}SUM\u{c9}", "au lait\nna\u{ef}ve"] {
                assert!(hit(spec(p, Encoding::Auto), &data), "{enc:?} {p}");
            }
            assert!(!hit(spec("cafe", Encoding::Auto), &data), "{enc:?}");
        }
    }

    #[test]
    fn explicit_encodings_read_only_their_own() {
        use FxEncoding as F;
        let cases = [
            (Encoding::Utf8, [F::Utf8, F::Utf8Bom].as_slice()),
            (Encoding::Utf16Le, &[F::Utf16LeBom, F::Utf16Le]),
            (Encoding::Utf16Be, &[F::Utf16BeBom]),
            (Encoding::Ansi, &[F::Ansi]),
        ];
        for (enc, own) in cases {
            for fx in FxEncoding::ALL {
                let data = encode(TEXT, fx);
                assert_eq!(
                    hit(spec("na\u{ef}ve", enc), &data),
                    own.contains(&fx),
                    "{enc:?} over {fx:?}"
                );
            }
        }
        // ASCII text reads the same in UTF-8 and Windows-1252.
        assert!(hit(spec("lait", Encoding::Ansi), &encode(TEXT, F::Utf8)));
        // Windows-1252 has the euro sign and curly quotes at 0x80 and 0x91..0x94.
        let ansi = b"price \x80 5 \x93quoted\x94";
        assert!(hit(spec("\u{20ac} 5 \u{201c}quoted", Encoding::Ansi), ansi));
        assert!(hit(spec("\u{20ac} 5", Encoding::Auto), ansi));
        // UTF-32 by byte order mark.
        let mut u32le = b"\xff\xfe\0\0".to_vec();
        for c in "z\u{e9}ro".chars() {
            u32le.extend_from_slice(&(c as u32).to_le_bytes());
        }
        assert!(hit(spec("Z\u{c9}RO", Encoding::Auto), &u32le));
    }

    #[test]
    fn binary_and_hex() {
        let data = b"\x00\x01PK\x03\x04\xffZip\x00";
        assert!(hit(spec("pk", Encoding::Binary), data));
        let mut s = spec("pk", Encoding::Binary);
        s.case = true;
        assert!(!hit(s, data));
        assert!(hit(spec("50 4b 03 04", Encoding::Hex), data));
        assert!(hit(spec("0x03,0x04,0xFF", Encoding::Hex), data));
        assert!(!hit(spec("504c", Encoding::Hex), data));
        // Hex ignores case options: 0x5a is `Z`, not `z`.
        assert!(!hit(spec("7a6970", Encoding::Hex), data));
        // Non-ASCII pattern characters are their UTF-8 bytes.
        assert!(hit(spec("\u{e9}", Encoding::Binary), "caf\u{e9}".as_bytes()));
        assert!(!hit(spec("\u{e9}", Encoding::Binary), b"caf\xe9"));
        assert_eq!(parse_hex("abc"), Err(PatternError::BadHex("abc".into())));
        assert_eq!(parse_hex("zz"), Err(PatternError::BadHex("zz".into())));
        assert_eq!(parse_hex("  "), Err(PatternError::Empty));
        assert_eq!(parse_hex("0xA0 b1-C2:d3").unwrap(), vec![0xa0, 0xb1, 0xc2, 0xd3]);
        // Binary regular expressions match any byte with `.`.
        let mut re = spec(r"PK..\xff", Encoding::Binary);
        re.regex = true;
        assert!(hit(re, data));
    }

    #[test]
    fn case_words_and_regex() {
        let data = b"The quick brown fox, fox-hole; foxes.";
        let mut s = spec("the QUICK", Encoding::Auto);
        assert!(hit(s.clone(), data));
        s.case = true;
        assert!(!hit(s, data));
        let mut ww = spec("fox", Encoding::Auto);
        ww.whole_words = true;
        assert_eq!(run(ww.clone(), data), Outcome::Match(Found { offset: 16, line: 1 }));
        ww.pattern = "foxe".into();
        assert!(!hit(ww.clone(), data));
        ww.pattern = "-hole;".into();
        assert!(hit(ww.clone(), data));
        let mut re = spec(r"f[aeiou]x(es)?\.$", Encoding::Auto);
        re.regex = true;
        assert!(hit(re.clone(), data));
        re.pattern = r"^quick".into();
        assert!(!hit(re, data));
        assert!(matches!(
            Matcher::new(Spec {
                regex: true,
                ..spec("(unclosed", Encoding::Auto)
            }),
            Err(PatternError::Regex(_))
        ));
        assert!(matches!(
            Matcher::new(spec("", Encoding::Auto)),
            Err(PatternError::Empty)
        ));
        assert!(matches!(
            Matcher::new(spec(&"x".repeat(MAX_PATTERN + 1), Encoding::Auto)),
            Err(PatternError::TooLong)
        ));
        // A regular expression too large to compile is refused, not built.
        assert!(matches!(
            Matcher::new(Spec {
                regex: true,
                ..spec(r"\w{50000}", Encoding::Auto)
            }),
            Err(PatternError::Regex(_))
        ));
    }

    #[test]
    fn line_modes() {
        let data = b"alpha\r\nbeta gamma\ndelta\n";
        let re = |p: &str, lines: Lines| Spec {
            regex: true,
            lines,
            ..spec(p, Encoding::Auto)
        };
        // Whole text: ^ and $ are the file's start and end.
        assert!(!hit(re("^beta", Lines::Whole), data));
        assert!(hit(re("^alpha", Lines::Whole), data));
        assert!(hit(re("alpha\\r\\nbeta", Lines::Whole), data));
        // multi-line: ^ and $ at line breaks.
        assert_eq!(
            run(re("^beta", Lines::MultiLine), data),
            Outcome::Match(Found { offset: 7, line: 2 })
        );
        assert!(hit(re("gamma$", Lines::MultiLine), data));
        // each-line: every line alone, without its \r.
        assert_eq!(
            run(re("^delta$", Lines::EachLine), data),
            Outcome::Match(Found { offset: 18, line: 3 })
        );
        assert!(hit(re("^alpha$", Lines::EachLine), data));
        assert!(!hit(re("alpha.*beta", Lines::EachLine), data));
        let mut dot = re("alpha.*beta", Lines::Whole);
        assert!(!hit(dot.clone(), data));
        dot.dot_all = true;
        assert!(hit(dot, data));
        // A plain pattern never spans lines in each-line mode.
        let mut plain = spec("alpha\r\nbeta", Encoding::Auto);
        assert!(hit(plain.clone(), data));
        plain.lines = Lines::EachLine;
        assert!(!hit(plain, data));
        // A last line without a newline is searched.
        assert!(hit(re("^delta$", Lines::EachLine), b"a\ndelta"));
    }

    /// Text that puts `needle` across the 1 MiB read boundary at `at`.
    fn across(at: usize, needle: &[u8], filler: u8) -> Vec<u8> {
        let mut v = vec![filler; at];
        v.extend_from_slice(needle);
        v.extend(std::iter::repeat_n(filler, 100));
        v
    }

    #[test]
    fn matches_across_chunks() {
        let data = across(CHUNK - 3, b" needle ", b'x');
        assert_eq!(
            run(spec("needle", Encoding::Auto), &data),
            Outcome::Match(Found { offset: (CHUNK - 2) as u64, line: 1 })
        );
        let mut ww = spec("needle", Encoding::Auto);
        ww.whole_words = true;
        assert!(hit(ww.clone(), &data));
        // A word cut by the chunk edge is not a whole word: "needle" then "x".
        let data = across(CHUNK - 6, b"needle", b'x');
        assert!(hit(spec("needle", Encoding::Auto), &data));
        assert!(!hit(ww.clone(), &data));
        // ...nor when the chunk ends right after it.
        let mut v = vec![b' '; CHUNK - 6];
        v.extend_from_slice(b"needlexx");
        assert!(!hit(ww, &v));
        // `$` and `^` do not match at chunk edges.
        let mut dollar = spec("needle$", Encoding::Auto);
        dollar.regex = true;
        let mut v = vec![b' '; CHUNK - 6];
        v.extend_from_slice(b"needle and more");
        assert!(!hit(dollar.clone(), &v));
        v.truncate(CHUNK);
        assert!(hit(dollar, &v));
        let mut caret = spec("^more", Encoding::Auto);
        caret.regex = true;
        let mut v = vec![b' '; CHUNK];
        v.extend_from_slice(b"more");
        assert!(!hit(caret, &v));
        // A multi-byte character across the edge, in UTF-8 and UTF-16.
        let data = across(CHUNK - 1, "\u{e9}t\u{e9}".as_bytes(), b'x');
        assert!(hit(spec("x\u{c9}T\u{c9}x", Encoding::Auto), &data));
        let utf16 = encode(&format!("{}\u{e9}t\u{e9}{}", "y".repeat(CHUNK / 2 - 1), "y"), FxEncoding::Utf16LeBom);
        assert!(hit(spec("y\u{e9}t\u{e9}y", Encoding::Utf16Le), &utf16));
        assert!(hit(spec("y\u{e9}t\u{e9}y", Encoding::Auto), &utf16));
        // Line numbers count lines in earlier chunks.
        let mut v = b"a\n".repeat(CHUNK);
        v.extend_from_slice(b"target\n");
        assert_eq!(
            run(spec("target", Encoding::Auto), &v),
            Outcome::Match(Found { offset: 2 * CHUNK as u64, line: CHUNK as u64 + 1 })
        );
        let mut each = spec("^target$", Encoding::Auto);
        each.regex = true;
        each.lines = Lines::EachLine;
        assert_eq!(
            run(each, &v),
            Outcome::Match(Found { offset: 2 * CHUNK as u64, line: CHUNK as u64 + 1 })
        );
        // A long regular expression match across the edge (within the 64 KiB window).
        let mut re = spec("begin x+ end", Encoding::Auto);
        re.regex = true;
        let mut v = vec![b'.'; CHUNK - 1000];
        v.extend_from_slice(b"begin ");
        v.extend(std::iter::repeat_n(b'x', 5000));
        v.extend_from_slice(b" end");
        assert!(hit(re, &v));
    }

    #[test]
    fn long_lines_are_searched_in_pieces() {
        let mut v = vec![b'q'; MAX_LINE * 2 + 10];
        v.extend_from_slice(b"tail");
        let mut each = spec("qtail", Encoding::Auto);
        each.lines = Lines::EachLine;
        assert!(hit(each, &v));
    }

    #[test]
    fn file_byte_cap() {
        let limits = Limits {
            max_file_bytes: 100,
            ..Limits::default()
        };
        let budget = || Budget::new(limits, Cancel::default(), false);
        let mut v = vec![b'.'; 50];
        v.extend_from_slice(b"early");
        v.extend(std::iter::repeat_n(b'.', 200));
        v.extend_from_slice(b"late");
        assert!(run_with(spec("early", Encoding::Auto), &v, &budget()).is_match());
        assert_eq!(
            run_with(spec("late", Encoding::Auto), &v, &budget()),
            Outcome::Unknown(Why::FileTooLarge)
        );
        // Exactly at the cap is complete.
        assert_eq!(
            run_with(spec("late", Encoding::Auto), &v[..100], &budget()),
            Outcome::NoMatch
        );
        // Several chunks under a larger cap.
        let limits = Limits {
            max_file_bytes: (CHUNK + 10) as u64,
            ..Limits::default()
        };
        let b = Budget::new(limits, Cancel::default(), false);
        let big = across(CHUNK * 2, b"far", b'z');
        assert_eq!(
            run_with(spec("far", Encoding::Auto), &big, &b),
            Outcome::Unknown(Why::FileTooLarge)
        );
        assert_eq!(b.bytes_read(), (CHUNK + 10) as u64);
    }

    #[test]
    fn time_and_query_limits_and_cancel() {
        let data = b"some text here";
        let zero_file = Limits {
            max_file_time: Duration::ZERO,
            ..Limits::default()
        };
        assert_eq!(
            run_with(
                spec("text", Encoding::Auto),
                data,
                &Budget::new(zero_file, Cancel::default(), false)
            ),
            Outcome::Unknown(Why::FileTimeLimit)
        );
        let zero_query = Limits {
            max_query_time: Duration::ZERO,
            ..Limits::default()
        };
        assert_eq!(
            run_with(
                spec("text", Encoding::Auto),
                data,
                &Budget::new(zero_query, Cancel::default(), false)
            ),
            Outcome::Unknown(Why::QueryTimeLimit)
        );
        // The query's byte total is shared by every file.
        let small = Limits {
            max_query_bytes: 20,
            ..Limits::default()
        };
        let b = Budget::new(small, Cancel::default(), false);
        let m = Matcher::new(spec("here", Encoding::Auto)).unwrap();
        assert!(m.match_reader(&data[..], &b).unwrap().is_match());
        let clone = b.clone();
        assert!(m.match_reader(&data[..], &clone).unwrap().is_match());
        assert_eq!(b.bytes_read(), 28);
        assert_eq!(
            m.match_reader(&data[..], &b).unwrap(),
            Outcome::Unknown(Why::QueryByteLimit)
        );
        // Cancel stops between chunks with an error.
        let cancel = Cancel::default();
        let b = Budget::new(Limits::default(), cancel.clone(), false);
        cancel.cancel();
        assert!(matches!(
            m.match_reader(&data[..], &b),
            Err(Error::Cancelled)
        ));
        // Cancelled after the first chunk of a large input.
        struct CancelAfter<'a>(&'a [u8], Cancel);
        impl Read for CancelAfter<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.read(buf)?;
                if n > 0 {
                    self.1.cancel();
                }
                Ok(n)
            }
        }
        let cancel = Cancel::default();
        let b = Budget::new(Limits::default(), cancel.clone(), false);
        let big = vec![b'x'; CHUNK * 3];
        assert!(matches!(
            m.match_reader(CancelAfter(&big, cancel), &b),
            Err(Error::Cancelled)
        ));
        assert_eq!(b.bytes_read(), CHUNK as u64);
    }

    #[test]
    fn files_extracted_text_and_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("doc.bin");
        std::fs::write(&p, encode(TEXT, FxEncoding::Utf16LeBom)).unwrap();
        let b = Budget::default();
        let m = Matcher::new(spec("au lait", Encoding::Auto)).unwrap();
        assert!(m.match_file(&p, None, &b).unwrap().is_match());
        // The library's text replaces the bytes for `content:`...
        assert_eq!(
            m.match_file(&p, Some("nothing here"), &b).unwrap(),
            Outcome::NoMatch
        );
        // ...but not for an explicit encoding.
        let m16 = Matcher::new(spec("au lait", Encoding::Utf16Le)).unwrap();
        assert!(m16.match_file(&p, Some("nothing"), &b).unwrap().is_match());
        assert_eq!(
            m.match_file(dir.path(), None, &b).unwrap(),
            Outcome::Unknown(Why::NotAFile)
        );
        assert!(matches!(
            m.match_file(&dir.path().join("missing"), None, &b),
            Err(Error::Io(_))
        ));
        // A placeholder is not opened (a missing path would fail if it were)...
        let missing = dir.path().join("cloud-only");
        assert_eq!(
            m.match_file_as(&missing, true, true, None, &b).unwrap(),
            Outcome::Unknown(Why::Placeholder)
        );
        // ...its extracted text is still searched, and recall reads it when allowed.
        assert!(m
            .match_file_as(&missing, true, true, Some("caf\u{e9} au lait"), &b)
            .unwrap()
            .is_match());
        let recall = Budget::new(Limits::default(), Cancel::default(), true);
        assert!(m.match_file_as(&p, true, true, None, &recall).unwrap().is_match());
    }

    /// A real offline file (the attribute cloud providers set on placeholders) is skipped.
    #[cfg(windows)]
    #[test]
    fn offline_attribute_is_never_read() {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{
            SetFileAttributesW, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_OFFLINE,
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("offline.txt");
        std::fs::write(&p, "au lait").unwrap();
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            SetFileAttributesW(
                PCWSTR(wide.as_ptr()),
                FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_ARCHIVE,
            )
            .unwrap();
        }
        let m = Matcher::new(spec("au lait", Encoding::Auto)).unwrap();
        let b = Budget::default();
        assert_eq!(
            m.match_file(&p, None, &b).unwrap(),
            Outcome::Unknown(Why::Placeholder)
        );
        assert_eq!(b.bytes_read(), 0);
    }

    #[test]
    fn spec_parse() {
        let s = Spec::parse("case:ww:regex:each-line:^fn main", Encoding::Auto);
        assert!(s.case && s.whole_words && s.regex && s.lines == Lines::EachLine);
        assert_eq!(s.pattern, "^fn main");
        let s = Spec::parse("utf16becontent:hex:00 41", Encoding::Auto);
        assert_eq!((s.encoding, s.pattern.as_str()), (Encoding::Hex, "00 41"));
        let s = Spec::parse("\"case:x\"", Encoding::Utf8);
        assert_eq!((s.encoding, s.case, s.pattern.as_str()), (Encoding::Utf8, false, "case:x"));
        let s = Spec::parse("http://x", Encoding::Auto);
        assert_eq!(s.pattern, "http://x");
        assert_eq!(Encoding::from_function("UTF16Content:"), Some(Encoding::Utf16Le));
        for e in [
            Encoding::Auto,
            Encoding::Utf8,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Ansi,
            Encoding::Binary,
            Encoding::Hex,
        ] {
            assert_eq!(Encoding::from_function(e.function_name()), Some(e));
        }
    }
}
