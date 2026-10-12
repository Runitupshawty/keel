//! `system.search`: the platform's file search (`keel_search::default_searcher`, the
//! backend of the window's search pane: Everything or Keel's own index on Windows,
//! Spotlight or locate elsewhere) with Everything's match options, sort and paging.
//! keel-daemon's Everything-compatible HTTP endpoint runs the same [`run`].
//!
//! The backend matches names; the options it does not know are applied here, to the hits
//! it returns: the backend is asked for a superset (the same words, or for
//! `ignore_punctuation` / `ignore_whitespace` a pattern of the longest word) and each hit
//! is checked against every plain word with the options. At most [`MAX_SCAN`] hits are
//! read from the backend per search: the total and the order cover those.

use crate::error::{ApiError, Result};
use crate::types::{SystemSearchParams, SystemSort};
pub use keel_search::{Hit, Query, Searcher};
use parking_lot::Mutex;
use regex::{Regex, RegexBuilder};
use std::sync::Arc;

/// Results a search returns when it does not say (`count`).
pub const DEFAULT_COUNT: usize = 32;
/// Most results one search returns.
pub const MAX_COUNT: usize = 1000;
/// Most hits read from the backend per search.
pub const MAX_SCAN: usize = 10_000;
/// Longest search text (bytes).
pub const MAX_QUERY: usize = 4096;
/// Size limit of a compiled regular expression.
const REGEX_LIMIT: usize = 1 << 20;

/// The backend `system.search` uses, made on first use (it probes Everything or loads an
/// index: seconds at worst, and only once).
#[derive(Default)]
pub struct SystemIndex(Mutex<Option<Arc<dyn Searcher>>>);

impl SystemIndex {
    /// The backend, made now if it is the first call. Blocks: worker threads only.
    pub fn get(&self) -> Arc<dyn Searcher> {
        let mut slot = self.0.lock();
        slot.get_or_insert_with(|| Arc::from(keel_search::default_searcher()))
            .clone()
    }

    /// Uses `searcher` from now on (tests, or a host with its own).
    pub fn set(&self, searcher: Arc<dyn Searcher>) {
        *self.0.lock() = Some(searcher);
    }
}

/// What [`run`] found.
pub struct Found {
    pub backend: String,
    /// Hits matching, of the ones read.
    pub total: usize,
    /// The backend returned `scan` hits: there may be more.
    pub capped: bool,
    pub offset: usize,
    /// The page, with size and date filled in.
    pub hits: Vec<Hit>,
}

/// Searches `searcher` with `p`, reading at most `scan` hits from it. Blocks on the
/// backend and the disk: worker threads only.
pub fn run(searcher: &dyn Searcher, p: &SystemSearchParams, scan: usize) -> Result<Found> {
    let plan = compile(p, searcher.name() == "Everything")?;
    let count = p.count.unwrap_or(DEFAULT_COUNT).min(MAX_COUNT);
    let offset = p.offset.unwrap_or(0);
    let query = Query {
        max: u32::try_from(scan).unwrap_or(u32::MAX),
        meta: false,
        ..plan.backend
    };
    if !searcher.available() {
        searcher.probe();
    }
    let hits = searcher
        .query(&query)
        .map_err(|e| ApiError::failed(format!("{}: {e:#}", searcher.name())))?;
    let capped = hits.len() >= scan;
    let mut hits: Vec<Hit> = hits
        .into_iter()
        .filter(|h| plan.filter.matches(h))
        .collect();
    let sort = p.sort.unwrap_or_default();
    if matches!(sort, SystemSort::DateModified | SystemSort::Size) {
        fill(&mut hits);
    }
    sort_hits(&mut hits, sort, p.ascending.unwrap_or(true));
    let total = hits.len();
    let mut page: Vec<Hit> = hits.into_iter().skip(offset).take(count).collect();
    fill(&mut page);
    Ok(Found {
        backend: searcher.name().to_owned(),
        total,
        capped,
        offset,
        hits: page,
    })
}

