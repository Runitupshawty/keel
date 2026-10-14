//! One search language for every Keel backend: the text of a [`Query`] is parsed and
//! evaluated by `keel-query` (Everything's syntax), and what is left of the old per-index
//! parsers is a cheap prefilter. A native index scans its name buffer with the regex of
//! the query's longest plain word (the "lead literal") and asks the compiled query about
//! each candidate only; the prefilter may admit too many names, never too few.
//!
//! Backends that cannot evaluate the language (Spotlight, `locate`) are asked for a
//! superset ([`backend_query`]) and their hits go through [`evaluate_hits`]; the
//! [`Evaluated`] wrapper does both.

use std::path::{Path, PathBuf};

use keel_query::{Ast, DateProp, Expr, FnKind, Modifier, Options, Props, Term, Timestamp};
use regex::{Regex, RegexBuilder};

use crate::{Hit, IndexSettings, Query, SearchState, Searcher};

/// Hits a backend without the language is asked for before they are evaluated here.
pub const EVALUATED_SCAN: u32 = 10_000;

pub(crate) fn options(q: &Query) -> Options {
    Options {
        match_case: q.match_case,
        match_diacritics: q.match_diacritics,
        match_whole_words: q.whole_word,
        match_path: q.match_path,
        match_prefix: q.match_prefix,
        match_suffix: q.match_suffix,
        ignore_punctuation: q.ignore_punctuation,
        ignore_whitespace: q.ignore_whitespace,
        whole_filename_wildcards: q.whole_filename_wildcards,
        allow_literal_operators: true,
        replace_forward_slashes: true,
        ..Options::default()
    }
}

/// The parsed query, `in:` lowered; `Query::regex` makes the whole text one regex term.
fn parse(q: &Query, opts: &Options) -> Result<Ast, keel_query::Error> {
    let mut ast = if q.regex {
        let span = 0..q.text.len();
        Ast {
            expr: Expr::Term(Term {
                span: span.clone(),
                modifiers: vec![Modifier::Regex(true)],
                function: None,
                items: vec![keel_query::Item {
                    op: None,
                    text: q.text.clone(),
                    span,
                }],
            }),
            globals: keel_query::Globals::default(),
        }
    } else {
        keel_query::parse_ast(&q.text, opts)?
    };
    // Keel's indexes always read `in:` as a path prefix, a partly typed last folder
    // included (`in:C:\Users\an`). Lower the parsed function; never tokenize twice.
    lower_in(&mut ast.expr);
    Ok(ast)
}

/// Compiles a native query with the pane's options. Errors say where in the text.
pub fn compile_query(q: &Query) -> anyhow::Result<keel_query::Query> {
    let opts = options(q);
    Ok(keel_query::compile(parse(q, &opts)?, &opts)?)
}

fn lower_in(expr: &mut Expr) {
    match expr {
        Expr::And(xs) | Expr::Or(xs) => xs.iter_mut().for_each(lower_in),
        Expr::Not(x) => lower_in(x),
        Expr::Term(t) => {
            let Some(f) = t.function.as_mut().filter(|f| f.kind == FnKind::In) else {
                return;
            };
            f.kind = FnKind::Text(keel_query::TextProp::FullPath);
            t.modifiers
                .extend([Modifier::StartWith, Modifier::Diacritics(true)]);
            for item in &mut t.items {
                // The indexes store resolved roots (macOS: /private/var for /var), so a
                // typed folder is resolved the same way when it exists.
                #[cfg(not(windows))]
                if let Ok(path) = std::fs::canonicalize(&item.text) {
                    item.text = path.to_string_lossy().into_owned();
                }
                item.text = keel_query::normalize(&item.text).into_owned();
            }
        }
    }
}

/// The positive terms every match must satisfy (the AND spine of the query).
fn conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Term>) {
    match expr {
        Expr::And(xs) => xs.iter().for_each(|x| conjuncts(x, out)),
        Expr::Term(t) => out.push(t),
        Expr::Or(_) | Expr::Not(_) => {}
    }
}

fn is_in(t: &Term) -> bool {
    t.function.as_ref().is_some_and(|f| f.name == "in")
}

fn has_in(expr: &Expr) -> bool {
    match expr {
        Expr::And(xs) | Expr::Or(xs) => xs.iter().any(has_in),
        Expr::Not(x) => has_in(x),
        Expr::Term(t) => is_in(t),
    }
}

