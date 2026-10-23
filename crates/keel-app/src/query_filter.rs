//! Adapters that let `keel-query` evaluate matches against `keel-search` hits.

use keel_query::{DateProp, Expr, Props, Term, Timestamp};
use keel_search::Hit;
use std::time::UNIX_EPOCH;

/// A search hit as the query evaluator sees it.
#[allow(dead_code)] // pending search-box wiring
pub struct HitProps<'a> {
    h: &'a Hit,
    path: String,
}

impl<'a> HitProps<'a> {
    #[allow(dead_code)] // pending search-box wiring
    pub fn new(h: &'a Hit) -> Self {
        let path = h
            .path
            .to_local_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| h.path.display());
        Self { h, path }
    }
}

impl Props for HitProps<'_> {
    fn name(&self) -> &str {
        self.h.path.name()
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn is_dir(&self) -> bool {
        self.h.is_dir
    }

    fn size(&self) -> Option<u64> {
        if self.h.is_dir {
            None
        } else {
            Some(self.h.size)
        }
    }

    fn allocated(&self) -> Option<u64> {
        None
    }

    fn date(&self, which: DateProp) -> Option<Timestamp> {
        (which == DateProp::Modified)
            .then(|| {
                self.h
                    .modified
                    .and_then(|d| d.duration_since(UNIX_EPOCH).ok())
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            })
            .flatten()
    }

    fn attributes(&self) -> Option<u32> {
        None
    }
}

// Re-evaluate a search's hits against a second query: keep only the hits whose
// name/path/size/date match `text` under `opts`.
#[allow(dead_code)] // pending search-box wiring
pub fn refine(
    hits: Vec<Hit>,
    text: &str,
    opts: &keel_query::Options,
) -> Result<Vec<Hit>, keel_query::Error> {
    let query = keel_query::parse(text, opts)?;
    let props: Vec<HitProps> = hits.iter().map(HitProps::new).collect();
    let selected = query.select(props.iter(), None)?;
    Ok(selected.into_iter().map(|i| hits[i].clone()).collect())
}

/// `true` when `text` needs the full query engine rather than the simple
/// word/glob matcher: `Or` or `Not`, a comparison operator, or any function
/// other than `regex`, `folder` or `in`. Parse errors are `false`.
#[allow(dead_code)] // pending search-box wiring
pub fn needs_full_language(text: &str) -> bool {
    match keel_query::parse_ast(text, &keel_query::Options::default()) {
        Ok(ast) => expr_needs_full(&ast.expr),
        Err(_) => false,
    }
}

fn expr_needs_full(expr: &Expr) -> bool {
    match expr {
        Expr::Or(_) | Expr::Not(_) => true,
        Expr::And(children) => children.iter().any(expr_needs_full),
        Expr::Term(term) => {
            term.items.iter().any(|item| item.op.is_some())
                || term
                    .function
                    .as_ref()
                    .is_some_and(|f| !matches!(f.name.as_str(), "regex" | "folder" | "in"))
        }
    }
}

/// The plain top-level words a backend can still use to narrow by name: the
/// top-level `And`/`Term` items with no function and no comparison operator,
/// joined by spaces. Parse errors give an empty string.
#[allow(dead_code)] // pending search-box wiring
pub fn backend_terms(text: &str) -> String {
    match keel_query::parse_ast(text, &keel_query::Options::default()) {
        Ok(ast) => {
            let mut out = Vec::new();
            top_level_terms(&ast.expr, &mut out);
            out.join(" ")
        }
        Err(_) => String::new(),
    }
}

fn top_level_terms(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::And(children) => {
            for child in children {
                if let Expr::Term(term) = child {
                    push_plain(term, out);
                }
            }
        }
        Expr::Term(term) => push_plain(term, out),
        Expr::Or(_) | Expr::Not(_) => {}
    }
}

fn push_plain(term: &Term, out: &mut Vec<String>) {
    if term.function.is_some() {
        return;
    }
    for item in &term.items {
        if item.op.is_none() {
            out.push(item.text.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::VPath;
    use std::time::Duration;

    fn file_hit(path: &str, size: u64, secs: u64) -> Hit {
        Hit {
            path: VPath::local(path),
            is_dir: false,
            size,
            modified: Some(UNIX_EPOCH + Duration::from_secs(secs)),
            snippet: None,
        }
    }

    #[test]
    fn hit_props_expose_name_path_size_and_date() {
        let hit = file_hit("/a/report.pdf", 2_000_000, 1_700_000_000);
        let p = HitProps::new(&hit);
        assert_eq!(p.name(), "report.pdf");
        assert_eq!(p.path(), "/a/report.pdf");
        assert!(!p.is_dir());
        assert_eq!(p.size(), Some(2_000_000));
        assert_eq!(p.allocated(), None);
        assert_eq!(p.attributes(), None);
        let ms: i64 = 1_700_000_000_000;
        assert_eq!(p.date(DateProp::Modified), Some(ms));
        assert_eq!(p.date(DateProp::Created), None);
    }

    #[test]
    fn refine_keeps_ext_and_size_matches() {
        let hits = vec![
            file_hit("/a/report.pdf", 2_000_000, 1_700_000_000),
            file_hit("/b/note.pdf", 1_000, 1_700_000_000),
            file_hit("/c/photo.jpg", 3_000_000, 1_700_000_000),
        ];
        let out = refine(hits, "ext:pdf size:>=1mb", &keel_query::Options::default()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path.name(), "report.pdf");
    }

    #[test]
    fn refine_runs_dupe_set_function() {
        let hits = vec![
            file_hit("/a/copy1.pdf", 1_000, 1_700_000_000),
            file_hit("/b/copy2.pdf", 1_000, 1_700_000_000),
            file_hit("/c/other.pdf", 5_000, 1_700_000_000),
        ];
        let out = refine(hits, "dupe:size", &keel_query::Options::default()).unwrap();
        assert_eq!(out.len(), 2);
        let names: Vec<&str> = out.iter().map(|h| h.path.name()).collect();
        assert!(names.contains(&"copy1.pdf"));
        assert!(names.contains(&"copy2.pdf"));
    }

    #[test]
    fn plain_words_and_globs_stay_simple() {
        assert!(!needs_full_language("report"));
        assert!(!needs_full_language("report.pdf"));
        assert!(!needs_full_language("*.pdf"));
        assert!(!needs_full_language("photo_2024*"));
        assert!(!needs_full_language("in:Documents"));
        assert!(!needs_full_language("regex:^photo"));
        assert!(!needs_full_language("folder:"));
        assert!(!needs_full_language("zzz:nope"));
    }

    #[test]
    fn ext_size_not_and_or_need_full_language() {
        assert!(needs_full_language("ext:pdf"));
        assert!(needs_full_language("size:>=1mb"));
        assert!(needs_full_language("name:report"));
        assert!(needs_full_language("!pdf"));
        assert!(needs_full_language("pdf | jpg"));
    }

    #[test]
    fn backend_terms_drop_functions_and_operators() {
        assert_eq!(backend_terms("report ext:pdf size:>1mb"), "report");
    }
}
