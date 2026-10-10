//! Library search (spec 2.10 "Search"): one query across every source's store, FTS5 over
//! names and paths with filters, ranked by bm25 plus a recency boost. Works for offline
//! sources (their last generation); each hit carries its source's status.

use crate::library::{RecordRef, Source, SourceStatus};
use crate::Library;
use anyhow::{Context, Result};
use keel_vfs::VPath;
use rusqlite::types::Value;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Hits returned when a query names no limit.
pub const DEFAULT_MAX: usize = 500;
/// The most a fresh file's score improves on an old one's (bm25 scores run about -1 to -25).
const RECENCY: f64 = 1.0;
/// Recency halves over this many seconds (30 days).
const RECENCY_SCALE: f64 = 2_592_000.0;
/// Text queries with more matches than this are not ranked by bm25.
const RANK_CAP: usize = 50_000;
/// Text queries with at most this many matches score every match after the join (no bm25,
/// whose document frequencies cost a full read of each word's postings).
const DIRECT: i64 = 5_000;
/// Score bonus for each query word found in the name (not only in the path).
const NAME_BOOST: f64 = 2.0;
/// Filter-only queries narrow through an index (extension, size) when it yields at most
/// this many records; broader filters walk the records newest first instead.
const NARROW_CAP: i64 = 20_000;
const NS: i64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KindFilter {
    File,
    Folder,
    Image,
    Video,
    Audio,
    Document,
    Archive,
}

impl KindFilter {
    fn parse(s: &str) -> Option<KindFilter> {
        Some(match s.to_ascii_lowercase().as_str() {
            "file" | "files" => KindFilter::File,
            "folder" | "folders" | "dir" => KindFilter::Folder,
            "image" | "images" | "pic" | "picture" => KindFilter::Image,
            "video" | "videos" => KindFilter::Video,
            "audio" | "music" => KindFilter::Audio,
            "doc" | "docs" | "document" | "documents" => KindFilter::Document,
            "archive" | "archives" | "zip" => KindFilter::Archive,
            _ => return None,
        })
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            KindFilter::File | KindFilter::Folder => &[],
            KindFilter::Image => &[
                "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "heic", "heif", "svg",
                "ico", "raw", "cr2", "nef", "arw", "dng",
            ],
            KindFilter::Video => &[
                "mp4", "mkv", "mov", "avi", "wmv", "webm", "m4v", "mpg", "mpeg", "flv", "3gp",
            ],
            KindFilter::Audio => &[
                "mp3", "flac", "wav", "aac", "m4a", "ogg", "opus", "wma", "aiff",
            ],
            KindFilter::Document => &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf",
                "txt", "md", "csv", "epub",
            ],
            KindFilter::Archive => &[
                "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso",
            ],
        }
    }
}

/// Inclusive bounds on a media number (`w:>4000`, `duration:<2m`); None = open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

impl Span {
    fn is_open(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }

    fn empty(&self) -> bool {
        self.min.zip(self.max).is_some_and(|(a, b)| a > b)
    }

    /// ` AND <col> >= ? AND <col> <= ?` for the bounds set.
    fn sql(&self, col: &str, sql: &mut String, args: &mut Vec<Value>) {
        for (bound, op) in [(self.min, ">="), (self.max, "<=")] {
            if let Some(b) = bound {
                sql.push_str(&format!(" AND {col} {op} ?"));
                args.push(Value::Integer(b));
            }
        }
    }
}