/// Whether the text has a typed `in:` anywhere; it takes the pane's folder's place.
pub(crate) fn has_typed_in(q: &Query) -> bool {
    parse(q, &options(q)).is_ok_and(|ast| has_in(&ast.expr))
}

/// The folder a search keeps to: a typed `in:` every match needs, else the pane's folder
/// (not when an `in:` is optional or negated: that takes its place).
fn scope(q: &Query, expr: &Expr) -> Option<PathBuf> {
    match mandatory_in(expr) {
        Some(dir) => Some(PathBuf::from(
            dir.replace('\\', std::path::MAIN_SEPARATOR_STR),
        )),
        None if has_in(expr) => None,
        None => q.within.clone(),
    }
}

pub(crate) fn folder_scope(q: &Query) -> anyhow::Result<Option<PathBuf>> {
    Ok(scope(q, &parse(q, &options(q))?.expr))
}

/// The folder of a typed `in:` every match must be under, as typed (lowered).
fn mandatory_in(expr: &Expr) -> Option<&str> {
    let mut terms = Vec::new();
    conjuncts(expr, &mut terms);
    terms
        .into_iter()
        .find(|t| is_in(t) && t.items.len() == 1 && !t.items[0].text.is_empty())
        .map(|t| t.items[0].text.as_str())
}

/// The longest plain word every match must contain in its name. Only a positive conjunct
/// can be required: an OR branch or a negated term cannot be used to skip names, and a
/// word with a modifier, a wildcard or a path separator is not a plain substring of the
/// name.
fn literal(expr: &Expr) -> Option<&str> {
    let mut terms = Vec::new();
    conjuncts(expr, &mut terms);
    terms
        .into_iter()
        .filter(|t| {
            t.function.is_none()
                && t.items.len() == 1
                // `file:` and `folder:` filter by type; they do not change how text matches.
                && t.modifiers
                    .iter()
                    .all(|m| matches!(m, Modifier::File | Modifier::Folder))
        })
        .map(|t| t.items[0].text.as_str())
        .filter(|s| {
            !s.is_empty()
                && s.is_ascii()
                && !s.contains(['*', '?', '\\', '/', ':', '$', '\n', '"'])
        })
        .max_by_key(|s| s.len())
}

/// A compiled native query and what narrows the scan before it runs.
pub(crate) struct Prefilter {
    pub query: keel_query::Query,
    /// Finds candidate names in the name buffer (multi-line mode).
    pub scan: Option<Regex>,
    /// `^` + the scan: the exact and prefix matches of the lead literal.
    pub prefix: Option<Regex>,
    pub literal: Option<String>,
    /// The folder the scan keeps to, lower case with `/` separators: a typed `in:` that
    /// every match needs, else the pane's folder (a typed `in:` takes its place).
    pub within: Option<String>,
}

pub(crate) fn prefilter(q: &Query) -> anyhow::Result<Prefilter> {
    let opts = options(q);
    let query = keel_query::compile(parse(q, &opts)?, &opts)?;
    let expr = &query.ast().expr;
    let lit = if q.match_path || q.ignore_punctuation || q.ignore_whitespace {
        None
    } else {
        literal(expr).map(str::to_owned)
    };
    let (mut scan, mut prefix) = (None, None);
    if let Some(lit) = &lit {
        // Any non-ASCII scalar is admitted at each position: accent and Unicode case
        // folding cannot make this prefilter discard a true match. Combining marks may
        // occur between letters when accent matching is off.
        let gap = if q.match_diacritics { "" } else { r"\p{M}*" };
        let src: String = lit
            .chars()
            .map(|c| format!("(?:{}|[^\\x00-\\x7F]){gap}", regex::escape(&c.to_string())))
            .collect();
        let build = |src: &str| {
            RegexBuilder::new(src)
                .case_insensitive(!q.match_case)
                .multi_line(true)
                .size_limit(keel_query::REGEX_SIZE_LIMIT)
                .build()
        };
        scan = Some(build(&src)?);
        prefix = Some(build(&format!("^{src}"))?);
    }
    let within = match mandatory_in(expr) {
        Some(typed) => Some(typed.to_owned()),
        None if has_in(expr) => None,
        None => q.within.as_ref().map(|dir| {
            #[cfg(not(windows))]
            let dir = &std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
            format!("{}\\", dir.to_string_lossy().trim_end_matches(['\\', '/']))
        }),
    }
    .map(|w| w.replace('\\', "/").to_lowercase());
    Ok(Prefilter {
        query,
        scan,
        prefix,
        literal: lit,
        within,
    })
}

