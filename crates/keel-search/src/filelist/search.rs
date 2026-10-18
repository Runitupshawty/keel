//! A file list as a search backend: the same query syntax as Keel's indexes (substring,
//! `*` / `?` wildcards, `regex:`, `folder:`, `in:`), matched against the listed names,
//! with the list's own sizes and dates. Nothing on disk is read.
//!
//! A query the index syntax cannot express (`|`, `!`, `< >`, search functions such as
//! `size:>1mb`, `dm:thisyear`, `ext:mp3;flac`, `filelist:`, `filelist-filename:`, modifiers
//! such as `case:`) is evaluated with Keel's search language (`keel-query`) over the
//! entries instead ([`ListItem`] is an entry as the language sees it).

use std::borrow::Cow;

use crate::walkindex::{compile, eq_case};
use crate::{Hit, Query, Searcher};

use super::time::system_from_filetime;
use super::{Entry, FileList, ATTR_DIRECTORY};

/// Searches the entries of one file list.
pub struct FileListSearcher {
    label: String,
    list: FileList,
    cap: Option<u32>,
    /// The list's own file, for `filelist-filename:` and `filelist-full-path:`.
    source: Option<String>,
}

impl FileListSearcher {
    /// `label` names the list (its file name) in the status bar.
    pub fn new(label: impl Into<String>, list: FileList) -> Self {
        Self {
            label: label.into(),
            list,
            cap: None,
            source: None,
        }
    }

    /// Answers up to `cap` hits whatever a query's `max` says (a file list tab shows
    /// more than a search tab's first page).
    pub fn with_cap(mut self, cap: u32) -> Self {
        self.cap = Some(cap);
        self
    }

    /// The file the list was read from (its full path): `filelist-filename:` and
    /// `filelist-full-path:` match it.
    pub fn with_source(mut self, path: impl Into<String>) -> Self {
        self.source = Some(path.into());
        self
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    pub fn list(&self) -> &FileList {
        &self.list
    }

    pub fn len(&self) -> usize {
        self.list.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.entries.is_empty()
    }

    /// The entries `q` finds (positions in the list), unranked: the index syntax, or the
    /// search language when the query needs it.
    fn found(&self, q: &Query) -> anyhow::Result<Found> {
        if !q.regex && wants_language(&q.text) {
            return self.found_by_language(q);
        }
        let m = compile(q)?;
        let mut found: Vec<(u8, usize)> = Vec::new();
        for (i, e) in self.list.entries.iter().enumerate() {
            if m.folders_only && !e.is_dir {
                continue;
            }
            let name = e.name();
            if !m.scan.iter().chain(&m.rest).all(|re| re.is_match(name)) {
                continue;
            }
            // Match path: every term against the full path instead (the terms are then
            // not in `scan` / `rest`).
            if !m.path.iter().all(|re| re.is_match(&e.path)) {
                continue;
            }
            if let Some(within) = &m.within {
                if !e.path.replace('\\', "/").to_lowercase().starts_with(within) {
                    continue;
                }
            }
            let quality = match &m.literal {
                Some(lit) if eq_case(name, lit, m.match_case) => 0,
                Some(lit)
                    if name
                        .get(..lit.len())
                        .is_some_and(|head| eq_case(head, lit, m.match_case)) =>
                {
                    1
                }
                _ => 2,
            };
            found.push((quality, i));
        }
        Ok(Found::Ranked(found))
    }

    fn found_by_language(&self, q: &Query) -> anyhow::Result<Found> {
        let opts = keel_query::Options {
            match_case: q.match_case,
            match_diacritics: q.match_diacritics,
            match_whole_words: q.whole_word,
            match_path: q.match_path,
            whole_filename_wildcards: q.whole_filename_wildcards,
            ..keel_query::Options::default()
        };
        let query = keel_query::parse(&q.text, &opts).map_err(|e| {
            let at = q.text.get(e.span.clone()).unwrap_or_default();
            anyhow::anyhow!("{} (at \"{at}\")", e.message)
        })?;
        let within = q.within.as_ref().map(|dir| {
            let dir = dir.display().to_string().replace('\\', "/").to_lowercase();
            format!("{}/", dir.trim_end_matches('/'))
        });
        let list = self.source.as_deref();
        let mut pass = query.set_pass();
        let mut found = Vec::new();
        for (i, entry) in self.list.entries.iter().enumerate() {
            if q.folders_only && !entry.is_dir {
                continue;
            }
            if let Some(within) = &within {
                if !entry
                    .path
                    .replace('\\', "/")
                    .to_lowercase()
                    .starts_with(within)
                {
                    continue;
                }
            }
            let item = ListItem { entry, list };
            if !query.matches(&item) {
                continue;
            }
            match &mut pass {
                Some(p) => {
                    if !p
                        .push(i as u64, &item, None)
                        .map_err(|e| anyhow::anyhow!(e.message))?
                    {
                        break;
                    }
                }
                None => found.push((2, i)),
            }
        }
        Ok(match pass {
            // Set functions (`dupe:`, `distinct-sort:`, `count:`) order their own result.
            Some(p) => Found::Ordered(p.finish().into_iter().map(|i| i as usize).collect()),
            None => Found::Ranked(found),
        })
    }
}

/// What a query found: (quality, position) pairs to rank, or positions in their order.
enum Found {
    Ranked(Vec<(u8, usize)>),
    Ordered(Vec<usize>),
}

/// Whether `text` needs the search language rather than the index syntax (plain words,
/// wildcards, `folder:`, `in:`, `regex:`): operators, functions or other modifiers. Text
/// the language cannot read at all because of an unknown `name:` (a path such as `C:x`)
/// stays with the index syntax; any other error is the language's to report.
pub(crate) fn wants_language(text: &str) -> bool {
    if !text.contains([':', '|', '!', '<', '>']) {
        return false;
    }
    match keel_query::parse_ast(text, &keel_query::Options::default()) {
        // `< >` groups leave no trace in the tree.
        Ok(ast) => text.contains('<') || needs(&ast.expr),
        Err(e) => e.kind != keel_query::ErrorKind::Unknown,
    }
}

fn needs(e: &keel_query::Expr) -> bool {
    use keel_query::{Expr, FnKind, Modifier};
    match e {
        Expr::And(v) => v.iter().any(needs),
        Expr::Or(v) => v.len() > 1 || v.iter().any(needs),
        Expr::Not(_) => true,
        Expr::Term(t) => {
            t.function.as_ref().is_some_and(|f| f.kind != FnKind::In)
                || (t.modifiers.iter())
                    .any(|m| !matches!(m, Modifier::Folder | Modifier::Regex(true)))
        }
    }
}

/// Milliseconds since 1970 (the search language's dates) from a FILETIME.
fn millis(filetime: u64) -> Option<i64> {
    i64::try_from(filetime / 10_000)
        .ok()
        .map(|ms| ms - 11_644_473_600_000)
}

/// A listed entry as the search language sees it: the list's name, path, size, dates and
/// attributes, and the list it comes from.
pub struct ListItem<'a> {
    pub entry: &'a Entry,
    /// The list's own file.
    pub list: Option<&'a str>,
}

