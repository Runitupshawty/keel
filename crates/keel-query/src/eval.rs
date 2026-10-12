//! [`Ast`] to an evaluation tree, and evaluation against [`Props`]. Terms are compiled
//! once (regexes, folded needles, parsed values); AND and OR children are sorted by cost
//! so the cheap indexed reads run first and short-circuit the lazy ones, unless
//! `no-reorder:` (or a `$0:`..`$9:` capture, which depends on order) says otherwise.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::{Datelike, Timelike};

use crate::date::{local_fields, month_name, time_of_day, weekday_name, DateCtx, DateVal};
use crate::functions::{self, DatePart, FnKind, NumProp, Rel, TextProp, FILTER_MACROS};
use crate::modifiers::Modifier;
use crate::parse::{Ast, Expr, Item, Term};
use crate::props::{attr, name_of, normalize, parent_of, ChildCounts, DateProp, Props};
use crate::text::{self, decode_hex, soundex, Anchors, Fold, RegexFlags, TextMatch, Wild};
use crate::tree::{NodeId, Tree};
use crate::values::{
    number_from_text, parse_number, parse_size, split_range, CmpOp, NumParse, NumTest, NumVal,
    Parsed,
};
use crate::{Error, ErrorKind, Options};

/// What compiling needs, shared with terms that compile per item (`$property:`).
pub(crate) struct Env {
    pub opts: Options,
    pub date: DateCtx,
    pub captures: bool,
}

/// Evaluation state of one item: regex captures, the tree the item belongs to, and what is
/// left of its scan budget (children and descendants visited by tree functions).
pub(crate) struct Ctx<'t> {
    captures: Vec<Option<String>>,
    tree: Option<&'t dyn Tree>,
    scan_left: u64,
}

impl<'t> Ctx<'t> {
    pub fn new(tree: Option<&'t dyn Tree>, scan_limit: u64) -> Self {
        Self {
            captures: Vec::new(),
            tree,
            scan_left: scan_limit,
        }
    }

    /// Takes one visit from the budget; `false` when it is spent.
    fn spend(&mut self) -> bool {
        if self.scan_left == 0 {
            return false;
        }
        self.scan_left -= 1;
        true
    }

    /// The tree and the item's node in it.
    fn node(&self, p: &dyn Props) -> Option<(&'t dyn Tree, NodeId)> {
        let tree = self.tree?;
        Some((tree, tree.node_of(p)?))
    }
}

pub(crate) enum Node {
    /// All must match; empty matches everything.
    And(Vec<Node>),
    /// Any must match.
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Box<Leaf>),
}

pub(crate) struct Leaf {
    test: Test,
    cost: u32,
}

enum Test {
    /// `Some(true)`: folders only; `Some(false)`: files only; `None`: anything.
    Only(Option<bool>),
    Text(TextProp, TextTest),
    Num(NumProp, NumTest),
    NumAsText(NumProp, TextTest),
    Date(DateProp, DateTest),
    DatePart(DateProp, DatePart, NumTest),
    Attr(Option<u32>),
    AttrUnknown(bool),
    Root,
    Empty,
    Kind(Vec<Kind>),
    In(String, Fold),
    Dynamic(Box<Term>),
    /// A related item (child, descendant, sibling...) of this kind matches the inner node.
    Rel(Rel, Option<bool>, Box<Node>),
    Exists(Box<ExistsPath>),
    /// The inner node, evaluated on the parent folder.
    OfParent(Box<Node>),
    /// Folded full paths without trailing separators.
    PathList(HashSet<String>, Fold),
}

/// An `exists:` value: a full path, or names from the item's folder.
struct ExistsPath {
    absolute: Option<String>,
    parts: Vec<String>,
    only: Option<bool>,
    fold: Fold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Folder,
    File,
    Ext(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Multi {
    One,
    Any,
    All,
}

struct TextTest {
    fold: Fold,
    kind: TextKind,
    negate: bool,
    multi: Multi,
    each_line: bool,
}

enum TextKind {
    Match(TextMatch),
    Cmp(CmpOp, String),
    NonEmpty,
    Unknown,
    Known,
    Length(NumTest),
    AsNumber(NumParse, NumTest),
}

enum DateTest {
    Num(NumTest),
    /// Wildcard dates: the fixed local fields compared as a tuple.
    Pattern {
        fields: Vec<usize>,
        op: CmpOp,
        lo: Vec<i64>,
        hi: Vec<i64>,
    },
}

/// Per-term settings: the options, then the term's modifiers in order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    pub fold: Fold,
    anchors: Anchors,
    regex: bool,
    wildcards: bool,
    star_all: bool,
    path: bool,
    only: Option<bool>,
    len: bool,
    metric: bool,
    hex: bool,
    to_num: Option<NumParse>,
    to_string: bool,
    multi: Multi,
    each_line: bool,
    flags: RegexFlags,
    soundex: bool,
    no_substitute: bool,
}

pub(crate) fn settings(mods: &[Modifier], o: &Options) -> Settings {
    let mut s = Settings {
        fold: Fold {
            case: o.match_case,
            diacritics: o.match_diacritics,
            punct: o.ignore_punctuation,
            ws: o.ignore_whitespace,
        },
        anchors: Anchors {
            whole_words: o.match_whole_words,
            prefix: o.match_prefix,
            suffix: o.match_suffix,
            ..Anchors::default()
        },
        regex: o.regex,
        wildcards: true,
        star_all: false,
        path: o.match_path,
        only: None,
        len: false,
        metric: o.metric,
        hex: false,
        to_num: None,
        to_string: false,
        multi: Multi::One,
        each_line: false,
        flags: RegexFlags::default(),
        soundex: false,
        no_substitute: false,
    };
    let mut binary = false;
    for m in mods {
        match *m {
            Modifier::Case(b) => s.fold.case = b,
            Modifier::Diacritics(b) => s.fold.diacritics = b,
            Modifier::WholeWords(b) => s.anchors.whole_words = b,
            Modifier::Path(b) => s.path = b,
            Modifier::Prefix(b) => s.anchors.prefix = b,
            Modifier::Suffix(b) => s.anchors.suffix = b,
            Modifier::StartWith => s.anchors.start = true,
            Modifier::EndWith => s.anchors.end = true,
            Modifier::Whole(b) => s.anchors.whole = b,
            Modifier::Regex(b) => s.regex = b,
            Modifier::Wildcards(b) => s.wildcards = b,
            Modifier::File => s.only = Some(false),
            Modifier::Folder => s.only = Some(true),
            Modifier::IgnorePunctuation(b) => s.fold.punct = b,
            Modifier::IgnoreWhitespace(b) => s.fold.ws = b,
            Modifier::Len => s.len = true,
            Modifier::Metric(b) => s.metric = b,
            Modifier::Hex => s.hex = true,
            Modifier::HexToNumber => s.to_num = Some(NumParse::Hex),
            Modifier::Binary => binary = true,
            Modifier::ToInteger => s.to_num = Some(NumParse::Integer),
            Modifier::ToNumber => s.to_num = Some(NumParse::Real),
            Modifier::ToString => s.to_string = true,
            Modifier::MultiString(b) => s.multi = if b { Multi::Any } else { Multi::One },
            Modifier::AllStrings => s.multi = Multi::All,
            Modifier::EachLine => s.each_line = true,
            Modifier::MultiLine => s.flags.multi_line = true,
            Modifier::DotAll => s.flags.dot_all = true,
            Modifier::Ungreedy => s.flags.ungreedy = true,
            Modifier::StarAll => s.star_all = true,
            Modifier::Soundex => s.soundex = true,
            Modifier::NoSubstitute => s.no_substitute = true,
            // Pinyin is refused by the parser; the rest change nothing about matching
            // (highlighting, menus, where the caller reads properties) or are global.
            _ => {}
        }
    }
    if binary {
        s.fold = Fold {
            case: true,
            diacritics: true,
            punct: false,
            ws: false,
        };
    }
    s.flags.case = s.fold.case;
    s
}

fn leaf(test: Test, cost: u32) -> Node {
    Node::Leaf(Box::new(Leaf { test, cost }))
}

pub(crate) fn value_err(item: &Item, message: String) -> Error {
    Error {
        span: item.span.clone(),
        kind: ErrorKind::Value,
        message,
    }
}

/// A `$name:` or `$0:` reference inside a value.
#[derive(Clone, Copy, Debug)]
enum SubRef {
    Capture(usize),
    Text(TextProp),
    Num(NumProp),
    Date(DateProp),
}

/// Every substitution in `text`, with its byte range.
fn subst_refs(text: &str) -> Vec<(std::ops::Range<usize>, SubRef)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = text[i..].find('$') {
        let s = i + off;
        let rest = &text[s + 1..];
        if let Some(e) = rest.find(':') {
            let name = &rest[..e];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                let lower = name.to_ascii_lowercase();
                let r = if lower.len() == 1 && lower.as_bytes()[0].is_ascii_digit() {
                    Some(SubRef::Capture(usize::from(lower.as_bytes()[0] - b'0')))
                } else {
                    match functions::lookup(&lower) {
                        Some(FnKind::Text(p)) => Some(SubRef::Text(p)),
                        Some(FnKind::Number(p)) => Some(SubRef::Num(p)),
                        Some(FnKind::Date(p, None)) => Some(SubRef::Date(p)),
                        _ => None,
                    }
                };
                if let Some(r) = r {
                    out.push((s..s + e + 2, r));
                    i = s + e + 2;
                    continue;
                }
            }
        }
        i = s + 1;
    }
    out
}

