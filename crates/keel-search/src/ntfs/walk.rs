//! Elevation-free fallback: the user's home folder (recursively) and the top level of
//! every fixed drive, walked once and kept current with `notify` (ReadDirectoryChangesW).
//! Watcher events become [`Op`]s off the index lock ([`plan`] reads the disk);
//! applying an op is cheap.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind};

use super::index::Index;

/// The walk index: synthetic ids under a virtual root (0) whose children are drives.
pub(crate) struct Walk {
    pub index: Index,
    /// Lowercase full path -> id; ordered, so a folder's descendants are one range.
    ids: BTreeMap<String, u64>,
    next: u64,
    /// Folders indexed recursively; elsewhere only one level is kept.
    pub deep: Vec<String>,
}

/// One change to the walk index, planned from a watcher event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Op {
    /// Drop a path and everything under it.
    Remove(PathBuf),
    /// Add a path (and missing ancestors).
    Add(PathBuf, bool),
    /// Events were lost (watcher overflow or error): walk everything again.
    Rescan,
}

fn key(path: &Path) -> Option<(String, Vec<String>)> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => parts.push(p.as_os_str().to_string_lossy().into_owned()),
            Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None,
        }
    }
    Some((parts.join("\\").to_lowercase(), parts))
}

impl Walk {
    pub fn new(deep: &[PathBuf]) -> Self {
        Self {
            index: Index::new("", 0),
            ids: BTreeMap::new(),
            next: 1,
            deep: deep.iter().filter_map(|d| key(d)).map(|(k, _)| k).collect(),
        }
    }

    /// Inserts `path` and any missing ancestors (as folders); returns its id.
    pub fn add(&mut self, path: &Path, is_dir: bool) -> Option<u64> {
        let (_, parts) = key(path)?;
        let mut parent = 0;
        let mut k = String::new();
        for (n, part) in parts.iter().enumerate() {
            if n > 0 {
                k.push('\\');
            }
            k.push_str(&part.to_lowercase());
            let last = n + 1 == parts.len();
            let id = match self.ids.get(&k) {
                Some(&id) => id,
                None => {
                    let id = self.next;
                    self.next += 1;
                    self.ids.insert(k.clone(), id);
                    id
                }
            };
            self.index.upsert(id, parent, part, !last || is_dir);
            parent = id;
        }
        Some(parent)
    }

    /// Adds `root` and, when it is a folder, everything under it (to `max_depth`),
    /// counting entries found in `progress`.
    pub fn add_tree(&mut self, root: &Path, max_depth: Option<usize>, progress: &AtomicUsize) {
        for (path, is_dir) in scan_tree(root, max_depth, progress) {
            self.add(&path, is_dir);
        }
    }

    /// Drops `path` and everything under it: its descendants are one key range.
    pub fn remove(&mut self, path: &Path) {
        let Some((k, _)) = key(path) else {
            return;
        };
        // Keys below `k` start with "k\"; ']' sorts right after '\'.
        let below: Vec<String> = self
            .ids
            .range(format!("{k}\\")..format!("{k}]"))
            .map(|(path, _)| path.clone())
            .collect();
        for path in below.iter().chain(std::iter::once(&k)) {
            if let Some(id) = self.ids.remove(path) {
                self.index.remove(id);
            }
        }
    }

    /// Applies one planned op; no disk access. `Rescan` is the caller's job.
    pub fn apply(&mut self, op: &Op) {
        match op {
            Op::Remove(path) => self.remove(path),
            Op::Add(path, is_dir) => {
                self.add(path, *is_dir);
            }
            Op::Rescan => {}
        }
    }
}

/// `root` and, when it is a folder, everything under it (to `max_depth`), as
/// (path, is folder), counting them in `progress`. Reads the disk: never call it
/// under the index lock.
pub(crate) fn scan_tree(
    root: &Path,
    max_depth: Option<usize>,
    progress: &AtomicUsize,
) -> Vec<(PathBuf, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .follow_links(false)
        .max_depth(max_depth)
        .build_parallel()
        .run(|| {
            let tx = tx.clone();
            Box::new(move |entry| {
                if let Ok(entry) = entry {
                    progress.fetch_add(1, Ordering::Relaxed);
                    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                    let _ = tx.send((entry.into_path(), is_dir));
                }
                ignore::WalkState::Continue
            })
        });
    drop(tx);
    rx.into_iter().collect()
}

fn is_deep(deep: &[String], path: &Path) -> bool {
    key(path).is_some_and(|(k, _)| {
        deep.iter()
            .any(|d| k == *d || k.starts_with(&format!("{d}\\")))
    })
}

