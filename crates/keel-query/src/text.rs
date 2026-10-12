//! Text matching: case and accent folding, punctuation and space removal, wildcards,
//! whole words, prefix/suffix, start/end/whole, regular expressions and Soundex.

use std::borrow::Cow;

use regex::{Regex, RegexBuilder};

/// Compiled regexes may not grow past this (bytes of program); a typed query is bounded.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// How text is normalized before it is compared. Both sides go through the same fold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Fold {
    pub case: bool,
    pub diacritics: bool,
    pub punct: bool,
    pub ws: bool,
}

impl Fold {
    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if self.case && self.diacritics && !self.punct && !self.ws {
            return Cow::Borrowed(s);
        }
        if s.is_ascii() && !self.punct && !self.ws {
            return if self.case || !s.bytes().any(|b| b.is_ascii_uppercase()) {
                Cow::Borrowed(s)
            } else {
                Cow::Owned(s.to_ascii_lowercase())
            };
        }
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if self.punct && is_punct(c) || self.ws && c.is_whitespace() {
                continue;
            }
            if self.case {
                self.push(c, &mut out);
            } else {
                for l in c.to_lowercase() {
                    self.push(l, &mut out);
                }
            }
        }
        Cow::Owned(out)
    }

    fn push(&self, c: char, out: &mut String) {
        if self.diacritics {
            out.push(c);
        } else if !is_combining(c) {
            out.push(strip_accent(c));
        }
    }
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(c, '\u{a1}'..='\u{bf}' | '\u{2010}'..='\u{2027}' | '\u{2030}'..='\u{205e}' | '\u{3000}'..='\u{303f}')
}

fn is_combining(c: char) -> bool {
    matches!(c, '\u{300}'..='\u{36f}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}' | '\u{20d0}'..='\u{20ff}' | '\u{fe20}'..='\u{fe2f}')
}

/// Base letters of U+00C0..U+017F (`.` = no plain base letter, kept as is).
const LATIN1: &[u8; 64] = b"AAAAAA.CEEEEIIIIDNOOOOO.OUUUUY..aaaaaa.ceeeeiiiidnooooo.ouuuuy.y";
const LATIN_EXT_A: &[u8; 128] = b"AaAaAaCcCcCcCcDdDdEeEeEeEeEeGgGgGgGgHhHhIiIiIiIiIi..JjKk.LlLlLlLlLlNnNnNn...OoOoOo..RrRrRrSsSsSsSsTtTtTtUuUuUuUuUuUuWwYyYZzZzZz.";

fn strip_accent(c: char) -> char {
    let cp = c as u32;
    let base = match cp {
        0xc0..=0xff => LATIN1[(cp - 0xc0) as usize],
        0x100..=0x17f => LATIN_EXT_A[(cp - 0x100) as usize],
        _ => b'.',
    };
    if base == b'.' {
        c
    } else {
        base as char
    }
}

/// American Soundex of the letters of `s` (`Robert` → `R163`); empty without letters.
pub(crate) fn soundex(s: &str) -> String {
    fn code(c: char) -> u8 {
        match c {
            'b' | 'f' | 'p' | 'v' => b'1',
            'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => b'2',
            'd' | 't' => b'3',
            'l' => b'4',
            'm' | 'n' => b'5',
            'r' => b'6',
            'h' | 'w' => b'h',
            _ => b'0',
        }
    }
    let fold = Fold::default();
    let letters: Vec<char> = fold
        .apply(s)
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    let Some(&first) = letters.first() else {
        return String::new();
    };
    let mut out = String::from(first.to_ascii_uppercase());
    let mut last = code(first);
    for &c in &letters[1..] {
        let d = code(c);
        if d == b'h' {
            continue;
        }
        if d != b'0' && d != last {
            out.push(d as char);
            if out.len() == 4 {
                break;
            }
        }
        last = d;
    }
    while out.len() < 4 {
        out.push('0');
    }
    out
}

/// Placement of a text inside the value.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Anchors {
    pub whole: bool,
    pub start: bool,
    pub end: bool,
    pub whole_words: bool,
    pub prefix: bool,
    pub suffix: bool,
}

