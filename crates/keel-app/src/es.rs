//! `keel es` (Task 79): a command line search compatible with voidtools' `es.exe`, over the
//! system index (`keel_search::default_searcher`: Everything, Keel's NTFS index, Spotlight,
//! locate or the folder walk) or, with `-library`, the library. Switches start with `-`
//! (`--` works too) or `/`, in any case; `/a…` and `/o…` are the DIR-style ones. Anything
//! else is the search; a word with spaces is searched as a phrase; after `--` everything is
//! the search.
//!
//! Results are read from the backend (at most [`FETCH_CAP`]), filtered (`-path`, `-parent`,
//! `/a-d`, whole words, prefix, suffix, diacritics), sorted (name by default, as
//! Everything), then `-o`/`-n` pick the page. Exit codes: 0 ok, 1 failed (or no results with
//! `-no-result-error`), 2 usage.

use crate::commands::{FAILED, OK, USAGE};
use keel_search::{SearchState, Searcher};
use regex::{Regex, RegexBuilder};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Most hits read from the system index before filtering and sorting.
pub const FETCH_CAP: u32 = 250_000;
/// The library's largest answer (the `search` operation's cap).
const LIBRARY_CAP: usize = 10_000;
/// How long a still-building index is waited for without `-timeout`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// FILETIME (100 ns since 1601) of the Unix epoch.
const FILETIME_UNIX: u64 = 116_444_736_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Extension,
    DateModified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Name,
    /// The folder the result is in (`-path-column`).
    Path,
    FullPath,
    Extension,
    Size,
    DateModified,
}

