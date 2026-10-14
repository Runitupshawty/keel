//! `system.search`: the platform's file search (`keel_search::default_searcher`, the
//! backend of the window's search pane: Everything or Keel's own index on Windows,
//! Spotlight or locate elsewhere) with Everything's match options, sort and paging.
//! keel-daemon's Everything-compatible HTTP endpoint runs the same [`run`].
//!
//! Native indexes evaluate the shared query language against their indexed properties.
//! Other platform backends supply a bounded candidate set for the same evaluator;
//! Everything receives its own language unchanged. At most [`MAX_SCAN`] hits are read
//! per search: the total and the order cover those.

use crate::error::{ApiError, Result};
use crate::types::{SystemSearchParams, SystemSort};
pub use keel_search::{Hit, HitFilter, Query, Searcher};
use parking_lot::Mutex;
use std::sync::Arc;

/// Results a search returns when it does not say (`count`).
pub const DEFAULT_COUNT: usize = 32;
/// Most results one search returns.
pub const MAX_COUNT: usize = 1000;
/// Most hits read from the backend per search.
pub const MAX_SCAN: usize = 10_000;
/// Longest search text (bytes).
pub const MAX_QUERY: usize = 4096;

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
    let everything = searcher.name() == "Everything";
    let plan = compile(p, everything)?;
    let count = p.count.unwrap_or(DEFAULT_COUNT).min(MAX_COUNT);
    let offset = p.offset.unwrap_or(0);
    // Everything and Keel's own indexes evaluate the language (with all the options)
    // themselves, on more properties than a hit keeps. Any other backend is asked for a
    // superset of the text and its hits are evaluated here.
    let native = searcher.unified_query() || plan.filter.is_none();
    let max = u32::try_from(scan).unwrap_or(u32::MAX);
    let query = match (native, &plan.filter) {
        (false, Some(_)) => Query {
            max,
            ..keel_search::backend_query(&plan.backend)
                .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?
        },
        _ => Query {
            max,
            meta: false,
            ..plan.backend.clone()
        },
    };
    if !searcher.available() {
        searcher.probe();
    }
    let hits = searcher
        .query(&query)
        .map_err(|e| ApiError::failed(format!("{}: {e:#}", searcher.name())))?;
    let capped = hits.len() >= scan;
    let mut hits = match (&plan.filter, native) {
        (Some(filter), false) => filter
            .select(hits)
            .map_err(|e| ApiError::invalid_params(format!("{e:#}")))?,
        _ => hits,
    };
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
    /// None for Everything.
    pub(crate) filter: Option<HitFilter>,
}

/// The query for `p` and the evaluator for the hits of a backend that does not evaluate
/// the search language itself. Everything is asked for the text as typed (it has its own
/// language); for every other backend the text must parse here, and the error says where.
pub(crate) fn compile(p: &SystemSearchParams, everything: bool) -> Result<Plan> {
    if p.query.len() > MAX_QUERY {
        return Err(ApiError::invalid_params(format!(
            "the search is longer than {MAX_QUERY} bytes"
        )));
    }
    let backend = Query {
        text: p.query.clone(),
        regex: p.regex,
        match_case: p.match_case,
        match_path: p.match_path,
        whole_word: p.whole_word,
        match_diacritics: p.match_diacritics,
        match_prefix: p.prefix,
        match_suffix: p.suffix,
        ignore_punctuation: p.ignore_punctuation,
        ignore_whitespace: p.ignore_whitespace,
        ..Query::default()
    };
    let filter = if everything {
        // Everything parses its own text; only a regex is checked here, as it always was.
        if p.regex {
            regex::Regex::new(&p.query)
                .map_err(|e| ApiError::invalid_params(format!("bad regular expression: {e}")))?;
        }
        None
    } else {
        let invalid = |e: anyhow::Error| ApiError::invalid_params(format!("{e:#}"));
        Some(HitFilter::new(&backend).map_err(invalid)?)
    };
    Ok(Plan { backend, filter })
}

/// Compiles `p` and checks `paths` (as hits) against it: the fuzz targets' way in.
#[doc(hidden)]
pub fn check_paths(p: &SystemSearchParams, paths: &[&str]) -> Result<Vec<bool>> {
    let filter = compile(p, false)?.filter;
    let _ = compile(p, true)?;
    Ok(paths
        .iter()
        .map(|path| {
            filter.as_ref().is_none_or(|f| {
                f.matches(&Hit {
                    path: keel_vfs::VPath::local(path),
                    is_dir: false,
                    size: 0,
                    modified: None,
                    snippet: None,
                })
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::VPath;
    use regex::Regex;
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
        // Function terms now run through the same evaluator as plain words.
        assert_eq!(n(q("ext:pdf report")), ["Report-Final.pdf"]);
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
        assert_eq!((b.text.as_str(), b.regex), (re.as_str(), true));
        assert!(b.match_path);
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
    fn query_options_and_everything_text_are_preserved() {
        let params = SystemSearchParams {
            match_case: true,
            prefix: true,
            ignore_punctuation: true,
            ..q("<report | summary> !ext:txt")
        };
        let b = compile(&params, false).unwrap().backend;
        assert_eq!(b.text, params.query);
        assert!(b.match_case && b.match_prefix && b.ignore_punctuation);
        // Everything supports terms Keel cannot evaluate; neither reject nor rewrite.
        let text = "unknown-everything-function:foo | docs/x";
        let b = compile(&q(text), true).unwrap().backend;
        assert_eq!(b.text, text);
        let f = fake();
        run(&f, &q("report | summary"), MAX_SCAN).unwrap();
        assert!(f.1.lock().as_ref().unwrap().text.is_empty());
    }

    #[test]
    fn boolean_queries_use_one_evaluator() {
        assert_eq!(
            names(&search(q("<report | summary> !ext:txt"))),
            ["preport.doc", "Report-Final.pdf", "summary.md"]
        );
        assert_eq!(
            names(&search(q("ext:pdf | ext:md"))),
            ["Report-Final.pdf", "summary.md"]
        );
        assert_eq!(
            names(&search(q("!<report | summary>"))),
            ["Café del Mar.mp3"]
        );
        assert_eq!(names(&search(q("size:>400"))), ["Café del Mar.mp3"]);
        assert_eq!(
            names(&search(SystemSearchParams {
                regex: true,
                ..q("Report-Final|del Mar")
            })),
            ["Café del Mar.mp3", "Report-Final.pdf"]
        );
    }

    #[test]
    fn native_hits_keep_index_only_properties_and_parse_errors_have_positions() {
        struct Native(Fake);
        impl Searcher for Native {
            fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
                self.0.query(q)
            }
            fn available(&self) -> bool {
                true
            }
            fn unified_query(&self) -> bool {
                true
            }
        }
        let native = Native(fake());
        // Native backend already evaluated child counts, which Hit does not retain.
        let found = run(&native, &q("child-count:>0"), MAX_SCAN).unwrap();
        assert_eq!(found.total, 5);
        assert_eq!(native.0 .1.lock().as_ref().unwrap().text, "child-count:>0");
        let error = run(&native, &q("<report"), MAX_SCAN).err().unwrap();
        assert_eq!(error.code, ApiError::INVALID_PARAMS);
        assert!(error.message.contains("at "), "{}", error.message);
    }
}