/// Size and date for the hits the backend gave without them.
fn fill(hits: &mut [Hit]) {
    let mut missing: Vec<usize> = (0..hits.len())
        .filter(|&i| hits[i].modified.is_none())
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut part: Vec<Hit> = missing.iter().map(|&i| hits[i].clone()).collect();
    keel_search::fill_meta(&mut part);
    for (i, h) in missing.drain(..).zip(part) {
        hits[i] = h;
    }
}

fn sort_hits(hits: &mut [Hit], sort: SystemSort, ascending: bool) {
    let key = |h: &Hit| h.path.name().to_lowercase();
    match sort {
        SystemSort::Name => hits.sort_by_cached_key(|h| (key(h), h.path.display())),
        SystemSort::Path => hits.sort_by_cached_key(|h| {
            let parent = h.path.parent().map(|p| p.display()).unwrap_or_default();
            (parent.to_lowercase(), key(h))
        }),
        SystemSort::DateModified => hits.sort_by_cached_key(|h| (h.modified, key(h))),
        SystemSort::Size => hits.sort_by_cached_key(|h| (h.size, key(h))),
    }
    if !ascending {
        hits.reverse();
    }
}

/// The backend's query and the check of its hits.
pub(crate) struct Plan {
    pub(crate) backend: Query,
    pub(crate) filter: Filter,
}

/// How names and words are compared.
#[derive(Clone, Copy, Debug, Default)]
struct Norm {
    case: bool,
    diacritics: bool,
    punctuation: bool,
    whitespace: bool,
}

impl Norm {
    fn apply(self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if (self.whitespace && c.is_whitespace()) || (self.punctuation && punctuation(c)) {
                continue;
            }
            let c = if self.diacritics { c } else { fold(c) };
            if self.case {
                out.push(c);
            } else {
                out.extend(c.to_lowercase());
            }
        }
        out
    }
}

/// One word of the search, as the filter checks it.
#[derive(Debug)]
enum Term {
    /// Plain text (normalized).
    Plain { text: String, path: bool },
    /// A wildcard pattern over the whole name (or path).
    Glob { re: Regex, path: bool },
}

/// Checks a backend hit against every plain word with the options.
pub(crate) struct Filter {
    norm: Norm,
    terms: Vec<Term>,
    regex: Option<(Regex, bool)>,
    start: bool,
    end: bool,
}

impl Filter {
    pub(crate) fn matches(&self, hit: &Hit) -> bool {
        let name = hit.path.name();
        let full = hit.path.display();
        if let Some((re, path)) = &self.regex {
            return re.is_match(if *path { &full } else { name });
        }
        let (mut norm_name, mut norm_full) = (None, None);
        self.terms.iter().all(|term| {
            let on_path = matches!(
                term,
                Term::Plain { path: true, .. } | Term::Glob { path: true, .. }
            );
            let subject: &str = if on_path {
                norm_full.get_or_insert_with(|| self.norm.apply(&full))
            } else {
                norm_name.get_or_insert_with(|| self.norm.apply(name))
            };
            match term {
                Term::Plain { text, .. } => find(subject, text, self.start, self.end),
                Term::Glob { re, .. } => re.is_match(subject),
            }
        })
    }
}

/// `needle` occurs in `hay`, at the start of a word when `start`, at the end of one when
/// `end` (every occurrence is tried, overlapping ones too).
fn find(hay: &str, needle: &str, start: bool, end: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if !start && !end {
        return hay.contains(needle);
    }
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    hay.char_indices().any(|(i, _)| {
        hay[i..].starts_with(needle)
            && (!start || !word(hay[..i].chars().next_back()))
            && (!end || !word(hay[i + needle.len()..].chars().next()))
    })
}

/// Punctuation and symbols `ignore_punctuation` skips: ASCII's and the common typographic
/// ones.
fn punctuation(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(
            c,
            '\u{a1}'..='\u{bf}' | '\u{2010}'..='\u{2027}' | '\u{2030}'..='\u{205e}' | '\u{3000}'..='\u{303f}' | '\u{d7}' | '\u{f7}'
        )
}

