//! File lists in the index (Settings → Search index → File lists): each list's entries are
//! searched with whatever answers the index (Everything, Keel's own index, Spotlight, ...),
//! as Everything's "File lists" index page does. A list is read on a worker when it is
//! added, and read again when its file changes (checked every [`POLL`]) unless its
//! **Monitor changes** is off. A list that cannot be read is skipped, with the reason in
//! the status bar. Only the lists' own files are read, never the paths they name.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use super::{read_file, FileListSearcher, Limits};
use crate::index_settings::{FileListRoot, IndexSettings};
use crate::walkindex::merge_hits;
use crate::{Hit, Query, SearchState, Searcher};

/// How often monitored lists are checked for changes.
pub const POLL: Duration = Duration::from_secs(3);

/// What identifies a version of a list's file: its length and modified time.
type Stamp = (u64, Option<SystemTime>);

fn stamp(path: &Path) -> Option<Stamp> {
    let md = std::fs::metadata(path).ok()?;
    md.is_file().then(|| (md.len(), md.modified().ok()))
}

/// One list of the settings, as read.
struct Loaded {
    root: FileListRoot,
    /// The file as last read (None: not read, or it was not there).
    stamp: Option<Stamp>,
    searcher: Option<Arc<FileListSearcher>>,
    error: Option<String>,
}

struct Inner {
    settings: Mutex<Vec<FileListRoot>>,
    lists: Mutex<Vec<Loaded>>,
    limits: Limits,
    /// A list was read (again) since the last `take_ready`.
    swapped: AtomicBool,
    /// One refresh at a time (the poll and a settings change).
    refreshing: Mutex<()>,
}

impl Inner {
    /// Reads the lists that are new, whose root changed, or (monitored) whose file changed;
    /// forgets the ones no longer in the settings.
    fn refresh(&self) {
        let _one = self.refreshing.lock();
        let wanted = self.settings.lock().clone();
        // What to read: (root, its file's stamp now).
        let todo: Vec<(FileListRoot, Option<Stamp>)> = {
            let lists = self.lists.lock();
            (wanted.iter())
                .filter_map(|root| {
                    let now = stamp(Path::new(&root.path));
                    let old = lists.iter().find(|l| l.root.path == root.path);
                    let read = match old {
                        None => true,
                        Some(l) if l.root != *root => true,
                        Some(l) => l.root.monitor && l.stamp != now,
                    };
                    read.then(|| (root.clone(), now))
                })
                .collect()
        };
        let mut read = Vec::with_capacity(todo.len());
        for (root, now) in todo {
            let path = PathBuf::from(&root.path);
            let label = path.file_name().map_or_else(
                || root.path.clone(),
                |n| n.to_string_lossy().into_owned(),
            );
            let (searcher, error) = match read_file(&path, &self.limits) {
                Ok(list) => (
                    Some(Arc::new(
                        FileListSearcher::new(label, list).with_source(root.path.clone()),
                    )),
                    None,
                ),
                Err(e) => (None, Some(format!("{label}: {e:#}"))),
            };
            read.push(Loaded {
                root,
                stamp: now,
                searcher,
                error,
            });
        }
        let mut lists = self.lists.lock();
        let before = lists.len();
        lists.retain(|l| wanted.iter().any(|w| w.path == l.root.path));
        let changed = lists.len() != before || !read.is_empty();
        for fresh in read {
            match lists.iter_mut().find(|l| l.root.path == fresh.root.path) {
                Some(slot) => *slot = fresh,
                None => lists.push(fresh),
            }
        }
        // In the settings' order.
        lists.sort_by_key(|l| wanted.iter().position(|w| w.path == l.root.path));
        if changed {
            self.swapped.store(true, Relaxed);
        }
    }

    fn searchers(&self) -> Vec<Arc<FileListSearcher>> {
        (self.lists.lock().iter())
            .filter_map(|l| l.searcher.clone())
            .collect()
    }
}

/// The file lists of Settings → Search index, searched as one backend.
pub struct FileListIndexes {
    inner: Arc<Inner>,
    stop: Arc<AtomicBool>,
}

impl FileListIndexes {
    /// The lists `settings` name, read and then watched on a background thread.
    pub fn open(settings: &IndexSettings) -> Self {
        let this = Self::new(settings, Limits::default());
        let (weak, stop) = (Arc::downgrade(&this.inner), this.stop.clone());
        let _ = std::thread::Builder::new()
            .name("keel-file-lists".into())
            .spawn(move || poll(weak, stop));
        this
    }