pub(crate) struct Compiler<'e> {
    pub env: &'e Env,
    /// Compiling a substituted copy: `$` is text now.
    pub substituted: bool,
}

impl Compiler<'_> {
    pub fn ast(&self, ast: &Ast) -> Result<Node, Error> {
        self.expr(&ast.expr, !ast.globals.no_reorder && !self.env.captures)
    }

    fn expr(&self, e: &Expr, reorder: bool) -> Result<Node, Error> {
        Ok(match e {
            Expr::And(v) => Node::And(self.list(v, reorder)?),
            Expr::Or(v) => Node::Or(self.list(v, reorder)?),
            Expr::Not(x) => Node::Not(Box::new(self.expr(x, reorder)?)),
            Expr::Term(t) => self.term(t)?,
        })
    }

    fn list(&self, v: &[Expr], reorder: bool) -> Result<Vec<Node>, Error> {
        let mut nodes = v
            .iter()
            .map(|e| self.expr(e, reorder))
            .collect::<Result<Vec<_>, _>>()?;
        if reorder {
            nodes.sort_by_key(Node::cost);
        }
        Ok(nodes)
    }

    pub fn term(&self, t: &Term) -> Result<Node, Error> {
        let s = settings(&t.modifiers, &self.env.opts);
        let dynamic = !self.substituted
            && !s.no_substitute
            && t.items.iter().any(|i| !subst_refs(&i.text).is_empty());
        let node = if dynamic {
            leaf(Test::Dynamic(Box::new(t.clone())), 60)
        } else {
            match &t.function {
                None => match t.items.first() {
                    None => Node::And(Vec::new()),
                    Some(item) => self.plain(item, &s)?,
                },
                Some(f) => self.function(f.kind, t, &s)?,
            }
        };
        Ok(match s.only {
            Some(dir) => Node::And(vec![leaf(Test::Only(Some(dir)), 0), node]),
            None => node,
        })
    }

    fn plain(&self, item: &Item, s: &Settings) -> Result<Node, Error> {
        let text = if s.hex {
            decode_hex(&item.text).map_err(|e| value_err(item, e))?
        } else {
            item.text.clone()
        };
        let item = Item {
            text,
            ..item.clone()
        };
        let prop = if s.soundex {
            TextProp::Stem
        } else if s.path || path_term(&item.text, s.regex) {
            TextProp::FullPath
        } else {
            TextProp::Name
        };
        let cost = text_cost(prop, s);
        Ok(leaf(
            Test::Text(prop, self.text_test(prop, &item, s)?),
            cost,
        ))
    }

    fn text_test(&self, prop: TextProp, item: &Item, s: &Settings) -> Result<TextTest, Error> {
        let mut t = TextTest {
            fold: s.fold,
            kind: TextKind::NonEmpty,
            negate: false,
            multi: s.multi,
            each_line: s.each_line,
        };
        let text = match prop {
            TextProp::Ext => item.text.trim_start_matches('.').to_string(),
            TextProp::Parent | TextProp::Ancestor => trim_sep(&item.text).to_string(),
            _ => item.text.clone(),
        };
        if s.len || s.to_num.is_some() {
            let test = num_test(item, &parse_number)?;
            t.kind = match s.to_num {
                Some(how) => TextKind::AsNumber(how, test),
                None => TextKind::Length(test),
            };
            return Ok(t);
        }
        if text.eq_ignore_ascii_case("unknown") && !s.regex {
            t.kind = match item.op {
                Some(CmpOp::Ne) => TextKind::Known,
                _ => TextKind::Unknown,
            };
            return Ok(t);
        }
        let whole_default = matches!(prop, TextProp::Ext | TextProp::Parent | TextProp::Ancestor);
        let mut anchors = s.anchors;
        anchors.whole |= whole_default;
        let build = |anchors: Anchors| -> Result<TextMatch, Error> {
            if s.regex {
                text::user_regex(&text, s.flags)
                    .map(TextMatch::UserRegex)
                    .map_err(|m| Error {
                        span: item.span.clone(),
                        kind: ErrorKind::Regex,
                        message: m,
                    })
            } else if s.soundex {
                Ok(TextMatch::Soundex(soundex(&text)))
            } else {
                let wild = Wild {
                    on: s.wildcards,
                    whole: self.env.opts.whole_filename_wildcards,
                    star_all: s.star_all,
                };
                text::build(&text, s.fold, anchors, wild).map_err(|m| value_err(item, m))
            }
        };
        t.kind = match item.op {
            None => TextKind::Match(build(anchors)?),
            Some(CmpOp::Eq) => TextKind::Match(build(Anchors {
                whole: true,
                ..anchors
            })?),
            Some(CmpOp::Ne) => {
                t.negate = true;
                TextKind::Match(build(anchors)?)
            }
            Some(op) => TextKind::Cmp(op, s.fold.apply(&text).into_owned()),
        };
        Ok(t)
    }

    fn function(&self, kind: FnKind, t: &Term, s: &Settings) -> Result<Node, Error> {
        match kind {
            FnKind::Root | FnKind::Empty => {
                if let Some(item) = t
                    .items
                    .iter()
                    .find(|i| !i.text.is_empty() || i.op.is_some())
                {
                    return Err(value_err(item, "this function takes no value".into()));
                }
                return Ok(match kind {
                    FnKind::Root => leaf(Test::Root, 2),
                    _ => leaf(Test::Empty, 3),
                });
            }
            FnKind::Ui | FnKind::Macro(_) | FnKind::Unsupported(_) => {
                // Handled by the parser.
                return Ok(Node::And(Vec::new()));
            }
            FnKind::Related(rel, only) => {
                let mut names = Vec::new();
                for item in &t.items {
                    let test = self.text_test(TextProp::Name, item, s)?;
                    names.push(leaf(Test::Text(TextProp::Name, test), 2));
                }
                let inner = match names.len() {
                    0 => Node::And(Vec::new()),
                    1 => names.pop().unwrap_or(Node::And(Vec::new())),
                    _ => Node::Or(names),
                };
                let cost = match rel {
                    Rel::Child | Rel::Sibling => 50,
                    Rel::ParentSibling => 55,
                    Rel::AncestorSibling => 70,
                    Rel::Descendant => 80,
                };
                return Ok(leaf(Test::Rel(rel, only, Box::new(inner)), cost));
            }
            FnKind::Exists(only) => {
                if t.items.is_empty() {
                    return Err(Error {
                        span: t.span.clone(),
                        kind: ErrorKind::Value,
                        message: "exists: needs a name or a path".into(),
                    });
                }
                let mut alts = Vec::new();
                for item in &t.items {
                    let e = exists_path(item, only, s.fold)?;
                    alts.push(leaf(Test::Exists(Box::new(e)), 30));
                }
                return Ok(if alts.len() == 1 {
                    alts.pop().unwrap_or(Node::And(Vec::new()))
                } else {
                    Node::Or(alts)
                });
            }
            FnKind::OfParent(inner) => {
                let node = self.function(*inner, t, s)?;
                return Ok(leaf(Test::OfParent(Box::new(node)), 20));
            }
            FnKind::PathList => {
                let fold = Fold {
                    diacritics: true,
                    ..s.fold
                };
                let mut set = HashSet::new();
                for item in &t.items {
                    if !matches!(item.op, None | Some(CmpOp::Eq)) {
                        return Err(value_err(item, "pathlist: takes full paths".into()));
                    }
                    set.insert(fold.apply(trim_sep(&normalize(&item.text))).into_owned());
                }
                return Ok(leaf(Test::PathList(set, fold), 3));
            }
            FnKind::Set(_) => {
                return Err(Error {
                    span: t.span.clone(),
                    kind: ErrorKind::Syntax,
                    message: "this function works on the whole result; it cannot be inside `|`, `!` or `$property:` terms".into(),
                });
            }
            _ => {}
        }
        if t.items.is_empty() {
            return Ok(match kind {
                FnKind::Text(p) => leaf(
                    Test::Text(
                        p,
                        TextTest {
                            fold: s.fold,
                            kind: TextKind::NonEmpty,
                            negate: false,
                            multi: Multi::One,
                            each_line: false,
                        },
                    ),
                    text_cost(p, s),
                ),
                FnKind::Number(p) => leaf(Test::Num(p, NumTest::NonZero), num_cost(p)),
                FnKind::Date(p, None) => {
                    leaf(Test::Date(p, DateTest::Num(NumTest::Known)), date_cost(p))
                }
                FnKind::Date(p, Some(part)) => {
                    leaf(Test::DatePart(p, part, NumTest::Known), date_cost(p) + 2)
                }
                FnKind::Attributes => leaf(Test::Attr(None), 1),
                _ => Node::And(Vec::new()),
            });
        }
        let mut alts = Vec::new();
        for item in &t.items {
            alts.push(self.item(kind, item, s)?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap_or(Node::And(Vec::new()))
        } else {
            Node::Or(alts)
        })
    }

    fn item(&self, kind: FnKind, item: &Item, s: &Settings) -> Result<Node, Error> {
        let unknown = item.text.trim().eq_ignore_ascii_case("unknown");
        Ok(match kind {
            FnKind::Text(p) => leaf(Test::Text(p, self.text_test(p, item, s)?), text_cost(p, s)),
            FnKind::Number(p) => {
                if s.to_string {
                    let t = self.text_test(TextProp::Name, item, s)?;
                    leaf(Test::NumAsText(p, t), num_cost(p) + 1)
                } else if unknown {
                    leaf(Test::Num(p, unknown_test(item)), num_cost(p))
                } else {
                    let metric = s.metric;
                    let parse = |v: &str| match p {
                        NumProp::Size | NumProp::SizeOnDisk => parse_size(v, metric),
                        _ => parse_number(v),
                    };
                    leaf(Test::Num(p, num_test(item, &parse)?), num_cost(p))
                }
            }
            FnKind::Date(p, None) => {
                let t = if unknown {
                    DateTest::Num(unknown_test(item))
                } else {
                    self.date_test(item)?
                };
                leaf(Test::Date(p, t), date_cost(p))
            }
            FnKind::Date(p, Some(part)) => {
                let t = if unknown {
                    unknown_test(item)
                } else {
                    let date = self.env.date;
                    num_test(item, &|v: &str| part_value(part, v, &date))?
                };
                leaf(Test::DatePart(p, part, t), date_cost(p) + 2)
            }
            FnKind::Attributes => {
                if unknown {
                    leaf(Test::AttrUnknown(item.op != Some(CmpOp::Ne)), 1)
                } else {
                    let mask = attr_mask(&item.text).map_err(|m| value_err(item, m))?;
                    let l = leaf(Test::Attr(Some(mask)), 1);
                    match item.op {
                        None | Some(CmpOp::Eq) => l,
                        Some(CmpOp::Ne) => Node::Not(Box::new(l)),
                        Some(_) => {
                            return Err(value_err(item, "attributes take no `<` or `>`".into()))
                        }
                    }
                }
            }
            FnKind::Kind => {
                let kinds = kinds(&item.text).map_err(|m| value_err(item, m))?;
                let l = leaf(Test::Kind(kinds), 2);
                if item.op == Some(CmpOp::Ne) {
                    Node::Not(Box::new(l))
                } else {
                    l
                }
            }
            FnKind::In => {
                let fold = Fold {
                    diacritics: true,
                    ..s.fold
                };
                let prefix = fold.apply(trim_sep(&normalize(&item.text))).into_owned();
                if prefix.is_empty() {
                    return Err(value_err(item, "in: needs a folder".into()));
                }
                leaf(Test::In(prefix, fold), 3)
            }
            FnKind::Root
            | FnKind::Empty
            | FnKind::Ui
            | FnKind::Macro(_)
            | FnKind::Unsupported(_)
            | FnKind::Related(..)
            | FnKind::Exists(_)
            | FnKind::OfParent(_)
            | FnKind::PathList
            | FnKind::Set(_) => Node::And(Vec::new()),
        })
    }

    fn date_test(&self, item: &Item) -> Result<DateTest, Error> {
        let date = self.env.date;
        let parse = |v: &str| date.parse(v);
        let parsed = split_range(&item.text, &parse).map_err(|m| value_err(item, m))?;
        let op = item.op;
        match parsed {
            Parsed::One(DateVal::Span(v)) => Ok(DateTest::Num(NumTest::single(op, v))),
            Parsed::One(DateVal::Pattern(f, prec)) => {
                let (fields, key) = pattern_key(&f, prec);
                Ok(DateTest::Pattern {
                    fields,
                    op: op.unwrap_or(CmpOp::Eq),
                    lo: key.clone(),
                    hi: key,
                })
            }
            Parsed::Range(lo, hi) => {
                let span = |v: Option<DateVal>| -> Result<Option<NumVal>, Error> {
                    match v {
                        None => Ok(None),
                        Some(DateVal::Span(v)) => Ok(Some(v)),
                        Some(DateVal::Pattern(..)) => Err(value_err(item, "a wildcard date range needs wildcards at both ends, in the same places".into())),
                    }
                };
                match (lo, hi) {
                    (Some(DateVal::Pattern(a, pa)), Some(DateVal::Pattern(b, pb))) => {
                        let (fa, ka) = pattern_key(&a, pa);
                        let (fb, kb) = pattern_key(&b, pb);
                        if fa != fb {
                            return Err(value_err(item, "a wildcard date range needs wildcards at both ends, in the same places".into()));
                        }
                        let op = match op {
                            None | Some(CmpOp::Eq) => CmpOp::Eq,
                            Some(CmpOp::Ne) => CmpOp::Ne,
                            Some(_) => {
                                return Err(value_err(item, "a range takes no `<` or `>`".into()))
                            }
                        };
                        Ok(DateTest::Pattern {
                            fields: fa,
                            op,
                            lo: ka,
                            hi: kb,
                        })
                    }
                    (lo, hi) => {
                        let t = NumTest::range(op, span(lo)?, span(hi)?)
                            .map_err(|m| value_err(item, m))?;
                        Ok(DateTest::Num(t))
                    }
                }
            }
        }
    }
}