/// A Latin letter without its accent (Latin-1 and Latin Extended-A); others as they are.
fn fold(c: char) -> char {
    const FROM: &str = "ÀÁÂÃÄÅàáâãäåĀāĂăĄąÇçĆćĈĉĊċČčĎďĐđÈÉÊËèéêëĒēĔĕĖėĘęĚěĜĝĞğĠġĢģĤĥĦħÌÍÎÏìíîïĨĩĪīĬĭĮįİıĴĵĶķĹĺĻļĽľĿŀŁłÑñŃńŅņŇňÒÓÔÕÖØòóôõöøŌōŎŏŐőŔŕŖŗŘřŚśŜŝŞşŠšŢţŤťŦŧÙÚÛÜùúûüŨũŪūŬŭŮůŰűŲųŴŵÝýÿŶŷŸŹźŻżŽž";
    const TO: &str = "AAAAAAaaaaaaAaAaAaCcCcCcCcCcDdDdEEEEeeeeEeEeEeEeEeGgGgGgGgHhHhIIIIiiiiIiIiIiIiIiJjKkLlLlLlLlLlNnNnNnNnOOOOOOooooooOoOoOoRrRrRrSsSsSsSsTtTtTtUUUUuuuuUuUuUuUuUuUuWwYyyYyYZzZzZz";
    if c.is_ascii() {
        return c;
    }
    FROM.chars()
        .position(|f| f == c)
        .and_then(|i| TO.chars().nth(i))
        .unwrap_or(c)
}

/// The search split into words: spaces separate them, double quotes group. Each word:
/// its text as typed (quotes kept) and without the quotes.
fn words(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (mut raw, mut plain, mut quoted) = (String::new(), String::new(), false);
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                raw.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !raw.is_empty() {
                    out.push((std::mem::take(&mut raw), std::mem::take(&mut plain)));
                }
            }
            c => {
                raw.push(c);
                plain.push(c);
            }
        }
    }
    if !raw.is_empty() {
        out.push((raw, plain));
    }
    out
}

/// Syntax the filter leaves to the backend: functions and modifiers (`ext:pdf`), `!`
/// (not), `|` (or) and `<` `>` (grouping).
fn backend_syntax(raw: &str) -> bool {
    let unquoted_colon = {
        let mut quoted = false;
        raw.chars().any(|c| {
            if c == '"' {
                quoted = !quoted;
            }
            c == ':' && !quoted
        })
    };
    // `C:\x` is a path, not a function.
    let drive = raw.len() >= 2
        && raw.as_bytes()[1] == b':'
        && raw.as_bytes()[0].is_ascii_alphabetic()
        && raw
            .as_bytes()
            .get(2)
            .is_none_or(|b| *b == b'\\' || *b == b'/');
    (unquoted_colon && !drive) || raw.starts_with('!') || raw.contains(['|', '<', '>'])
}

fn separator(s: &str) -> bool {
    s.contains(['\\', '/'])
}

/// `*` and `?` over the whole (normalized) subject.
fn glob(pattern: &str, norm: Norm) -> Result<Regex> {
    let mut re = String::from("^");
    let mut literal = String::new();
    let flush = |re: &mut String, literal: &mut String| {
        re.push_str(&regex::escape(&norm.apply(literal)));
        literal.clear();
    };
    for c in pattern.chars() {
        match c {
            '*' | '?' => {
                flush(&mut re, &mut literal);
                re.push_str(if c == '*' { "(?s:.*)" } else { "(?s:.)" });
            }
            c => literal.push(c),
        }
    }
    flush(&mut re, &mut literal);
    re.push('$');
    build(&re, true)
}

fn build(pattern: &str, case: bool) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case)
        .size_limit(REGEX_LIMIT)
        .dfa_size_limit(REGEX_LIMIT)
        .build()
        .map_err(|e| ApiError::invalid_params(format!("bad regular expression: {e}")))
}

