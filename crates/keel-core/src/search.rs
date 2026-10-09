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

/// A parsed library query. `LibraryQuery::parse` reads an Everything-like string:
/// words (prefix match on name or path words), `"exact phrase"`, `ext:pdf;docx`,
/// `size:>1mb` / `size:<=10kb` / `size:1mb..5mb`, `dm:2026-10` / `dm:>=2026-01-15` /
/// `dm:2026-01..2026-03` / `dm:today`, `kind:image` (also `file:` and `folder:`),
/// `tag:work`, `in:"source label"`. Dates are UTC.
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

/// `[start, end)` in unix seconds of `2026`, `2026-10`, `2026-10-09`, `today`, `yesterday`.
fn date_range(s: &str, now: i64) -> Option<(i64, i64)> {
    const DAY: i64 = 86_400;
    let today = now.div_euclid(DAY) * DAY;
    match s.to_ascii_lowercase().as_str() {
        "today" => return Some((today, today + DAY)),
        "yesterday" => return Some((today - DAY, today)),
        _ => {}
    }
    let parts: Vec<i64> = s
        .split(['-', '/', '.'])
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let day = |y, m, d| days_from_civil(y, m, d) * DAY;
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

impl LibraryQuery {
    pub fn parse(s: &str) -> Result<LibraryQuery> {
        Self::parse_at(s, crate::now())
    }

    fn parse_at(s: &str, now: i64) -> Result<LibraryQuery> {
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
                    q.max_size = before.map(|b| (b - 1) as u64);
                }
                "dm" | "datemodified" | "modified" => {
                    let (from, before) = range_filter(&value, |x| date_range(x, now))
                        .with_context(|| format!("bad date: {value}"))?;
                    q.modified_from = from;
                    q.modified_before = before;
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

    /// The FTS5 MATCH expression for the words and phrases, None when there are none.
    fn text_match(&self) -> Option<String> {
        let mut parts: Vec<String> = self
            .terms
            .iter()
            .map(|t| format!("{}*", quote(t)))
            .collect();
        parts.extend(self.phrases.iter().map(|p| quote(p)));
        (!parts.is_empty()).then(|| parts.join(" AND "))
    }

    /// Extensions are name tokens too: a text-less `ext:` query narrows through the index
    /// before the exact LIKE check.
    fn ext_match(&self) -> Option<String> {
        let usable = !self.ext.is_empty()
            && self
                .ext
                .iter()
                .all(|e| e.chars().any(char::is_alphanumeric));
        usable.then(|| {
            let any: Vec<String> = self
                .ext
                .iter()
                .map(|e| format!("name : {}", quote(e)))
                .collect();
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
    }

    /// SQL conditions on `record r` (each starting with " AND ") and their arguments; None
    /// when nothing can match.
    fn filters(&self) -> Option<(String, Vec<Value>)> {
        let mut sql = String::from(" AND r.parent IS NOT NULL");
        let mut args = Vec::new();
        let mut ext: Vec<&str> = self.ext.iter().map(String::as_str).collect();
        match self.kind {
            Some(KindFilter::Folder) => sql.push_str(" AND r.kind = 1"),
            Some(k) => {
                sql.push_str(" AND r.kind = 0");
                if !k.extensions().is_empty() {
                    if ext.is_empty() {
                        ext = k.extensions().to_vec();
                    } else {
                        ext.retain(|e| k.extensions().contains(e));
                        if ext.is_empty() {
                            return None;
                        }
                    }
                }
            }
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
        for (bound, op) in [(self.modified_from, ">="), (self.modified_before, "<")] {
            if let Some(t) = bound {
                sql.push_str(&format!(" AND r.mtime {op} ?"));
                args.push(Value::Integer(t));
            }
        }
        // Tag names resolve through the store's copy of the library's tags (nested included).
        for tag in &self.tags {
            sql.push_str(
                " AND r.id IN (SELECT record FROM record_tag WHERE tag IN (
                     WITH RECURSIVE t(id) AS (
                         SELECT id FROM tag WHERE name = ? COLLATE NOCASE
                         UNION SELECT tag.id FROM tag JOIN t ON tag.parent = t.id)
                     SELECT id FROM t))",
            );
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
        modified: r.get(5)?,
        score,
    })
}

/// Runs one candidate query (`id`, `rank` columns) through the filters, best first.
fn run(
    src: &Source,
    q: &LibraryQuery,
    now: i64,
    cand: &str,
    mut args: Vec<Value>,
) -> Result<Vec<LibraryHit>> {
    let Some((filters, filter_args)) = q.filters() else {
        return Ok(Vec::new());
    };
    args.extend(filter_args);
    args.push(Value::Integer(q.max as i64));
    let sql = format!(
        "SELECT {HIT_COLUMNS},
                cand.rank - {RECENCY} / (1.0 + max(0, {now} - coalesce(r.mtime, 0)) / {RECENCY_SCALE})
                    AS score
         FROM ({cand}) AS cand JOIN record r ON r.id = cand.id
         WHERE 1{filters} ORDER BY score LIMIT ?"
    );
    let c = src.store.get()?;
    let mut stmt = c.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
        let score = r.get(6)?;
        hit_of(src, r, score)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn search_source(src: &Source, q: &LibraryQuery, now: i64) -> Result<Vec<LibraryHit>> {
    let text = |m: &String| Value::Text(m.clone());
    let Some(m) = q.text_match() else {
        // No words: filters only, newest first.
        return match q.ext_match() {
            Some(e) => run(
                src,
                q,
                now,
                "SELECT rowid AS id, 0.0 AS rank FROM record_fts WHERE record_fts MATCH ?",
                vec![text(&e)],
            ),
            None => run(
                src,
                q,
                now,
                "SELECT id, 0.0 AS rank FROM record",
                Vec::new(),
            ),
        };
    };
    let matches: i64 = src.store.get()?.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM record_fts WHERE record_fts MATCH ?1 LIMIT ?2)",
        rusqlite::params![m, RANK_CAP as i64 + 1],
        |r| r.get(0),
    )?;
    let candidates = (q.max * 4).max(1_000) as i64;
    if matches as usize > RANK_CAP {
        // ponytail: too broad to rank within budget (about 1 us a match): the first matches
        // by record id, newest first. A narrower query ranks.
        return run(
            src,
            q,
            now,
            "SELECT rowid AS id, 0.0 AS rank FROM record_fts WHERE record_fts MATCH ? LIMIT ?",
            vec![text(&m), Value::Integer(candidates)],
        );
    }
    // The best bm25 candidates (name weighted over path), re-ranked by recency after the join.
    const RANKED: &str = "SELECT rowid AS id, bm25(record_fts, 4.0, 1.0) AS rank FROM record_fts
                          WHERE record_fts MATCH ? ORDER BY rank LIMIT ?";
    let hits = run(
        src,
        q,
        now,
        RANKED,
        vec![text(&m), Value::Integer(candidates)],
    )?;
    if hits.len() < q.max && matches > candidates && q.has_filters() {
        // Selective filters dropped too many candidates: rank every match.
        return run(src, q, now, RANKED, vec![text(&m), Value::Integer(matches)]);
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
            match search_source(src, q, now) {
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
        let mut lq = LibraryQuery::parse(&q.text)?;
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
