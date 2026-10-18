//! The Disk Usage window's **File Watcher** (WinDirStat 2.8): after a scan, its folders
//! are watched ([`keel_vfs::du::watch`]) and what changes is applied to the tree shown, a
//! batch at a time on a worker; the **Changes** tab lists the changes (newest first,
//! narrowed by a regular expression on the path). The toolbar pauses it (changes wait and
//! are applied on resume) and clears the list.

use keel_vfs::du::watch::{Applied, Change, ChangeLog, PathFilter, Watcher};
use keel_vfs::du::Tree;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Changes are applied at most this often (a burst of them is one batch).
const BATCH: Duration = Duration::from_millis(1500);
/// Changes taken from the watcher per frame.
const PER_FRAME: usize = 5000;
/// Paths waiting beyond this: the tree is too far behind; a rescan is the way back.
const MAX_PENDING: usize = 200_000;

/// A tree with the changes applied, from the worker.
pub struct Updated {
    /// The scan it belongs to (`DiskUsage::generation` when it was asked).
    pub generation: u64,
    pub tree: Tree,
    pub applied: Applied,
}

#[derive(Default)]
pub struct FileWatch {
    watcher: Option<Watcher>,
    /// A watcher being set up (inotify adds a watch per folder).
    starting: Option<(u64, Receiver<anyhow::Result<Watcher>>)>,
    /// The scan the watcher is for.
    generation: u64,
    pub paused: bool,
    /// Changes that came while paused.
    held: Vec<Change>,
    pub log: ChangeLog,
    pending: Vec<PathBuf>,
    pending_set: HashSet<PathBuf>,
    /// The tree fell too far behind (too many changes waiting).
    pub overflowed: bool,
    applying: Option<Receiver<anyhow::Result<Updated>>>,
    last_apply: Option<Instant>,
    pub filter_text: String,
    pub filter: PathFilter,
    pub filter_error: Option<String>,
    pub error: Option<String>,
    /// Totals of what was applied since the scan.
    pub added: u64,
    pub removed: u64,
    pub updated: u64,
}

impl FileWatch {
    /// Watches `roots` for scan `generation` (a watcher of an older scan stops).
    pub fn start(&mut self, roots: Vec<PathBuf>, generation: u64) {
        self.stop();
        self.generation = generation;
        if roots.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        crate::worker::spawn("keel-du-watch", move || {
            let _ = tx.send(Watcher::new(&roots));
        });
        self.starting = Some((generation, rx));
    }

    /// Stops watching; the change list stays.
    pub fn stop(&mut self) {
        self.watcher = None;
        self.starting = None;
        self.applying = None;
        self.held.clear();
        self.pending.clear();
        self.pending_set.clear();
        self.overflowed = false;
        self.error = None;
        (self.added, self.removed, self.updated) = (0, 0, 0);
    }

    pub fn watching(&self) -> bool {
        self.watcher.is_some()
    }

    pub fn starting(&self) -> bool {
        self.starting.is_some()
    }

    /// The folders watched.
    pub fn roots(&self) -> &[PathBuf] {
        self.watcher.as_ref().map_or(&[], |w| w.roots())
    }

    /// Changes waiting to be applied (and being applied).
    pub fn waiting(&self) -> usize {
        self.pending.len() + self.held.len()
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if !paused {
            for c in std::mem::take(&mut self.held) {
                self.take(c);
            }
        }
    }

    pub fn clear(&mut self) {
        self.log.clear();
    }

    /// The path filter typed: compiled, or why not (the list then shows everything).
    pub fn set_filter(&mut self, text: String) {
        match PathFilter::new(&text) {
            Ok(f) => {
                self.filter = f;
                self.filter_error = None;
            }
            Err(e) => {
                self.filter = PathFilter::default();
                self.filter_error = Some(e);
            }
        }
        self.filter_text = text;
    }

    fn take(&mut self, c: Change) {
        if self.pending.len() < MAX_PENDING {
            if self.pending_set.insert(c.path.clone()) {
                self.pending.push(c.path.clone());
            }
        } else {
            self.overflowed = true;
        }
        self.log.push(c);
    }

