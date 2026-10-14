//! Shared query compilation. The name-buffer regex is only a conservative prefilter;
//! the compiled language always decides whether a candidate matches.
use keel_query::{Ast, Expr, FnKind, Item, Modifier, Options, Term};
use regex::{Regex, RegexBuilder};

use crate::Query;

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

/// Compile a native query, including the pane's options. Errors include byte positions.
pub fn compile_query(q: &Query) -> anyhow::Result<keel_query::Query> {
    let opts = options(q);
    let mut ast = if q.regex {
        Ast {
            expr: Expr::Term(Term {
                span: 0..q.text.len(),
                modifiers: vec![Modifier::Regex(true)],
                function: None,
                items: vec![Item { op: None, text: q.text.clone(), span: 0..q.text.len() }],
            }),
            globals: Default::default(),
        }
    } else {
        keel_query::parse_ast(&q.text, &opts)?
    };
    // Preserve native in: path-prefix searches, including partially typed folders.
    // Lower the parsed function, never tokenize the query a second time.
    lower_in(&mut ast.expr);
    Ok(keel_query::compile(ast, &opts)?)
}

fn lower_in(expr: &mut Expr) {
    match expr {
        Expr::And(xs) | Expr::Or(xs) => xs.iter_mut().for_each(lower_in),
        Expr::Not(x) => lower_in(x),
        Expr::Term(t) if t.function.as_ref().is_some_and(|f| f.kind == FnKind::In) => {
            let f = t.function.as_mut().unwrap();
            f.kind = FnKind::Text(keel_query::TextProp::FullPath);
            t.modifiers.extend([Modifier::StartWith, Modifier::Diacritics(true)]);
            for item in &mut t.items {
                #[cfg(not(windows))]
                if let Ok(path) = std::fs::canonicalize(&item.text) {
                    item.text = path.to_string_lossy().into_owned();
                }
                item.text = keel_query::normalize(&item.text).into_owned();
            }
        }
        _ => {}
    }
}

pub(crate) fn has_in(expr: &Expr) -> bool {
    match expr {
        Expr::And(xs) | Expr::Or(xs) => xs.iter().any(has_in),
        Expr::Not(x) => has_in(x),
        Expr::Term(t) => t.function.as_ref().is_some_and(|f| f.name == "in"),
    }
}

/// Only a positive conjunct can be mandatory. An OR branch or a negated term
/// cannot be used to skip names. Modifiers/substitutions need the evaluator.
fn literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::And(xs) => xs.iter().filter_map(literal).max_by_key(|s| s.len()),
        Expr::Term(t) if t.function.is_none() && t.modifiers.is_empty() && t.items.len() == 1 => {
            let s = &t.items[0].text;
            (!s.is_empty() && s.is_ascii() && !s.contains(['*', '?', '\\', '/', '$', '\n']))
                .then_some(s.as_str())
        }
        _ => None,
    }
}

pub(crate) struct Prefilter {
    pub query: keel_query::Query,
    pub scan: Option<Regex>,
    pub literal: Option<String>,
    pub within: Option<String>,
}

pub(crate) fn prefilter(q: &Query) -> anyhow::Result<Prefilter> {
    let query = compile_query(q)?;
    let lit = if q.match_path || q.ignore_punctuation || q.ignore_whitespace {
        None
    } else {
        literal(&query.ast().expr).map(str::to_owned)
    };
    let scan = lit.as_ref().map(|s| {
        // Any non-ASCII scalar is admitted at each position: accent and Unicode
        // case folding cannot make this prefilter discard a true match. Combining
        // marks may occur between letters when accent matching is off.
        let gap = if q.match_diacritics { "" } else { r"[\p{M}]*" };
        let src = s.chars().map(|c| format!("(?:{}|[^\\x00-\\x7F]){gap}", regex::escape(&c.to_string())))
            .collect::<String>();
        RegexBuilder::new(&src).case_insensitive(!q.match_case)
            .multi_line(true).size_limit(keel_query::REGEX_SIZE_LIMIT).build()
    }).transpose()?;
    let within = q.within.as_ref().filter(|_| !has_in(&query.ast().expr)).map(|d| {
        #[cfg(not(windows))]
        let d = &std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
        format!("{}/", d.to_string_lossy().replace('\\', "/").trim_end_matches('/')).to_lowercase()
    });
    Ok(Prefilter { query, scan, literal: lit, within })
}

pub(crate) fn filetime(t: i64) -> Option<i64> {
    (t != 0).then(|| t / 10_000 - 11_644_473_600_000)
}