/// A term with `\` searches paths; in a regex only an escaped `\\` does (`\.` is a dot).
fn path_term(text: &str, regex: bool) -> bool {
    if regex {
        text.contains(r"\\")
    } else {
        text.contains('\\')
    }
}

/// An `exists:` value: `C:\x`, `\x` or `\\server\share\x` is a full path; anything else is
/// names from the item's folder (`a.txt`, `..\b`, `sub\c.txt`).
fn exists_path(item: &Item, only: Option<bool>, fold: Fold) -> Result<ExistsPath, Error> {
    if !matches!(item.op, None | Some(CmpOp::Eq)) {
        return Err(value_err(item, "exists: takes a name or a path".into()));
    }
    let text = normalize(item.text.trim()).into_owned();
    let drive = text.as_bytes().get(1) == Some(&b':') && text.as_bytes()[0].is_ascii_alphabetic();
    let fold = Fold {
        diacritics: true,
        ..fold
    };
    if drive || text.starts_with('\\') {
        let trimmed = trim_sep(&text);
        let absolute = if trimmed.is_empty() || (drive && trimmed.len() == 2) {
            format!("{trimmed}\\")
        } else {
            trimmed.to_string()
        };
        return Ok(ExistsPath {
            absolute: Some(absolute),
            parts: Vec::new(),
            only,
            fold,
        });
    }
    let parts: Vec<String> = text
        .split('\\')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(|p| fold.apply(p).into_owned())
        .collect();
    if parts.is_empty() {
        return Err(value_err(item, "exists: needs a name or a path".into()));
    }
    Ok(ExistsPath {
        absolute: None,
        parts,
        only,
        fold,
    })
}