    /// Takes in a watcher that started, new changes, and a finished batch (returned for the
    /// window to show); starts the next batch with `tree` when one is due. Repaints while
    /// anything is going on.
    pub fn poll(
        &mut self,
        ctx: &egui::Context,
        tree: Option<&Arc<Tree>>,
        generation: u64,
    ) -> Option<Updated> {
        if let Some((gen, rx)) = &self.starting {
            match rx.try_recv() {
                Ok(result) => {
                    let current = *gen == generation;
                    self.starting = None;
                    match result {
                        Ok(w) if current => {
                            if let Some((p, why)) = w.failed.first() {
                                self.error = Some(format!("{}: {why}", p.display()));
                            }
                            self.watcher = Some(w);
                        }
                        Ok(_) => {}
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
                Err(TryRecvError::Disconnected) => self.starting = None,
            }
        }
        if let Some(w) = &self.watcher {
            let changes = w.drain(PER_FRAME);
            for c in changes {
                if self.paused {
                    if self.held.len() < MAX_PENDING {
                        self.held.push(c);
                    } else {
                        self.overflowed = true;
                    }
                } else {
                    self.take(c);
                }
            }
            // Notifications arrive between frames: look again soon.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        let mut done = None;
        if let Some(rx) = &self.applying {
            match rx.try_recv() {
                Ok(Ok(u)) => {
                    self.applying = None;
                    if u.generation == generation {
                        self.added += u.applied.added;
                        self.removed += u.applied.removed;
                        self.updated += u.applied.updated;
                        done = Some(u);
                    }
                }
                Ok(Err(e)) => {
                    self.applying = None;
                    self.error = Some(format!("{e:#}"));
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => self.applying = None,
            }
        }
        let due = self.last_apply.is_none_or(|t| t.elapsed() >= BATCH);
        if let (true, None, true, Some(tree)) = (
            due && !self.paused,
            &self.applying,
            !self.pending.is_empty(),
            tree,
        ) {
            if generation == self.generation {
                self.spawn_apply(tree.clone(), generation);
            }
        } else if !self.pending.is_empty() && !self.paused {
            ctx.request_repaint_after(BATCH);
        }
        done
    }

    fn spawn_apply(&mut self, tree: Arc<Tree>, generation: u64) {
        let paths = std::mem::take(&mut self.pending);
        self.pending_set.clear();
        self.last_apply = Some(Instant::now());
        let (tx, rx) = channel();
        crate::worker::spawn("keel-du-apply", move || {
            let mut tree = (*tree).clone();
            let result = tree.apply_changes(&paths).map(|applied| Updated {
                generation,
                tree,
                applied,
            });
            let _ = tx.send(result);
        });
        self.applying = Some(rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::du::{self, Root, ScanHandle, ScanOptions};

    fn wait(w: &mut FileWatch, ctx: &egui::Context, tree: &Arc<Tree>, gen: u64) -> Updated {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(u) = w.poll(ctx, Some(tree), gen) {
                return u;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no update; error {:?}", w.error);
    }

    /// The watcher end to end: a new file in a scanned folder ends up in the tree, the
    /// change list shows it, pausing holds changes until resumed.
    #[test]
    fn watched_changes_reach_the_tree_and_the_list() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("old.txt"), b"12345").unwrap();
        let tree = Arc::new(
            du::scan(
                &[Root::Path(root.clone())],
                &ScanOptions::default(),
                &ScanHandle::new(),
            )
            .unwrap(),
        );
        let ctx = egui::Context::default();
        let mut w = FileWatch::default();
        w.start(vec![root.clone()], 1);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !w.watching() {
            assert!(Instant::now() < deadline, "watcher did not start: {:?}", w.error);
            w.poll(&ctx, Some(&tree), 1);
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::write(root.join("new.txt"), vec![b'x'; 1000]).unwrap();
        let u = wait(&mut w, &ctx, &tree, 1);
        let mut tree = Arc::new(u.tree);
        assert!(tree.find(&root.join("new.txt")).is_some());
        assert_eq!(tree.node(du::TOP).size, 1005);
        // The new file's further notifications settle.
        let settle = Instant::now() + Duration::from_secs(2);
        while Instant::now() < settle {
            if let Some(u) = w.poll(&ctx, Some(&tree), 1) {
                tree = Arc::new(u.tree);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(w.log.newest_first(&w.filter).any(|c| c.path.ends_with("new.txt")));
        w.set_filter(r"old\.".into());
        assert!(!w.log.newest_first(&w.filter).any(|c| c.path.ends_with("new.txt")));
        w.set_filter("[".into());
        assert!(w.filter_error.is_some(), "a bad expression is shown, not applied");
        // Paused: the change waits.
        w.set_paused(true);
        std::fs::remove_file(root.join("old.txt")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while w.waiting() == 0 {
            assert!(Instant::now() < deadline, "no change seen");
            if let Some(u) = w.poll(&ctx, Some(&tree), 1) {
                assert!(u.tree.find(&root.join("old.txt")).is_some(), "applied while paused");
                tree = Arc::new(u.tree);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for _ in 0..10 {
            if let Some(u) = w.poll(&ctx, Some(&tree), 1) {
                assert!(u.tree.find(&root.join("old.txt")).is_some(), "applied while paused");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        w.set_paused(false);
        let u = wait(&mut w, &ctx, &tree, 1);
        assert!(u.tree.find(&root.join("old.txt")).is_none());
        assert_eq!(u.tree.node(du::TOP).size, 1000);
        w.clear();
        assert!(w.log.is_empty());
        // A newer scan: an older one's batches are dropped.
        w.start(vec![root.clone()], 2);
        assert!(!w.watching() && w.waiting() == 0);
    }
}
