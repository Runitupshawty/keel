//! What the search toggles do to a term, shared by the native matchers (the NTFS index's
//! `ntfs/pattern.rs`, the walk index and the plain walk) and by result highlighting:
//! diacritic folding (Match Diacritics off: `cafe` finds `café`), whole-word bounds
//! (Match Whole Words), whole-filename wildcards and the [`Highlighter`].

use regex::{Regex, RegexBuilder};

use crate::Query;

/// Each row is a base letter followed by the same letter with diacritics; lower and upper
/// case rows apart, so Match Case still tells them apart.
const FOLD: &[&str] = &[
    "aàáâãäåāăąǎȁȃȧạảấầẩẫậắằẳẵặ",
    "AÀÁÂÃÄÅĀĂĄǍȀȂȦẠẢẤẦẨẪẬẮẰẲẴẶ",
    "cçćĉċč",
    "CÇĆĈĊČ",
    "dďđ",
    "DĎĐ",
    "eèéêëēĕėęěȅȇẹẻẽếềểễệ",
    "EÈÉÊËĒĔĖĘĚȄȆẸẺẼẾỀỂỄỆ",
    "gĝğġģǧ",
    "GĜĞĠĢǦ",
    "hĥħ",
    "HĤĦ",
    "iìíîïĩīĭįǐȉȋịỉ",
    "IÌÍÎÏĨĪĬĮİǏȈȊỊỈ",
    "jĵ",
    "JĴ",
    "kķǩ",
    "KĶǨ",
    "lĺļľŀł",
    "LĹĻĽĿŁ",
    "nñńņňǹ",
    "NÑŃŅŇǸ",
    "oòóôõöøōŏőơǒȍȏọỏốồổỗộớờởỡợ",
    "OÒÓÔÕÖØŌŎŐƠǑȌȎỌỎỐỒỔỖỘỚỜỞỠỢ",
    "rŕŗřȑȓ",
    "RŔŖŘȐȒ",
    "sśŝşšș",
    "SŚŜŞŠȘ",
    "tţťŧț",
    "TŢŤŦȚ",
    "uùúûüũūŭůűųưǔȕȗụủứừửữự",
    "UÙÚÛÜŨŪŬŮŰŲƯǓȔȖỤỦỨỪỬỮỰ",
    "wŵẁẃẅ",
    "WŴẀẂẄ",
    "yýÿŷỳỵỷỹ",
    "YÝŸŶỲỴỶỸ",
    "zźżž",
    "ZŹŻŽ",
];

/// The fold row holding `c` (the letter and all its accented forms).
fn variants(c: char) -> Option<&'static str> {
    if c.is_ascii() && !c.is_ascii_alphabetic() {
        return None;
    }
    FOLD.iter().copied().find(|row| row.contains(c))
}

/// One character of a term as a regex: itself, or with Match Diacritics off the class of
/// its accented forms.
fn char_pattern(c: char, q: &Query, out: &mut String) {
    match variants(c).filter(|_| !q.match_diacritics) {
        Some(row) => {
            out.push('[');
            out.push_str(row);
            out.push(']');
        }
        None => out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
    }
}

/// A literal term as a regex source: escaped, and with Match Diacritics off each letter
/// matches its accented forms (and an accented letter its plain one).
pub(crate) fn literal(s: &str, q: &Query) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        char_pattern(c, q, &mut out);
    }
    out
}

/// `src` (the pattern of literal `lit`) bounded to whole words: `\b` on each side where
/// the literal starts or ends with a word character (a term like `.txt` keeps its dot
/// side open).
pub(crate) fn bounded(src: &str, lit: &str) -> String {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let edge = |w: bool| if w { r"\b" } else { "" };
    format!(
        "{}(?:{src}){}",
        edge(word(lit.chars().next())),
        edge(word(lit.chars().last()))
    )
}

