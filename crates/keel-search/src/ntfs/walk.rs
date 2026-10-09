//! Elevation-free fallback: the user's home folder (recursively) and the top level of
//! every fixed drive, walked once and kept current with `notify` (ReadDirectoryChangesW).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind};

use super::index::Index;

/// The walk index: synthetic ids under a virtual root (0) whose children are drives.
pub(crate) struct Walk {
    pub index: Index,
    /// Lowercase full path -> id.
    ids: HashMap<String, u64>,
    next: u64,
    /// Folders indexed recursively; elsewhere only one level is kept.
    deep: Vec<String>,
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
            ids: HashMap::new(),
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

    /// Adds `root` and, when it is a folder, everything under it (to `max_depth`).
    pub fn add_tree(&mut self, root: &Path, max_depth: Option<usize>) {
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
                        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                        let _ = tx.send((entry.into_path(), is_dir));
                    }
                    ignore::WalkState::Continue
                })
            });
        drop(tx);
        for (path, is_dir) in rx {
            self.add(&path, is_dir);
        }
    }

    /// Drops `path` and everything under it.
    pub fn remove(&mut self, path: &Path) {
        let Some((k, _)) = key(path) else {
            return;
        };
        if let Some(id) = self.ids.remove(&k) {
            self.index.remove(id);
        }
        let below = format!("{k}\\");
        let index = &mut self.index;
        self.ids.retain(|path, id| {
            let keep = !path.starts_with(&below);
            if !keep {
                index.remove(*id);
            }
            keep
        });
    }

    fn is_deep(&self, path: &Path) -> bool {
        key(path).is_some_and(|(k, _)| {
            self.deep
                .iter()
                .any(|d| k == *d || k.starts_with(&format!("{d}\\")))
        })
    }

    fn created(&mut self, path: &Path) {
        if self.is_deep(path) {
            self.add_tree(path, None);
        } else if let Ok(meta) = std::fs::symlink_metadata(path) {
            self.add(path, meta.is_dir());
        }
    }

    /// Applies one watcher event.
    pub fn apply(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                event.paths.iter().for_each(|p| self.created(p))
            }
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                event.paths.iter().for_each(|p| self.remove(p))
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() == 2 => {
                self.remove(&event.paths[0]);
                self.created(&event.paths[1]);
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                for p in &event.paths {
                    if p.exists() {
                        self.created(p);
                    } else {
                        self.remove(p);
                    }
                }
            }
            _ => {}
        }
    }
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
        walk.add_tree(&root, None);
        let needle = root.join("sub").join("KeelWalkNeedle.txt");
        assert_eq!(
            names(&walk, "keelwalkneedle"),
            [needle.display().to_string()]
        );

        let moved = root.join("Renamed.txt");
        std::fs::rename(&needle, &moved).unwrap();
        walk.apply(
            &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(needle.clone())
                .add_path(moved.clone()),
        );
        assert!(names(&walk, "keelwalkneedle").is_empty());
        assert_eq!(names(&walk, "renamed"), [moved.display().to_string()]);

        std::fs::create_dir_all(root.join("new").join("deeper")).unwrap();
        std::fs::write(root.join("new").join("deeper").join("inside.txt"), b"x").unwrap();
        walk.apply(
            &Event::new(EventKind::Create(notify::event::CreateKind::Folder))
                .add_path(root.join("new")),
        );
        assert_eq!(names(&walk, "inside.txt").len(), 1);

        walk.apply(
            &Event::new(EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(root.join("new")),
        );
        assert!(names(&walk, "inside.txt").is_empty());
        assert!(names(&walk, "deeper").is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