fn unknown_test(item: &Item) -> NumTest {
    if item.op == Some(CmpOp::Ne) {
        NumTest::Known
    } else {
        NumTest::Unknown
    }
}

pub(crate) fn num_test(
    item: &Item,
    parse: &dyn Fn(&str) -> Result<NumVal, String>,
) -> Result<NumTest, Error> {
    match split_range(item.text.trim(), parse).map_err(|m| value_err(item, m))? {
        Parsed::One(v) => Ok(NumTest::single(item.op, v)),
        Parsed::Range(lo, hi) => NumTest::range(item.op, lo, hi).map_err(|m| value_err(item, m)),
    }
}

fn pattern_key(f: &[Option<i64>; 7], prec: usize) -> (Vec<usize>, Vec<i64>) {
    f[..prec]
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.map(|v| (i, v)))
        .unzip()
}

/// A date component value: names for months and weekdays, times of day, dates.
fn part_value(part: DatePart, v: &str, date: &DateCtx) -> Result<NumVal, String> {
    let t = v.trim().to_ascii_lowercase();
    let bad = |what: &str| format!("`{v}` is not {what}");
    let int = |lo: f64, hi: f64, what: &str| -> Result<NumVal, String> {
        match parse_number(&t) {
            Ok(n) if (lo..=hi).contains(&n.eq.0) && n.eq.0.fract() == 0.0 => Ok(n),
            _ => Err(bad(what)),
        }
    };
    match part {
        DatePart::Year => int(-9999.0, 9999.0, "a year"),
        DatePart::Day => int(1.0, 31.0, "a day of the month (1..31)"),
        DatePart::Month => match month_name(&t) {
            Some(m) => Ok(NumVal::point(f64::from(m))),
            None => int(1.0, 12.0, "a month (january, jan or 1..12)"),
        },
        DatePart::DayOfWeek => match weekday_name(&t) {
            Some(d) => Ok(NumVal::point(f64::from(d))),
            None => int(0.0, 6.0, "a weekday (monday, mon, or 0 for Sunday..6)"),
        },
        DatePart::Hour => match time_of_day(&t, true) {
            Some((secs, _)) => Ok(NumVal::point((secs / 3600) as f64)),
            None => Err(bad("an hour (0..23, 2pm)")),
        },
        DatePart::Time => match time_of_day(&t, true) {
            Some((secs, gran)) => Ok(NumVal {
                eq: (secs as f64, (secs + gran - 1) as f64),
                cmp: (secs as f64, (secs + gran - 1) as f64),
            }),
            None => Err(bad("a time of day (10am, 17:30)")),
        },
        DatePart::Date => match date.parse(&t)? {
            DateVal::Span(v) => {
                let day = |ms: f64| date.local_day(ms as i64) as f64;
                Ok(NumVal::span(day(v.eq.0), day(v.eq.1)))
            }
            DateVal::Pattern(..) => Err(bad("a date without wildcards")),
        },
    }
}