/// A parsed library query. `LibraryQuery::parse` reads an Everything-like string:
/// words (prefix match on name or path words), `"exact phrase"`, `ext:pdf;docx`,
/// `size:>1mb` / `size:<=10kb` / `size:1mb..5mb`, `dm:2026-10` / `dm:>=2026-01-15` /
/// `dm:2026-01..2026-03` / `dm:today`, `kind:image` (also `file:` and `folder:`),
/// `tag:work`, `in:"source label"`. Media filters (from the sidecar `media` rows):
/// `camera:canon`, `taken:2024` (same dates as `dm:`; no capture time, no match),
/// `w:>4000` / `h:<=1080` (pixels, `size:` comparisons), `duration:>30s` / `duration:<2m`
/// (videos), `has:gps`, `kind:photo`. Dates are local days for the UTC offset passed to
/// `parse`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryQuery {
    pub terms: Vec<String>,
    pub phrases: Vec<String>,
    pub kind: Option<KindFilter>,
    /// Lowercase, without the dot; any of them.
    pub ext: Vec<String>,
    /// Inclusive byte bounds (files only).
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Unix seconds: `modified_from <= mtime < modified_before`.
    pub modified_from: Option<i64>,
    pub modified_before: Option<i64>,
    /// Source labels (case-insensitive) or ids; any of them.
    pub sources: Vec<String>,
    /// Tag names (a tag also matches its nested tags); all of them.
    pub tags: Vec<String>,
    /// Camera text, prefix match on its words (case-insensitive).
    #[serde(default)]
    pub camera: Option<String>,
    /// Unix seconds, `taken_at` (never the modified time).
    #[serde(default)]
    pub taken: Span,
    #[serde(default)]
    pub width: Span,
    #[serde(default)]
    pub height: Span,
    #[serde(default)]
    pub duration_ms: Span,
    #[serde(default)]
    pub has_gps: bool,
    /// `kind:photo`: images with a media row and no duration.
    #[serde(default)]
    pub photo: bool,
    pub max: usize,
}

impl Default for LibraryQuery {
    fn default() -> Self {
        LibraryQuery {
            terms: Vec::new(),
            phrases: Vec::new(),
            kind: None,
            ext: Vec::new(),
            min_size: None,
            max_size: None,
            modified_from: None,
            modified_before: None,
            sources: Vec::new(),
            tags: Vec::new(),
            camera: None,
            taken: Span::default(),
            width: Span::default(),
            height: Span::default(),
            duration_ms: Span::default(),
            has_gps: false,
            photo: false,
            max: DEFAULT_MAX,
        }
    }
}

/// Splits on whitespace outside double quotes.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(s: &str) -> String {
    s.replace('"', "")
}

/// `1.5mb` -> bytes (1024-based units).
fn bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    Some((n * mult as f64) as u64)
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `[start, end)` in unix seconds of `2026`, `2026-10`, `2026-10-09`, `today`, `yesterday`,
/// as local dates `offset` seconds east of UTC.
fn date_range(s: &str, now: i64, offset: i64) -> Option<(i64, i64)> {
    const DAY: i64 = 86_400;
    let today = (now + offset).div_euclid(DAY) * DAY - offset;
    match s.to_ascii_lowercase().as_str() {
        "today" => return Some((today, today + DAY)),
        "yesterday" => return Some((today - DAY, today)),
        _ => {}
    }
    let parts: Vec<i64> = s
        .split(['-', '/', '.'])
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let day = |y, m, d| days_from_civil(y, m, d) * DAY - offset;
    match parts[..] {
        [y] => Some((day(y, 1, 1), day(y + 1, 1, 1))),
        [y, m] if (1..=12).contains(&m) => {
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            Some((day(y, m, 1), day(ny, nm, 1)))
        }
        [y, m, d] if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            Some((day(y, m, d), day(y, m, d) + DAY))
        }
        _ => None,
    }
}

/// `>x`, `>=x`, `<x`, `<=x`, `a..b`, `x` over ranges `[start, end)` of `x`:
/// the result is a `[from, before)` pair.
fn range_filter(
    v: &str,
    range: impl Fn(&str) -> Option<(i64, i64)>,
) -> Option<(Option<i64>, Option<i64>)> {
    if let Some((a, b)) = v.split_once("..") {
        return Some((Some(range(a)?.0), Some(range(b)?.1)));
    }
    for (op, f) in [(">=", 0), ("<=", 1), (">", 2), ("<", 3), ("=", 4)] {
        if let Some(x) = v.strip_prefix(op) {
            let (start, end) = range(x)?;
            return Some(match f {
                0 => (Some(start), None),
                1 => (None, Some(end)),
                2 => (Some(end), None),
                3 => (None, Some(start)),
                _ => (Some(start), Some(end)),
            });
        }
    }
    let (start, end) = range(v)?;
    Some((Some(start), Some(end)))
}

