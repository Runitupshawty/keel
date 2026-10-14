//! A file list as a search backend: the same search language as Keel's indexes
//! (`keel-query`: Everything's syntax), matched against the listed names, paths, sizes,
//! dates and attributes. Nothing on disk is read.

use keel_query::{DateProp, Props, Timestamp};

use crate::walkindex::compile;
use crate::{Hit, Query, Searcher};

use super::time::system_from_filetime;
use super::{Entry, FileList};

/// Searches the entries of one file list.
pub struct FileListSearcher {
    label: String,
    list: FileList,
    cap: Option<u32>,
}

/// A listed entry as the evaluator sees it.
struct ListProps<'a>(&'a Entry);

impl Props for ListProps<'_> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn path(&self) -> &str {
        &self.0.path
    }

    fn is_dir(&self) -> bool {
        self.0.is_dir
    }

    fn size(&self) -> Option<u64> {
        self.0.size
    }

    fn allocated(&self) -> Option<u64> {
        None
    }

    fn date(&self, which: DateProp) -> Option<Timestamp> {
        let filetime = match which {
            DateProp::Modified => self.0.modified,
            DateProp::Created => self.0.created,
            _ => return None,
        }?;
        crate::language::filetime(i64::try_from(filetime).ok()?)
    }

    fn attributes(&self) -> Option<u32> {
        self.0.attributes
    }
}

impl FileListSearcher {
    /// `label` names the list (its file name) in the status bar.
    pub fn new(label: impl Into<String>, list: FileList) -> Self {
        Self {
            label: label.into(),
            list,
            cap: None,
        }
    }

    /// Answers up to `cap` hits whatever a query's `max` says (a file list tab shows
    /// more than a search tab's first page).
    pub fn with_cap(mut self, cap: u32) -> Self {
        self.cap = Some(cap);
        self
    }

    pub fn label(&self) -> &str {
        &self.label
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
}

impl Searcher for FileListSearcher {
    fn unified_query(&self) -> bool {
        true
    }

    /// Every entry whose name matches, ranked like the index (exact name, then name
    /// prefix, then the rest; by path within each); an empty query lists everything.
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let m = compile(q)?;
        let max = self.cap.unwrap_or(q.max) as usize;
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut found: Vec<(u8, usize)> = Vec::new();
        for (i, e) in self.list.entries.iter().enumerate() {
            if m.folders_only && !e.is_dir {
                continue;
            }
            let name = e.name();
            if m.scan.as_ref().is_some_and(|scan| !scan.is_match(name)) {
                continue;
            }
            if let Some(within) = &m.within {
                if !e.path.replace('\\', "/").to_lowercase().starts_with(within) {
                    continue;
                }
            }
            if !m.query.matches(&ListProps(e)) {
                continue;
            }
            found.push((m.quality(name), i));
        }
        // Quality, then path without regard to case, then list order; no per-entry copies
        // (an empty query over a big list ranks every entry).
        let order = |a: &(u8, usize), b: &(u8, usize)| {
            let lower = |i: usize| (self.list.entries[i].path.chars()).flat_map(char::to_lowercase);
            (a.0.cmp(&b.0))
                .then_with(|| lower(a.1).cmp(lower(b.1)))
                .then(a.1.cmp(&b.1))
        };
        // Set functions (`dupe:`, `unique:`, `count:`) see every match, in rank order.
        if let Some(mut pass) = m.query.set_pass() {
            found.sort_unstable_by(order);
            for &(_, i) in &found {
                if !pass.push(i as u64, &ListProps(&self.list.entries[i]), None)? {
                    break;
                }
            }
            found = (pass.finish().into_iter())
                .map(|i| (m.quality(self.list.entries[i as usize].name()), i as usize))
                .collect();
        }
        if found.len() > max {
            found.select_nth_unstable_by(max - 1, order);
            found.truncate(max);
        }
        found.sort_unstable_by(order);
        Ok(found
            .into_iter()
            .map(|(_, i)| {
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