impl Column {
    fn header(self) -> &'static str {
        match self {
            Column::Name => "Name",
            Column::Path => "Path",
            Column::FullPath => "Filename",
            Column::Extension => "Extension",
            Column::Size => "Size",
            Column::DateModified => "Date Modified",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Column::Name => "name",
            Column::Path => "path",
            Column::FullPath => "filename",
            Column::Extension => "extension",
            Column::Size => "size",
            Column::DateModified => "date_modified",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Text,
    Txt,
    Csv,
    Tsv,
    Efu,
    Json,
    M3u,
    M3u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Only {
    Files,
    Folders,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    Count,
    TotalSize,
    FolderSize(PathBuf),
}

/// The parsed switches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EsArgs {
    /// The search, one argument each (a phrase keeps its spaces).
    pub terms: Vec<String>,
    pub regex: bool,
    pub case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub diacritics: bool,
    pub prefix: bool,
    pub suffix: bool,
    pub max: Option<usize>,
    pub offset: usize,
    /// `-path`: in this folder and its subfolders.
    pub path: Option<PathBuf>,
    /// `-parent-path`: in the folder that holds this path.
    pub parent_path: Option<PathBuf>,
    /// `-parent`: directly in this folder.
    pub parent: Option<PathBuf>,
    pub sort: Option<SortKey>,
    pub ascending: Option<bool>,
    pub only: Option<Only>,
    pub columns: Vec<Column>,
    pub size_format: Option<u8>,
    pub date_format: Option<u8>,
    pub format: Format,
    pub export: Option<PathBuf>,
    pub no_header: bool,
    pub utf8_bom: bool,
    pub report: Option<Report>,
    pub timeout: Option<Duration>,
    pub no_result_error: bool,
    pub library: bool,
    pub help: bool,
}

pub const HELP: &str = "\
keel es [switches] [search]   (es.exe compatible; switches start with - or /)

Search:
  -r, -regex <search>      the search is a regular expression
  -i, -case                match case
  -w, -ww, -whole-word     match whole words
  -p, -match-path          match the full path (needs Everything)
  -a, -diacritics          match diacritics (Keel's own index always does)
  -prefix, -suffix         the name starts / ends with the (one) search word
  -path <folder>           in this folder and its subfolders
  -parent-path <path>      in the folder that holds this path
  -parent <folder>         directly in this folder
  /ad, /a-d                folders only, files only
  -library                 search the library instead of the system index
Results:
  -n, -max-results <n>     at most n results;  -o, -offset <n>  skip the first n
  -sort <name|path|size|extension|date-modified>[-ascending|-descending]
  -sort-ascending, -sort-descending;  /on /o-n /os /o-s /oe /o-e /od /o-d
Columns (in the order given; default the full path):
  -name -path-column -full-path-and-name -extension -size -date-modified
  -size-format <0 auto|1 bytes|2 KB|3 MB>
  -date-format <0 local|1 ISO-8601|2 FILETIME|3 ISO-8601 UTC>
Output:
  -csv -efu -json -m3u -m3u8 -tsv -txt, or -export-<format> <file>
  -no-header, -utf8-bom
  -count, -get-result-count, -get-total-size, -get-folder-size <folder>
  -timeout <ms>            wait this long for an index still being built (default 10000)
  -no-result-error         exit 1 when nothing is found
  -ipc1 -ipc2 -ipc3        accepted and ignored";

/// Parses `keel es` arguments (everything after `es`).
pub fn parse(args: &[String]) -> Result<EsArgs, String> {
    let mut a = EsArgs::default();
    let mut it = args.iter();
    let mut text_only = false;
    while let Some(arg) = it.next() {
        if text_only {
            a.terms.push(arg.clone());
            continue;
        }
        if arg == "--" {
            text_only = true;
            continue;
        }
        let (slash, name) = match arg.strip_prefix('/') {
            Some(name) => (true, name),
            None => match arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) {
                Some(name) => (false, name),
                None => {
                    a.terms.push(arg.clone());
                    continue;
                }
            },
        };
        let lower = name.to_ascii_lowercase();
        if name.is_empty() || (slash && dir_switch(&lower, &mut a)?) {
            if name.is_empty() {
                a.terms.push(arg.clone());
            }
            continue;
        }
        let mut value = |what: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs {what}"))
        };
        let number = |v: String| -> Result<usize, String> {
            v.parse()
                .map_err(|_| format!("{arg} needs a number, not {v:?}"))
        };
        let column = |a: &mut EsArgs, c: Column| {
            if !a.columns.contains(&c) {
                a.columns.push(c);
            }
        };
        let export = |a: &mut EsArgs, f: Format, file: String| {
            a.format = f;
            a.export = Some(PathBuf::from(file));
        };
        match lower.as_str() {
            "r" | "regex" => {
                a.regex = true;
                a.terms.push(value("a search")?);
            }
            "i" | "case" => a.case = true,
            "no-case" => a.case = false,
            "w" | "ww" | "whole-word" | "whole-words" => a.whole_word = true,
            "no-whole-word" | "no-whole-words" => a.whole_word = false,
            "p" | "match-path" => a.match_path = true,
            "no-match-path" => a.match_path = false,
            "a" | "diacritics" => a.diacritics = true,
            "no-diacritics" => a.diacritics = false,
            "prefix" => a.prefix = true,
            "no-prefix" => a.prefix = false,
            "suffix" => a.suffix = true,
            "no-suffix" => a.suffix = false,
            "n" | "max-results" => a.max = Some(number(value("a number")?)?),
            "o" | "offset" => a.offset = number(value("a number")?)?,
            "path" => a.path = Some(value("a folder")?.into()),
            "parent-path" => a.parent_path = Some(value("a path")?.into()),
            "parent" => a.parent = Some(value("a folder")?.into()),
            "sort" => {
                let (key, ascending) = sort_value(&value("a sort")?)?;
                a.sort = Some(key);
                if ascending.is_some() {
                    a.ascending = ascending;
                }
            }
            "sort-ascending" => a.ascending = Some(true),
            "sort-descending" => a.ascending = Some(false),
            "name" => column(&mut a, Column::Name),
            "path-column" => column(&mut a, Column::Path),
            "full-path-and-name" | "filename-column" => column(&mut a, Column::FullPath),
            "extension" | "ext" => column(&mut a, Column::Extension),
            "size" => column(&mut a, Column::Size),
            "date-modified" | "dm" => column(&mut a, Column::DateModified),
            "size-format" => a.size_format = Some(choice(&value("0, 1, 2 or 3")?, arg)?),
            "date-format" => a.date_format = Some(choice(&value("0, 1, 2 or 3")?, arg)?),
            "csv" => a.format = Format::Csv,
            "efu" => a.format = Format::Efu,
            "json" => a.format = Format::Json,
            "m3u" => a.format = Format::M3u,
            "m3u8" => a.format = Format::M3u8,
            "tsv" => a.format = Format::Tsv,
            "txt" => a.format = Format::Txt,
            "export-csv" => export(&mut a, Format::Csv, value("a file")?),
            "export-efu" => export(&mut a, Format::Efu, value("a file")?),
            "export-json" => export(&mut a, Format::Json, value("a file")?),
            "export-m3u" => export(&mut a, Format::M3u, value("a file")?),
            "export-m3u8" => export(&mut a, Format::M3u8, value("a file")?),
            "export-tsv" => export(&mut a, Format::Tsv, value("a file")?),
            "export-txt" => export(&mut a, Format::Txt, value("a file")?),
            "no-header" => a.no_header = true,
            "utf8-bom" => a.utf8_bom = true,
            "count" | "get-result-count" => a.report = Some(Report::Count),
            "get-total-size" => a.report = Some(Report::TotalSize),
            "get-folder-size" => a.report = Some(Report::FolderSize(value("a folder")?.into())),
            "timeout" => {
                let ms = number(value("milliseconds")?)?;
                a.timeout = Some(Duration::from_millis(ms as u64));
            }
            "no-result-error" => a.no_result_error = true,
            "ipc1" | "ipc2" | "ipc3" => {}
            "library" => a.library = true,
            "h" | "help" | "?" => a.help = true,
            // `/home/me` and other Unix paths are searched for.
            _ if slash => a.terms.push(arg.clone()),
            _ => return Err(format!("unknown switch {arg} (keel es -help lists them)")),
        }
    }
    a.check()?;
    Ok(a)
}

