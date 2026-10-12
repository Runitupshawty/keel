//! Query text to [`Matcher`]: Everything-style terms. Space-separated terms must all
//! match the name; `"quoted text"` is one term; a term with `*` or `?` is a glob over
//! the whole name; `regex:` makes the rest of a term a regular expression (or the
//! whole text when `Query::regex` is set); `folder:` keeps folders only; `in:<path>`
//! keeps entries under a path prefix. Anything else is a substring. The search toggles
//! (whole words, path, diacritics, whole-filename wildcards) come from `crate::terms`.

use regex::{Regex, RegexBuilder};

use super::index::Matcher;
use crate::{terms, Query};

enum Term {
    Literal(String),
    Pattern(String),
}

fn split_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
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

fn strip_ci<'a>(term: &'a str, prefix: &str) -> Option<&'a str> {
    term.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &term[prefix.len()..])
}

pub(crate) fn compile(query: &Query) -> anyhow::Result<Matcher> {
    let mut folders_only = query.folders_only;
    let mut within = None;
    let mut terms = Vec::new();
    if query.regex {
        if !query.text.is_empty() {
            terms.push(Term::Pattern(query.text.clone()));
        }
    } else {
        for term in split_terms(&query.text) {
            if let Some(rest) = strip_ci(&term, "folder:") {
                folders_only = true;
                if rest.is_empty() {
                    continue;
                }
                terms.push(term_of(rest, query));
            } else if let Some(rest) = strip_ci(&term, "in:") {
                within = Some(rest.to_owned());
            } else if let Some(rest) = strip_ci(&term, "regex:") {
                terms.push(Term::Pattern(rest.to_owned()));
            } else {
                terms.push(term_of(&term, query));
            }
        }
    }
    // Scan with the longest substring (most selective), else the first pattern.
    let lead = terms
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match t {
            Term::Literal(s) => Some((s.len(), i)),
            Term::Pattern(_) => None,
        })
        .max()
        .map(|(_, i)| i)
        .or((!terms.is_empty()).then_some(0));
    let regex = |src: &str| -> anyhow::Result<Regex> {
        Ok(RegexBuilder::new(src)
            .multi_line(true)
            .case_insensitive(!query.match_case)
            .build()?)
    };
    let build = |t: &Term| match t {
        Term::Literal(s) => regex(&terms::literal(s, query)),
        Term::Pattern(p) => regex(p),
    };
    // Match Whole Words and Match Path (shared with the walk index, `terms.rs`).
    let words = terms::word_terms(&literals(&terms), query, &regex)?;
    if query.match_path {
        let mut path = terms
            .iter()
            .map(build)
            .collect::<anyhow::Result<Vec<_>>>()?;
        path.extend(words);
        return Ok(Matcher {
            scan: None,
            rest: Vec::new(),
            literal: None,
            prefix: None,
            match_case: query.match_case,
            folders_only,
            within,
            path,
        });
    }
    let mut scan = None;
    let mut literal = None;
    let mut prefix = None;
    let mut rest = Vec::new();
    for (i, t) in terms.iter().enumerate() {
        if Some(i) == lead {
            scan = Some(build(t)?);
            if let Term::Literal(s) = t {
                literal = Some(s.clone());
                prefix = Some(regex(&format!("^{}", terms::literal(s, query)))?);
            }
        } else {
            rest.push(build(t)?);
        }
    }
    rest.extend(words);
    Ok(Matcher {
        scan,
        rest,
        literal,
        prefix,
        match_case: query.match_case,
        folders_only,
        within,
        path: Vec::new(),
    })
}

fn literals(terms: &[Term]) -> Vec<&str> {
    (terms.iter())
        .filter_map(|t| match t {
            Term::Literal(s) => Some(s.as_str()),
            Term::Pattern(_) => None,
        })
        .collect()
}

