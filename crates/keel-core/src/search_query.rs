//! Shared grammar, SQL lowering, and residual evaluation for library searches.
//!
//! Existing conjunctions retain the ranked FTS planner. For mixed expressions SQL
//! supplies both a conservative candidate predicate and the truth of SQL-only leaves
//! (FTS, tags, media). Rust combines those with keel-query property predicates. No
//! candidate limit is applied before that combination, including underneath NOT.

use super::*;
use keel_query::{Ast, DateProp, Expr, Modifier, Options, Props, Term};
use std::cell::OnceCell;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expression {
    text: String,
    now: i64,
    offset: i64,
}

fn options(now: i64, offset: i64) -> Options {
    Options {
        now: now.saturating_mul(1000),
        utc_offset_secs: offset.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        allow_literal_operators: false,
        external_functions: [
            "tag", "content", "-content", "contents", "source", "camera", "taken", "w", "width",
            "h", "height", "duration", "length", "has", "modified", "type",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        ..Options::default()
    }
}

/// Whether the term can use the library's established SQL semantics. A modifier,
/// wildcard, or macro remains a keel-query predicate instead of an FTS word.
fn sql_term(term: &Term, text: &str, now: i64, offset: i64) -> Result<Option<LibraryQuery>> {
    let raw = text.get(term.span.clone()).unwrap_or("");
    let key = raw.split_once(':').map(|(k, _)| k.to_ascii_lowercase());
    if term
        .modifiers
        .iter()
        .any(|m| !matches!(m, Modifier::File | Modifier::Folder))
    {
        return Ok(None);
    }
    if !term.modifiers.is_empty()
        && !matches!(raw.to_ascii_lowercase().as_str(), "file:" | "folder:")
    {
        return Ok(None);
    }
    let supported = match &term.function {
        None => !term.items.iter().any(|i| i.text.contains(['*', '?'])),
        Some(function) => {
            // Expanded macros have the original macro's span, not the expanded text.
            key.as_deref() == Some(function.name.as_str())
                && matches!(
                    function.name.as_str(),
                    "ext"
                        | "size"
                        | "dm"
                        | "datemodified"
                        | "modified"
                        | "kind"
                        | "type"
                        | "tag"
                        | "content"
                        | "-content"
                        | "name"
                        | "contents"
                        | "in"
                        | "source"
                        | "camera"
                        | "taken"
                        | "w"
                        | "width"
                        | "h"
                        | "height"
                        | "duration"
                        | "length"
                        | "has"
                )
        }
    };
    if !supported {
        return Ok(None);
    }
    if key.as_deref() == Some("in") && term.items.iter().any(|i| i.text.contains(['/', '\\'])) {
        return Ok(None);
    }
    match LibraryQuery::parse_sql_terms(raw, now, offset) {
        Ok(q) => Ok(Some(q)),
        // A word the old subset has no meaning for (`size:huge`, `dm:thisweek`) is one of
        // the shared language's constants: the evaluator reads it. A malformed number or
        // date (`dm:2026-13`) stays the error it always was.
        Err(_) if named_value(key.as_deref(), term) => Ok(None),
        Err(e) => Err(e.context(format!("search term at {}", term.span.start))),
    }
}

/// `size:` or a date term whose values all start with a letter (after any comparison):
/// constants such as `huge` or `lastweek`, not numbers or dates.
fn named_value(key: Option<&str>, term: &Term) -> bool {
    matches!(key, Some("size" | "dm" | "datemodified" | "modified"))
        && !term.items.is_empty()
        && term.items.iter().all(|i| {
            i.text
                .trim_start_matches(['<', '>', '=', '!'])
                .starts_with(|c: char| c.is_ascii_alphabetic())
        })
}

fn compile_native(term: &Term, ast: &Ast, opts: &Options) -> Result<keel_query::Query> {
    let query = keel_query::compile(
        Ast {
            expr: Expr::Term(term.clone()),
            globals: ast.globals.clone(),
        },
        opts,
    )?;
    if query.has_set_functions() {
        anyhow::bail!(
            "set functions are not supported by library search (at {})",
            term.span.start
        );
    }
    Ok(query)
}

/// Parses `text`. The library never refused a word that ends in a colon (`ferry:` is a
/// word typed before its value), so such a word the language does not know is quoted and
/// read as text; `name:value` with an unknown name is an error. Returns the text that
/// was parsed.
fn parse_text(text: &str, opts: &Options) -> Result<(String, Ast)> {
    let mut text = text.to_owned();
    for _ in 0..32 {
        let error = match keel_query::parse_ast(&text, opts) {
            Ok(ast) => return Ok((text, ast)),
            Err(e) => e,
        };
        let start = error.span.start;
        let end = text
            .get(start..)
            .and_then(|rest| rest.find(char::is_whitespace))
            .map_or(text.len(), |n| start + n);
        let word = text.get(start..end).unwrap_or("");
        if error.kind != keel_query::ErrorKind::Unknown
            || !word.ends_with(':')
            || word.contains('"')
        {
            return Err(error.into());
        }
        text = format!("{}\"{}\"{}", &text[..start], word, &text[end..]);
    }
    Ok((text.clone(), keel_query::parse_ast(&text, opts)?))
}

/// What a walk over the parsed terms learns: the terms the old planner reads, and whether
/// the query is only those, ANDed.
struct Scan<'a> {
    ast: &'a Ast,
    opts: &'a Options,
    text: &'a str,
    now: i64,
    offset: i64,
    parts: Vec<String>,
    flat: bool,
    contents: bool,
}