fn attr_mask(s: &str) -> Result<u32, String> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u32::from_str_radix(h, 16)
            .map_err(|_| format!("`{s}` is not a hex attribute mask"));
    }
    let mut mask = 0;
    for c in t.chars() {
        let up = c.to_ascii_uppercase();
        match attr::LETTERS.iter().find(|(l, _)| *l == up) {
            Some((_, bit)) => mask |= bit,
            None => {
                return Err(format!(
                    "`{c}` is not an attribute letter (A C D E H I L M N O P R S T U V X)"
                ))
            }
        }
    }
    if mask == 0 {
        return Err("attributes: needs letters such as RHSA".into());
    }
    Ok(mask)
}

fn kinds(s: &str) -> Result<Vec<Kind>, String> {
    let ext = |names: &str| -> Vec<Kind> {
        FILTER_MACROS
            .iter()
            .find(|(n, _)| n.contains(&names))
            .map(|(_, exts)| exts.split(';').map(Kind::Ext).collect())
            .unwrap_or_default()
    };
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "folder" | "folders" | "dir" | "directory" => vec![Kind::Folder],
        "file" | "files" => vec![Kind::File],
        "audio" | "music" | "sound" => ext("audio"),
        "video" | "videos" | "movie" | "movies" => ext("video"),
        "image" | "images" | "picture" | "pictures" | "pic" | "photo" | "photos" => ext("pic"),
        "document" | "documents" | "doc" | "docs" | "text" => ext("doc"),
        "executable" | "executables" | "exe" | "program" | "programs" | "app" => ext("exe"),
        "archive" | "archives" | "zip" | "compressed" => ext("zip"),
        _ => return Err(format!("`{s}` is not a kind (folder, file, audio, video, image, document, executable, archive)")),
    })
}

/// The path without trailing separators (`C:\` → `C:`, `\` → ``).
pub(crate) fn trim_sep(s: &str) -> &str {
    s.trim_end_matches(['\\', '/'])
}

fn text_cost(p: TextProp, s: &Settings) -> u32 {
    let base = match p {
        TextProp::Name | TextProp::Stem | TextProp::Ext | TextProp::Extension => 2,
        TextProp::FullPath | TextProp::Parent | TextProp::ParentName => 4,
        TextProp::Ancestor | TextProp::AncestorName => 5,
        TextProp::Type => 10,
        TextProp::HardLinkNames | TextProp::StreamNames => 30,
        TextProp::ContainerNames => 40,
    };
    base + if s.regex || s.soundex { 3 } else { 0 }
}

fn num_cost(p: NumProp) -> u32 {
    match p {
        NumProp::Size
        | NumProp::SizeOnDisk
        | NumProp::NameLength
        | NumProp::FileId
        | NumProp::ParentFileId => 1,
        NumProp::FullPathLength | NumProp::ParentCount => 2,
        NumProp::ChildCount
        | NumProp::ChildFileCount
        | NumProp::ChildFolderCount
        | NumProp::RunCount => 3,
        NumProp::HardLinkCount | NumProp::StreamCount => 20,
        NumProp::DescendantCount
        | NumProp::DescendantFileCount
        | NumProp::DescendantFolderCount => 30,
        NumProp::ContainerFileCount => 40,
    }
}

fn date_cost(p: DateProp) -> u32 {
    if p.is_indexed() {
        1
    } else {
        40
    }
}

impl Node {
    fn cost(&self) -> u32 {
        match self {
            Node::And(v) | Node::Or(v) => v.iter().map(Node::cost).fold(0u32, u32::saturating_add),
            Node::Not(x) => x.cost(),
            Node::Leaf(l) => l.cost,
        }
    }

    pub fn eval(&self, p: &dyn Props, env: &Env, ctx: &mut Ctx) -> bool {
        match self {
            Node::And(v) => v.iter().all(|n| n.eval(p, env, ctx)),
            Node::Or(v) => v.iter().any(|n| n.eval(p, env, ctx)),
            Node::Not(x) => !x.eval(p, env, ctx),
            Node::Leaf(l) => l.test.eval(p, env, ctx),
        }
    }
}

/// The built-in type description by extension (Windows' defaults).
pub(crate) fn builtin_type(p: &dyn Props) -> String {
    if p.is_dir() {
        return "File folder".into();
    }
    let ext = p.extension().to_ascii_lowercase();
    let known = match ext.as_str() {
        "txt" | "log" => "Text Document",
        "md" => "Markdown File",
        "pdf" => "PDF Document",
        "doc" | "docx" => "Microsoft Word Document",
        "xls" | "xlsx" => "Microsoft Excel Worksheet",
        "ppt" | "pptx" => "Microsoft PowerPoint Presentation",
        "csv" => "Microsoft Excel Comma Separated Values File",
        "htm" | "html" => "HTML Document",
        "jpg" | "jpeg" | "jfif" => "JPEG Image",
        "png" => "PNG Image",
        "gif" => "GIF Image",
        "bmp" => "Bitmap Image",
        "svg" => "SVG Document",
        "mp3" => "MP3 File",
        "wav" => "WAV File",
        "mp4" => "MP4 File",
        "zip" => "Compressed (zipped) Folder",
        "exe" => "Application",
        "dll" => "Application extension",
        "msi" => "Windows Installer Package",
        "bat" => "Windows Batch File",
        "cmd" => "Windows Command Script",
        "ps1" => "Windows PowerShell Script",
        "lnk" => "Shortcut",
        "ini" => "Configuration settings",
        "ttf" => "TrueType font file",
        "" => "File",
        _ => return format!("{} File", ext.to_ascii_uppercase()),
    };
    known.into()
}