/// What to highlight in a result: a plain or wildcard word, or a regular expression.
pub(crate) enum Mark {
    Text(String),
    Regex(String),
}

/// The words and regular expressions of the query that can be highlighted: the positive
/// (not negated) plain terms. Functions (`ext:`, `size:`, `in:`) show nothing, and a
/// query that does not parse has nothing.
pub(crate) fn marked_terms(q: &Query) -> Vec<Mark> {
    fn collect(expr: &Expr, out: &mut Vec<Mark>) {
        match expr {
            Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect(x, out)),
            Expr::Not(_) => {}
            Expr::Term(t) if t.function.is_some() => {}
            Expr::Term(t) => {
                let regex = t.modifiers.contains(&Modifier::Regex(true));
                for item in t.items.iter().filter(|i| !i.text.is_empty()) {
                    out.push(if regex {
                        Mark::Regex(item.text.clone())
                    } else {
                        Mark::Text(item.text.clone())
                    });
                }
            }
        }
    }
    if q.regex {
        return if q.text.is_empty() {
            Vec::new()
        } else {
            vec![Mark::Regex(q.text.clone())]
        };
    }
    let mut out = Vec::new();
    if let Ok(ast) = keel_query::parse_ast(&q.text, &options(q)) {
        collect(&ast.expr, &mut out);
    }
    out
}

/// FILETIME (100 ns since 1601) to the evaluator's milliseconds since 1970; 0 is unknown.
pub(crate) fn filetime(t: i64) -> Option<Timestamp> {
    (t != 0).then(|| t / 10_000 - 11_644_473_600_000)
}

// ---------------------------------------------------------------------------------
// Backends that cannot evaluate the language

/// The query for a backend that matches names with its own, smaller syntax: a superset of
/// what `q` selects (the longest plain word of the query, and the folder it is under),
/// asked for up to [`EVALUATED_SCAN`] hits. Pass the hits to [`evaluate_hits`].
pub fn backend_query(q: &Query) -> anyhow::Result<Query> {
    let opts = options(q);
    let ast = parse(q, &opts)?;
    keel_query::compile(ast.clone(), &opts)?;
    let mut backend = Query {
        folders_only: q.folders_only,
        match_case: q.match_case,
        max: q.max.max(EVALUATED_SCAN),
        within: scope(q, &ast.expr),
        ..Query::default()
    };
    if q.regex && !q.match_path {
        // A name regex narrows the same way in every backend.
        backend.text = q.text.clone();
        backend.regex = true;
    } else if !(q.match_path || q.ignore_punctuation || q.ignore_whitespace) {
        backend.text = literal(&ast.expr).unwrap_or_default().to_owned();
    }
    Ok(backend)
}

/// A hit as the evaluator sees it: what a backend returns, and nothing more.
struct HitProps<'a> {
    hit: &'a Hit,
    path: String,
}

impl Props for HitProps<'_> {
    fn name(&self) -> &str {
        self.hit.path.name()
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn is_dir(&self) -> bool {
        self.hit.is_dir
    }
    fn size(&self) -> Option<u64> {
        (!self.hit.is_dir).then_some(self.hit.size)
    }
    fn allocated(&self) -> Option<u64> {
        None
    }
    fn date(&self, which: DateProp) -> Option<Timestamp> {
        if which != DateProp::Modified {
            return None;
        }
        let since = self.hit.modified?.duration_since(std::time::UNIX_EPOCH);
        match since {
            Ok(d) => i64::try_from(d.as_millis()).ok(),
            Err(e) => i64::try_from(e.duration().as_millis()).ok().map(|ms| -ms),
        }
    }
    fn attributes(&self) -> Option<u32> {
        Some(if self.hit.is_dir {
            keel_query::attr::DIRECTORY
        } else {
            0
        })
    }
}

/// The search language run over the hits of a backend that does not evaluate it.
pub struct HitFilter {
    query: keel_query::Query,
    folders_only: bool,
}

impl HitFilter {
    /// Fails with the position of the error in the text when `q` does not parse.
    pub fn new(q: &Query) -> anyhow::Result<Self> {
        Ok(Self {
            query: compile_query(q)?,
            folders_only: q.folders_only,
        })
    }