impl Scan<'_> {
    fn visit(&mut self, expr: &Expr) -> Result<()> {
        match expr {
            Expr::And(children) => {
                for child in children {
                    self.visit(child)?;
                }
            }
            Expr::Or(children) => {
                self.flat = false;
                for child in children {
                    self.visit(child)?;
                }
            }
            Expr::Not(child) => {
                self.flat = false;
                self.visit(child)?;
            }
            Expr::Term(term) => {
                if let Some(q) = sql_term(term, self.text, self.now, self.offset)? {
                    if term.function.as_ref().is_some_and(|f| f.name == "contents") {
                        self.contents = q.contents;
                    }
                    self.parts.push(self.text[term.span.clone()].to_owned());
                } else {
                    compile_native(term, self.ast, self.opts)?;
                    self.flat = false;
                }
            }
        }
        Ok(())
    }
}

pub(super) fn parse(text: &str, now: i64, offset: i64) -> Result<LibraryQuery> {
    let opts = options(now, offset);
    let (text, ast) = parse_text(text, &opts)?;
    let mut scan = Scan {
        ast: &ast,
        opts: &opts,
        text: &text,
        now,
        offset,
        parts: Vec::new(),
        flat: true,
        contents: true,
    };
    scan.visit(&ast.expr)?;
    if scan.flat {
        return LibraryQuery::parse_sql_terms(&scan.parts.join(" "), now, offset);
    }
    let contents = scan.contents;
    Ok(LibraryQuery {
        expression: Some(Expression {
            text,
            now,
            offset,
        }),
        contents,
        ..LibraryQuery::default()
    })
}

enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Sql(usize),
    Native(keel_query::Query),
}

struct SqlLeaf {
    predicate: String,
    args: Vec<Value>,
}

impl Node {
    fn matches(&self, sql: &[bool], props: &RowProps) -> bool {
        match self {
            Self::And(children) => children.iter().all(|c| c.matches(sql, props)),
            Self::Or(children) => children.iter().any(|c| c.matches(sql, props)),
            Self::Not(child) => !child.matches(sql, props),
            Self::Sql(i) => sql[*i],
            Self::Native(query) => query.matches(props),
        }
    }

    /// A necessary condition for matching (or for not matching under NOT).
    fn candidate(&self, leaves: &[SqlLeaf], negate: bool, args: &mut Vec<Value>) -> String {
        match self {
            Self::And(children) | Self::Or(children) => {
                let and = matches!(self, Self::And(_)) != negate;
                if children.is_empty() {
                    return if and { "1" } else { "0" }.into();
                }
                let parts: Vec<_> = children
                    .iter()
                    .map(|c| c.candidate(leaves, negate, args))
                    .collect();
                format!("({})", parts.join(if and { " AND " } else { " OR " }))
            }
            Self::Not(child) => child.candidate(leaves, !negate, args),
            Self::Sql(i) => {
                args.extend(leaves[*i].args.clone());
                format!(
                    "{}({})",
                    if negate { "NOT " } else { "" },
                    leaves[*i].predicate
                )
            }
            Self::Native(_) => "1".into(),
        }
    }
}

fn lower_sql(src: &Source, q: &LibraryQuery, contents: bool) -> SqlLeaf {
    if !q.wants(src) {
        return SqlLeaf {
            predicate: "0".into(),
            args: Vec::new(),
        };
    }
    let Some((filters, mut args)) = q.filters() else {
        return SqlLeaf {
            predicate: "0".into(),
            args: Vec::new(),
        };
    };
    let mut predicate = format!("1{filters}");
    let words = q.text_parts(true);
    if !words.is_empty() {
        let m = words.join(" AND ");
        let mut text_sql =
            "r.id IN (SELECT rowid FROM record_fts WHERE record_fts MATCH ?)".to_owned();
        args.push(Value::Text(m.clone()));
        if q.name.is_empty() {
            text_sql.push_str(" OR r.id IN (SELECT rowid FROM media_fts WHERE media_fts MATCH ?)");
            args.push(Value::Text(m));
            if contents {
                if let Some(m) = q.content_match() {
                    text_sql.push_str(&format!(" OR r.id IN ({IN_CONTENT})"));
                    args.push(Value::Text(m));
                }
            }
        }
        predicate.push_str(&format!(" AND ({text_sql})"));
    }
    // SQL NULL (unknown metadata) is false before Boolean negation, just as Props.
    SqlLeaf {
        predicate: format!("coalesce(({predicate}), 0)"),
        args,
    }
}

