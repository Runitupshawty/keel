//! Query text to [`Matcher`]: Everything-style terms. Space-separated terms must all
//! match the name; `"quoted text"` is one term; a term with `*` or `?` is a glob over
//! the whole name; `regex:` makes the rest of a term a regular expression (or the
//! whole text when `Query::regex` is set); `folder:` keeps folders only; `in:<path>`
//! keeps entries under a path prefix. Anything else is a substring.

use regex::{Regex, RegexBuilder};

use super::index::Matcher;
use crate::Query;

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

/// Whole-name glob as a regex. A leading or trailing `*` drops that anchor instead
/// (same matches, and the regex engine can then skip ahead on the literal).
fn glob(term: &str) -> String {
    let mut re = String::new();
    if !term.starts_with('*') {
        re.push('^');
    }
    for c in term.trim_matches('*').chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    if !term.ends_with('*') {
        re.push('$');
    }
    re
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
                terms.push(term_of(rest));
            } else if let Some(rest) = strip_ci(&term, "in:") {
                within = Some(rest.to_owned());
            } else if let Some(rest) = strip_ci(&term, "regex:") {
                terms.push(Term::Pattern(rest.to_owned()));
            } else {
                terms.push(term_of(&term));
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
        Term::Literal(s) => regex(&regex::escape(s)),
        Term::Pattern(p) => regex(p),
    };
    let mut scan = None;
    let mut literal = None;
    let mut prefix = None;
    let mut rest = Vec::new();
    for (i, t) in terms.iter().enumerate() {
        if Some(i) == lead {
            scan = Some(build(t)?);
            if let Term::Literal(s) = t {
                literal = Some(s.clone());
                prefix = Some(regex(&format!("^{}", regex::escape(s)))?);
            }
        } else {
            rest.push(build(t)?);
        }
    }
    Ok(Matcher {
        scan,
        rest,
        literal,
        prefix,
        match_case: query.match_case,
        folders_only,
        within,
    })
}

fn term_of(text: &str) -> Term {
    if text.contains(['*', '?']) {
        Term::Pattern(glob(text))
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