/// The backend query and the filter for `p`; `everything`: the backend is Everything
/// (which matches paths with its own `path:`).
pub(crate) fn compile(p: &SystemSearchParams, everything: bool) -> Result<Plan> {
    if p.query.len() > MAX_QUERY {
        return Err(ApiError::invalid_params(format!(
            "the search is longer than {MAX_QUERY} bytes"
        )));
    }
    let norm = Norm {
        case: p.match_case,
        diacritics: p.match_diacritics,
        punctuation: p.ignore_punctuation,
        whitespace: p.ignore_whitespace,
    };
    let mut backend = Query {
        text: String::new(),
        regex: false,
        match_case: p.match_case,
        ..Query::default()
    };
    if p.regex {
        let re = build(&p.query, p.match_case)?;
        // The backends match names only, so a regex meant for the whole path would find
        // almost nothing there: they send every name (bounded by the scan, which `capped`
        // reports) and the filter applies the regex to the full path.
        if !p.match_path {
            backend.text = p.query.clone();
            backend.regex = true;
        }
        return Ok(Plan {
            backend,
            filter: Filter {
                norm,
                terms: Vec::new(),
                regex: Some((re, p.match_path)),
                start: false,
                end: false,
            },
        });
    }
    let squeeze = p.ignore_punctuation || p.ignore_whitespace;
    let mut terms = Vec::new();
    let mut passed = Vec::new();
    let mut longest: Option<(String, bool)> = None;
    for (raw, plain) in words(&p.query) {
        if backend_syntax(&raw) {
            if squeeze {
                return Err(ApiError::invalid_params(
                    "ignore_punctuation and ignore_whitespace take plain words only",
                ));
            }
            passed.push(raw);
            continue;
        }
        let path = p.match_path || separator(&plain);
        let wild = plain.contains(['*', '?']);
        // Windows paths read with `\`; a `/` typed in a word means the same.
        let checked = if path && cfg!(windows) {
            plain.replace('/', "\\")
        } else {
            plain.clone()
        };
        terms.push(if wild {
            Term::Glob {
                re: glob(&checked, norm)?,
                path,
            }
        } else {
            Term::Plain {
                text: norm.apply(&checked),
                path,
            }
        });
        // What the backend gets: the backend matches names, so for a path word the part
        // after its last separator (Everything: the word with `path:`).
        let name_part = |s: &str| s.rsplit(['\\', '/']).next().unwrap_or("").to_owned();
        if squeeze {
            let part = if path {
                name_part(&plain)
            } else {
                plain.clone()
            };
            if !wild && longest.as_ref().is_none_or(|(l, _)| part.len() > l.len()) {
                longest = Some((part, path));
            }
        } else if path && everything {
            passed.push(format!("path:{raw}"));
        } else if path && separator(&plain) {
            let part = name_part(&plain);
            if !part.is_empty() {
                passed.push(if part.contains(char::is_whitespace) {
                    format!("\"{part}\"")
                } else {
                    part
                });
            }
        } else {
            passed.push(raw);
        }
    }
    if squeeze {
        // Each character of the longest word, with skipped characters allowed between.
        if let Some((word, _)) = longest {
            let gap = match (p.ignore_punctuation, p.ignore_whitespace) {
                (true, true) => r"[\p{P}\p{S}\s]*",
                (true, false) => r"[\p{P}\p{S}]*",
                _ => r"\s*",
            };
            let chars: Vec<String> = norm_keep_case(&word, norm)
                .chars()
                .map(|c| regex::escape(c.encode_utf8(&mut [0; 4])))
                .collect();
            backend.text = chars.join(gap);
            backend.regex = !backend.text.is_empty();
        }
    } else {
        backend.text = passed.join(" ");
    }
    Ok(Plan {
        backend,
        filter: Filter {
            norm,
            terms,
            regex: None,
            start: p.prefix || p.whole_word,
            end: p.suffix || p.whole_word,
        },
    })
}