/// Match Whole Words: one extra regex per literal term that it must also match, as a
/// whole word. Checked per candidate name: a Unicode `\b` in the scan over the whole
/// name buffer would make the scan slow.
pub(crate) fn word_terms(
    literals: &[&str],
    q: &Query,
    regex: &dyn Fn(&str) -> anyhow::Result<Regex>,
) -> anyhow::Result<Vec<Regex>> {
    if !q.whole_word {
        return Ok(Vec::new());
    }
    (literals.iter())
        .map(|s| regex(&bounded(&literal(s, q), s)))
        .collect()
}

/// A `*` / `?` term as a regex. With Match whole filename when using wildcards (the
/// default) the glob is anchored to the whole name; a leading or trailing `*` drops that
/// anchor instead (same matches, and the regex engine can skip ahead on the literal).
/// Without it the glob may match anywhere in the name.
pub(crate) fn glob(term: &str, q: &Query) -> String {
    let whole = q.whole_filename_wildcards;
    let mut re = String::new();
    if whole && !term.starts_with('*') {
        re.push('^');
    }
    for c in term.trim_matches('*').chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => char_pattern(c, q, &mut re),
        }
    }
    if whole && !term.ends_with('*') {
        re.push('$');
    }
    re
}

/// Space-separated terms; `"` toggles quoting (a quoted run is one term).
pub(crate) fn split_terms(text: &str) -> Vec<String> {
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

/// `term` without `prefix` (ASCII case-insensitive), when it starts with it.
pub(crate) fn strip_ci<'a>(term: &'a str, prefix: &str) -> Option<&'a str> {
    term.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &term[prefix.len()..])
}