/// `30s`, `2m`, `1.5h`, `500ms`; a bare number is seconds. Milliseconds.
fn millis(s: &str) -> Option<i64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let mult = match unit {
        "ms" => 1.0,
        "" | "s" | "sec" => 1e3,
        "m" | "min" => 6e4,
        "h" | "hr" => 3.6e6,
        _ => return None,
    };
    Some((num.parse::<f64>().ok()? * mult) as i64)
}

/// An integer comparison (`>4000`, `a..b`) as inclusive bounds, like `size:`.
fn span(v: &str, num: impl Fn(&str) -> Option<i64>) -> Option<Span> {
    let (from, before) = range_filter(v, |x| {
        let n = num(x)?;
        Some((n, n + 1))
    })?;
    Some(Span {
        min: from,
        max: before.map(|b| b - 1),
    })
}

impl LibraryQuery {
    /// `utc_offset`: seconds east of UTC of the user's local time, for `dm:` dates.
    pub fn parse(s: &str, utc_offset: i64) -> Result<LibraryQuery> {
        Self::parse_at(s, crate::now(), utc_offset)
    }

    fn parse_at(s: &str, now: i64, utc_offset: i64) -> Result<LibraryQuery> {
        let mut q = LibraryQuery::default();
        for tok in tokens(s) {
            if tok.starts_with('"') {
                let phrase = unquote(&tok);
                if !phrase.trim().is_empty() {
                    q.phrases.push(phrase);
                }
                continue;
            }
            let (key, value) = match tok.split_once(':') {
                Some((k, v)) => (k.to_ascii_lowercase(), unquote(v)),
                None => (String::new(), String::new()),
            };
            match key.as_str() {
                "ext" => q.ext.extend(
                    value
                        .split([';', ','])
                        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                        .filter(|e| !e.is_empty()),
                ),
                "size" => {
                    // Sizes as an integer range: exclusive end = inclusive max + 1.
                    let (from, before) = range_filter(&value, |x| {
                        let b = bytes(x)? as i64;
                        Some((b, b + 1))
                    })
                    .with_context(|| format!("bad size: {value}"))?;
                    q.min_size = from.map(|b| b as u64);
                    q.max_size = before.map(|b| (b - 1).max(0) as u64);
                    if before.is_some_and(|b| b <= 0) {
                        // `size:<0`: nothing (an empty range, not every size).
                        q.min_size = Some(1);
                    }
                }
                "dm" | "datemodified" | "modified" => {
                    let (from, before) = range_filter(&value, |x| date_range(x, now, utc_offset))
                        .with_context(|| format!("bad date: {value}"))?;
                    q.modified_from = from;
                    q.modified_before = before;
                }
                "camera" if !value.is_empty() => q.camera = Some(value),
                "taken" => {
                    let (from, before) = range_filter(&value, |x| date_range(x, now, utc_offset))
                        .with_context(|| format!("bad date: {value}"))?;
                    q.taken = Span {
                        min: from,
                        max: before.map(|b| b - 1),
                    };
                }
                "w" | "width" | "h" | "height" => {
                    let sp = span(&value, |x| x.trim().parse().ok())
                        .with_context(|| format!("bad pixel count: {value}"))?;
                    if key.starts_with('w') {
                        q.width = sp;
                    } else {
                        q.height = sp;
                    }
                }
                "duration" | "length" => {
                    q.duration_ms =
                        span(&value, millis).with_context(|| format!("bad duration: {value}"))?;
                }
                "has" if value.eq_ignore_ascii_case("gps") => q.has_gps = true,
                "kind" | "type"
                    if matches!(value.to_ascii_lowercase().as_str(), "photo" | "photos") =>
                {
                    q.photo = true;
                }
                "kind" | "type" => {
                    q.kind = Some(
                        KindFilter::parse(&value)
                            .with_context(|| format!("unknown kind: {value}"))?,
                    );
                }
                "file" if value.is_empty() => q.kind = Some(KindFilter::File),
                "folder" if value.is_empty() => q.kind = Some(KindFilter::Folder),
                "tag" if !value.is_empty() => q.tags.push(value),
                "in" | "source" if !value.is_empty() => q.sources.push(value),
                _ => {
                    let term = unquote(&tok);
                    if term.chars().any(char::is_alphanumeric) {
                        q.terms.push(term);
                    }
                }
            }
        }
        Ok(q)
    }

