//! Folder sizes (Task 46): the totals behind the Details Size column, the status bar and the
//! Sizes view. Inside a library source they come from the index (`LibraryUi::folder_total`);
//! elsewhere from Calculate sizes, a cancellable walk in the jobs panel whose totals are
//! kept for the session by path and modified time.

use crate::library::LibraryUi;
use crate::tab::Tab;
use keel_core::Total;
use keel_vfs::{Entry, Kind, VPath};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// Walked totals kept this session; past this many the cache starts over.
/// ponytail: one cap for all, an LRU if whole-disk walks make it churn.
const MAX_KNOWN: usize = 500_000;

/// A folder's size as the views show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// Being computed (`…`).
    Pending,
    Known(Total),
}

/// Totals walked by Calculate sizes this session.
#[derive(Default)]
pub struct Sizes {
    /// Folder -> (its modified time when walked, total).
    known: HashMap<VPath, (Option<SystemTime>, Total)>,
    /// Folders a running walk is computing -> its job id.
    pending: HashMap<VPath, u64>,
    /// Folders a walk was started in since their last listing (started on their own once).
    started: HashSet<VPath>,
    /// Bumped whenever a total arrives or a walk ends (tabs sort again).
    pub gen: u64,
}

impl Sizes {
    /// A walked folder's size; a total is stale once the folder's modified time moved.
    pub fn get(&self, e: &Entry) -> Option<Size> {
        if self.pending.contains_key(&e.path) {
            return Some(Size::Pending);
        }
        match self.known.get(&e.path) {
            Some((at, total)) if *at == e.modified => Some(Size::Known(*total)),
            _ => None,
        }
    }

    pub fn started(&self, dir: &VPath) -> bool {
        self.started.contains(dir)
    }

    /// A new listing of `dir`: a walk may start there on its own again.
    pub fn listed(&mut self, dir: &VPath) {
        self.started.remove(dir);
    }

    /// The folders of `entries` a walk should compute: all of them when `fresh` (Calculate
    /// sizes asked again; their totals are dropped), else those without a total.
    pub fn wanted(&mut self, entries: &[Entry], fresh: bool) -> Vec<(VPath, Option<SystemTime>)> {
        let mut out = Vec::new();
        for e in entries {
            if e.kind != Kind::Dir || e.is_link || self.pending.contains_key(&e.path) {
                continue;
            }
            if fresh {
                self.known.remove(&e.path);
            }
            if self.get(e).is_none() {
                out.push((e.path.clone(), e.modified));
            }
        }
        out
    }

    /// Job `id` walks these folders of `dir`.
    pub fn walking(&mut self, dir: &VPath, folders: &[(VPath, Option<SystemTime>)], id: u64) {
        self.started.insert(dir.clone());
        for (p, _) in folders {
            self.pending.insert(p.clone(), id);
        }
        self.gen += 1;
    }

    /// Totals a walk found (each folder below a walked one too, so going down is instant).
    pub fn got(&mut self, totals: Vec<(VPath, Option<SystemTime>, Total)>) {
        if self.known.len() + totals.len() > MAX_KNOWN {
            self.known.clear();
        }
        for (p, at, total) in totals {
            self.pending.remove(&p);
            self.known.insert(p, (at, total));
        }
        self.gen += 1;
    }

    /// Every walked total (Task 83: the Sizes view's charts go below the shown folder).
    pub fn walked(&self) -> impl Iterator<Item = (&VPath, Total)> {
        self.known.iter().map(|(p, (_, t))| (p, *t))
    }

    /// Job `id` ended (done, failed or cancelled): what it did not reach is unknown again.
    pub fn job_done(&mut self, id: u64) {
        let before = self.pending.len();
        self.pending.retain(|_, job| *job != id);
        if self.pending.len() != before {
            self.gen += 1;
        }
    }
}

/// A folder entry's size: from the library index when the folder is in a source, else from
/// a walk. None for files and unknown folders.
pub fn size_of(lib: &LibraryUi, sizes: &Sizes, e: &Entry) -> Option<Size> {
    if e.kind != Kind::Dir {
        return None;
    }
    let indexed = lib
        .is_open()
        .then(|| crate::library::real_of(&lib.sources, &e.path))
        .flatten()
        .and_then(|real| lib.folder_total(&real));
    indexed.map(Size::Known).or_else(|| sizes.get(e))
}