/// Turns watcher events into ops, in order. Reads the disk (a folder created under a
/// deep root is walked, only that subtree), so it runs without the index lock. A
/// watcher error or a rescan flag (lost events) becomes [`Op::Rescan`].
pub(crate) fn plan(
    deep: &[String],
    events: impl IntoIterator<Item = notify::Result<Event>>,
) -> Vec<Op> {
    let mut ops = Vec::new();
    let created = |ops: &mut Vec<Op>, path: &Path| {
        if is_deep(deep, path) {
            let tree = scan_tree(path, None, &AtomicUsize::new(0)).into_iter();
            ops.extend(tree.map(|(p, is_dir)| Op::Add(p, is_dir)));
        } else if let Ok(meta) = std::fs::symlink_metadata(path) {
            ops.push(Op::Add(path.to_path_buf(), meta.is_dir()));
        }
    };
    for event in events {
        let event = match event {
            Ok(event) if !event.need_rescan() => event,
            _ => {
                ops.push(Op::Rescan);
                continue;
            }
        };
        match &event.kind {
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                event.paths.iter().for_each(|p| created(&mut ops, p))
            }
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                ops.extend(event.paths.iter().map(|p| Op::Remove(p.clone())))
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() == 2 => {
                ops.push(Op::Remove(event.paths[0].clone()));
                created(&mut ops, &event.paths[1]);
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                for p in &event.paths {
                    if p.exists() {
                        created(&mut ops, p);
                    } else {
                        ops.push(Op::Remove(p.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    ops
}

/// Folders the fallback indexes: home recursively, each fixed drive one level.
pub(crate) fn roots() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let deep: Vec<PathBuf> = directories::UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .into_iter()
        .collect();
    let shallow = super::win::fixed_ntfs_volumes()
        .iter()
        .map(|v| PathBuf::from(format!("{}\\", v.drive())))
        .collect();
    (deep, shallow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::pattern::compile;
    use crate::Query;

    fn apply(walk: &mut Walk, event: Event) {
        for op in plan(&walk.deep.clone(), [Ok(event)]) {
            walk.apply(&op);
        }
    }

    fn names(walk: &Walk, text: &str) -> Vec<String> {
        let m = compile(&Query {
            text: text.into(),
            ..Query::default()
        })
        .unwrap();
        walk.index
            .search(&m, 100)
            .into_iter()
            .map(|f| f.path)
            .collect()
    }

    #[test]
    fn walk_index_tracks_creates_renames_and_deletes() {
        let root = std::env::temp_dir().join(format!("keel-ntfs-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("KeelWalkNeedle.txt"), b"x").unwrap();
        let mut walk = Walk::new(std::slice::from_ref(&root));
        let progress = AtomicUsize::new(0);
        walk.add_tree(&root, None, &progress);
        assert_eq!(progress.load(Ordering::Relaxed), 3, "root, sub, needle");
        let needle = root.join("sub").join("KeelWalkNeedle.txt");
        assert_eq!(
            names(&walk, "keelwalkneedle"),
            [needle.display().to_string()]
        );

        let moved = root.join("Renamed.txt");
        std::fs::rename(&needle, &moved).unwrap();
        apply(
            &mut walk,
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(needle.clone())
                .add_path(moved.clone()),
        );
        assert!(names(&walk, "keelwalkneedle").is_empty());
        assert_eq!(names(&walk, "renamed"), [moved.display().to_string()]);

        std::fs::create_dir_all(root.join("new").join("deeper")).unwrap();
        std::fs::write(root.join("new").join("deeper").join("inside.txt"), b"x").unwrap();
        apply(
            &mut walk,
            Event::new(EventKind::Create(notify::event::CreateKind::Folder))
                .add_path(root.join("new")),
        );
        assert_eq!(names(&walk, "inside.txt").len(), 1);

        apply(
            &mut walk,
            Event::new(EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(root.join("new")),
        );
        assert!(names(&walk, "inside.txt").is_empty());
        assert!(names(&walk, "deeper").is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lost_events_ask_for_a_rescan() {
        use notify::event::{Flag, RemoveKind};
        let gone = PathBuf::from(r"C:\gone");
        let ops = plan(
            &[],
            [
                Err(notify::Error::generic("overflow")),
                Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)),
                Ok(Event::new(EventKind::Remove(RemoveKind::Any)).add_path(gone.clone())),
            ],
        );
        assert_eq!(ops, [Op::Rescan, Op::Rescan, Op::Remove(gone)]);
    }
}