    /// The FTS5 MATCH expressions (ANDed) for the words and phrases. Only the last word is a
    /// prefix (it may be half typed) unless `all_prefix`: a prefix of a
    /// very common word (one in every path) costs a full read of its postings.
    fn text_parts(&self, all_prefix: bool) -> Vec<String> {
        let last = self.terms.len().saturating_sub(1);
        let mut parts: Vec<String> = self
            .terms
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if all_prefix || i == last {
                    format!("{}*", quote(t))
                } else {
                    quote(t)
                }
            })
            .collect();
        parts.extend(self.phrases.iter().map(|p| quote(p)));
        parts
    }

    /// The extensions a match must have (`ext:`, narrowed by `kind:`); empty for any, None
    /// when none can match.
    fn extensions(&self) -> Option<Vec<&str>> {
        let mut ext: Vec<&str> = self.ext.iter().map(String::as_str).collect();
        if let Some(k) = self.kind.filter(|k| !k.extensions().is_empty()) {
            if ext.is_empty() {
                ext = k.extensions().to_vec();
            } else {
                ext.retain(|e| k.extensions().contains(e));
                if ext.is_empty() {
                    return None;
                }
            }
        }
        Some(ext)
    }

    /// Extensions are name tokens too: a query with `ext:` (or a `kind:` that implies
    /// extensions) narrows through the index before the exact LIKE check.
    fn ext_match(&self) -> Option<String> {
        let ext = self.extensions()?;
        let usable = !ext.is_empty() && ext.iter().all(|e| e.chars().any(char::is_alphanumeric));
        usable.then(|| {
            let any: Vec<String> = ext.iter().map(|e| format!("name : {}", quote(e))).collect();
            any.join(" OR ")
        })
    }

    fn has_filters(&self) -> bool {
        self.kind.is_some()
            || !self.ext.is_empty()
            || self.min_size.is_some()
            || self.max_size.is_some()
            || self.modified_from.is_some()
            || self.modified_before.is_some()
            || !self.tags.is_empty()
            || self.has_media_filters()
    }

    fn has_media_filters(&self) -> bool {
        self.camera.is_some()
            || !(self.taken.is_open() && self.width.is_open() && self.height.is_open())
            || !self.duration_ms.is_open()
            || self.has_gps
            || self.photo
    }

    /// Conditions on `media m` (each starting with " AND "); None when none can match.
    /// The camera is a word-prefix match in `media_fts` (case-insensitive).
    fn media_conds(&self) -> Option<(String, Vec<Value>)> {
        let (mut sql, mut args) = (String::new(), Vec::new());
        let spans = [
            (&self.taken, "m.taken_at"),
            (&self.width, "m.width"),
            (&self.height, "m.height"),
            (&self.duration_ms, "m.duration_ms"),
        ];
        for (sp, col) in spans {
            if sp.empty() {
                return None;
            }
            sp.sql(col, &mut sql, &mut args);
        }
        if self.has_gps {
            sql.push_str(" AND m.gps_lat IS NOT NULL AND m.gps_lon IS NOT NULL");
        }
        if self.photo {
            sql.push_str(" AND m.duration_ms IS NULL");
        }
        if let Some(c) = &self.camera {
            sql.push_str(" AND m.record IN (SELECT rowid FROM media_fts WHERE media_fts MATCH ?)");
            args.push(Value::Text(format!("camera : {}*", quote(c))));
        }
        Some((sql, args))
    }

    /// SQL conditions on `record r` (each starting with " AND ") and their arguments; None
    /// when nothing can match.
    fn filters(&self) -> Option<(String, Vec<Value>)> {
        let mut sql = String::from(" AND r.parent IS NOT NULL");
        let mut args = Vec::new();
        let ext = self.extensions()?;
        if self.min_size.zip(self.max_size).is_some_and(|(a, b)| a > b) {
            return None;
        }
        match self.kind {
            Some(KindFilter::Folder) => sql.push_str(" AND r.kind = 1"),
            Some(_) => sql.push_str(" AND r.kind = 0"),
            None => {}
        }
        if !ext.is_empty() {
            let likes = vec!["r.name LIKE ? ESCAPE '\\'"; ext.len()].join(" OR ");
            sql.push_str(&format!(" AND ({likes})"));
            args.extend(
                ext.iter()
                    .map(|e| Value::Text(format!("%.{}", like_escape(e)))),
            );
        }
        if self.min_size.is_some() || self.max_size.is_some() {
            sql.push_str(" AND r.kind = 0");
        }
        for (bound, op) in [(self.min_size, ">="), (self.max_size, "<=")] {
            if let Some(b) = bound {
                sql.push_str(&format!(" AND r.size {op} ?"));
                args.push(Value::Integer(b.min(i64::MAX as u64) as i64));
            }
        }
        // Stored in nanoseconds.
        for (bound, op) in [(self.modified_from, ">="), (self.modified_before, "<")] {
            if let Some(t) = bound {
                sql.push_str(&format!(" AND r.mtime {op} ?"));
                args.push(Value::Integer(t.saturating_mul(NS)));
            }
        }
        if self.has_media_filters() {
            let (m, margs) = self.media_conds()?;
            sql.push_str(&format!(
                " AND r.id IN (SELECT m.record FROM media m WHERE 1{m})"
            ));
            args.extend(margs);
        }
        // Tag names resolve through the store's copy of the library's tags (nested included).
        for tag in &self.tags {
            sql.push_str(&format!(" AND r.id IN ({TAGGED})"));
            args.push(Value::Text(tag.clone()));
        }
        Some((sql, args))
    }

    fn wants(&self, src: &Source) -> bool {
        self.sources.is_empty()
            || self
                .sources
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&src.def.label) || *s == src.id.0)
    }
}