fn text_value<'a>(prop: TextProp, p: &'a dyn Props) -> Option<Cow<'a, str>> {
    Some(match prop {
        TextProp::Name => Cow::Borrowed(p.name()),
        TextProp::Stem => Cow::Borrowed(p.stem()),
        TextProp::Extension | TextProp::Ext => Cow::Borrowed(p.extension()),
        TextProp::FullPath => normalize(p.path()),
        TextProp::Parent => Cow::Owned(trim_sep(&parent_of(&normalize(p.path()))?).to_string()),
        TextProp::ParentName => Cow::Owned(name_of(&parent_of(&normalize(p.path()))?).to_string()),
        TextProp::Type => p.type_name().unwrap_or_else(|| Cow::Owned(builtin_type(p))),
        list => Cow::Owned(text_list(list, p)?.join(";")),
    })
}

/// The values of a list property ([`TextProp::is_list`]).
fn text_list(prop: TextProp, p: &dyn Props) -> Option<Vec<String>> {
    match prop {
        TextProp::Ancestor | TextProp::AncestorName => {
            let mut out = Vec::new();
            let mut cur = parent_of(&normalize(p.path()));
            while let Some(dir) = cur {
                out.push(if prop == TextProp::Ancestor {
                    trim_sep(&dir).to_string()
                } else {
                    name_of(&dir).to_string()
                });
                cur = parent_of(&dir);
            }
            Some(out)
        }
        TextProp::HardLinkNames => p.hard_link_names(),
        TextProp::StreamNames => p.stream_names(),
        TextProp::ContainerNames => p.container_file_names(),
        _ => text_value(prop, p).map(|v| vec![v.into_owned()]),
    }
}

fn utf16_len(s: &str) -> f64 {
    s.encode_utf16().count() as f64
}

fn num_value(prop: NumProp, p: &dyn Props, ctx: &mut Ctx) -> Option<f64> {
    let count = |n: Option<u64>, names: &dyn Fn() -> Option<Vec<String>>| {
        n.or_else(|| names().map(|v| v.len() as u64))
            .map(|v| v as f64)
    };
    match prop {
        NumProp::Size => p.size().map(|v| v as f64),
        NumProp::SizeOnDisk => p.allocated().map(|v| v as f64),
        NumProp::NameLength => Some(utf16_len(p.name())),
        NumProp::FullPathLength => Some(utf16_len(p.path())),
        NumProp::ParentCount => Some(f64::from(p.depth())),
        NumProp::ChildCount => child_counts(p, ctx).map(|c| (c.files + c.folders) as f64),
        NumProp::ChildFileCount => child_counts(p, ctx).map(|c| c.files as f64),
        NumProp::ChildFolderCount => child_counts(p, ctx).map(|c| c.folders as f64),
        NumProp::RunCount => p.run_count().map(|v| v as f64),
        NumProp::FileId => p.file_id().map(|v| v as f64),
        NumProp::ParentFileId => p.parent_file_id().map(|v| v as f64),
        NumProp::DescendantCount => descendant_counts(p, ctx).map(|c| (c.files + c.folders) as f64),
        NumProp::DescendantFileCount => descendant_counts(p, ctx).map(|c| c.files as f64),
        NumProp::DescendantFolderCount => descendant_counts(p, ctx).map(|c| c.folders as f64),
        NumProp::HardLinkCount => count(p.hard_link_count(), &|| p.hard_link_names()),
        NumProp::StreamCount => count(p.stream_count(), &|| p.stream_names()),
        NumProp::ContainerFileCount => {
            count(p.container_file_count(), &|| p.container_file_names())
        }
    }
}

/// Direct children of a folder: the source's count, else counted through the tree.
fn child_counts(p: &dyn Props, ctx: &mut Ctx) -> Option<ChildCounts> {
    if let Some(c) = p.child_counts() {
        return Some(c);
    }
    if !p.is_dir() {
        return None;
    }
    let (tree, me) = ctx.node(p)?;
    count_below(tree, me, false, ctx)
}

/// Everything below a folder: the tree's aggregates, else a bounded scan.
fn descendant_counts(p: &dyn Props, ctx: &mut Ctx) -> Option<ChildCounts> {
    if !p.is_dir() {
        return None;
    }
    let (tree, me) = ctx.node(p)?;
    if let Some(c) = tree.descendant_counts(me) {
        return Some(c);
    }
    count_below(tree, me, true, ctx)
}

/// Counts children (or, `deep`, descendants); `None` when the scan budget runs out.
fn count_below(tree: &dyn Tree, root: NodeId, deep: bool, ctx: &mut Ctx) -> Option<ChildCounts> {
    let mut counts = ChildCounts::default();
    let mut stack = vec![root];
    let mut spent = false;
    while let Some(dir) = stack.pop() {
        tree.children(dir, &mut |n| {
            if !ctx.spend() {
                spent = true;
                return false;
            }
            tree.with_props(n, &mut |cp| {
                if cp.is_dir() {
                    counts.folders += 1;
                    if deep {
                        stack.push(n);
                    }
                } else {
                    counts.files += 1;
                }
            });
            true
        });
        if spent {
            return None;
        }
    }
    Some(counts)
}

type ItemTest<'a, 't> = dyn FnMut(&dyn Props, &mut Ctx<'t>) -> bool + 'a;

/// Whether a child of `dir` other than `skip` passes `test`.
fn any_child<'t>(
    tree: &'t dyn Tree,
    dir: NodeId,
    skip: Option<NodeId>,
    ctx: &mut Ctx<'t>,
    test: &mut ItemTest<'_, 't>,
) -> bool {
    let mut hit = false;
    tree.children(dir, &mut |c| {
        if Some(c) == skip {
            return true;
        }
        if !ctx.spend() {
            return false;
        }
        tree.with_props(c, &mut |cp| hit = test(cp, ctx));
        !hit
    });
    hit
}

/// Whether anything below `dir` passes `test` (depth first, bounded by the scan budget).
fn any_descendant<'t>(
    tree: &'t dyn Tree,
    dir: NodeId,
    ctx: &mut Ctx<'t>,
    test: &mut ItemTest<'_, 't>,
) -> bool {
    let mut stack = vec![dir];
    let mut hit = false;
    let mut spent = false;
    while let Some(d) = stack.pop() {
        tree.children(d, &mut |c| {
            if !ctx.spend() {
                spent = true;
                return false;
            }
            tree.with_props(c, &mut |cp| {
                hit = test(cp, ctx);
                if cp.is_dir() {
                    stack.push(c);
                }
            });
            !hit
        });
        if hit || spent {
            break;
        }
    }
    hit
}

/// The child of `dir` called `name` (folded).
fn child_named<'t>(
    tree: &'t dyn Tree,
    dir: NodeId,
    name: &str,
    fold: Fold,
    ctx: &mut Ctx<'t>,
) -> Option<NodeId> {
    let mut found = None;
    tree.children(dir, &mut |c| {
        if !ctx.spend() {
            return false;
        }
        tree.with_props(c, &mut |cp| {
            if fold.apply(cp.name()) == name {
                found = Some(c);
            }
        });
        found.is_none()
    });
    found
}

