//! Tokens to [`Ast`]: operators with Everything's precedence (OR before AND by default),
//! `< >` groups, modifier and function prefixes, value lists and comparison operators,
//! filter and user macros. Every error carries the byte range it is about.

use std::ops::Range;

use crate::functions::{self, FnKind, TextProp};
use crate::lex::{lex, Ch, Tok};
use crate::modifiers::{self, Modifier};
use crate::values::CmpOp;
use crate::{Error, ErrorKind, Options};

/// Groups may nest this deep.
pub const MAX_GROUP_DEPTH: usize = 32;
/// Macros may expand inside macros this deep.
pub const MAX_MACRO_DEPTH: usize = 8;

/// Which operator binds first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Precedence {
    /// `a b|c` is `a <b|c>` (Everything's default).
    #[default]
    OrFirst,
    /// `a b|c` is `<a b>|c`.
    AndFirst,
}

/// How selected filters combine (`and-filter:`, `or-filter:`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterCombine {
    And,
    Or,
}

/// A UI-state function (`sort:size`, `columns:name;size`), for the search UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiDirective {
    pub name: String,
    pub value: String,
}

/// Settings of the whole query, from global modifiers and UI functions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Globals {
    pub precedence: Precedence,
    /// `no-reorder:`, or a query that uses `$0:`..`$9:` captures.
    pub no_reorder: bool,
    pub search_current_results: Option<bool>,
    pub result_omissions: Option<bool>,
    pub temp_result_omissions: Option<bool>,
    pub filter_combine: Option<FilterCombine>,
    pub menu_break: bool,
    pub menu_separator: bool,
    pub ui: Vec<UiDirective>,
}

/// A parsed query.
#[derive(Clone, Debug, PartialEq)]
pub struct Ast {
    pub expr: Expr,
    pub globals: Globals,
}

/// An expression. `And(vec![])` matches everything.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Term(Term),
}

/// One term: modifiers, an optional function and its values.
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    pub span: Range<usize>,
    /// Per-term modifiers in the order typed (global ones are in [`Globals`]).
    pub modifiers: Vec<Modifier>,
    pub function: Option<Function>,
    /// A plain term has one item; a function has one per `;`-separated value (none for
    /// `size:` alone: "any non-zero / non-empty value").
    pub items: Vec<Item>,
}

/// The function of a term.
#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    /// As typed, lower case, without `*`.
    pub name: String,
    pub kind: FnKind,
    /// `name*:`: the rest of the query is the text.
    pub literal: bool,
    pub span: Range<usize>,
}

/// One value of a term.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub op: Option<CmpOp>,
    pub text: String,
    pub span: Range<usize>,
}

/// Parses `src`.
pub fn parse(src: &str, opts: &Options) -> Result<Ast, Error> {
    parse_at(src, opts, 0, None)
}

fn parse_at(
    src: &str,
    opts: &Options,
    macro_depth: usize,
    remap: Option<Range<usize>>,
) -> Result<Ast, Error> {
    let is_function = |n: &str| functions::lookup(n).is_some();
    let toks = lex(src, opts.allow_literal_operators, &is_function);
    let mut globals = Globals {
        precedence: opts.precedence,
        ..Globals::default()
    };
    // Precedence is read before parsing; the last one typed wins.
    for t in &toks {
        if let Tok::Term { chars, .. } = t {
            let mut i = 0;
            while let Some((name, next, _)) = prefix_at(chars, i) {
                match modifiers::lookup(&name.to_lowercase()) {
                    Some(Modifier::AndPrecedence) => globals.precedence = Precedence::AndFirst,
                    Some(Modifier::OrPrecedence) => globals.precedence = Precedence::OrFirst,
                    Some(_) => {}
                    None => break,
                }
                i = next;
            }
        }
    }
    let mut p = Parser {
        toks,
        pos: 0,
        opts,
        globals,
        depth: 0,
        macro_depth,
        remap,
    };
    let expr = p.seq()?;
    if let Some(t) = p.toks.get(p.pos) {
        return Err(p.err(
            tok_at(t)..tok_at(t) + 1,
            ErrorKind::Syntax,
            "`>` without a group to close".into(),
        ));
    }
    Ok(Ast {
        expr,
        globals: p.globals,
    })
}