impl keel_query::Props for ListItem<'_> {
    fn name(&self) -> &str {
        self.entry.name()
    }

    fn path(&self) -> &str {
        &self.entry.path
    }

    fn is_dir(&self) -> bool {
        self.entry.is_dir
    }

    fn size(&self) -> Option<u64> {
        self.entry.size
    }

    fn allocated(&self) -> Option<u64> {
        None
    }

    fn date(&self, which: keel_query::DateProp) -> Option<keel_query::Timestamp> {
        match which {
            keel_query::DateProp::Modified => self.entry.modified.and_then(millis),
            keel_query::DateProp::Created => self.entry.created.and_then(millis),
            _ => None,
        }
    }

    fn attributes(&self) -> Option<u32> {
        match (self.entry.attributes, self.entry.is_dir) {
            (Some(a), true) => Some(a | ATTR_DIRECTORY),
            (a, _) => a,
        }
    }

    fn file_list(&self) -> Option<Cow<'_, str>> {
        self.list.map(Cow::Borrowed)
    }
}

impl Searcher for FileListSearcher {
    /// Every entry whose name matches, ranked like the index (exact name, then name
    /// prefix, then the rest; by path within each); an empty query lists everything.
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let max = self.cap.unwrap_or(q.max) as usize;
        if max == 0 {
            return Ok(Vec::new());
        }
        let positions: Vec<usize> = match self.found(q)? {
            Found::Ordered(mut v) => {
                v.truncate(max);
                v
            }
            Found::Ranked(mut found) => {
                // Quality, then path without regard to case, then list order; no
                // per-entry copies (an empty query over a big list ranks every entry).
                let order = |a: &(u8, usize), b: &(u8, usize)| {
                    let lower =
                        |i: usize| (self.list.entries[i].path.chars()).flat_map(char::to_lowercase);
                    (a.0.cmp(&b.0))
                        .then_with(|| lower(a.1).cmp(lower(b.1)))
                        .then(a.1.cmp(&b.1))
                };
                if found.len() > max {
                    found.select_nth_unstable_by(max - 1, order);
                    found.truncate(max);
                }
                found.sort_unstable_by(order);
                found.into_iter().map(|(_, i)| i).collect()
            }
        };
        Ok(positions
            .into_iter()
            .map(|i| {
                let e = &self.list.entries[i];
                Hit {
                    path: keel_vfs::VPath::local(&e.path),
                    is_dir: e.is_dir,
                    size: if e.is_dir { 0 } else { e.size.unwrap_or(0) },
                    modified: e.modified.and_then(system_from_filetime),
                    snippet: None,
                }
            })
            .collect())
    }

    fn available(&self) -> bool {
        true
    }

    fn status(&self) -> Option<String> {
        Some(format!(
            "file list {}: {} entries",
            self.label,
            self.list.entries.len()
        ))
    }

    fn name(&self) -> &'static str {
        "File list"
    }
}