fn term_of(text: &str, query: &Query) -> Term {
    if text.contains(['*', '?']) {
        Term::Pattern(terms::glob(text, query))
    } else {
        Term::Literal(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::index::Index;

    fn index() -> Index {
        let mut ix = Index::new("C:", 5);
        for (frn, parent, name, dir) in [
            (10, 5, "Users", true),
            (11, 10, "ann", true),
            (12, 11, "report.pdf", false),
            (13, 11, "Report 2026.PDF", false),
            (14, 11, "report", true),
            (15, 11, "notes.txt", false),
            (16, 11, "my report.docx", false),
            (20, 5, "Windows", true),
            (21, 20, "report.pdf", false),
            (22, 11, "reportage", false),
        ] {
            ix.upsert(frn, parent, name, dir);
        }
        ix
    }

    fn find(text: &str) -> Vec<String> {
        find_q(Query {
            text: text.into(),
            ..Query::default()
        })
    }

    fn find_q(q: Query) -> Vec<String> {
        let m = compile(&q).unwrap();
        index()
            .search(&m, q.max as usize)
            .into_iter()
            .map(|f| f.path)
            .collect()
    }

    #[test]
    fn substring_is_case_insensitive_and_ranked() {
        assert_eq!(
            find("REPORT"),
            [
                r"C:\Users\ann\report",
                r"C:\Users\ann\Report 2026.PDF",
                r"C:\Users\ann\report.pdf",
                r"C:\Users\ann\reportage",
                r"C:\Windows\report.pdf",
                r"C:\Users\ann\my report.docx",
            ]
        );
        assert!(find("nothing-like-this").is_empty());
    }

    #[test]
    fn match_case_is_exact() {
        let hits = find_q(Query {
            text: "Report".into(),
            match_case: true,
            ..Query::default()
        });
        assert_eq!(hits, [r"C:\Users\ann\Report 2026.PDF"]);
    }

    #[test]
    fn glob_matches_the_whole_name() {
        assert_eq!(
            find("*.pdf"),
            [
                r"C:\Users\ann\Report 2026.PDF",
                r"C:\Users\ann\report.pdf",
                r"C:\Windows\report.pdf",
            ]
        );
        assert_eq!(find("not?s.*"), [r"C:\Users\ann\notes.txt"]);
        assert!(find("*.pd").is_empty());
    }

    #[test]
    fn regex_prefix_and_regex_mode() {
        assert_eq!(find(r"regex:^rep.*\d"), [r"C:\Users\ann\Report 2026.PDF"]);
        let hits = find_q(Query {
            text: r"^n.tes\.txt$".into(),
            regex: true,
            ..Query::default()
        });
        assert_eq!(hits, [r"C:\Users\ann\notes.txt"]);
        // A pattern that could run across names still only matches within one.
        assert!(find(r"regex:docx\swindows").is_empty());
        assert!(compile(&Query {
            text: "regex:(".into(),
            ..Query::default()
        })
        .is_err());
    }

    #[test]
    fn terms_and_folder_filter() {
        assert_eq!(
            find("report pdf"),
            [
                r"C:\Users\ann\Report 2026.PDF",
                r"C:\Users\ann\report.pdf",
                r"C:\Windows\report.pdf",
            ]
        );
        assert_eq!(find("\"my report\""), [r"C:\Users\ann\my report.docx"]);
        assert_eq!(find("folder: report"), [r"C:\Users\ann\report"]);
        let all_folders = find_q(Query {
            folders_only: true,
            ..Query::default()
        });
        assert_eq!(
            all_folders,
            [
                r"C:\Users",
                r"C:\Users\ann",
                r"C:\Users\ann\report",
                r"C:\Windows"
            ]
        );
    }

    #[test]
    fn in_filter_takes_a_path_prefix() {
        assert_eq!(
            find(r"report.pdf in:c:\windows"),
            [r"C:\Windows\report.pdf"]
        );
        assert_eq!(find(r"report.pdf in:C:\Win"), [r"C:\Windows\report.pdf"]);
        assert_eq!(find(r"in:C:\Users\ann\rep").len(), 4);
        assert_eq!(find(r"report.pdf in:C:\").len(), 2);
        assert!(find(r"report in:D:\").is_empty());
        assert!(find(r"report in:C:\Nope\x").is_empty());
        assert_eq!(find(r#"in:"C:\Users\ann\my report.docx""#).len(), 1);
    }

    /// Match Whole Words, Match Path, Match Diacritics and whole-filename wildcards.
    #[test]
    fn search_toggles() {
        let with = |text: &str, set: fn(&mut Query)| {
            let mut q = Query {
                text: text.into(),
                ..Query::default()
            };
            set(&mut q);
            find_q(q)
        };
        let words = with("report", |q| q.whole_word = true);
        assert_eq!(words.len(), 5);
        assert!(!words.iter().any(|p| p.ends_with("reportage")));
        assert_eq!(with("port", |q| q.whole_word = true).len(), 0);
        assert_eq!(find("ann"), [r"C:\Users\ann"]);
        assert_eq!(with("ann", |q| q.match_path = true).len(), 7);
        assert_eq!(
            with("windows pdf", |q| q.match_path = true),
            [r"C:\Windows\report.pdf"]
        );
        assert_eq!(
            with(r"*\ann\rep* folder:", |q| q.match_path = true),
            [r"C:\Users\ann\report"],
            "a wildcard over the whole path"
        );
        assert_eq!(find("nOtés"), [r"C:\Users\ann\notes.txt"]);
        assert!(with("notés", |q| q.match_diacritics = true).is_empty());
        assert!(find("*.pd").is_empty());
        assert_eq!(
            with("*.pd", |q| q.whole_filename_wildcards = false).len(),
            3
        );
    }

    #[test]
    fn max_keeps_the_best() {
        let hits = find_q(Query {
            text: "report".into(),
            max: 2,
            ..Query::default()
        });
        // The exact name, then the first prefix match in index order.
        assert_eq!(hits, [r"C:\Users\ann\report", r"C:\Users\ann\report.pdf"]);
    }
}