/// DIR-style `/a…` and `/o…`; false when `name` is not one.
fn dir_switch(name: &str, a: &mut EsArgs) -> Result<bool, String> {
    let mut chars = name.chars();
    let head = chars.next().map(String::from).unwrap_or_default();
    let rest = chars.as_str();
    let letter = rest.strip_prefix('-').unwrap_or(rest);
    let negated = rest.starts_with('-');
    if letter.chars().count() > 1 {
        return Ok(false);
    }
    match head.as_str() {
        "a" => match letter {
            "" => {}
            "d" => a.only = Some(if negated { Only::Files } else { Only::Folders }),
            l if "rhsavntplcoie".contains(l) => {
                return Err(format!(
                    "/a{rest}: only /ad and /a-d are supported (no attribute index)"
                ))
            }
            _ => return Ok(false),
        },
        "o" => {
            let key = match letter {
                "n" => SortKey::Name,
                "s" => SortKey::Size,
                "e" => SortKey::Extension,
                "d" => SortKey::DateModified,
                "" => return Ok(false),
                l => {
                    return Err(format!(
                        "/o{l}: sort by n (name), s (size), e (extension) or d (date modified)"
                    ))
                }
            };
            a.sort = Some(key);
            a.ascending = Some(!negated);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn sort_value(v: &str) -> Result<(SortKey, Option<bool>), String> {
    let v = v.to_ascii_lowercase().replace('_', "-");
    let (key, ascending) = if let Some(k) = v.strip_suffix("-ascending") {
        (k, Some(true))
    } else if let Some(k) = v.strip_suffix("-descending") {
        (k, Some(false))
    } else {
        (v.as_str(), None)
    };
    let key = match key {
        "name" => SortKey::Name,
        "path" | "full-path" => SortKey::Path,
        "size" => SortKey::Size,
        "extension" | "ext" => SortKey::Extension,
        "date-modified" | "dm" => SortKey::DateModified,
        other => {
            return Err(format!(
                "sort by {other:?} is not supported (name, path, size, extension, date-modified)"
            ))
        }
    };
    Ok((key, ascending))
}

fn choice(v: &str, arg: &str) -> Result<u8, String> {
    match v.parse::<u8>() {
        Ok(n) if n <= 3 => Ok(n),
        _ => Err(format!("{arg} takes 0, 1, 2 or 3, not {v:?}")),
    }
}

impl EsArgs {
    fn check(&self) -> Result<(), String> {
        if self.regex && (self.prefix || self.suffix || self.whole_word) {
            return Err("-regex cannot be combined with -prefix, -suffix or -whole-word".into());
        }
        if self.library && (self.regex || self.case || self.match_path) {
            return Err(
                "the library search has no -regex, -case or -match-path (its words match \
                 names and document text)"
                    .into(),
            );
        }
        if (self.prefix || self.suffix) && literal_terms(&self.text()).len() != 1 {
            return Err("-prefix and -suffix take one search word".into());
        }
        Ok(())
    }

    /// The search as one string: a term with spaces becomes a phrase.
    pub fn text(&self) -> String {
        let quote = |t: &String| {
            if !self.regex && t.chars().any(char::is_whitespace) && !t.contains('"') {
                format!("\"{t}\"")
            } else {
                t.clone()
            }
        };
        self.terms.iter().map(quote).collect::<Vec<_>>().join(" ")
    }

    /// The folder the backend can narrow the search to (`in:`), absolute.
    fn scope(&self) -> Option<PathBuf> {
        let parent_path = self
            .parent_path
            .as_deref()
            .and_then(|p| absolute(p).parent().map(Path::to_owned));
        (self.parent.as_deref().map(absolute))
            .or(self.path.as_deref().map(absolute))
            .or(parent_path)
    }

    /// The text for the system index: `path:` per term with `-p` (Everything only), and the
    /// folder as `in:` (Keel's indexes and Everything read it).
    fn backend_text(&self, everything: bool) -> Result<String, String> {
        let mut text = self.text();
        if self.match_path {
            if !everything {
                return Err(
                    "-p (match path) needs Everything running: Keel's own index \
                            matches names only (-path <folder> searches under a folder)"
                        .into(),
                );
            }
            if self.regex {
                return Err("-p cannot be combined with -regex".into());
            }
            text = split_terms(&text, true)
                .iter()
                .map(|t| match t.strip_prefix('!') {
                    Some(rest) => format!("!path:{rest}"),
                    None => format!("path:{t}"),
                })
                .collect::<Vec<_>>()
                .join(" ");
        }
        if let (false, Some(scope)) = (self.regex, self.scope()) {
            text = format!("in:\"{}\" {text}", scope.display());
        }
        Ok(text.trim().to_owned())
    }

    /// Filters on each result's name (its full path with `-p`) the backend may not apply:
    /// whole words, prefix, suffix, and diacritics (Everything ignores them by default).
    fn name_filters(&self) -> Result<Vec<Regex>, String> {
        if self.regex {
            return Ok(Vec::new());
        }
        let words = literal_terms(&self.text());
        let build = |src: String| {
            RegexBuilder::new(&src)
                .case_insensitive(!self.case)
                .build()
                .map_err(|e| format!("{e}"))
        };
        let mut out = Vec::new();
        for w in &words {
            let w = regex::escape(w);
            if self.whole_word {
                out.push(build(format!(
                    r"(?:^|[^\p{{Alphabetic}}\p{{N}}]){w}(?:$|[^\p{{Alphabetic}}\p{{N}}])"
                ))?);
            }
            if self.prefix {
                out.push(build(format!("^{w}"))?);
            }
            if self.suffix {
                out.push(build(format!("{w}$"))?);
            }
            if self.diacritics {
                out.push(build(w)?);
            }
        }
        Ok(out)
    }

    fn sort_order(&self) -> (SortKey, bool) {
        (
            self.sort.unwrap_or(SortKey::Name),
            self.ascending.unwrap_or(true),
        )
    }

    /// Size and date are needed for every result (not just the page shown).
    fn meta_for_all(&self) -> bool {
        matches!(self.sort_order().0, SortKey::Size | SortKey::DateModified)
            || self.report == Some(Report::TotalSize)
    }

    fn columns(&self) -> Vec<Column> {
        if self.columns.is_empty() {
            vec![Column::FullPath]
        } else {
            self.columns.clone()
        }
    }

    /// The page needs sizes or dates.
    fn meta_for_page(&self) -> bool {
        self.format == Format::Efu
            || (self.columns())
                .iter()
                .any(|c| matches!(c, Column::Size | Column::DateModified))
    }
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_owned())
}

/// Space-separated terms (a `"quoted phrase"` is one); `keep_quotes` leaves the quotes in.
fn split_terms(text: &str, keep_quotes: bool) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                if keep_quotes {
                    cur.push(c);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    terms.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    terms
}

/// The plain words of a search: no wildcards, functions (`ext:`), operators or NOTs.
fn literal_terms(text: &str) -> Vec<String> {
    split_terms(text, false)
        .into_iter()
        .filter(|t| !t.starts_with('!') && !t.contains(['*', '?', ':', '|', '<', '>']))
        .collect()
}

/// One result.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    /// Files only; None while unknown.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Size and date were read (else [`fill`] reads them).
    pub meta: bool,
}

impl Row {
    fn from_hit(hit: keel_search::Hit) -> Self {
        let meta = hit.modified.is_some();
        let path = (hit.path.to_local_path())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| hit.path.display());
        Row {
            name: hit.path.name().to_owned(),
            path,
            is_dir: hit.is_dir,
            size: (meta && !hit.is_dir).then_some(hit.size),
            modified: hit.modified,
            meta,
        }
    }

    fn from_library(hit: keel_api::types::Hit) -> Self {
        Row {
            modified: hit
                .modified
                .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s.max(0) as u64))),
            size: (!hit.is_dir).then_some(hit.size),
            path: hit.path,
            name: hit.name,
            is_dir: hit.is_dir,
            meta: true,
        }
    }

    fn extension(&self) -> String {
        if self.is_dir {
            return String::new();
        }
        Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn folder(&self) -> String {
        Path::new(&self.path)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }
}