fn is_dir_node(tree: &dyn Tree, n: NodeId) -> bool {
    let mut dir = false;
    tree.with_props(n, &mut |p| dir = p.is_dir());
    dir
}

impl ExistsPath {
    fn eval(&self, p: &dyn Props, ctx: &mut Ctx) -> bool {
        let Some(tree) = ctx.tree else {
            return false;
        };
        let target = match &self.absolute {
            Some(path) => tree.find(path),
            None => (|| {
                let me = tree.node_of(p)?;
                let mut cur = tree.parent(me)?;
                for part in &self.parts {
                    cur = if part == ".." {
                        tree.parent(cur)?
                    } else {
                        child_named(tree, cur, part, self.fold, ctx)?
                    };
                }
                Some(cur)
            })(),
        };
        target.is_some_and(|n| self.only.is_none_or(|d| is_dir_node(tree, n) == d))
    }
}

fn part_of(part: DatePart, ts: i64, date: &DateCtx) -> Option<f64> {
    if part == DatePart::Date {
        return Some(date.local_day(ts) as f64);
    }
    let t = date.local(ts)?;
    Some(f64::from(match part {
        DatePart::Year => return Some(f64::from(t.year())),
        DatePart::Month => t.month(),
        DatePart::Day => t.day(),
        DatePart::DayOfWeek => t.weekday().num_days_from_sunday(),
        DatePart::Hour => t.hour(),
        DatePart::Time => t.num_seconds_from_midnight(),
        DatePart::Date => return None,
    }))
}

impl TextTest {
    fn eval(&self, value: Option<&str>, ctx: &mut Ctx, env: &Env) -> bool {
        let Some(v) = value else {
            return matches!(self.kind, TextKind::Unknown);
        };
        self.hit(v, ctx, env) != self.negate
    }

    /// A list property: any value matches (`!=`: none does).
    fn eval_list(&self, values: Option<Vec<String>>, ctx: &mut Ctx, env: &Env) -> bool {
        let Some(values) = values else {
            return matches!(self.kind, TextKind::Unknown);
        };
        values.iter().any(|v| self.hit(v, ctx, env)) != self.negate
    }

    fn hit(&self, v: &str, ctx: &mut Ctx, env: &Env) -> bool {
        let one = |s: &str, ctx: &mut Ctx| -> bool {
            if self.each_line {
                s.lines().any(|l| self.one(l, ctx, env))
            } else {
                self.one(s, ctx, env)
            }
        };
        match self.multi {
            Multi::One => one(v, ctx),
            Multi::Any => v.split(';').any(|s| one(s.trim(), ctx)),
            Multi::All => v.split(';').all(|s| one(s.trim(), ctx)),
        }
    }

    fn one(&self, s: &str, ctx: &mut Ctx, env: &Env) -> bool {
        match &self.kind {
            TextKind::Match(m) => {
                if m.raw() {
                    if let (true, TextMatch::UserRegex(re)) = (env.captures, m) {
                        return match re.captures(s) {
                            Some(c) => {
                                ctx.captures = (0..10)
                                    .map(|i| c.get(i).map(|m| m.as_str().to_string()))
                                    .collect();
                                true
                            }
                            None => false,
                        };
                    }
                    m.is_match(s)
                } else {
                    m.is_match(&self.fold.apply(s))
                }
            }
            TextKind::Cmp(op, v) => {
                let s = self.fold.apply(s);
                let o = s.as_ref().cmp(v.as_str());
                match op {
                    CmpOp::Lt => o.is_lt(),
                    CmpOp::Le => o.is_le(),
                    CmpOp::Gt => o.is_gt(),
                    CmpOp::Ge => o.is_ge(),
                    CmpOp::Eq => o.is_eq(),
                    CmpOp::Ne => o.is_ne(),
                }
            }
            TextKind::NonEmpty => !s.is_empty(),
            TextKind::Unknown => false,
            TextKind::Known => true,
            TextKind::Length(t) => t.eval(Some(utf16_len(s))),
            TextKind::AsNumber(how, t) => t.eval(number_from_text(s, *how)),
        }
    }
}

