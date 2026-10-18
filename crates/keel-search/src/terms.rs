//! What the search toggles do to a word when it is only highlighted, and the wildcard
//! rule of file list creation: diacritic folding (Match Diacritics off: `cafe` finds
//! `café`), whole-word bounds (Match Whole Words), whole-filename wildcards and the
//! [`Highlighter`]. What matches is decided by `keel-query` (`crate::language`).

use regex::{Regex, RegexBuilder};

use crate::language::Mark;
use crate::Query;

/// Each row is a base letter followed by the same letter with diacritics; lower and upper
/// case rows apart, so Match Case still tells them apart. Written as escapes: the app's
/// glyph test scans string literals for characters its fonts must draw.
const FOLD: &[&str] = &[
    "a\u{e0}\u{e1}\u{e2}\u{e3}\u{e4}\u{e5}\u{101}\u{103}\u{105}\u{1ce}\u{201}\u{203}\u{227}\u{1ea1}\u{1ea3}\u{1ea5}\u{1ea7}\u{1ea9}\u{1eab}\u{1ead}\u{1eaf}\u{1eb1}\u{1eb3}\u{1eb5}\u{1eb7}",
    "A\u{c0}\u{c1}\u{c2}\u{c3}\u{c4}\u{c5}\u{100}\u{102}\u{104}\u{1cd}\u{200}\u{202}\u{226}\u{1ea0}\u{1ea2}\u{1ea4}\u{1ea6}\u{1ea8}\u{1eaa}\u{1eac}\u{1eae}\u{1eb0}\u{1eb2}\u{1eb4}\u{1eb6}",
    "c\u{e7}\u{107}\u{109}\u{10b}\u{10d}",
    "C\u{c7}\u{106}\u{108}\u{10a}\u{10c}",
    "d\u{10f}\u{111}",
    "D\u{10e}\u{110}",
    "e\u{e8}\u{e9}\u{ea}\u{eb}\u{113}\u{115}\u{117}\u{119}\u{11b}\u{205}\u{207}\u{1eb9}\u{1ebb}\u{1ebd}\u{1ebf}\u{1ec1}\u{1ec3}\u{1ec5}\u{1ec7}",
    "E\u{c8}\u{c9}\u{ca}\u{cb}\u{112}\u{114}\u{116}\u{118}\u{11a}\u{204}\u{206}\u{1eb8}\u{1eba}\u{1ebc}\u{1ebe}\u{1ec0}\u{1ec2}\u{1ec4}\u{1ec6}",
    "g\u{11d}\u{11f}\u{121}\u{123}\u{1e7}",
    "G\u{11c}\u{11e}\u{120}\u{122}\u{1e6}",
    "h\u{125}\u{127}",
    "H\u{124}\u{126}",
    "i\u{ec}\u{ed}\u{ee}\u{ef}\u{129}\u{12b}\u{12d}\u{12f}\u{1d0}\u{209}\u{20b}\u{1ecb}\u{1ec9}",
    "I\u{cc}\u{cd}\u{ce}\u{cf}\u{128}\u{12a}\u{12c}\u{12e}\u{130}\u{1cf}\u{208}\u{20a}\u{1eca}\u{1ec8}",
    "j\u{135}",
    "J\u{134}",
    "k\u{137}\u{1e9}",
    "K\u{136}\u{1e8}",
    "l\u{13a}\u{13c}\u{13e}\u{140}\u{142}",
    "L\u{139}\u{13b}\u{13d}\u{13f}\u{141}",
    "n\u{f1}\u{144}\u{146}\u{148}\u{1f9}",
    "N\u{d1}\u{143}\u{145}\u{147}\u{1f8}",
    "o\u{f2}\u{f3}\u{f4}\u{f5}\u{f6}\u{f8}\u{14d}\u{14f}\u{151}\u{1a1}\u{1d2}\u{20d}\u{20f}\u{1ecd}\u{1ecf}\u{1ed1}\u{1ed3}\u{1ed5}\u{1ed7}\u{1ed9}\u{1edb}\u{1edd}\u{1edf}\u{1ee1}\u{1ee3}",
    "O\u{d2}\u{d3}\u{d4}\u{d5}\u{d6}\u{d8}\u{14c}\u{14e}\u{150}\u{1a0}\u{1d1}\u{20c}\u{20e}\u{1ecc}\u{1ece}\u{1ed0}\u{1ed2}\u{1ed4}\u{1ed6}\u{1ed8}\u{1eda}\u{1edc}\u{1ede}\u{1ee0}\u{1ee2}",
    "r\u{155}\u{157}\u{159}\u{211}\u{213}",
    "R\u{154}\u{156}\u{158}\u{210}\u{212}",
    "s\u{15b}\u{15d}\u{15f}\u{161}\u{219}",
    "S\u{15a}\u{15c}\u{15e}\u{160}\u{218}",
    "t\u{163}\u{165}\u{167}\u{21b}",
    "T\u{162}\u{164}\u{166}\u{21a}",
    "u\u{f9}\u{fa}\u{fb}\u{fc}\u{169}\u{16b}\u{16d}\u{16f}\u{171}\u{173}\u{1b0}\u{1d4}\u{215}\u{217}\u{1ee5}\u{1ee7}\u{1ee9}\u{1eeb}\u{1eed}\u{1eef}\u{1ef1}",
    "U\u{d9}\u{da}\u{db}\u{dc}\u{168}\u{16a}\u{16c}\u{16e}\u{170}\u{172}\u{1af}\u{1d3}\u{214}\u{216}\u{1ee4}\u{1ee6}\u{1ee8}\u{1eea}\u{1eec}\u{1eee}\u{1ef0}",
    "w\u{175}\u{1e81}\u{1e83}\u{1e85}",
    "W\u{174}\u{1e80}\u{1e82}\u{1e84}",
    "y\u{fd}\u{ff}\u{177}\u{1ef3}\u{1ef5}\u{1ef7}\u{1ef9}",
    "Y\u{dd}\u{178}\u{176}\u{1ef2}\u{1ef4}\u{1ef6}\u{1ef8}",
    "z\u{17a}\u{17c}\u{17e}",
    "Z\u{179}\u{17b}\u{17d}",
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
        for mark in crate::language::marked_terms(q) {
            match mark {
                Mark::Regex(src) => res.extend(build(&src)),
                Mark::Text(term) => {
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