/// The bytes an entry counts for: a file's length, a folder's known total.
pub fn bytes_of(lib: &LibraryUi, sizes: &Sizes, e: &Entry) -> Option<u64> {
    match size_of(lib, sizes, e) {
        Some(Size::Known(t)) => Some(t.bytes),
        Some(Size::Pending) => None,
        None if e.kind == Kind::Dir => None,
        None => Some(e.size),
    }
}

/// Copies the known folder totals into the tab (for sorting by size) when they changed.
pub fn sync_tab(tab: &mut Tab, lib: &LibraryUi, sizes: &Sizes) {
    let key = (tab.generation(), lib.sizes_gen, sizes.gen);
    if tab.totals_key == Some(key) {
        return;
    }
    tab.totals_key = Some(key);
    let totals: HashMap<String, u64> = tab
        .entries()
        .iter()
        .filter(|e| e.kind == Kind::Dir)
        .filter_map(|e| match size_of(lib, sizes, e) {
            Some(Size::Known(t)) => Some((e.name.clone(), t.bytes)),
            _ => None,
        })
        .collect();
    tab.set_totals(totals);
}

/// The Size column's text for an entry: a folder's total, `…` while it is computed, empty
/// when unknown.
pub fn size_text(lib: &LibraryUi, sizes: &Sizes, e: &Entry) -> String {
    match size_of(lib, sizes, e) {
        Some(Size::Known(t)) => crate::view_details::size_text_of(t.bytes),
        Some(Size::Pending) => "…".into(),
        None => crate::view_details::size_text(e),
    }
}

/// Whether a walk may start in `dir` without being asked: a folder on a fixed disk of this
/// computer; never a network share (UNC or mapped), a removable drive, a cloud, remote or
/// device path, or an archive. Elsewhere the Sizes view offers a Calculate sizes button.
pub fn walks_on_its_own(dir: &VPath) -> bool {
    dir.split_archive().is_none()
        && dir
            .to_local_path()
            .is_some_and(|p| keel_vfs::is_fixed_disk(&p))
}

/// Calculate sizes starts on its own in `dir` when a listing arrives: in folders on a fixed
/// disk outside the library, with the setting on.
pub fn auto_start(dir: &VPath, lib: &LibraryUi, setting: bool) -> bool {
    let indexed = lib.is_open() && crate::library::source_of(&lib.sources, dir).is_some();
    setting && !indexed && walks_on_its_own(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::test_entry;

    const UNC_DIR: &str = r"\\server\share\photos";

    #[test]
    fn walked_totals_go_stale_with_the_folder_time_and_pending_clears_with_its_job() {
        let dir = VPath::parse("mem://t/").unwrap();
        let mut a = test_entry(&dir, "a", Kind::Dir, 0);
        let b = test_entry(&dir, "b", Kind::Dir, 0);
        let file = test_entry(&dir, "f.txt", Kind::File, 5);
        let mut s = Sizes::default();
        let want = s.wanted(&[a.clone(), b.clone(), file], false);
        assert_eq!(want.len(), 2, "folders only");
        s.walking(&dir, &want, 7);
        assert!(s.started(&dir));
        assert_eq!(s.get(&a), Some(Size::Pending));
        let total = Total { bytes: 9, files: 2 };
        s.got(vec![(a.path.clone(), a.modified, total)]);
        assert_eq!(s.get(&a), Some(Size::Known(total)));
        s.job_done(7);
        assert_eq!(s.get(&b), None, "cancelled: unknown again");
        assert!(s.wanted(&[a.clone()], false).is_empty(), "known");
        a.modified = Some(SystemTime::UNIX_EPOCH);
        assert_eq!(s.get(&a), None, "the folder changed");
        s.listed(&dir);
        assert!(!s.started(&dir));
    }

    #[test]
    fn only_local_folders_start_on_their_own_with_the_setting() {
        let lib = LibraryUi::new(
            &std::sync::Arc::new(keel_vfs::Router::new()),
            crossbeam_channel::unbounded().0,
            egui::Context::default(),
        );
        let local = VPath::local(std::env::temp_dir());
        assert!(auto_start(&local, &lib, true));
        assert!(!auto_start(&local, &lib, false));
        let remote = VPath::parse("sftp://host/home").unwrap();
        assert!(!auto_start(&remote, &lib, true));
        assert!(!walks_on_its_own(&remote));
        // A network share never starts on its own, setting or not.
        let share = VPath::local(if cfg!(windows) {
            UNC_DIR
        } else {
            "/mnt/share/photos"
        });
        assert!(!auto_start(&share, &lib, true), "{}", share.display());
        assert!(!walks_on_its_own(&share));
    }
}
