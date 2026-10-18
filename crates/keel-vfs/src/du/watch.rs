//! The **File Watcher** (WinDirStat 2.8): after a scan, the scanned folders are watched
//! and what changes in them is applied to the tree as it happens.
//!
//! - [`Watcher`] watches the roots (recursively, with the system's notifications:
//!   `ReadDirectoryChangesW`, inotify, FSEvents) and hands over each change once.
//! - [`Tree::apply_changes`] reads the changed items again: a new item is listed and added
//!   (a new folder is walked), a changed file is read again in place, a removed item is
//!   dropped, and the totals above are summed again. It looks at the disk, not at what the
//!   notification said, so a burst of notifications for one item costs one read.
//! - [`ChangeLog`] keeps the latest changes for the window's list, and [`PathFilter`]
//!   narrows it with a regular expression on the path.
//!
//! Adding an item to a folder moves that folder's children to the end of the arena and the
//! tree is compacted afterwards, so ids change: [`Applied::remap`] maps the old ones.
//!
//! [`Tree::apply_changes`]: super::Tree::apply_changes

use super::NodeId;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

/// What happened to a path, as the notification says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Created,
    Deleted,
    Modified,
    /// Renamed from or to this name.
    Renamed,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
        }
    }
}

/// One notification for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// When it arrived (unix ns).
    pub at: i64,
}

/// What [`Tree::apply_changes`](super::Tree::apply_changes) did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Items added (a new folder counts once, with whatever is in it).
    pub added: u64,
    /// Items removed (a removed folder counts once).
    pub removed: u64,
    /// Files and links read again in place.
    pub updated: u64,
    /// When ids changed: each old id's new one ([`super::NONE`] for an item removed),
    /// indexed by the old id. None: every id is as it was.
    pub remap: Option<Vec<NodeId>>,
}

impl Applied {
    /// Nothing in the tree changed.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.updated == 0
    }

    /// `id`'s id after the changes (None when it was removed).
    pub fn map(&self, id: NodeId) -> Option<NodeId> {
        match &self.remap {
            None => Some(id),
            Some(r) => r.get(id as usize).copied().filter(|&n| n != super::NONE),
        }
    }
}

/// Watches folders recursively; drop it to stop.
pub struct Watcher {
    _inner: notify::RecommendedWatcher,
    rx: Receiver<Change>,
    roots: Vec<PathBuf>,
    /// Roots that could not be watched, and why.
    pub failed: Vec<(PathBuf, String)>,
}

impl Watcher {
    /// Watches every folder of `roots` with what is below it. Fails when none can be
    /// watched. On Linux this adds one watch per folder: call it off the UI thread.
    pub fn new(roots: &[PathBuf]) -> Result<Self> {
        use notify::Watcher as _;
        let (tx, rx) = channel();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            let Some(kind) = kind_of(&ev.kind) else {
                return;
            };
            let at = super::walk::now_ns();
            for path in ev.paths {
                let _ = tx.send(Change { path, kind, at });
            }
        })
        .context("the file watcher could not start")?;
        let mut watched = Vec::new();
        let mut failed = Vec::new();
        for r in roots {
            match inner.watch(r, notify::RecursiveMode::Recursive) {
                Ok(()) => watched.push(r.clone()),
                Err(e) => failed.push((r.clone(), e.to_string())),
            }
        }
        if watched.is_empty() {
            let why = (failed.first()).map_or_else(|| "nothing to watch".to_owned(), |f| f.1.clone());
            anyhow::bail!("the folders cannot be watched: {why}");
        }
        Ok(Watcher {
            _inner: inner,
            rx,
            roots: watched,
            failed,
        })
    }

    /// The folders watched.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The changes that arrived since the last call (at most `max`).
    pub fn drain(&self, max: usize) -> Vec<Change> {
        self.rx.try_iter().take(max).collect()
    }
}

/// The change a notification is; None for reads (access) only.
fn kind_of(kind: &notify::EventKind) -> Option<ChangeKind> {
    use notify::event::ModifyKind;
    use notify::EventKind as E;
    Some(match kind {
        E::Create(_) => ChangeKind::Created,
        E::Remove(_) => ChangeKind::Deleted,
        E::Modify(ModifyKind::Name(_)) => ChangeKind::Renamed,
        E::Modify(_) | E::Any | E::Other => ChangeKind::Modified,
        E::Access(_) => return None,
    })
}

/// The latest changes, newest last, at most `cap` of them.
#[derive(Clone, Debug)]
pub struct ChangeLog {
    entries: VecDeque<Change>,
    cap: usize,
    /// Changes logged since the last [`clear`](Self::clear) (also the ones dropped for room).
    pub total: u64,
}

impl Default for ChangeLog {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl ChangeLog {
    pub fn new(cap: usize) -> Self {
        ChangeLog {
            entries: VecDeque::new(),
            cap: cap.max(1),
            total: 0,
        }
    }

    pub fn push(&mut self, c: Change) {
        if self.entries.len() == self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(c);
        self.total += 1;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.total = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Newest first, only the paths `filter` matches.
    pub fn newest_first<'a>(&'a self, filter: &'a PathFilter) -> impl Iterator<Item = &'a Change> {
        self.entries.iter().rev().filter(|c| filter.matches(&c.path))
    }
}

/// A regular expression on the path (any case), or nothing (every path).
#[derive(Clone, Debug, Default)]
pub struct PathFilter(Option<regex::Regex>);

impl PathFilter {
    /// `text` as a case-insensitive regular expression; "" matches everything.
    pub fn new(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self(None));
        }
        regex::RegexBuilder::new(text)
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
            .map(|r| Self(Some(r)))
            .map_err(|e| e.to_string())
    }

    pub fn matches(&self, path: &Path) -> bool {
        match &self.0 {
            None => true,
            Some(r) => r.is_match(&path.to_string_lossy()),
        }
    }
}