    /// Without the background thread: nothing is read until [`refresh`](Self::refresh).
    pub fn new(settings: &IndexSettings, limits: Limits) -> Self {
        Self {
            inner: Arc::new(Inner {
                settings: Mutex::new(settings.file_lists.clone()),
                lists: Mutex::new(Vec::new()),
                limits,
                swapped: AtomicBool::new(false),
                refreshing: Mutex::new(()),
            }),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Reads new and changed lists now.
    pub fn refresh(&self) {
        self.inner.refresh();
    }

    /// How many entries each readable list has, in the settings' order.
    pub fn counts(&self) -> Vec<(String, usize)> {
        (self.inner.lists.lock().iter())
            .filter_map(|l| Some((l.root.path.clone(), l.searcher.as_ref()?.len())))
            .collect()
    }
}

impl Drop for FileListIndexes {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

fn poll(inner: Weak<Inner>, stop: Arc<AtomicBool>) {
    let mut first = true;
    loop {
        if !first {
            std::thread::sleep(POLL);
        }
        first = false;
        if stop.load(Relaxed) {
            return;
        }
        let Some(inner) = inner.upgrade() else {
            return;
        };
        inner.refresh();
    }
}

impl Searcher for FileListIndexes {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut hits = Vec::new();
        let mut error = None;
        for list in self.inner.searchers() {
            match list.query(q) {
                Ok(h) => hits.extend(h),
                Err(e) => error = Some(e),
            }
        }
        match error {
            // A bad query fails in every list alike.
            Some(e) if hits.is_empty() => Err(e),
            _ => Ok(merge_hits(q, hits)),
        }
    }

    fn available(&self) -> bool {
        (self.inner.lists.lock().iter()).any(|l| l.searcher.is_some())
    }

    /// A list never holds a folder the way an index does: a pane's folder is searched
    /// by the index (or walked), with the lists' entries in it added.
    fn covers(&self, _dir: &Path) -> bool {
        false
    }

    fn state(&self) -> SearchState {
        if self.available() {
            SearchState::Ready
        } else {
            SearchState::Unavailable
        }
    }

    /// Only the lists that could not be read.
    fn status(&self) -> Option<String> {
        let errors: Vec<String> = (self.inner.lists.lock().iter())
            .filter_map(|l| l.error.clone())
            .collect();
        (!errors.is_empty()).then(|| format!("file list {}", errors.join("; ")))
    }

    fn name(&self) -> &'static str {
        "File lists"
    }

    fn take_ready(&self) -> bool {
        self.inner.swapped.swap(false, Relaxed)
    }

    fn set_index_settings(&self, settings: &IndexSettings) {
        {
            let mut s = self.inner.settings.lock();
            if *s == settings.file_lists {
                return;
            }
            s.clone_from(&settings.file_lists);
        }
        self.inner.refresh();
    }
}

/// A search backend with the file lists of Settings → Search index added to its answers.
/// Everything else (name, state, what it covers, the index settings it changed) is the
/// backend's.
pub(crate) struct WithFileLists {
    base: Box<dyn Searcher>,
    lists: FileListIndexes,
}

impl WithFileLists {
    pub(crate) fn new(base: Box<dyn Searcher>, lists: FileListIndexes) -> Self {
        Self { base, lists }
    }
}

impl Searcher for WithFileLists {
    fn query(&self, query: &Query) -> anyhow::Result<Vec<Hit>> {
        let base = self.base.query(query);
        let lists = if self.lists.available() {
            self.lists.query(query).ok().filter(|h| !h.is_empty())
        } else {
            None
        };
        match (base, lists) {
            (Ok(mut hits), Some(more)) => {
                hits.extend(more);
                Ok(merge_hits(query, hits))
            }
            (Ok(hits), None) => Ok(hits),
            // The index is still building, or down: the lists answer meanwhile.
            (Err(_), Some(more)) => Ok(more),
            (Err(e), None) => Err(e),
        }
    }

    fn available(&self) -> bool {
        self.base.available() || self.lists.available()
    }

    fn probe(&self) -> bool {
        self.base.probe() || self.lists.available()
    }

    fn status(&self) -> Option<String> {
        let notes: Vec<String> = [self.base.status(), self.lists.status()]
            .into_iter()
            .flatten()
            .collect();
        (!notes.is_empty()).then(|| notes.join("; "))
    }

    fn name(&self) -> &'static str {
        self.base.name()
    }

    fn state(&self) -> SearchState {
        match self.base.state() {
            SearchState::Unavailable if self.lists.available() => SearchState::Ready,
            state => state,
        }
    }

    fn take_ready(&self) -> bool {
        let base = self.base.take_ready();
        self.lists.take_ready() || base
    }

    fn offline_roots(&self) -> Vec<PathBuf> {
        self.base.offline_roots()
    }

    fn set_index_settings(&self, settings: &IndexSettings) {
        self.base.set_index_settings(settings);
        self.lists.set_index_settings(settings);
    }

    fn index_settings_changed(&self) -> Option<IndexSettings> {
        self.base.index_settings_changed()
    }

    fn covers(&self, dir: &Path) -> bool {
        self.base.covers(dir)
    }
}