struct Lower<'a> {
    src: &'a Source,
    expression: &'a Expression,
    ast: &'a Ast,
    opts: &'a Options,
    contents: bool,
    leaves: Vec<SqlLeaf>,
}

impl Lower<'_> {
    fn node(&mut self, expr: &Expr) -> Result<Node> {
        Ok(match expr {
            Expr::And(children) => Node::And(
                children
                    .iter()
                    .map(|c| self.node(c))
                    .collect::<Result<_>>()?,
            ),
            Expr::Or(children) => Node::Or(
                children
                    .iter()
                    .map(|c| self.node(c))
                    .collect::<Result<_>>()?,
            ),
            Expr::Not(child) => Node::Not(Box::new(self.node(child)?)),
            Expr::Term(term) => {
                if let Some(q) = sql_term(
                    term,
                    &self.expression.text,
                    self.expression.now,
                    self.expression.offset,
                )? {
                    let i = self.leaves.len();
                    self.leaves.push(lower_sql(self.src, &q, self.contents));
                    Node::Sql(i)
                } else {
                    Node::Native(compile_native(term, self.ast, self.opts)?)
                }
            }
        })
    }
}

/// One record as the evaluator sees it. What costs a query (a folder's total, its child
/// counts) is read only when a term asks.
struct RowProps<'a> {
    conn: &'a rusqlite::Connection,
    id: i64,
    name: String,
    path: String,
    dir: bool,
    /// A file's size.
    size: Option<u64>,
    modified: Option<i64>,
    changed: Option<i64>,
    flags: i64,
    total: OnceCell<Option<u64>>,
    children: OnceCell<Option<keel_query::ChildCounts>>,
}