    /// Whether `hit` matches. Set functions (`dupe:`, `count:`) need the whole result:
    /// see [`HitFilter::select`].
    pub fn matches(&self, hit: &Hit) -> bool {
        (hit.is_dir || !self.folders_only)
            && self.query.matches(&HitProps {
                hit,
                path: hit.path.display(),
            })
    }

    /// The hits that match, in order, with the set functions applied.
    pub fn select(&self, hits: Vec<Hit>) -> anyhow::Result<Vec<Hit>> {
        let mut hits = hits;
        if self.folders_only {
            hits.retain(|h| h.is_dir);
        }
        let props: Vec<HitProps> = hits
            .iter()
            .map(|hit| HitProps {
                hit,
                path: hit.path.display(),
            })
            .collect();
        let selected = self.query.select(props.iter(), None)?;
        let mut hits: Vec<Option<Hit>> = hits.into_iter().map(Some).collect();
        Ok(selected
            .into_iter()
            .filter_map(|i| hits[i].take())
            .collect())
    }
}

/// The hits of `q` among `hits`, in order (set functions applied): the language run over
/// what a backend returned.
pub fn evaluate_hits(q: &Query, hits: Vec<Hit>) -> anyhow::Result<Vec<Hit>> {
    HitFilter::new(q)?.select(hits)
}

/// A backend with its own smaller syntax, searched with the shared language: it is asked
/// for a superset ([`backend_query`]) and its hits are evaluated ([`evaluate_hits`]).
pub struct Evaluated(Box<dyn Searcher>);

impl Evaluated {
    pub fn new(inner: Box<dyn Searcher>) -> Self {
        Self(inner)
    }
}