/// Wildcard handling.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Wild {
    /// `*` and `?` are wildcards.
    pub on: bool,
    /// A wildcard pattern must match the whole value.
    pub whole: bool,
    /// `*` crosses `\` (otherwise only `**` does).
    pub star_all: bool,
}

/// User regex flags.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RegexFlags {
    pub case: bool,
    pub multi_line: bool,
    pub dot_all: bool,
    pub ungreedy: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum TextMatch {
    Contains(String),
    Equals(String),
    StartsWith(String),
    EndsWith(String),
    /// Built from wildcards or word anchors, run on folded text.
    Pattern(Regex),
    /// A `regex:` term, run on the text as it is.
    UserRegex(Regex),
    Soundex(String),
}

pub(crate) fn user_regex(src: &str, flags: RegexFlags) -> Result<Regex, String> {
    RegexBuilder::new(src)
        .case_insensitive(!flags.case)
        .multi_line(flags.multi_line)
        .dot_matches_new_line(flags.dot_all)
        .swap_greed(flags.ungreedy)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| match e {
            regex::Error::CompiledTooBig(_) => "the regular expression is too large".to_string(),
            e => format!(
                "bad regular expression: {}",
                e.to_string().lines().last().unwrap_or("")
            ),
        })
}

const WORD_LEFT: &str = r"(?:^|[^\p{L}\p{N}_])";
const WORD_RIGHT: &str = r"(?:[^\p{L}\p{N}_]|$)";

/// Compiles `pattern` (already free of operators) into a matcher on folded text.
pub(crate) fn build(
    pattern: &str,
    fold: Fold,
    a: Anchors,
    wild: Wild,
) -> Result<TextMatch, String> {
    let has_wild = wild.on && pattern.contains(['*', '?']);
    if !has_wild {
        let needle = fold.apply(pattern).into_owned();
        let words = a.whole_words || a.prefix || a.suffix;
        if !words {
            return Ok(if a.whole || a.start && a.end {
                TextMatch::Equals(needle)
            } else if a.start {
                TextMatch::StartsWith(needle)
            } else if a.end {
                TextMatch::EndsWith(needle)
            } else {
                TextMatch::Contains(needle)
            });
        }
        return pattern_regex(&regex::escape(&needle), a, false);
    }
    let mut body = String::new();
    let mut lit = String::new();
    let mut chars = pattern.chars().peekable();
    let flush = |lit: &mut String, body: &mut String| {
        if !lit.is_empty() {
            body.push_str(&regex::escape(&fold.apply(lit)));
            lit.clear();
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                flush(&mut lit, &mut body);
                if chars.peek() == Some(&'*') {
                    chars.next();
                    body.push_str(".*");
                } else if wild.star_all {
                    body.push_str(".*");
                } else {
                    body.push_str(r"[^\\]*");
                }
            }
            '?' => {
                flush(&mut lit, &mut body);
                body.push_str(r"[^\\]");
            }
            c => lit.push(c),
        }
    }
    flush(&mut lit, &mut body);
    pattern_regex(&body, a, wild.whole)
}

fn pattern_regex(body: &str, a: Anchors, whole_default: bool) -> Result<TextMatch, String> {
    let words = a.whole_words || a.prefix || a.suffix;
    let left = if a.whole || a.start || whole_default && !words {
        "^"
    } else if a.whole_words || a.prefix {
        WORD_LEFT
    } else {
        ""
    };
    let right = if a.whole || a.end || whole_default && !words {
        "$"
    } else if a.whole_words || a.suffix {
        WORD_RIGHT
    } else {
        ""
    };
    RegexBuilder::new(&format!("(?s){left}(?:{body}){right}"))
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map(TextMatch::Pattern)
        .map_err(|_| "the pattern is too large".to_string())
}