/// A search result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryHit {
    pub record: RecordRef,
    pub source_label: String,
    /// Offline sources answer from their last generation.
    pub status: SourceStatus,
    pub path: VPath,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds.
    pub modified: Option<i64>,
    /// Lower ranks first.
    pub score: f64,
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The record columns every hit query selects (`r` = record), in `hit_of` order.
pub(crate) const HIT_COLUMNS: &str = "r.id, r.path, r.name, r.kind, r.size, r.mtime";

pub(crate) fn hit_of(src: &Source, r: &rusqlite::Row, score: f64) -> rusqlite::Result<LibraryHit> {
    let path: String = r.get(1)?;
    Ok(LibraryHit {
        record: RecordRef {
            source: src.id.clone(),
            id: r.get(0)?,
        },
        source_label: src.def.label.clone(),
        status: src.status.read().clone(),
        path: src.absolute(&path),
        name: r.get(2)?,
        is_dir: r.get::<_, i64>(3)? == crate::fsid::DIR,
        size: r.get::<_, i64>(4)? as u64,
        modified: r.get::<_, Option<i64>>(5)?.map(|ns| ns.div_euclid(NS)),
        score,
    })
}

/// The recency part of a hit's score (`r` = record): up to [`RECENCY`] off for a fresh file.
fn recency(now: i64) -> String {
    format!("{RECENCY} / (1.0 + max(0, {now} - coalesce(r.mtime, 0) / {NS}) / {RECENCY_SCALE})")
}