impl Test {
    fn eval(&self, p: &dyn Props, env: &Env, ctx: &mut Ctx) -> bool {
        match self {
            Test::Only(only) => only.is_none_or(|dir| p.is_dir() == dir),
            Test::Text(prop, t) if prop.is_list() => t.eval_list(text_list(*prop, p), ctx, env),
            Test::Text(prop, t) => t.eval(text_value(*prop, p).as_deref(), ctx, env),
            Test::Num(prop, t) => t.eval(num_value(*prop, p, ctx)),
            Test::NumAsText(prop, t) => {
                let v = num_value(*prop, p, ctx).map(fmt_num);
                t.eval(v.as_deref(), ctx, env)
            }
            Test::Date(prop, t) => {
                let ts = p.date(*prop);
                match t {
                    DateTest::Num(t) => t.eval(ts.map(|v| v as f64)),
                    DateTest::Pattern { fields, op, lo, hi } => {
                        let Some(local) = ts.and_then(|ts| env.date.local(ts)) else {
                            return false;
                        };
                        let all = local_fields(local);
                        let key: Vec<i64> = fields.iter().map(|&i| all[i]).collect();
                        let inside = *lo <= key && key <= *hi;
                        match op {
                            CmpOp::Eq => inside,
                            CmpOp::Ne => !inside,
                            CmpOp::Lt => key < *lo,
                            CmpOp::Le => key <= *hi,
                            CmpOp::Gt => key > *hi,
                            CmpOp::Ge => key >= *lo,
                        }
                    }
                }
            }
            Test::DatePart(prop, part, t) => {
                t.eval(p.date(*prop).and_then(|ts| part_of(*part, ts, &env.date)))
            }
            Test::Attr(mask) => match (p.attributes(), mask) {
                (Some(a), Some(m)) => a & m == *m,
                (Some(a), None) => a != 0,
                (None, _) => false,
            },
            Test::AttrUnknown(want) => p.attributes().is_none() == *want,
            Test::Root => parent_of(&normalize(p.path())).is_none(),
            Test::Empty => {
                p.is_dir() && child_counts(p, ctx).is_some_and(|c| c.files == 0 && c.folders == 0)
            }
            Test::Kind(kinds) => kinds.iter().any(|k| match k {
                Kind::Folder => p.is_dir(),
                Kind::File => !p.is_dir(),
                Kind::Ext(e) => !p.is_dir() && p.extension().eq_ignore_ascii_case(e),
            }),
            Test::In(prefix, fold) => {
                let path = normalize(p.path());
                let path = fold.apply(&path);
                path.len() > prefix.len()
                    && path.starts_with(prefix.as_str())
                    && path[prefix.len()..].starts_with('\\')
            }
            Test::Dynamic(term) => {
                let mut t = (**term).clone();
                for item in &mut t.items {
                    item.text = substitute(&item.text, p, env, ctx);
                }
                let c = Compiler {
                    env,
                    substituted: true,
                };
                c.term(&t).is_ok_and(|n| n.eval(p, env, ctx))
            }
            Test::Rel(rel, only, inner) => {
                let Some((tree, me)) = ctx.node(p) else {
                    return false;
                };
                let mut test = |cp: &dyn Props, ctx: &mut Ctx<'_>| {
                    only.is_none_or(|d| cp.is_dir() == d) && inner.eval(cp, env, ctx)
                };
                match rel {
                    Rel::Child => p.is_dir() && any_child(tree, me, None, ctx, &mut test),
                    Rel::Descendant => p.is_dir() && any_descendant(tree, me, ctx, &mut test),
                    Rel::Sibling => tree
                        .parent(me)
                        .is_some_and(|dir| any_child(tree, dir, Some(me), ctx, &mut test)),
                    Rel::ParentSibling => {
                        let Some(parent) = tree.parent(me) else {
                            return false;
                        };
                        tree.parent(parent)
                            .is_some_and(|g| any_child(tree, g, Some(parent), ctx, &mut test))
                    }
                    Rel::AncestorSibling => {
                        let mut cur = tree.parent(me);
                        while let Some(a) = cur {
                            let up = tree.parent(a);
                            if let Some(g) = up {
                                if any_child(tree, g, Some(a), ctx, &mut test) {
                                    return true;
                                }
                            }
                            if !ctx.spend() {
                                return false;
                            }
                            cur = up;
                        }
                        false
                    }
                }
            }
            Test::Exists(e) => e.eval(p, ctx),
            Test::OfParent(inner) => {
                let Some((tree, me)) = ctx.node(p) else {
                    return false;
                };
                let Some(parent) = tree.parent(me) else {
                    return false;
                };
                let mut hit = false;
                tree.with_props(parent, &mut |pp| hit = inner.eval(pp, env, ctx));
                hit
            }
            Test::PathList(set, fold) => {
                set.contains(fold.apply(trim_sep(&normalize(p.path()))).as_ref())
            }
        }
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e18 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// `$name:` and `$0:` replaced by the item's values.
fn substitute(text: &str, p: &dyn Props, env: &Env, ctx: &mut Ctx) -> String {
    let mut out = String::new();
    let mut last = 0;
    for (range, r) in subst_refs(text) {
        out.push_str(&text[last..range.start]);
        let v: String = match r {
            SubRef::Capture(i) => ctx.captures.get(i).cloned().flatten().unwrap_or_default(),
            SubRef::Text(prop) => text_value(prop, p)
                .map(|v| v.into_owned())
                .unwrap_or_default(),
            SubRef::Num(prop) => num_value(prop, p, ctx).map(fmt_num).unwrap_or_default(),
            SubRef::Date(prop) => p
                .date(prop)
                .and_then(|ts| env.date.local(ts))
                .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string())
                .unwrap_or_default(),
        };
        out.push_str(&v);
        last = range.end;
    }
    out.push_str(&text[last..]);
    out
}

/// Whether any item of the query recalls a regex capture (`$1:`).
pub(crate) fn uses_captures(e: &Expr) -> bool {
    match e {
        Expr::And(v) | Expr::Or(v) => v.iter().any(uses_captures),
        Expr::Not(x) => uses_captures(x),
        Expr::Term(t) => t.items.iter().any(|i| {
            subst_refs(&i.text)
                .iter()
                .any(|(_, r)| matches!(r, SubRef::Capture(_)))
        }),
    }
}

/// One property of a `dupe:`/`distinct:`/`unique:` list.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Key {
    Text(TextProp, Fold),
    Num(NumProp),
    Date(DateProp, Option<DatePart>),
    Attr,
}

impl Key {
    /// The key for a property name (`size`, `name`, `dmyear`), or why it cannot be one.
    pub fn of(name: &str, fold: Fold) -> Result<Key, String> {
        match functions::lookup(&name.trim().to_ascii_lowercase()) {
            Some(FnKind::Text(p)) => Ok(Key::Text(p, fold)),
            Some(FnKind::Number(p)) => Ok(Key::Num(p)),
            Some(FnKind::Date(p, part)) => Ok(Key::Date(p, part)),
            Some(FnKind::Attributes) => Ok(Key::Attr),
            Some(FnKind::Unsupported(why)) => Err(format!("`{name}` is not yet supported: {why}")),
            _ => Err(format!(
                "`{name}` is not a property to compare (name, size, ext, dm, attributes, ...)"
            )),
        }
    }
}

/// Appends the item's values for `keys` to `out`, encoded so byte order is value order;
/// `false` (and `out` unchanged) when a value is unknown.
pub(crate) fn push_key(
    keys: &[Key],
    p: &dyn Props,
    env: &Env,
    ctx: &mut Ctx,
    out: &mut Vec<u8>,
) -> bool {
    let start = out.len();
    for k in keys {
        let ok = match *k {
            Key::Text(prop, fold) => {
                let v = if prop.is_list() {
                    text_list(prop, p).map(|v| Cow::Owned(v.join(";")))
                } else {
                    text_value(prop, p)
                };
                v.map(|v| {
                    out.extend_from_slice(fold.apply(&v).as_bytes());
                    out.push(0);
                })
                .is_some()
            }
            Key::Num(prop) => num_value(prop, p, ctx)
                .map(|v| out.extend_from_slice(&f64_key(v)))
                .is_some(),
            Key::Date(prop, None) => p
                .date(prop)
                .map(|v| out.extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes()))
                .is_some(),
            Key::Date(prop, Some(part)) => p
                .date(prop)
                .and_then(|ts| part_of(part, ts, &env.date))
                .map(|v| out.extend_from_slice(&f64_key(v)))
                .is_some(),
            Key::Attr => p
                .attributes()
                .map(|a| out.extend_from_slice(&a.to_be_bytes()))
                .is_some(),
        };
        if !ok {
            out.truncate(start);
            return false;
        }
    }
    true
}

/// A float as bytes that sort like the numbers.
fn f64_key(v: f64) -> [u8; 8] {
    let v = if v == 0.0 { 0.0 } else { v };
    let bits = v.to_bits();
    let ordered = if bits >> 63 == 1 {
        !bits
    } else {
        bits ^ (1 << 63)
    };
    ordered.to_be_bytes()
}

/// Shared environment for a compiled query.
pub(crate) fn env(opts: &Options, captures: bool) -> Arc<Env> {
    Arc::new(Env {
        opts: opts.clone(),
        date: opts.date_ctx(),
        captures,
    })
}