/// Compiles `p` and checks `paths` (as hits) against it: the fuzz targets' way in.
#[doc(hidden)]
pub fn check_paths(p: &SystemSearchParams, paths: &[&str]) -> Result<Vec<bool>> {
    let plan = compile(p, false)?;
    let _ = compile(p, true)?;
    Ok(paths
        .iter()
        .map(|path| {
            plan.filter.matches(&Hit {
                path: keel_vfs::VPath::local(path),
                is_dir: false,
                size: 0,
                modified: None,
                snippet: None,
            })
        })
        .collect())
}

/// `word` without the characters `norm` skips, case kept (the backend folds case itself).
fn norm_keep_case(word: &str, norm: Norm) -> String {
    Norm {
        case: true,
        diacritics: true,
        ..norm
    }
    .apply(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::VPath;
    use std::time::{Duration, UNIX_EPOCH};

    fn hit(path: &str, size: u64, secs: u64) -> Hit {
        Hit {
            path: VPath::local(path),
            is_dir: false,
            size,
            modified: Some(UNIX_EPOCH + Duration::from_secs(secs)),
            snippet: None,
        }
    }

    /// Returns its hits whatever the query; records the query.
    struct Fake(Vec<Hit>, Mutex<Option<Query>>);
    impl Searcher for Fake {
        fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
            *self.1.lock() = Some(q.clone());
            Ok(self.0.iter().take(q.max as usize).cloned().collect())
        }
        fn available(&self) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "Fake"
        }
    }

    fn root() -> &'static str {
        if cfg!(windows) {
            r"C:\data"
        } else {
            "/data"
        }
    }

    fn p(rel: &str) -> String {
        let sep = if cfg!(windows) { "\\" } else { "/" };
        format!("{}{sep}{}", root(), rel.replace('/', sep))
    }

    fn names(f: &Found) -> Vec<&str> {
        f.hits.iter().map(|h| h.path.name()).collect()
    }

    fn fake() -> Fake {
        Fake(
            vec![
                hit(&p("docs/Report-Final.pdf"), 300, 30),
                hit(&p("docs/report.txt"), 100, 10),
                hit(&p("music/Café del Mar.mp3"), 500, 50),
                hit(&p("reports/summary.md"), 200, 20),
                hit(&p("misc/preport.doc"), 400, 40),
            ],
            Mutex::new(None),
        )
    }

    fn search(q: SystemSearchParams) -> Found {
        run(&fake(), &q, MAX_SCAN).unwrap()
    }

    fn q(text: &str) -> SystemSearchParams {
        SystemSearchParams {
            query: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_words_sort_and_page() {
        let f = search(q("report"));
        // Name order, case-insensitive; "reports" is a folder name, not the file's.
        assert_eq!(names(&f), ["preport.doc", "Report-Final.pdf", "report.txt"]);
        assert_eq!((f.total, f.capped, f.backend.as_str()), (3, false, "Fake"));
        let page = search(SystemSearchParams {
            offset: Some(1),
            count: Some(1),
            ..q("report")
        });
        assert_eq!((names(&page), page.total), (vec!["Report-Final.pdf"], 3));
        let none = search(SystemSearchParams {
            count: Some(0),
            ..q("report")
        });
        assert!(none.hits.is_empty() && none.total == 3);
        let past = search(SystemSearchParams {
            offset: Some(99),
            ..q("report")
        });
        assert!(past.hits.is_empty() && past.total == 3);
        let by_size = search(SystemSearchParams {
            sort: Some(SystemSort::Size),
            ascending: Some(false),
            ..q("")
        });
        assert_eq!(by_size.hits[0].size, 500);
        let by_date = search(SystemSearchParams {
            sort: Some(SystemSort::DateModified),
            ..q("")
        });
        assert_eq!(names(&by_date)[0], "report.txt");
        let by_path = search(SystemSearchParams {
            sort: Some(SystemSort::Path),
            ..q("")
        });
        assert_eq!(names(&by_path)[0], "Report-Final.pdf");
        assert_eq!(names(&by_path)[4], "summary.md");
    }

    #[test]
    fn counts_are_bounded() {
        let many = Fake(
            (0..50)
                .map(|i| hit(&p(&format!("f{i:02}.txt")), 1, 1))
                .collect(),
            Mutex::new(None),
        );
        let f = run(&many, &q(""), 20).unwrap();
        assert!(f.capped, "the backend filled the scan");
        assert_eq!(f.total, 20);
        assert_eq!(many.1.lock().as_ref().unwrap().max, 20);
        assert_eq!(f.hits.len(), DEFAULT_COUNT.min(20));
        let all = run(
            &many,
            &SystemSearchParams {
                count: Some(usize::MAX),
                offset: Some(usize::MAX),
                ..q("")
            },
            MAX_SCAN,
        )
        .unwrap();
        assert!(all.hits.is_empty() && !all.capped && all.total == 50);
        let big = run(
            &many,
            &SystemSearchParams {
                count: Some(5000),
                ..q("")
            },
            MAX_SCAN,
        )
        .unwrap();
        assert_eq!(big.hits.len(), 50, "at most MAX_COUNT, here all 50");
        let long = q(&"x".repeat(MAX_QUERY + 1));
        assert!(run(&many, &long, MAX_SCAN).is_err());
    }

    #[test]
    fn match_options() {
        let n = |q: SystemSearchParams| -> Vec<String> {
            names(&search(q)).into_iter().map(str::to_owned).collect()
        };
        assert_eq!(
            n(SystemSearchParams {
                match_case: true,
                ..q("Report")
            }),
            ["Report-Final.pdf"]
        );
        assert_eq!(
            n(SystemSearchParams {
                whole_word: true,
                ..q("report")
            }),
            ["Report-Final.pdf", "report.txt"]
        );
        assert_eq!(
            n(SystemSearchParams {
                suffix: true,
                ..q("port")
            }),
            ["preport.doc", "Report-Final.pdf", "report.txt"]
        );
        assert_eq!(
            n(SystemSearchParams {
                prefix: true,
                ..q("port")
            }),
            Vec::<String>::new()
        );
        assert_eq!(
            n(SystemSearchParams {
                prefix: true,
                ..q("fin")
            }),
            ["Report-Final.pdf"]
        );
        // Diacritics: ignored unless asked for.
        assert_eq!(n(q("cafe")), ["Café del Mar.mp3"]);
        assert!(n(SystemSearchParams {
            match_diacritics: true,
            ..q("cafe")
        })
        .is_empty());
        // Path: the folder name matches too; a word with a separator always matches paths.
        assert_eq!(
            n(SystemSearchParams {
                match_path: true,
                ..q("reports")
            }),
            ["summary.md"]
        );
        assert_eq!(n(q("music/cafe")), ["Café del Mar.mp3"]);
        // Punctuation and white space.
        assert_eq!(
            n(SystemSearchParams {
                ignore_punctuation: true,
                ..q("reportfinal")
            }),
            ["Report-Final.pdf"]
        );
        assert_eq!(
            n(SystemSearchParams {
                ignore_whitespace: true,
                ..q("cafédel")
            }),
            ["Café del Mar.mp3"]
        );
        // Wildcards match the whole name; quotes keep spaces; all words must match.
        assert_eq!(n(q("*.md")), ["summary.md"]);
        assert_eq!(n(q("\"del mar\"")), ["Café del Mar.mp3"]);
        assert_eq!(n(q("report txt")), ["report.txt"]);
        // Regex over names (or paths).
        assert_eq!(
            n(SystemSearchParams {
                regex: true,
                ..q(r"^report\.")
            }),
            ["report.txt"]
        );
        assert!(run(
            &fake(),
            &SystemSearchParams {
                regex: true,
                ..q("(unclosed")
            },
            MAX_SCAN
        )
        .is_err());
        // Backend syntax is passed on and not checked here.
        assert_eq!(n(q("ext:pdf report")).len(), 3);
    }

    /// Review 1: `regex=1&path=1` sent the regex to a backend that matches names only, so
    /// `\docs\.*\.pdf$` found nothing. The backend now gets every name; the regex runs on
    /// the full path.
    #[test]
    fn a_path_regex_is_matched_on_the_full_path_not_by_the_backend() {
        let sep = if cfg!(windows) { r"\\" } else { "/" };
        let re = format!(r"{sep}docs{sep}.*\.pdf$");
        let params = SystemSearchParams {
            regex: true,
            match_path: true,
            ..q(&re)
        };
        let b = compile(&params, false).unwrap().backend;
        assert_eq!((b.text.as_str(), b.regex), ("", false), "every name");
        // A backend like the real ones: the query's text against the name only.
        struct Names(Vec<Hit>);
        impl Searcher for Names {
            fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
                let re = q.regex.then(|| Regex::new(&q.text).unwrap());
                Ok(self
                    .0
                    .iter()
                    .filter(|h| match &re {
                        Some(re) => re.is_match(h.path.name()),
                        None => h.path.name().contains(&q.text),
                    })
                    .cloned()
                    .collect())
            }
            fn available(&self) -> bool {
                true
            }
            fn name(&self) -> &'static str {
                "Names"
            }
        }
        let hits = fake().0;
        let f = run(&Names(hits.clone()), &params, MAX_SCAN).unwrap();
        assert_eq!(names(&f), ["Report-Final.pdf"], "{:?}", f.total);
        // Without path=1 the regex still goes to the backend and matches names.
        let by_name = SystemSearchParams {
            match_path: false,
            ..params.clone()
        };
        let b = compile(&by_name, false).unwrap().backend;
        assert_eq!((b.text.as_str(), b.regex), (re.as_str(), true));
        let f = run(&Names(hits), &by_name, MAX_SCAN).unwrap();
        assert!(f.hits.is_empty(), "the name has no folder in it");
    }

    #[test]
    fn the_backend_gets_a_superset() {
        let plan = |q: SystemSearchParams, everything| compile(&q, everything).unwrap().backend;
        let b = plan(q("report ext:pdf"), false);
        assert_eq!((b.text.as_str(), b.regex), ("report ext:pdf", false));
        let b = plan(
            SystemSearchParams {
                match_case: true,
                ..q("\"a b\" docs/x")
            },
            false,
        );
        assert_eq!(b.text, "\"a b\" x");
        assert!(b.match_case);
        let b = plan(q("docs/x"), true);
        assert_eq!(b.text, "path:docs/x");
        let b = plan(
            SystemSearchParams {
                ignore_punctuation: true,
                ..q("ab cde")
            },
            false,
        );
        assert!(b.regex);
        assert_eq!(b.text, r"c[\p{P}\p{S}]*d[\p{P}\p{S}]*e");
        assert!(compile(
            &SystemSearchParams {
                ignore_whitespace: true,
                ..q("ext:pdf x")
            },
            false
        )
        .is_err());
        let b = plan(
            SystemSearchParams {
                regex: true,
                ..q("^a.*b$")
            },
            false,
        );
        assert_eq!((b.text.as_str(), b.regex), ("^a.*b$", true));
        // Drive letters are paths, not functions.
        assert!(!backend_syntax(r"C:\x") && !backend_syntax("c:") && backend_syntax("ext:pdf"));
    }

    #[test]
    fn find_respects_word_edges() {
        assert!(find("aaa", "aa", true, false));
        assert!(find("x-aa", "aa", true, true));
        assert!(!find("xaa", "aa", true, false));
        assert!(find("a aab", "aa", true, false));
        assert!(!find("baab", "aa", false, true));
        assert_eq!(fold('É'), 'E');
        assert_eq!(fold('ž'), 'z');
        assert_eq!(fold('ß'), 'ß');
    }
}