fn tok_at(t: &Tok) -> usize {
    match t {
        Tok::Term { span, .. } => span.start,
        Tok::Or(a) | Tok::And(a) | Tok::Not(a) | Tok::Open(a) | Tok::Close(a) => *a,
    }
}

/// `name:` at `chars[i..]`: the lower-case-able name, the index after the `:`, and its span.
fn prefix_at(chars: &[Ch], i: usize) -> Option<(String, usize, Range<usize>)> {
    let mut j = i;
    while j < chars.len() {
        let c = chars[j];
        if c.quoted {
            return None;
        }
        if c.c == ':' {
            break;
        }
        if !(c.c.is_ascii_alphanumeric()
            || matches!(c.c, '-' | '_' | '!' | '#' | '^' | '$' | '\\' | '*'))
        {
            return None;
        }
        j += 1;
    }
    if j == i || j >= chars.len() {
        return None;
    }
    let name: String = chars[i..j].iter().map(|c| c.c).collect();
    Some((name, j + 1, chars[i].at..chars[j].at + 1))
}

/// A name that is meant as a function (so a typo is an error, not text): a letter, then
/// letters, digits, `-` or `_`, two characters at least (`C:` stays a drive).
fn looks_like_name(s: &str) -> bool {
    let mut cs = s.chars();
    s.len() >= 2
        && cs.next().is_some_and(|c| c.is_ascii_alphabetic())
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Everything's character macros: `quot:` `apos:` `amp:` `lt:` `gt:` `#N:` `#xN:`.
fn text_macro(name: &str) -> Option<char> {
    match name {
        "quot" => Some('"'),
        "apos" => Some('\''),
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        _ => {
            let n = name.strip_prefix('#')?;
            let code = match n.strip_prefix('x') {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(code).filter(|c| *c != '\0')
        }
    }
}

fn text_of(chars: &[Ch]) -> String {
    chars.iter().map(|c| c.c).collect()
}

fn span_of(chars: &[Ch], fallback: &Range<usize>) -> Range<usize> {
    match (chars.first(), chars.last()) {
        (Some(a), Some(b)) => a.at..b.at + b.c.len_utf8(),
        _ => fallback.end..fallback.end,
    }
}

/// An operator at the start of `chars` (unquoted only).
fn split_op(chars: &[Ch]) -> (Option<CmpOp>, &[Ch]) {
    let head: String = chars
        .iter()
        .take(2)
        .take_while(|c| !c.quoted)
        .map(|c| c.c)
        .collect();
    let (op, rest) = CmpOp::split(&head);
    let used = head.len() - rest.len();
    (op, &chars[used..])
}

enum MacroHit {
    Builtin(&'static str),
    User(String),
}

struct Parser<'o> {
    toks: Vec<Tok>,
    pos: usize,
    opts: &'o Options,
    globals: Globals,
    depth: usize,
    macro_depth: usize,
    remap: Option<Range<usize>>,
}

impl Parser<'_> {
    fn err(&self, span: Range<usize>, kind: ErrorKind, message: String) -> Error {
        Error {
            span: self.remap.clone().unwrap_or(span),
            kind,
            message,
        }
    }

    fn sp(&self, span: Range<usize>) -> Range<usize> {
        self.remap.clone().unwrap_or(span)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn at_end(&self) -> bool {
        matches!(self.peek(), None | Some(Tok::Close(_)))
    }

    fn seq(&mut self) -> Result<Expr, Error> {
        match self.globals.precedence {
            Precedence::OrFirst => self.seq_or_first(),
            Precedence::AndFirst => self.seq_and_first(),
        }
    }

    /// `a b|c` = `a <b|c>`.
    fn seq_or_first(&mut self) -> Result<Expr, Error> {
        let mut all = Vec::new();
        while !self.at_end() {
            match self.peek() {
                Some(Tok::Or(_)) | Some(Tok::And(_)) => self.pos += 1,
                _ => {
                    let mut alts = Vec::new();
                    alts.extend(self.unary()?);
                    while let Some(Tok::Or(_)) = self.peek() {
                        self.pos += 1;
                        alts.extend(self.unary()?);
                    }
                    all.extend(one_or(alts, Expr::Or));
                }
            }
        }
        Ok(one_or(all, Expr::And).unwrap_or(Expr::And(Vec::new())))
    }

    /// `a b|c` = `<a b>|c`.
    fn seq_and_first(&mut self) -> Result<Expr, Error> {
        let mut alts = Vec::new();
        let mut cur = Vec::new();
        while !self.at_end() {
            match self.peek() {
                Some(Tok::Or(_)) => {
                    self.pos += 1;
                    alts.extend(one_or(std::mem::take(&mut cur), Expr::And));
                }
                Some(Tok::And(_)) => self.pos += 1,
                _ => cur.extend(self.unary()?),
            }
        }
        alts.extend(one_or(cur, Expr::And));
        Ok(one_or(alts, Expr::Or).unwrap_or(Expr::And(Vec::new())))
    }

    /// `!`* then a group or a term; `None` for nothing (a dangling `!`, an empty group, a
    /// term of global modifiers only).
    fn unary(&mut self) -> Result<Option<Expr>, Error> {
        let mut negate = false;
        while let Some(Tok::Not(_)) = self.peek() {
            negate = !negate;
            self.pos += 1;
        }
        let e = match self.peek().cloned() {
            None | Some(Tok::Close(_)) | Some(Tok::Or(_)) | Some(Tok::And(_))
            | Some(Tok::Not(_)) => None,
            Some(Tok::Open(at)) => {
                self.pos += 1;
                if self.depth >= MAX_GROUP_DEPTH {
                    return Err(self.err(
                        at..at + 1,
                        ErrorKind::Syntax,
                        "groups are nested too deeply".into(),
                    ));
                }
                self.depth += 1;
                let inner = self.seq()?;
                self.depth -= 1;
                match self.peek() {
                    Some(Tok::Close(_)) => self.pos += 1,
                    _ => {
                        return Err(self.err(
                            at..at + 1,
                            ErrorKind::Syntax,
                            "this `<` has no closing `>`".into(),
                        ));
                    }
                }
                match inner {
                    Expr::And(v) if v.is_empty() => None,
                    e => Some(e),
                }
            }
            Some(Tok::Term {
                chars,
                span,
                literal,
            }) => {
                self.pos += 1;
                self.term(&chars, span, literal)?
            }
        };
        Ok(e.map(|e| if negate { not(e) } else { e }))
    }

    fn global(&mut self, m: Modifier) {
        let g = &mut self.globals;
        match m {
            Modifier::NoReorder => g.no_reorder = true,
            Modifier::SearchCurrentResults(b) => g.search_current_results = Some(b),
            Modifier::AndFilter => g.filter_combine = Some(FilterCombine::And),
            Modifier::OrFilter => g.filter_combine = Some(FilterCombine::Or),
            Modifier::ResultOmissions(b) => g.result_omissions = Some(b),
            Modifier::TempResultOmissions(b) => g.temp_result_omissions = Some(b),
            Modifier::MenuBreak => g.menu_break = true,
            Modifier::MenuSeparator => g.menu_separator = true,
            _ => {}
        }
    }

    fn term(
        &mut self,
        chars: &[Ch],
        span: Range<usize>,
        literal: bool,
    ) -> Result<Option<Expr>, Error> {
        let mut mods: Vec<Modifier> = Vec::new();
        let mut regex_mode = self.opts.regex;
        let mut lead = String::new();
        let mut function = None;
        let mut hit = None;
        let mut i = 0;
        while let Some((name, next, nspan)) = prefix_at(chars, i) {
            let lower = name.to_lowercase();
            if let Some(m) = modifiers::lookup(&lower) {
                match m {
                    Modifier::Regex(on) => regex_mode = on,
                    Modifier::Pinyin(true) | Modifier::PinyinJian | Modifier::PinyinQuan => {
                        return Err(self.err(
                            nspan,
                            ErrorKind::Unsupported,
                            format!("`{lower}:` is not yet supported: Pinyin matching needs a Pinyin table Keel does not ship yet"),
                        ));
                    }
                    _ => {}
                }
                if m.is_global() {
                    self.global(m);
                } else {
                    mods.push(m);
                }
                i = next;
                continue;
            }
            if let Some(c) = text_macro(&lower) {
                lead.push(c);
                i = next;
                continue;
            }
            if !lead.is_empty() {
                break;
            }
            let (base, star) = match lower.strip_suffix('*') {
                Some(b) => (b, true),
                None => (lower.as_str(), false),
            };
            // Functions first, then user macros (which may replace the built-in filter
            // macros), then the built-in ones.
            let kind = functions::lookup(base);
            let user = self
                .opts
                .macros
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(base));
            match (kind, user) {
                (Some(FnKind::Unsupported(why)), _) => {
                    return Err(self.err(
                        nspan,
                        ErrorKind::Unsupported,
                        format!("`{base}:` is not yet supported: {why}"),
                    ));
                }
                (Some(FnKind::Macro(_)) | None, Some((_, text))) => {
                    hit = Some(MacroHit::User(text.clone()));
                    function = Some((base.to_string(), FnKind::Ui, false, nspan));
                    i = next;
                    break;
                }
                (Some(kind), _) => {
                    if let FnKind::Macro(exts) = kind {
                        hit = Some(MacroHit::Builtin(exts));
                    }
                    function = Some((base.to_string(), kind, star && literal, nspan));
                    i = next;
                    break;
                }
                (None, None) => {}
            }
            if !regex_mode && looks_like_name(&lower) {
                return Err(self.err(
                    nspan,
                    ErrorKind::Unknown,
                    format!("unknown function or modifier `{lower}:` (put the text in quotes to search for it)"),
                ));
            }
            break;
        }
        let value = &chars[i..];
        match (hit, function) {
            (Some(hit), Some((name, _, _, nspan))) => {
                self.expand(hit, &name, nspan, mods, value, span)
            }
            (None, Some((name, FnKind::Ui, _, _))) => {
                self.globals.ui.push(UiDirective {
                    name,
                    value: text_of(value),
                });
                Ok(None)
            }
            (None, Some((name, kind, literal, nspan))) => {
                let items = self.items(value, literal, kind, &mods, &span);
                Ok(Some(Expr::Term(Term {
                    span: self.sp(span),
                    modifiers: mods,
                    function: Some(Function {
                        name,
                        kind,
                        literal,
                        span: self.sp(nspan),
                    }),
                    items,
                })))
            }
            _ => Ok(self.plain(mods, lead, value, span)),
        }
    }

    /// A term without a function.
    fn plain(
        &self,
        mods: Vec<Modifier>,
        lead: String,
        chars: &[Ch],
        span: Range<usize>,
    ) -> Option<Expr> {
        let mut rest = chars;
        let mut negate = false;
        if lead.is_empty() && !mods.contains(&Modifier::NoNot) {
            while rest.first().is_some_and(|c| c.c == '!' && !c.quoted) {
                negate = !negate;
                rest = &rest[1..];
            }
        }
        let numeric = mods.iter().any(|m| {
            matches!(
                m,
                Modifier::Len | Modifier::ToNumber | Modifier::ToInteger | Modifier::HexToNumber
            )
        });
        let (op, rest) = if numeric {
            split_op(rest)
        } else {
            (None, rest)
        };
        let regex = mods.iter().rev().find_map(|m| match m {
            Modifier::Regex(on) => Some(*on),
            _ => None,
        });
        let mut text = lead;
        text.push_str(&text_of(rest));
        if self.opts.replace_forward_slashes && !regex.unwrap_or(self.opts.regex) {
            text = text.replace('/', "\\");
        }
        if text.is_empty() && op.is_none() {
            if !mods
                .iter()
                .any(|m| matches!(m, Modifier::File | Modifier::Folder))
            {
                return None;
            }
            return Some(Expr::Term(Term {
                span: self.sp(span),
                modifiers: mods,
                function: None,
                items: Vec::new(),
            }));
        }
        let item = Item {
            op,
            text,
            span: self.sp(span_of(rest, &span)),
        };
        let term = Expr::Term(Term {
            span: self.sp(span),
            modifiers: mods,
            function: None,
            items: vec![item],
        });
        Some(if negate { not(term) } else { term })
    }

    /// A function's values: `;`-separated, each with an optional operator.
    fn items(
        &self,
        chars: &[Ch],
        literal: bool,
        kind: FnKind,
        mods: &[Modifier],
        span: &Range<usize>,
    ) -> Vec<Item> {
        if literal {
            return vec![Item {
                op: None,
                text: text_of(chars),
                span: self.sp(span_of(chars, span)),
            }];
        }
        let regex = mods.iter().rev().find_map(|m| match m {
            Modifier::Regex(on) => Some(*on),
            _ => None,
        });
        let slashes = self.opts.replace_forward_slashes
            && !regex.unwrap_or(self.opts.regex)
            && matches!(
                kind,
                FnKind::Text(TextProp::FullPath | TextProp::Parent) | FnKind::In
            );
        let parts: Vec<&[Ch]> = chars.split(|c| c.c == ';' && !c.quoted).collect();
        let mut items = Vec::new();
        for part in parts {
            let (op, rest) = split_op(part);
            let mut text = text_of(rest);
            // `size:` alone (or an empty list item) means "any value".
            if text.is_empty() && op.is_none() {
                continue;
            }
            if slashes {
                text = text.replace('/', "\\");
            }
            items.push(Item {
                op,
                text,
                span: self.sp(span_of(part, span)),
            });
        }
        items
    }

    fn expand(
        &mut self,
        hit: MacroHit,
        name: &str,
        nspan: Range<usize>,
        mods: Vec<Modifier>,
        value: &[Ch],
        span: Range<usize>,
    ) -> Result<Option<Expr>, Error> {
        let body = match hit {
            MacroHit::Builtin(exts) => Expr::Term(Term {
                span: self.sp(span.clone()),
                modifiers: mods.clone(),
                function: Some(Function {
                    name: "ext".into(),
                    kind: FnKind::Text(TextProp::Ext),
                    literal: false,
                    span: self.sp(nspan.clone()),
                }),
                items: exts
                    .split(';')
                    .map(|e| Item {
                        op: None,
                        text: e.into(),
                        span: self.sp(nspan.clone()),
                    })
                    .collect(),
            }),
            MacroHit::User(text) => {
                if self.macro_depth >= MAX_MACRO_DEPTH {
                    return Err(self.err(
                        nspan,
                        ErrorKind::Syntax,
                        format!("macro `{name}:` expands too deeply"),
                    ));
                }
                let remap = Some(self.sp(nspan.clone()));
                let inner =
                    parse_at(&text, self.opts, self.macro_depth + 1, remap).map_err(|e| Error {
                        message: format!("in macro `{name}:`: {}", e.message),
                        ..e
                    })?;
                self.globals.ui.extend(inner.globals.ui);
                inner.expr
            }
        };
        let rest = self.plain(mods, String::new(), value, span);
        Ok(Some(match rest {
            Some(r) => Expr::And(vec![body, r]),
            None => body,
        }))
    }
}

fn not(e: Expr) -> Expr {
    match e {
        Expr::Not(inner) => *inner,
        e => Expr::Not(Box::new(e)),
    }
}

fn one_or(mut v: Vec<Expr>, wrap: fn(Vec<Expr>) -> Expr) -> Option<Expr> {
    match v.len() {
        0 => None,
        1 => v.pop(),
        _ => Some(wrap(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(e: &Expr) -> String {
        match e {
            Expr::And(v) if v.is_empty() => "ALL".into(),
            Expr::And(v) => format!("({})", v.iter().map(show).collect::<Vec<_>>().join(" ")),
            Expr::Or(v) => format!("({})", v.iter().map(show).collect::<Vec<_>>().join("|")),
            Expr::Not(x) => format!("!{}", show(x)),
            Expr::Term(t) => {
                let mut s = String::new();
                for m in &t.modifiers {
                    s.push_str(m.name());
                    s.push(':');
                }
                if let Some(f) = &t.function {
                    s.push_str(&f.name);
                    s.push(':');
                }
                let items: Vec<String> = t
                    .items
                    .iter()
                    .map(|i| match i.op {
                        Some(op) => format!("{op:?}{}", i.text),
                        None => i.text.clone(),
                    })
                    .collect();
                s + &items.join(";")
            }
        }
    }

    fn p(s: &str) -> String {
        show(&parse(s, &Options::default()).unwrap().expr)
    }

    #[test]
    fn precedence() {
        assert_eq!(p("a b|c"), "(a (b|c))");
        assert_eq!(p("and-precedence: a b|c"), "((a b)|c)");
        assert_eq!(p("a b|c or-precedence:"), "(a (b|c))");
        let o = Options {
            precedence: Precedence::AndFirst,
            ..Options::default()
        };
        assert_eq!(show(&parse("a b|c", &o).unwrap().expr), "((a b)|c)");
        assert_eq!(p("!a|b"), "(!a|b)");
        assert_eq!(p("!!a"), "a");
        assert_eq!(p("<a b>|c"), "((a b)|c)");
        assert_eq!(p("a | | b"), "(a|b)");
        assert_eq!(p(""), "ALL");
        assert_eq!(p("<>"), "ALL");
    }

    #[test]
    fn terms() {
        assert_eq!(p("case:regex:^a"), "case:regex:^a");
        assert_eq!(p("ext:jpg;png"), "ext:jpg;png");
        assert_eq!(p("size:>=1mb"), "size:Ge1mb");
        assert_eq!(p("name:\"a;b\""), "name:a;b");
        assert_eq!(p("C:\\Windows"), "C:\\Windows");
        assert_eq!(p("quot:x"), "\"x");
        assert_eq!(p("#65:b"), "Ab");
        assert_eq!(p("name*:a b|c"), "name:a b|c");
        assert_eq!(p("audio:abba"), "(ext:aac;ac3;aif;aifc;aiff;au;cda;dts;fla;flac;it;m1a;m2a;m3u;m4a;mid;midi;mka;mod;mp2;mp3;mpa;ogg;opus;ra;rmi;snd;spc;umx;voc;wav;wma;xm abba)");
        assert_eq!(p("no-!:!x"), "no-!:!x");
        assert_eq!(p("case:!x"), "!case:x");
    }

    #[test]
    fn globals_and_ui() {
        let ast = parse("no-reorder: sort:size a", &Options::default()).unwrap();
        assert!(ast.globals.no_reorder);
        assert_eq!(
            ast.globals.ui,
            [UiDirective {
                name: "sort".into(),
                value: "size".into()
            }]
        );
        assert_eq!(show(&ast.expr), "a");
    }

    #[test]
    fn errors() {
        let e = parse("a <b", &Options::default()).unwrap_err();
        assert_eq!((e.span, e.kind), (2..3, ErrorKind::Syntax));
        let e = parse("x extt:jpg", &Options::default()).unwrap_err();
        assert_eq!((e.span, e.kind), (2..7, ErrorKind::Unknown));
        let e = parse("dupe:size", &Options::default()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        assert!(e.message.contains("not yet supported"));
        let deep = "<".repeat(40) + "a" + &">".repeat(40);
        assert!(parse(&deep, &Options::default()).is_err());
    }

    #[test]
    fn macros() {
        let mut o = Options::default();
        o.macros.insert("rock".into(), "ext:mp3 genre-free".into());
        o.macros.insert("loop".into(), "loop:".into());
        assert_eq!(
            show(&parse("rock:x", &o).unwrap().expr),
            "((ext:mp3 genre-free) x)"
        );
        let e = parse("ab loop:", &o).unwrap_err();
        assert_eq!(e.span, 3..8);
    }
}
