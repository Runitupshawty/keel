//! Keel's search language: voidtools Everything's syntax (operators, groups, wildcards,
//! regex, path terms, ranges, lists, comparison operators, modifiers, search functions,
//! date and size constants, filter and user macros), parsed into an [`Ast`] with
//! positioned [`Error`]s and evaluated against anything that implements [`Props`].
//!
//! ```
//! use keel_query::{parse, Options};
//! let q = parse("ext:pdf;docx size:>1mb !temp", &Options::default()).unwrap();
//! # let _ = q;
//! ```
//!
//! Tree functions (`child:`, `descendant:`, `sibling:`, `exists:`, ...) look at other items
//! through a [`Tree`] passed to [`Query::matches_in`]; set functions (`dupe:`, `unique:`,
//! `distinct:`, `count:`) run over the whole result through a [`SetPass`] (or
//! [`Query::select`], which does both).
//!
//! The language reference is `docs/search.md`. Functions this crate does not evaluate yet
//! (content and media functions) are refused with [`ErrorKind::Unsupported`] and the task
//! that adds them ([`functions::UNSUPPORTED`]).

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

mod date;
mod eval;
pub mod functions;
mod lex;
pub mod modifiers;
mod parse;
mod props;
mod set;
mod text;
mod tree;
mod values;

#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;

pub use date::DateOrder;
pub use functions::{DatePart, FnKind, NumProp, Rel, SetFn, TextProp};
pub use modifiers::Modifier;
pub use parse::{
    Ast, Expr, FilterCombine, Function, Globals, Item, Precedence, Term, UiDirective,
    MAX_GROUP_DEPTH, MAX_MACRO_DEPTH,
};
pub use props::{
    attr, depth_of, extension_of, name_of, normalize, parent_of, stem_of, ChildCounts, DateProp,
    Props, Timestamp,
};
pub use set::SetPass;
pub use text::REGEX_SIZE_LIMIT;
pub use tree::{NodeId, Tree};
pub use values::CmpOp;

/// [`Options::tree_scan_limit`] by default.
pub const DEFAULT_TREE_SCAN_LIMIT: u64 = 100_000;
/// [`Options::set_memory_limit`] by default: 512 MiB.
pub const DEFAULT_SET_MEMORY_LIMIT: usize = 512 << 20;

/// The search options (Everything's Search menu and Options → Search) and the clock.
#[derive(Clone, Debug)]
pub struct Options {
    pub match_case: bool,
    pub match_diacritics: bool,
    pub match_whole_words: bool,
    pub match_path: bool,
    pub match_prefix: bool,
    pub match_suffix: bool,
    pub ignore_punctuation: bool,
    pub ignore_whitespace: bool,
    /// The whole query is a regular expression per term.
    pub regex: bool,
    /// **Match whole filename when using wildcards** (on).
    pub whole_filename_wildcards: bool,
    /// **Allow literal operators**: `AND`, `OR` and `NOT` (upper case) are operators.
    pub allow_literal_operators: bool,
    /// **Replace forward slashes with backslashes** in path terms.
    pub replace_forward_slashes: bool,
    /// **Operator precedence** (OR first, like Everything).
    pub precedence: Precedence,
    /// Size units are 1,000 (`metric:` per term).
    pub metric: bool,
    /// "Now" for date constants, UTC milliseconds.
    pub now: Timestamp,
    /// Local time minus UTC, seconds (date constants and components are local).
    pub utc_offset_secs: i32,
    /// Weeks start on Sunday (else Monday) for `thisweek`/`lastweek`.
    pub week_starts_sunday: bool,
    /// How `a/b/c` dates are read.
    pub date_order: DateOrder,
    /// User macros and filters: `name` → search text (`name:` in a query).
    pub macros: BTreeMap<String, String>,
    /// Tree functions visit at most this many other items per item (children, descendants,
    /// siblings); past it the answer is unknown.
    pub tree_scan_limit: u64,
    /// A [`SetPass`] (`dupe:`, `unique:`, `distinct:`) keeps at most this many bytes of
    /// candidates; past it the search fails with [`ErrorKind::Limit`].
    pub set_memory_limit: usize,
}

impl Default for Options {
    fn default() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Self {
            match_case: false,
            match_diacritics: false,
            match_whole_words: false,
            match_path: false,
            match_prefix: false,
            match_suffix: false,
            ignore_punctuation: false,
            ignore_whitespace: false,
            regex: false,
            whole_filename_wildcards: true,
            allow_literal_operators: false,
            replace_forward_slashes: false,
            precedence: Precedence::OrFirst,
            metric: false,
            now,
            utc_offset_secs: chrono::Local::now().offset().local_minus_utc(),
            week_starts_sunday: false,
            date_order: DateOrder::Mdy,
            macros: BTreeMap::new(),
            tree_scan_limit: DEFAULT_TREE_SCAN_LIMIT,
            set_memory_limit: DEFAULT_SET_MEMORY_LIMIT,
        }
    }
}