impl Props for RowProps<'_> {
    fn name(&self) -> &str {
        &self.name
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn is_dir(&self) -> bool {
        self.dir
    }

    /// A folder's is the total of the files below it.
    fn size(&self) -> Option<u64> {
        if !self.dir {
            return self.size;
        }
        *self.total.get_or_init(|| {
            self.conn
                .query_row(
                    "SELECT bytes FROM folder_total WHERE record = ?1",
                    [self.id],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
                .and_then(|n| u64::try_from(n).ok())
        })
    }

    fn allocated(&self) -> Option<u64> {
        None
    }

    fn date(&self, which: DateProp) -> Option<i64> {
        match which {
            DateProp::Modified => self.modified,
            DateProp::Changed => self.changed,
            _ => None,
        }
    }

    fn attributes(&self) -> Option<u32> {
        use keel_query::attr;
        let flag = |on: bool, bit: u32| if on { bit } else { 0 };
        Some(
            flag(self.dir, attr::DIRECTORY)
                | flag(self.flags & crate::index::HIDDEN != 0, attr::HIDDEN)
                | flag(self.flags & crate::index::LINK != 0, attr::REPARSE_POINT),
        )
    }

    fn child_counts(&self) -> Option<keel_query::ChildCounts> {
        if !self.dir {
            return None;
        }
        *self.children.get_or_init(|| {
            self.conn
                .query_row(
                    "SELECT coalesce(sum(kind <> 1), 0), coalesce(sum(kind = 1), 0)
                     FROM record WHERE parent = ?1",
                    [self.id],
                    |r| {
                        Ok(keel_query::ChildCounts {
                            files: r.get::<_, i64>(0)?.max(0) as u64,
                            folders: r.get::<_, i64>(1)?.max(0) as u64,
                        })
                    },
                )
                .ok()
        })
    }
}

pub(super) fn search(
    src: &Source,
    q: &LibraryQuery,
    expression: &Expression,
    now: i64,
    contents: bool,
) -> Result<Vec<LibraryHit>> {
    let opts = options(expression.now, expression.offset);
    let ast = keel_query::parse_ast(&expression.text, &opts)?;
    let mut lower = Lower {
        src,
        expression,
        ast: &ast,
        opts: &opts,
        contents: contents && q.contents,
        leaves: Vec::new(),
    };
    let root = lower.node(&ast.expr)?;
    let mut args = Vec::new();
    let mut truth = String::new();
    for leaf in &lower.leaves {
        truth.push_str(&format!(", ({})", leaf.predicate));
        args.extend(leaf.args.clone());
    }
    let candidate = root.candidate(&lower.leaves, false, &mut args);
    // Callers can still add filters (e.g. the pane's folders-only toggle) after parse.
    let Some((filters, filter_args)) = q.filters() else {
        return Ok(Vec::new());
    };
    args.extend(filter_args);
    let sql = format!(
        "SELECT {HIT_COLUMNS}, r.ctime, r.flags {truth}
         FROM record r WHERE ({candidate}){filters} ORDER BY r.mtime DESC, r.id"
    );
    let c = src.store.get()?;
    let mut stmt = c.prepare_cached(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        let rel: String = row.get(1)?;
        let dir = row.get::<_, i64>(3)? == crate::fsid::DIR;
        let ms = |i: usize| -> rusqlite::Result<Option<i64>> {
            Ok(row
                .get::<_, Option<i64>>(i)?
                .map(|n| n.div_euclid(1_000_000)))
        };
        let props = RowProps {
            conn: &c,
            id: row.get(0)?,
            name: row.get(2)?,
            path: src.absolute(&rel).display(),
            dir,
            size: row
                .get::<_, Option<i64>>(4)?
                .and_then(|s| u64::try_from(s).ok()),
            modified: ms(5)?,
            changed: ms(6)?,
            flags: row.get(7)?,
            total: OnceCell::new(),
            children: OnceCell::new(),
        };
        let truth: Vec<bool> = (0..lower.leaves.len())
            .map(|i| row.get(8 + i))
            .collect::<rusqlite::Result<_>>()?;
        if root.matches(&truth, &props) {
            let modified = props.modified.unwrap_or(0).div_euclid(1000);
            let age = now.saturating_sub(modified).max(0) as f64;
            let score = -RECENCY / (1.0 + age / RECENCY_SCALE);
            hits.push(hit_of(src, row, score)?);
            if hits.len() >= q.max {
                break;
            }
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{walk, write};
    use crate::library::tests::folder;

    #[test]
    fn library_shared_language_conformance() {
        let files = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        write(&files.path().join("Reports/alpha.txt"), "a");
        write(&files.path().join("Reports/beta.pdf"), "bbbb");
        write(&files.path().join("photo.jpg"), "picture");
        let lib = Library::open(data.path(), "query").unwrap();
        let src = lib
            .source(&lib.add_source(folder("Fixture", files.path())).unwrap())
            .unwrap();
        walk(&src, &lib.router()).unwrap();
        let names = |query: &str| {
            let mut names: Vec<_> = lib
                .search(&LibraryQuery::parse(query, 0).unwrap())
                .unwrap()
                .into_iter()
                .map(|h| h.name)
                .collect();
            names.sort();
            names
        };
        for (query, expected) in [
            ("ext:txt | ext:pdf", vec!["alpha.txt", "beta.pdf"]),
            ("<ext:txt | ext:pdf> !alpha", vec!["beta.pdf"]),
            ("<ext:txt | ext:pdf> size:>1", vec!["beta.pdf"]),
            (
                "regex:^alpha\\.txt$ | ext:jpg",
                vec!["alpha.txt", "photo.jpg"],
            ),
            ("ext:pdf !regex:^alpha", vec!["beta.pdf"]),
            ("wfn:alpha.txt", vec!["alpha.txt"]),
            ("file: !<ext:pdf | ext:jpg>", vec!["alpha.txt"]),
            ("source:Missing | ext:jpg", vec!["photo.jpg"]),
            ("folder: child-file-count:2", vec!["Reports"]),
        ] {
            assert_eq!(names(query), expected, "{query}");
        }
        let path = files.path().join("Reports").to_string_lossy().into_owned();
        assert_eq!(
            names(&format!("in:\"{path}\" file:")),
            ["alpha.txt", "beta.pdf"]
        );
        for query in ["<ext:txt", "size:wat", "regex:[", "unknownfn:value"] {
            assert!(
                LibraryQuery::parse(query, 0)
                    .unwrap_err()
                    .to_string()
                    .contains("at"),
                "{query}"
            );
        }
    }

    #[test]
    fn residual_filter_precedes_the_limit_and_negation_is_conservative() {
        let files = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        for i in 0..20 {
            write(&files.path().join(format!("item{i:02}.txt")), "x");
        }
        let lib = Library::open(data.path(), "query").unwrap();
        let src = lib
            .source(&lib.add_source(folder("Fixture", files.path())).unwrap())
            .unwrap();
        walk(&src, &lib.router()).unwrap();
        let mut q = LibraryQuery::parse("ext:txt !regex:\"^item(?:0|1[0-8])\"", 0).unwrap();
        q.max = 1;
        let hits = lib.search(&q).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "item19.txt");
    }
}