impl Searcher for Evaluated {
    fn unified_query(&self) -> bool {
        true
    }

    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let backend = backend_query(q)?;
        let hits = evaluate_hits(q, self.0.query(&backend)?)?;
        Ok(hits.into_iter().take(q.max as usize).collect())
    }

    fn available(&self) -> bool {
        self.0.available()
    }

    fn probe(&self) -> bool {
        self.0.probe()
    }

    fn status(&self) -> Option<String> {
        self.0.status()
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn state(&self) -> SearchState {
        self.0.state()
    }

    fn take_ready(&self) -> bool {
        self.0.take_ready()
    }

    fn offline_roots(&self) -> Vec<PathBuf> {
        self.0.offline_roots()
    }

    fn set_index_settings(&self, settings: &IndexSettings) {
        self.0.set_index_settings(settings);
    }

    fn index_settings_changed(&self) -> Option<IndexSettings> {
        self.0.index_settings_changed()
    }

    fn covers(&self, dir: &Path) -> bool {
        self.0.covers(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::VPath;
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    fn q(text: &str) -> Query {
        Query {
            text: text.into(),
            ..Query::default()
        }
    }

    fn lit(text: &str) -> Option<String> {
        prefilter(&q(text)).ok()?.literal
    }

    #[test]
    fn the_lead_literal_is_the_longest_required_plain_word() {
        assert_eq!(lit("report 2026").as_deref(), Some("report"));
        assert_eq!(lit("ab abc ext:pdf").as_deref(), Some("abc"));
        // Not required, or not a plain substring of the name.
        assert_eq!(lit("report | summary"), None);
        assert_eq!(lit("!report"), None);
        assert_eq!(lit("*.pdf"), None);
        assert_eq!(lit("regex:^a"), None);
        assert_eq!(lit("docs/report"), None);
        assert_eq!(lit("c:"), None);
        assert_eq!(lit("caf\u{e9}"), None);
        assert_eq!(lit("wfn:report"), None);
        // An AND inside a group still binds every match.
        assert_eq!(lit("<report big> ext:pdf").as_deref(), Some("report"));
        // The options that read other text than the name.
        let mut path = q("report");
        path.match_path = true;
        assert_eq!(prefilter(&path).unwrap().literal, None);
        let mut punct = q("report");
        punct.ignore_punctuation = true;
        assert_eq!(prefilter(&punct).unwrap().literal, None);
    }

    #[test]
    fn the_scan_regexes_keep_the_case_option_and_admit_accents() {
        let p = prefilter(&q("Cafe")).unwrap();
        let (scan, prefix) = (p.scan.unwrap(), p.prefix.unwrap());
        assert!(scan.is_match("my CAFE.txt") && scan.is_match("caf\u{e9}"));
        assert!(prefix.is_match("cafe.txt") && prefix.is_match("CAFE") && !prefix.is_match("a cafe"));
        // Several names: `^` is per line.
        assert!(prefix.is_match("zzz\ncafe\n"));
        let mut exact = q("Cafe");
        exact.match_case = true;
        let p = prefilter(&exact).unwrap();
        assert!(!p.scan.as_ref().unwrap().is_match("cafe"));
        assert!(!p.prefix.unwrap().is_match("cafe"));
    }

    #[test]
    fn a_folder_comes_from_a_required_in_or_the_pane() {
        let within = |text: &str, pane: Option<&str>| {
            let mut query = q(text);
            query.within = pane.map(Into::into);
            prefilter(&query).unwrap().within
        };
        assert_eq!(within("report", None), None);
        assert_eq!(
            within("report in:C:\\Users\\An", None).as_deref(),
            Some("c:/users/an")
        );
        assert_eq!(
            within("report", Some("C:\\Users\\")).as_deref(),
            Some("c:/users/")
        );
        // A typed in: takes the pane's place, required or not.
        assert_eq!(
            within("report in:C:\\a", Some("C:\\b")).as_deref(),
            Some("c:/a")
        );
        assert_eq!(within("report | in:C:\\a", Some("C:\\b")), None);
        assert_eq!(within("report !in:C:\\a", Some("C:\\b")), None);
    }

    #[test]
    fn errors_name_the_position() {
        for text in ["<report", "ext:pdf size:big", "nope:x"] {
            let err = compile_query(&q(text)).unwrap_err().to_string();
            assert!(err.contains("(at "), "{text}: {err}");
        }
        let err = compile_query(&q("report nope:x")).unwrap_err().to_string();
        assert!(err.ends_with("(at 7)"), "{err}");
        let mut regex = q("(");
        regex.regex = true;
        assert!(compile_query(&regex).is_err());
    }

    fn hit(path: &str, size: u64, secs: u64) -> Hit {
        Hit {
            path: VPath::local(path),
            is_dir: path.ends_with('/'),
            size,
            modified: Some(UNIX_EPOCH + Duration::from_secs(secs)),
            snippet: None,
        }
    }

    struct Fake(Mutex<Option<Query>>, Vec<Hit>);

    impl Searcher for Fake {
        fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
            *self.0.lock().unwrap() = Some(q.clone());
            Ok(self.1.clone())
        }
        fn available(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_backend_with_its_own_syntax_gets_a_superset_and_is_evaluated() {
        let fake = Fake(
            Mutex::new(None),
            vec![
                hit("/d/report.pdf", 5, 1),
                hit("/d/report.txt", 50, 2),
                hit("/d/summary.md", 5, 3),
                hit("/d/other.md", 5, 4),
            ],
        );
        let searcher = Evaluated::new(Box::new(fake));
        let names = |text: &str| -> Vec<String> {
            searcher
                .query(&q(text))
                .unwrap()
                .iter()
                .map(|h| h.path.name().to_owned())
                .collect()
        };
        assert_eq!(names("report ext:pdf"), ["report.pdf"]);
        assert_eq!(names("<report | summary> !ext:txt"), ["report.pdf", "summary.md"]);
        assert_eq!(names("size:>10"), ["report.txt"]);
        assert!(searcher.unified_query());
        assert!(searcher.query(&q("<report")).is_err());
    }

    #[test]
    fn the_backend_query_is_asked_for_a_superset() {
        let b = |text: &str| backend_query(&q(text)).unwrap();
        assert_eq!(b("report ext:pdf").text, "report");
        assert_eq!(b("report | summary").text, "");
        assert_eq!(b("!report").text, "");
        assert!(b("report").max >= EVALUATED_SCAN);
        let mut regex = q("^a.*b$");
        regex.regex = true;
        let r = backend_query(&regex).unwrap();
        assert_eq!((r.text.as_str(), r.regex), ("^a.*b$", true));
        regex.match_path = true;
        let r = backend_query(&regex).unwrap();
        assert_eq!((r.text.as_str(), r.regex), ("", false));
        assert_eq!(
            b("report in:/home/me").within,
            Some(PathBuf::from("/home/me").to_string_lossy().replace('/', std::path::MAIN_SEPARATOR_STR).into())
        );
        let mut pane = q("report | summary");
        pane.within = Some("/x".into());
        assert_eq!(backend_query(&pane).unwrap().within, Some("/x".into()));
    }
}