/// Runs one candidate query (`id`, `rank` columns) through the filters, best first: the
/// candidate's rank, minus [`NAME_BOOST`] for each word found in the name, minus recency.
/// The candidates drive the join (`CROSS JOIN`), never an index on a broad filter.
fn run(
    src: &Source,
    q: &LibraryQuery,
    now: i64,
    cand: &str,
    cand_args: Vec<Value>,
    also: &[String],
) -> Result<Vec<LibraryHit>> {
    let Some((filters, filter_args)) = q.filters() else {
        return Ok(Vec::new());
    };
    // Placeholders in text order: name boost (select list), candidates, filters, limit.
    let mut args = Vec::new();
    let mut boost = String::from("0");
    for w in q.terms.iter().chain(&q.phrases) {
        boost.push_str(" + (instr(lower(r.name), ?) > 0)");
        args.push(Value::Text(w.to_lowercase()));
    }
    args.extend(cand_args);
    args.extend(filter_args);
    // Words left out of the candidate query: substrings of the path.
    let mut filters = filters;
    for word in also {
        filters.push_str(" AND lower(r.path) LIKE ? ESCAPE '\\'");
        args.push(Value::Text(format!(
            "%{}%",
            like_escape(&word.to_lowercase())
        )));
    }
    args.push(Value::Integer(q.max as i64));
    let sql = format!(
        "SELECT {HIT_COLUMNS}, cand.rank - {NAME_BOOST} * ({boost}) - {} AS score
         FROM ({cand}) AS cand CROSS JOIN record r ON r.id = cand.id
         WHERE 1{filters} ORDER BY score LIMIT ?",
        recency(now)
    );
    let c = src.store.get()?;
    let mut stmt = c.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
        let score = r.get(6)?;
        hit_of(src, r, score)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Rows `sql` (a candidate query) yields, counted up to `cap + 1`.
fn count_upto(src: &Source, sql: &str, args: &[Value], cap: i64) -> Result<i64> {
    let mut all = args.to_vec();
    all.push(Value::Integer(cap + 1));
    Ok(src.store.get()?.query_row(
        &format!("SELECT count(*) FROM ({sql} LIMIT ?)"),
        rusqlite::params_from_iter(all),
        |r| r.get(0),
    )?)
}

/// A filter-only query: through the index of a selective filter (an extension or a size
/// range) when it yields at most [`NARROW_CAP`] records, all of them scored; else
/// the records newest first (the mtime and kind indexes), checked until enough pass, so a
/// broad filter stops early.
fn search_filters(src: &Source, q: &LibraryQuery, now: i64) -> Result<Vec<LibraryHit>> {
    let mut narrow: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(e) = q.ext_match() {
        narrow.push((
            "SELECT rowid AS id, 0.0 AS rank FROM record_fts WHERE record_fts MATCH ?".into(),
            vec![Value::Text(e)],
        ));
    }
    if q.min_size.is_some() || q.max_size.is_some() {
        narrow.push((
            "SELECT id, 0.0 AS rank FROM record INDEXED BY record_size
             WHERE kind = 0 AND size >= ? AND size <= ?"
                .into(),
            vec![
                Value::Integer(q.min_size.unwrap_or(0).min(i64::MAX as u64) as i64),
                Value::Integer(q.max_size.unwrap_or(u64::MAX).min(i64::MAX as u64) as i64),
            ],
        ));
    }
    if q.has_media_filters() {
        // The media table is small next to the records: its taken index when dated, else a
        // scan (ponytail: no index on width/height/duration; a rare one costs ~50 ns a
        // media row, add an index in the next schema migration if that ever shows).
        let Some((m, margs)) = q.media_conds() else {
            return Ok(Vec::new());
        };
        let by = if q.taken.is_open() {
            ""
        } else {
            " INDEXED BY media_taken"
        };
        narrow.insert(
            0,
            (
                format!("SELECT m.record AS id, 0.0 AS rank FROM media m{by} WHERE 1{m}"),
                margs,
            ),
        );
    }
    for (sql, args) in narrow {
        if count_upto(src, &sql, &args, NARROW_CAP)? <= NARROW_CAP {
            return run(src, q, now, &sql, args, &[]);
        }
    }
    let Some((filters, mut args)) = q.filters() else {
        return Ok(Vec::new());
    };
    args.push(Value::Integer(q.max as i64));
    // Newest first is best first here (no words: the score is recency alone).
    let sql = format!(
        "SELECT {HIT_COLUMNS}, 0.0 - {} AS score FROM record r
         WHERE 1{filters} ORDER BY r.mtime DESC LIMIT ?",
        recency(now)
    );
    let c = src.store.get()?;
    let mut stmt = c.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
        let score = r.get(6)?;
        hit_of(src, r, score)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The records carrying the tag named by the one parameter, or a tag nested in it.
const TAGGED: &str = "SELECT record FROM record_tag WHERE tag IN (
    WITH RECURSIVE t(id) AS (
        SELECT id FROM tag WHERE name = ? COLLATE NOCASE
        UNION SELECT tag.id FROM tag JOIN t ON tag.parent = t.id)
    SELECT id FROM t)";

/// Rank of a media-only (camera or keyword) hit: worse than any name or path hit.
const MEDIA_RANK: f64 = 10.0;

fn search_source(src: &Source, q: &LibraryQuery, now: i64) -> Result<Vec<LibraryHit>> {
    let parts = q.text_parts(false);
    if parts.is_empty() {
        return search_filters(src, q, now);
    }
    let mut hits = search_text(src, q, now, parts)?;
    if hits.is_empty() && q.terms.len() > 1 {
        // Nothing with whole earlier words: every word as a prefix.
        hits = search_text(src, q, now, q.text_parts(true))?;
    }
    if hits.is_empty() {
        // No name or path hit: the words may be a camera or XMP keywords (`media_fts`).
        let m = q.text_parts(true).join(" AND ");
        let cand = format!(
            "SELECT rowid AS id, {MEDIA_RANK} AS rank FROM media_fts WHERE media_fts MATCH ?"
        );
        return run(src, q, now, &cand, vec![Value::Text(m)], &[]);
    }
    Ok(hits)
}

/// SQLITE_SCHEMA: the store's schema changed since this connection read it.
fn schema_changed(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(c.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _)) if f.code == rusqlite::ErrorCode::SchemaChanged)
    })
}