/// Reads size and date for the rows that lack them.
fn fill(rows: &mut [Row]) {
    let todo: Vec<usize> = (0..rows.len()).filter(|&i| !rows[i].meta).collect();
    if todo.is_empty() {
        return;
    }
    let mut hits: Vec<keel_search::Hit> = (todo.iter())
        .map(|&i| keel_search::Hit {
            path: keel_vfs::VPath::local(&rows[i].path),
            is_dir: rows[i].is_dir,
            size: 0,
            modified: None,
            snippet: None,
        })
        .collect();
    keel_search::fill_meta(&mut hits);
    for (&i, hit) in todo.iter().zip(hits) {
        let row = &mut rows[i];
        row.meta = true;
        row.modified = hit.modified;
        row.size = (!row.is_dir && hit.modified.is_some()).then_some(hit.size);
    }
}

/// Paths compared as the platform does: `/` for `\`, no trailing separator, and without
/// case on Windows and macOS.
fn norm(p: &str) -> String {
    let p = p.replace('\\', "/");
    let p = p.trim_end_matches('/');
    if cfg!(any(windows, target_os = "macos")) {
        p.to_lowercase()
    } else {
        p.to_owned()
    }
}

/// `path` is inside `folder` (below it, not the folder itself).
fn under(path: &str, folder: &str) -> bool {
    let (p, f) = (norm(path), norm(folder));
    p.len() > f.len() && p.starts_with(&f) && p[f.len()..].starts_with('/')
}