/// A `function:value` term the highlighter leaves alone (`ext:pdf`, `tag:x`, `in:...`);
/// a drive letter (`C:\x`) is one letter, so it stays a term.
fn is_function(term: &str) -> bool {
    let term = term.strip_prefix('-').unwrap_or(term);
    term.split_once(':').is_some_and(|(key, _)| {
        key.len() >= 2
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// Finds the parts of a name or path that a query matched, for highlighting in the
/// results: the literal terms (with the query's case, diacritics and whole-word
/// toggles), the fixed parts of wildcard terms, and `regex:` terms (or the whole text
/// when the query is a regex). Functions like `ext:` or `in:` are not highlighted.
#[derive(Clone, Debug)]
pub struct Highlighter {
    res: Vec<Regex>,
    /// The terms were matched against the whole path (Match Path): the folder column is
    /// highlighted too.
    pub path: bool,
}

impl Highlighter {
    /// None when nothing in the query can be highlighted.
    pub fn new(q: &Query) -> Option<Self> {
        let build = |src: &str| {
            RegexBuilder::new(src)
                .case_insensitive(!q.match_case)
                .size_limit(1 << 20)
                .build()
                .ok()
        };
        let mut res = Vec::new();
        if q.regex {
            res.extend(build(&q.text));
        } else {
            for term in split_terms(&q.text) {
                let term = strip_ci(&term, "folder:").unwrap_or(&term).to_owned();
                if let Some(re) = strip_ci(&term, "regex:") {
                    res.extend(build(re));
                    continue;
                }
                if term.is_empty() || is_function(&term) {
                    continue;
                }
                if term.contains(['*', '?']) {
                    for part in term.split(['*', '?']).filter(|p| !p.is_empty()) {
                        res.extend(build(&literal(part, q)));
                    }
                } else {
                    let src = literal(&term, q);
                    let src = if q.whole_word {
                        bounded(&src, &term)
                    } else {
                        src
                    };
                    res.extend(build(&src));
                }
            }
        }
        (!res.is_empty()).then_some(Self {
            res,
            path: q.match_path,
        })
    }

    /// Byte ranges of `text` to highlight, ascending, merged where they touch.
    pub fn marks(&self, text: &str) -> Vec<(usize, usize)> {
        let mut marks: Vec<(usize, usize)> = self
            .res
            .iter()
            .flat_map(|re| re.find_iter(text).take(64))
            .filter(|m| !m.is_empty())
            .map(|m| (m.start(), m.end()))
            .collect();
        marks.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(marks.len());
        for (a, b) in marks {
            match out.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> Query {
        Query {
            text: text.into(),
            ..Query::default()
        }
    }

    fn is_match(src: &str, q: &Query, hay: &str) -> bool {
        RegexBuilder::new(src)
            .case_insensitive(!q.match_case)
            .build()
            .unwrap()
            .is_match(hay)
    }

    #[test]
    fn diacritics_fold_both_ways_unless_matched() {
        let fold = q("");
        assert!(is_match(&literal("cafe", &fold), &fold, "Café.txt"));
        assert!(is_match(&literal("café", &fold), &fold, "cafe.txt"));
        assert!(is_match(&literal("RESUME", &fold), &fold, "résumé.pdf"));
        let exact = Query {
            match_diacritics: true,
            ..q("")
        };
        assert!(!is_match(&literal("cafe", &exact), &exact, "café.txt"));
        assert!(is_match(&literal("café", &exact), &exact, "café.txt"));
        // Case and diacritics are separate toggles.
        let case = Query {
            match_case: true,
            ..q("")
        };
        assert!(is_match(&literal("Cafe", &case), &case, "Café"));
        assert!(!is_match(&literal("cafe", &case), &case, "Café"));
        // Pattern characters stay literal.
        assert!(!is_match(&literal("a.c", &fold), &fold, "abc"));
        assert!(is_match(&literal("a.c", &fold), &fold, "a.c"));
    }

    #[test]
    fn whole_words_keep_punctuation_sides_open() {
        let w = |lit: &str, hay: &str| is_match(&bounded(&regex::escape(lit), lit), &q(""), hay);
        assert!(w("report", "my report.pdf"));
        assert!(!w("report", "reportage.pdf"));
        assert!(!w("port", "report.pdf"));
        assert!(w(".pdf", "report.pdf"));
        assert!(!w("report", "report_2026"));
    }

    #[test]
    fn wildcards_match_the_whole_name_unless_switched_off() {
        let whole = q("");
        assert!(is_match(&glob("*.pdf", &whole), &whole, "a.pdf"));
        assert!(!is_match(&glob("*.pd", &whole), &whole, "a.pdf"));
        assert!(!is_match(&glob("rep?rt", &whole), &whole, "my report"));
        let part = Query {
            whole_filename_wildcards: false,
            ..q("")
        };
        assert!(is_match(&glob("*.pd", &part), &part, "a.pdf"));
        assert!(is_match(&glob("rep?rt", &part), &part, "my report"));
    }

    #[test]
    fn highlighter_marks_terms_and_skips_functions() {
        let h = Highlighter::new(&q("rep ext:pdf *.PDF in:C:\\x")).unwrap();
        assert_eq!(h.marks("My Report.pdf"), [(3, 6), (9, 13)]);
        assert!(!h.path);
        // Overlapping terms merge.
        let h = Highlighter::new(&q("repo port")).unwrap();
        assert_eq!(h.marks("report"), [(0, 6)]);
        // Diacritics fold here too; byte ranges cover the accented letter.
        let h = Highlighter::new(&q("cafe")).unwrap();
        assert_eq!(h.marks("Café.txt"), [(0, 5)]);
        // Whole words and regex mode.
        let h = Highlighter::new(&Query {
            whole_word: true,
            ..q("port")
        })
        .unwrap();
        assert_eq!(h.marks("report port"), [(7, 11)]);
        let h = Highlighter::new(&Query {
            regex: true,
            match_path: true,
            ..q(r"\d+")
        })
        .unwrap();
        assert_eq!(h.marks("a12b3"), [(1, 3), (4, 5)]);
        assert!(h.path);
        assert!(Highlighter::new(&q("ext:pdf folder:")).is_none());
        assert!(Highlighter::new(&q("regex:(")).is_none(), "bad regex");
    }
}