/// Matches of `m`, at most `RANK_CAP + 1`.
fn count(src: &Source, m: &str) -> Result<i64> {
    Ok(src.store.get()?.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM record_fts WHERE record_fts MATCH ?1 LIMIT ?2)",
        rusqlite::params![m, RANK_CAP as i64 + 1],
        |r| r.get(0),
    )?)
}

fn search_text(
    src: &Source,
    q: &LibraryQuery,
    now: i64,
    parts: Vec<String>,
) -> Result<Vec<LibraryHit>> {
    // Extensions are name tokens: narrowing by them in the index keeps candidates relevant.
    let ext = q.ext_match().map(|e| format!("({e})"));
    let all_of = |parts: &[String]| -> String {
        parts
            .iter()
            .chain(&ext)
            .cloned()
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let m = all_of(&parts);
    // A tag holding few records: those that match, scored (ranking every match and then
    // dropping the untagged ones read all of a common word's matches twice).
    if let Some(tag) = q.tags.first() {
        let tag = Value::Text(tag.clone());
        if count_upto(src, TAGGED, std::slice::from_ref(&tag), NARROW_CAP)? <= NARROW_CAP {
            let cand = format!(
                "SELECT DISTINCT record AS id, 0.0 AS rank FROM ({TAGGED})
                 WHERE record IN (SELECT rowid FROM record_fts WHERE record_fts MATCH ?)"
            );
            return run(src, q, now, &cand, vec![tag, Value::Text(m)], &[]);
        }
    }
    let matches = count(src, &m)?;
    let candidates = (q.max * 4).max(1_000) as i64;
    const ALL: &str = "SELECT rowid AS id, 0.0 AS rank FROM record_fts WHERE record_fts MATCH ?";
    if matches <= DIRECT.max(candidates) {
        return run(src, q, now, ALL, vec![Value::Text(m)], &[]);
    }
    if matches as usize > RANK_CAP {
        // ponytail: too broad to rank within budget (about 1 us a match): the first matches
        // by record id that pass the filters (checked before the limit, so a rare filter
        // still finds its few), scored. A narrower query ranks.
        let Some((filters, filter_args)) = q.filters() else {
            return Ok(Vec::new());
        };
        let mut args = vec![Value::Text(m)];
        args.extend(filter_args);
        args.push(Value::Integer(candidates));
        return run(
            src,
            q,
            now,
            &format!(
                "SELECT record_fts.rowid AS id, 0.0 AS rank
                 FROM record_fts JOIN record r ON r.id = record_fts.rowid
                 WHERE record_fts MATCH ?{filters} LIMIT ?"
            ),
            args,
            &[],
        );
    }
    // bm25 reads every posting of each word to weigh it, and a word in most records weighs
    // nothing: rank by the rarer words and check the common ones in the candidates' paths
    // (ponytail: as substrings, so `client` also passes `subclient`; a word that common
    // matches nearly everything anyway).
    let words: Vec<&String> = q.terms.iter().chain(&q.phrases).collect();
    let (mut rare, mut common, mut common_words) = (Vec::new(), Vec::new(), Vec::new());
    for (part, word) in parts.into_iter().zip(words) {
        if count(src, &part)? as usize > RANK_CAP {
            common.push(part);
            common_words.push(word.clone());
        } else {
            rare.push(part);
        }
    }
    if rare.is_empty() {
        (rare, common_words) = (common, Vec::new());
    }
    // The best bm25 candidates (name weighted over path), re-scored after the join.
    const RANKED: &str = "SELECT rowid AS id, bm25(record_fts, 4.0, 1.0) AS rank FROM record_fts
                          WHERE record_fts MATCH ? ORDER BY rank LIMIT ?";
    let ranked = all_of(&rare);
    let hits = run(
        src,
        q,
        now,
        RANKED,
        vec![Value::Text(ranked), Value::Integer(candidates)],
        &common_words,
    )?;
    if hits.len() < q.max && (q.has_filters() || !common_words.is_empty()) {
        // Filters or common words dropped too many candidates: score every match.
        return run(src, q, now, ALL, vec![Value::Text(m)], &[]);
    }
    Ok(hits)
}

impl Library {
    /// Searches every source (offline ones from their last generation), best first. A store
    /// that cannot be read is skipped (logged).
    pub fn search(&self, q: &LibraryQuery) -> Result<Vec<LibraryHit>> {
        let now = crate::now();
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        let mut hits = Vec::new();
        for src in sources.iter().filter(|s| q.wants(s)) {
            // A walk rebuilds the filter indexes (a schema change); FTS5 reports that from
            // its constructor on a connection that read the old schema instead of preparing
            // again, so the search would come back empty: once more on the fresh schema.
            let found = search_source(src, q, now).or_else(|e| match schema_changed(&e) {
                true => search_source(src, q, now),
                false => Err(e),
            });
            match found {
                Ok(h) => hits.extend(h),
                Err(e) => tracing::warn!("search in {}: {e:#}", src.def.label),
            }
        }
        hits.sort_by(|a, b| a.score.total_cmp(&b.score));
        hits.truncate(q.max);
        Ok(hits)
    }
}

/// The library as a `keel_search` backend, so the search tab can pick "Library".
pub struct LibrarySearcher(pub Arc<Library>);

impl keel_search::Searcher for LibrarySearcher {
    fn query(&self, q: &keel_search::Query) -> Result<Vec<keel_search::Hit>> {
        let mut lq = LibraryQuery::parse(&q.text, self.0.utc_offset())?;
        if q.folders_only {
            lq.kind = Some(KindFilter::Folder);
        }
        lq.max = q.max as usize;
        Ok(self
            .0
            .search(&lq)?
            .into_iter()
            .map(|h| keel_search::Hit {
                path: h.path,
                is_dir: h.is_dir,
                size: h.size,
                modified: h.modified.map(|t| {
                    let d = std::time::Duration::from_secs(t.unsigned_abs());
                    if t >= 0 {
                        std::time::UNIX_EPOCH + d
                    } else {
                        std::time::UNIX_EPOCH - d
                    }
                }),
            })
            .collect())
    }

    fn available(&self) -> bool {
        true
    }

    fn status(&self) -> Option<String> {
        let offline = self
            .0
            .sources()
            .iter()
            .filter(|s| matches!(s.status, SourceStatus::Offline { .. }))
            .count();
        (offline > 0).then(|| format!("{offline} offline source(s): last indexed contents"))
    }

    fn name(&self) -> &'static str {
        "Library"
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