impl TextMatch {
    /// `hay` is already folded (raw for [`TextMatch::UserRegex`] and Soundex).
    pub fn is_match(&self, hay: &str) -> bool {
        match self {
            TextMatch::Contains(n) => hay.contains(n.as_str()),
            TextMatch::Equals(n) => hay == n,
            TextMatch::StartsWith(n) => hay.starts_with(n.as_str()),
            TextMatch::EndsWith(n) => hay.ends_with(n.as_str()),
            TextMatch::Pattern(r) | TextMatch::UserRegex(r) => r.is_match(hay),
            TextMatch::Soundex(code) => !code.is_empty() && soundex(hay) == *code,
        }
    }

    /// Whether the value is compared unfolded.
    pub fn raw(&self) -> bool {
        matches!(self, TextMatch::UserRegex(_) | TextMatch::Soundex(_))
    }
}

/// Decodes `hex:` text (pairs of hex digits, spaces allowed) as UTF-8.
pub(crate) fn decode_hex(s: &str) -> Result<String, String> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err("hex: needs pairs of hex digits".into());
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| "hex: needs hex digits".to_string())?;
        bytes.push(u8::from_str_radix(s, 16).map_err(|_| format!("hex: `{s}` is not a hex byte"))?);
    }
    String::from_utf8(bytes).map_err(|_| "hex: the bytes are not UTF-8 text".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ci() -> Fold {
        Fold::default()
    }

    #[test]
    fn folding() {
        assert_eq!(ci().apply("Résumé ÀÉÎÕÜ"), "resume aeiou");
        assert_eq!(ci().apply("Łódź"), "lodz");
        let keep = Fold {
            diacritics: true,
            ..Fold::default()
        };
        assert_eq!(keep.apply("Résumé"), "résumé");
        let p = Fold {
            punct: true,
            ws: true,
            ..Fold::default()
        };
        assert_eq!(p.apply("a-b c_d.e"), "abcde");
        assert_eq!(ci().apply("e\u{301}"), "e");
    }

    #[test]
    fn soundex_codes() {
        assert_eq!(soundex("Robert"), "R163");
        assert_eq!(soundex("Rupert"), "R163");
        assert_eq!(soundex("Tymczak"), "T522");
        assert_eq!(soundex("Ashcraft"), "A261");
        assert_eq!(soundex("123"), "");
    }

    #[test]
    fn wildcards() {
        let w = Wild {
            on: true,
            whole: true,
            star_all: false,
        };
        let m = build("*.TXT", ci(), Anchors::default(), w).unwrap();
        assert!(m.is_match("a.txt"));
        assert!(!m.is_match("a.txt.bak"));
        assert!(!m.is_match("x\\a.txt"));
        let m = build("**.txt", ci(), Anchors::default(), w).unwrap();
        assert!(m.is_match("x\\a.txt"));
        let m = build("a?c", ci(), Anchors::default(), w).unwrap();
        assert!(m.is_match("abc") && !m.is_match("abcd"));
    }

    #[test]
    fn regex_flags() {
        let re = |src: &str, f: RegexFlags| user_regex(src, f).unwrap();
        assert!(!re("a.b", RegexFlags::default()).is_match("a\nb"));
        assert!(re(
            "a.b",
            RegexFlags {
                dot_all: true,
                ..RegexFlags::default()
            }
        )
        .is_match("a\nb"));
        assert!(!re("^b", RegexFlags::default()).is_match("a\nb"));
        assert!(re(
            "^b",
            RegexFlags {
                multi_line: true,
                ..RegexFlags::default()
            }
        )
        .is_match("a\nb"));
        assert!(!re(
            "A",
            RegexFlags {
                case: true,
                ..RegexFlags::default()
            }
        )
        .is_match("a"));
        let lazy = re(
            "a+",
            RegexFlags {
                ungreedy: true,
                ..RegexFlags::default()
            },
        );
        assert_eq!(lazy.find("aaa").map(|m| m.as_str()), Some("a"));
        assert!(user_regex("(", RegexFlags::default()).is_err());
        assert!(user_regex("(a{1000}){1000}", RegexFlags::default()).is_err());
    }

    #[test]
    fn hex() {
        assert_eq!(decode_hex("4b 65 65 6c").unwrap(), "Keel");
        assert!(decode_hex("4").is_err());
        assert!(decode_hex("zz").is_err());
        assert!(decode_hex("ff").is_err());
    }
}