/// `path` is directly in `folder`.
fn child_of(path: &str, folder: &str) -> bool {
    let (p, f) = (norm(path), norm(folder));
    p.rfind('/').is_some_and(|i| p[..i] == f)
}

/// Keeps the rows the switches ask for (in place).
pub fn filter(rows: &mut Vec<Row>, a: &EsArgs) -> Result<(), String> {
    let names = a.name_filters()?;
    let path = a.path.as_deref().map(|p| absolute(p).display().to_string());
    let parent_path = (a.parent_path.as_deref())
        .and_then(|p| absolute(p).parent().map(|p| p.display().to_string()));
    let parent = a
        .parent
        .as_deref()
        .map(|p| absolute(p).display().to_string());
    rows.retain(|r| {
        let target = if a.match_path { &r.path } else { &r.name };
        match a.only {
            Some(Only::Files) if r.is_dir => return false,
            Some(Only::Folders) if !r.is_dir => return false,
            _ => {}
        }
        path.as_deref().is_none_or(|f| under(&r.path, f))
            && parent_path.as_deref().is_none_or(|f| under(&r.path, f))
            && parent.as_deref().is_none_or(|f| child_of(&r.path, f))
            && names.iter().all(|re| re.is_match(target))
    });
    Ok(())
}

pub fn sort(rows: &mut [Row], key: SortKey, ascending: bool) {
    let lower = |s: &str| s.to_lowercase();
    match key {
        SortKey::Name => rows.sort_by_cached_key(|r| (lower(&r.name), lower(&r.path))),
        SortKey::Path => rows.sort_by_cached_key(|r| lower(&r.path)),
        SortKey::Extension => {
            rows.sort_by_cached_key(|r| (lower(&r.extension()), lower(&r.name), lower(&r.path)))
        }
        SortKey::Size => rows.sort_by_cached_key(|r| (r.size, lower(&r.name))),
        SortKey::DateModified => rows.sort_by_cached_key(|r| (r.modified, lower(&r.name))),
    }
    if !ascending {
        rows.reverse();
    }
}

