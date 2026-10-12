//! Query text to tokens: terms, `|`, `!`, `<`, `>` and (with **Allow literal operators**)
//! the words `AND`, `OR` and `NOT`. Every character of a term keeps its byte offset in the
//! query and whether it was inside `"quotes"`, so later stages can report positions and
//! leave quoted `;`, `|`, `!`, `<`, `>` and spaces alone.

use std::ops::Range;

/// One character of a term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ch {
    pub c: char,
    pub quoted: bool,
    pub at: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    /// A term; `literal` is set for `function*:` (the rest of the query is its text).
    Term {
        chars: Vec<Ch>,
        span: Range<usize>,
        literal: bool,
    },
    Or(usize),
    And(usize),
    Not(usize),
    Open(usize),
    Close(usize),
}

/// Splits `src` into tokens. `is_function` tells a `name*:` literal-text function from a
/// term that merely contains `*:`.
pub(crate) fn lex(
    src: &str,
    literal_operators: bool,
    is_function: &dyn Fn(&str) -> bool,
) -> Vec<Tok> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let n = chars.len();
    let mut toks = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < n {
        let (at, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '|' => {
                toks.push(Tok::Or(at));
                i += 1;
                continue;
            }
            '!' => {
                toks.push(Tok::Not(at));
                i += 1;
                continue;
            }
            '<' => {
                toks.push(Tok::Open(at));
                depth += 1;
                i += 1;
                continue;
            }
            '>' if depth > 0 => {
                toks.push(Tok::Close(at));
                depth -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        let start = at;
        let mut out: Vec<Ch> = Vec::new();
        let mut quoted = false;
        let mut literal = false;
        while i < n {
            let (at, c) = chars[i];
            if c == '"' && !literal {
                quoted = !quoted;
                i += 1;
                continue;
            }
            if !quoted {
                if c.is_whitespace() || c == '|' {
                    break;
                }
                // `>` closes a group unless it is a comparison right after `function:`.
                if c == '>'
                    && depth > 0
                    && out.last().map(|l| (l.c, l.quoted)) != Some((':', false))
                {
                    break;
                }
                if c == '*'
                    && chars.get(i + 1).map(|x| x.1) == Some(':')
                    && ends_with_function(&out, is_function)
                {
                    out.push(Ch {
                        c: '*',
                        quoted: false,
                        at,
                    });
                    out.push(Ch {
                        c: ':',
                        quoted: false,
                        at: chars[i + 1].0,
                    });
                    // The rest of the query, as typed, is the function's text.
                    for &(at, c) in &chars[i + 2..] {
                        out.push(Ch {
                            c,
                            quoted: true,
                            at,
                        });
                    }
                    i = n;
                    literal = true;
                    break;
                }
            }
            out.push(Ch { c, quoted, at });
            i += 1;
        }
        let end = if i < n { chars[i].0 } else { src.len() };
        if literal_operators && !out.is_empty() && out.iter().all(|c| !c.quoted) {
            let word: String = out.iter().map(|c| c.c).collect();
            match word.as_str() {
                "AND" => {
                    toks.push(Tok::And(start));
                    continue;
                }
                "OR" => {
                    toks.push(Tok::Or(start));
                    continue;
                }
                "NOT" => {
                    toks.push(Tok::Not(start));
                    continue;
                }
                _ => {}
            }
        }
        if out.is_empty() && end == start {
            // Only reachable for a stray character the loop above refused; skip it.
            i += 1;
            continue;
        }
        toks.push(Tok::Term {
            chars: out,
            span: start..end,
            literal,
        });
    }
    toks
}

/// Whether the unquoted text after the last `:` of `out` (or all of it) names a function.
fn ends_with_function(out: &[Ch], is_function: &dyn Fn(&str) -> bool) -> bool {
    if out.iter().any(|c| c.quoted) {
        return false;
    }
    let from = out.iter().rposition(|c| c.c == ':').map_or(0, |p| p + 1);
    let name: String = out[from..]
        .iter()
        .map(|c| c.c.to_ascii_lowercase())
        .collect();
    !name.is_empty() && is_function(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<String> {
        lex(s, true, &|n| n == "name")
            .into_iter()
            .map(|t| match t {
                Tok::Term { chars, literal, .. } => {
                    let s: String = chars.iter().map(|c| c.c).collect();
                    if literal {
                        format!("L({s})")
                    } else {
                        format!("T({s})")
                    }
                }
                Tok::Or(_) => "|".into(),
                Tok::And(_) => "&".into(),
                Tok::Not(_) => "!".into(),
                Tok::Open(_) => "<".into(),
                Tok::Close(_) => ">".into(),
            })
            .collect()
    }

    #[test]
    fn operators_and_groups() {
        assert_eq!(kinds("a b|c"), ["T(a)", "T(b)", "|", "T(c)"]);
        assert_eq!(kinds("!<a | b>"), ["!", "<", "T(a)", "|", "T(b)", ">"]);
        assert_eq!(kinds("<size:>1mb>"), ["<", "T(size:>1mb)", ">"]);
        assert_eq!(kinds("a>b"), ["T(a>b)"]);
        assert_eq!(kinds("\"a b|c\""), ["T(a b|c)"]);
        assert_eq!(
            kinds("a AND b OR NOT c"),
            ["T(a)", "&", "T(b)", "|", "!", "T(c)"]
        );
        assert_eq!(kinds("name*:a \"b\" | c"), ["L(name*:a \"b\" | c)"]);
        assert_eq!(kinds("x*:a b"), ["T(x*:a)", "T(b)"]);
    }

    #[test]
    fn offsets() {
        let toks = lex("ab  \"c d\"", false, &|_| false);
        match &toks[1] {
            Tok::Term { chars, span, .. } => {
                assert_eq!(span, &(4..9));
                assert_eq!(
                    chars[0],
                    Ch {
                        c: 'c',
                        quoted: true,
                        at: 5
                    }
                );
            }
            t => panic!("{t:?}"),
        }
    }
}