impl Options {
    pub(crate) fn date_ctx(&self) -> date::DateCtx {
        date::DateCtx {
            now: self.now,
            offset_ms: i64::from(self.utc_offset_secs) * 1000,
            week_start: if self.week_starts_sunday {
                chrono::Weekday::Sun
            } else {
                chrono::Weekday::Mon
            },
            order: self.date_order,
        }
    }
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Groups: an unclosed `<`, a stray `>`, nesting too deep.
    Syntax,
    /// `name:` that is no function, modifier or macro.
    Unknown,
    /// A known Everything function or modifier Keel does not evaluate yet.
    Unsupported,
    /// A value that does not parse (`size:big`, `dm:soon`).
    Value,
    /// A `regex:` that does not compile or is too large.
    Regex,
    /// A set function (`dupe:`, `unique:`, `distinct:`) over more results than
    /// [`Options::set_memory_limit`] allows.
    Limit,
}

/// An error with the byte range of the query it is about, for showing under the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub span: Range<usize>,
    pub kind: ErrorKind,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at {})", self.message, self.span.start)
    }
}

impl std::error::Error for Error {}

/// A compiled query.
pub struct Query {
    ast: Ast,
    root: eval::Node,
    env: Arc<eval::Env>,
    sets: Option<Arc<set::SetSpec>>,
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("ast", &self.ast)
            .finish_non_exhaustive()
    }
}

impl Query {
    /// Whether `item` matches, without a tree: tree functions are unknown. Set functions
    /// are not applied here; see [`Query::set_pass`].
    pub fn matches(&self, item: &dyn Props) -> bool {
        self.matches_in(item, None)
    }

    /// Whether `item`, an item of `tree`, matches. Set functions are not applied here; see
    /// [`Query::set_pass`].
    pub fn matches_in(&self, item: &dyn Props, tree: Option<&dyn Tree>) -> bool {
        let mut ctx = eval::Ctx::new(tree, self.env.opts.tree_scan_limit);
        self.root.eval(item, &self.env, &mut ctx)
    }

    /// Whether the query has set functions (`dupe:`, `unique:`, `distinct:`, `count:`):
    /// then the matching items go through [`Query::set_pass`].
    pub fn has_set_functions(&self) -> bool {
        self.sets.is_some()
    }

    /// A pass for the query's set functions, or `None` when it has none: push every item
    /// that [`matches_in`](Query::matches_in), in result order, then
    /// [`finish`](SetPass::finish).
    pub fn set_pass(&self) -> Option<SetPass> {
        self.sets
            .as_ref()
            .map(|s| SetPass::new(Arc::clone(s), Arc::clone(&self.env)))
    }

    /// The positions (in `items`) of the result: matching, then the set functions.
    pub fn select<'a, P: Props + 'a>(
        &self,
        items: impl IntoIterator<Item = &'a P>,
        tree: Option<&dyn Tree>,
    ) -> Result<Vec<usize>, Error> {
        let mut pass = self.set_pass();
        let mut out = Vec::new();
        for (i, item) in items.into_iter().enumerate() {
            if !self.matches_in(item, tree) {
                continue;
            }
            match &mut pass {
                Some(p) => {
                    if !p.push(i as u64, item, tree)? {
                        break;
                    }
                }
                None => out.push(i),
            }
        }
        Ok(match pass {
            Some(p) => p.finish().into_iter().map(|h| h as usize).collect(),
            None => out,
        })
    }

    pub fn ast(&self) -> &Ast {
        &self.ast
    }

    /// Global settings and UI directives typed in the query.
    pub fn globals(&self) -> &Globals {
        &self.ast.globals
    }
}

/// Parses `text` into an [`Ast`] without compiling values.
pub fn parse_ast(text: &str, opts: &Options) -> Result<Ast, Error> {
    parse::parse(text, opts)
}

/// Parses and compiles `text`.
pub fn parse(text: &str, opts: &Options) -> Result<Query, Error> {
    compile(parse::parse(text, opts)?, opts)
}

/// Compiles a parsed query.
pub fn compile(mut ast: Ast, opts: &Options) -> Result<Query, Error> {
    let captures = eval::uses_captures(&ast.expr);
    if captures {
        ast.globals.no_reorder = true;
    }
    let env = eval::env(opts, captures);
    let mut terms = Vec::new();
    let mut filter = ast.clone();
    set::take(&mut filter.expr, true, &mut terms)?;
    let sets = set::compile(&terms, &env)?.map(Arc::new);
    let root = eval::Compiler {
        env: &env,
        substituted: false,
    }
    .ast(&filter)?;
    Ok(Query {
        ast,
        root,
        env,
        sets,
    })
}

/// Keel's built-in type description for an item (`Text Document`, `File folder`, `XYZ File`).
pub fn builtin_type(item: &dyn Props) -> String {
    eval::builtin_type(item)
}