/// Size as `-size-format` says (0 auto, 1 bytes, 2 KB, 3 MB).
fn size_text(size: Option<u64>, format: u8) -> String {
    let Some(n) = size else {
        return String::new();
    };
    match format {
        0 => humansize::format_size(n, humansize::WINDOWS),
        2 => format!("{} KB", n.div_ceil(1 << 10)),
        3 => format!("{} MB", n.div_ceil(1 << 20)),
        _ => n.to_string(),
    }
}

fn filetime(t: SystemTime) -> u64 {
    let ticks = |d: Duration| (d.as_nanos() / 100).min(u128::from(u64::MAX)) as u64;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => FILETIME_UNIX.saturating_add(ticks(d)),
        Err(e) => FILETIME_UNIX.saturating_sub(ticks(e.duration())),
    }
}

/// Date as `-date-format` says (0 local, 1 ISO-8601 local, 2 FILETIME, 3 ISO-8601 UTC).
fn date_text(t: Option<SystemTime>, format: u8) -> String {
    let Some(t) = t else {
        return String::new();
    };
    match format {
        1 => chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string(),
        2 => filetime(t).to_string(),
        3 => chrono::DateTime::<chrono::Utc>::from(t)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        _ => chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
    }
}

/// A cell: its text and whether it is a number (unquoted in CSV, a number in JSON).
/// Exports write raw bytes and FILETIMEs unless a format was asked for.
fn cell(row: &Row, col: Column, a: &EsArgs, export: bool) -> (String, bool) {
    match col {
        Column::Name => (row.name.clone(), false),
        Column::Path => (row.folder(), false),
        Column::FullPath => (row.path.clone(), false),
        Column::Extension => (row.extension(), false),
        Column::Size => match a.size_format {
            Some(f) => (size_text(row.size, f), f == 1),
            None => (size_text(row.size, 1), true),
        },
        Column::DateModified => match (a.date_format, export) {
            (Some(f), _) => (date_text(row.modified, f), f == 2),
            (None, true) => (date_text(row.modified, 2), true),
            (None, false) => (date_text(row.modified, 0), false),
        },
    }
}

fn csv_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Writes `rows` in the format asked for.
pub fn write_rows(rows: &[Row], a: &EsArgs, out: &mut dyn Write) -> std::io::Result<()> {
    if a.utf8_bom {
        out.write_all("\u{feff}".as_bytes())?;
    }
    let cols = a.columns();
    let header = !a.no_header;
    match a.format {
        Format::Text | Format::Txt => {
            if a.columns.is_empty() {
                for r in rows {
                    writeln!(out, "{}", r.path)?;
                }
                return Ok(());
            }
            let cells: Vec<Vec<(String, bool)>> = (rows.iter())
                .map(|r| cols.iter().map(|&c| cell(r, c, a, false)).collect())
                .collect();
            let widths: Vec<usize> = (0..cols.len())
                .map(|i| {
                    (cells.iter())
                        .map(|row| row[i].0.chars().count())
                        .max()
                        .unwrap_or(0)
                })
                .collect();
            for row in &cells {
                let mut line = String::new();
                for (i, (text, number)) in row.iter().enumerate() {
                    if i > 0 {
                        line.push_str("  ");
                    }
                    let pad = " ".repeat(widths[i] - text.chars().count());
                    match (cols[i], *number) {
                        (Column::Size, _) | (_, true) => {
                            line.push_str(&pad);
                            line.push_str(text);
                        }
                        _ => {
                            line.push_str(text);
                            line.push_str(&pad);
                        }
                    }
                }
                writeln!(out, "{}", line.trim_end())?;
            }
        }
        Format::Csv | Format::Tsv => {
            let tsv = a.format == Format::Tsv;
            let sep = if tsv { "\t" } else { "," };
            let clean = |s: &str| s.replace(['\t', '\r', '\n'], " ");
            if header {
                let names: Vec<String> = (cols.iter())
                    .map(|c| match tsv {
                        true => c.header().to_owned(),
                        false => csv_quote(c.header()),
                    })
                    .collect();
                writeln!(out, "{}", names.join(sep))?;
            }
            for r in rows {
                let line: Vec<String> = (cols.iter())
                    .map(|&c| {
                        let (text, number) = cell(r, c, a, true);
                        match (tsv, number || text.is_empty()) {
                            (true, _) => clean(&text),
                            (false, true) => text,
                            (false, false) => csv_quote(&text),
                        }
                    })
                    .collect();
                writeln!(out, "{}", line.join(sep))?;
            }
        }
        Format::Efu => {
            if header {
                writeln!(out, "Filename,Size,Date Modified,Attributes")?;
            }
            for r in rows {
                let size = r.size.map(|s| s.to_string()).unwrap_or_default();
                let date = r.modified.map(|t| filetime(t).to_string());
                writeln!(
                    out,
                    "{},{size},{},{}",
                    csv_quote(&r.path),
                    date.unwrap_or_default(),
                    if r.is_dir { 16 } else { 0 }
                )?;
            }
        }
        Format::Json => {
            writeln!(out, "[")?;
            for (i, r) in rows.iter().enumerate() {
                let mut obj = serde_json::Map::new();
                for &c in &cols {
                    let (text, number) = cell(r, c, a, true);
                    let value = match (number, text.parse::<u64>()) {
                        (true, Ok(n)) => serde_json::Value::from(n),
                        (true, Err(_)) => serde_json::Value::Null,
                        (false, _) => serde_json::Value::from(text),
                    };
                    obj.insert(c.key().to_owned(), value);
                }
                let comma = if i + 1 < rows.len() { "," } else { "" };
                writeln!(out, "{}{comma}", serde_json::Value::Object(obj))?;
            }
            writeln!(out, "]")?;
        }
        Format::M3u | Format::M3u8 => {
            if header {
                writeln!(out, "#EXTM3U")?;
            }
            for r in rows.iter().filter(|r| !r.is_dir) {
                writeln!(out, "{}", r.path)?;
            }
        }
    }
    Ok(())
}

/// Why `keel es` stopped.
#[derive(Debug, PartialEq)]
pub enum Fail {
    Usage(String),
    Failed(String),
}

/// Where the results come from.
pub enum Source<'a> {
    System(&'a dyn Searcher),
    Library(&'a mut dyn keel_api::Backend),
}

/// Waits (up to `timeout`) for an index that is still being built.
fn wait_ready(s: &dyn Searcher, timeout: Duration) -> Result<(), Fail> {
    let deadline = Instant::now() + timeout;
    loop {
        match s.state() {
            SearchState::Indexing { done } if Instant::now() >= deadline => {
                return Err(Fail::Failed(format!(
                    "the search index is still being built ({done} entries so far); try again \
                     later or wait longer with -timeout <ms>"
                )))
            }
            SearchState::Indexing { .. } => std::thread::sleep(Duration::from_millis(100)),
            SearchState::Ready | SearchState::Unavailable => return Ok(()),
        }
    }
}

/// The matching rows, filtered and sorted (no page applied).
pub fn search(a: &EsArgs, source: Source) -> Result<Vec<Row>, Fail> {
    let mut rows: Vec<Row> = match source {
        Source::System(s) => {
            wait_ready(s, a.timeout.unwrap_or(DEFAULT_TIMEOUT))?;
            let text = a
                .backend_text(s.name() == "Everything")
                .map_err(Fail::Usage)?;
            let hits = s
                .query(&keel_search::Query {
                    text,
                    folders_only: a.only == Some(Only::Folders),
                    max: FETCH_CAP,
                    regex: a.regex,
                    match_case: a.case,
                    meta: false,
                })
                .map_err(|e| Fail::Failed(format!("{e:#}")))?;
            if hits.len() >= FETCH_CAP as usize {
                eprintln!(
                    "keel es: only the first {FETCH_CAP} matches were read; narrow the search \
                     for complete sorting and counts"
                );
            }
            hits.into_iter().map(Row::from_hit).collect()
        }
        Source::Library(b) => {
            let hits = b
                .call(
                    "search",
                    serde_json::json!({"query": a.text(), "max": LIBRARY_CAP}),
                )
                .map_err(|e| Fail::Failed(e.message))?;
            let hits: Vec<keel_api::types::Hit> = serde_json::from_value(hits)
                .map_err(|e| Fail::Failed(format!("unexpected answer: {e}")))?;
            hits.into_iter().map(Row::from_library).collect()
        }
    };
    filter(&mut rows, a).map_err(Fail::Usage)?;
    if a.meta_for_all() {
        fill(&mut rows);
    }
    let (key, ascending) = a.sort_order();
    sort(&mut rows, key, ascending);
    Ok(rows)
}

/// Prints what `a` asks for from `rows` (all matches, sorted): a report, or the page in its
/// format. Returns how many results there were.
pub fn output(mut rows: Vec<Row>, a: &EsArgs, out: &mut dyn Write) -> Result<usize, Fail> {
    let total = rows.len();
    let io = |e: std::io::Error| Fail::Failed(format!("writing the results: {e}"));
    match &a.report {
        Some(Report::Count) => {
            writeln!(out, "{total}").map_err(io)?;
            return Ok(total);
        }
        Some(Report::TotalSize) => {
            let bytes: u64 = rows.iter().filter_map(|r| r.size).sum();
            let format = a.size_format.unwrap_or(1);
            writeln!(out, "{}", size_text(Some(bytes), format)).map_err(io)?;
            return Ok(total);
        }
        _ => {}
    }
    let start = a.offset.min(total);
    let end = a.max.map_or(total, |n| start.saturating_add(n).min(total));
    let mut page: Vec<Row> = rows.drain(start..end).collect();
    if a.meta_for_page() {
        fill(&mut page);
    }
    match &a.export {
        Some(file) => {
            let f = std::fs::File::create(file)
                .map_err(|e| Fail::Failed(format!("{}: {e}", file.display())))?;
            let mut w = std::io::BufWriter::new(f);
            write_rows(&page, a, &mut w)
                .and_then(|()| w.flush())
                .map_err(io)?;
        }
        None => write_rows(&page, a, out).map_err(io)?,
    }
    Ok(total)
}

/// `-get-folder-size`: the folder's total, walked.
fn folder_size(dir: &Path, a: &EsArgs, out: &mut dyn Write) -> Result<(), Fail> {
    let dir = keel_vfs::VPath::local(absolute(dir));
    let never = std::sync::atomic::AtomicBool::new(false);
    let walked = keel_vfs::sizes::folder_total(
        &keel_vfs::Router::new(),
        &dir,
        None,
        &never,
        &mut |_, _, _| {},
    )
    .map_err(|e| Fail::Failed(format!("{}: {e:#}", dir.display())))?;
    let text = size_text(Some(walked.bytes), a.size_format.unwrap_or(1));
    writeln!(out, "{text}").map_err(|e| Fail::Failed(e.to_string()))
}

/// `keel es ARGS`: exit code. `json` is the global `--json` (the same as `-json`).
pub fn run(args: &[String], profile: &str, json: bool) -> i32 {
    let mut a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("keel es: {e}");
            return USAGE;
        }
    };
    if json && a.format == Format::Text {
        a.format = Format::Json;
    }
    let mut out = std::io::stdout().lock();
    if a.help {
        let _ = writeln!(out, "{HELP}");
        return OK;
    }
    let result = match &a.report {
        Some(Report::FolderSize(dir)) => folder_size(dir, &a, &mut out).map(|()| 1),
        _ if a.library => library_rows(&a, profile).and_then(|rows| output(rows, &a, &mut out)),
        _ => {
            let searcher = keel_search::default_searcher();
            search(&a, Source::System(searcher.as_ref()))
                .and_then(|rows| output(rows, &a, &mut out))
        }
    };
    match result {
        Ok(0) if a.no_result_error => FAILED,
        Ok(_) => OK,
        Err(Fail::Usage(e)) => {
            eprintln!("keel es: {e}");
            USAGE
        }
        Err(Fail::Failed(e)) => {
            eprintln!("keel es: {e}");
            FAILED
        }
    }
}

fn library_rows(a: &EsArgs, profile: &str) -> Result<Vec<Row>, Fail> {
    let cfg =
        keel_api::config::HostConfig::load(profile).map_err(|e| Fail::Failed(format!("{e:#}")))?;
    let mut backend = crate::commands::connect(&cfg, keel_api::client::REQUEST_WAIT)
        .map_err(|e| Fail::Failed(format!("{e:#}")))?;
    search(a, Source::Library(&mut *backend))
}

#[cfg(test)]
#[path = "es_tests.rs"]
mod tests;
