//! The Disk Usage window's previous scans (**Recent scans**): the folders and drives
//! scanned before, newest first, to scan again in one click. Kept in the settings
//! (`[disk_usage] history`), at most [`MAX`] of them; a scan of the same roots moves to the
//! front, and [`prune`] drops the scans whose folders are all gone.

use keel_vfs::du::Root;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Scans remembered.
pub const MAX: usize = 12;

/// One previous scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The folders and drives scanned (local paths only).
    pub roots: Vec<PathBuf>,
    /// When (unix seconds).
    pub scanned: i64,
    /// What was found: bytes and files.
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub files: u64,
}

impl Entry {
    /// The roots to scan again.
    pub fn scan_roots(&self) -> Vec<Root> {
        self.roots.iter().cloned().map(Root::Path).collect()
    }

    /// "C:\data, D:\" (the menu entry).
    pub fn label(&self) -> String {
        let names: Vec<String> = self.roots.iter().map(|p| p.display().to_string()).collect();
        names.join(", ")
    }

    /// The same roots as `other`, in any order and any case on Windows.
    fn same_roots(&self, other: &[PathBuf]) -> bool {
        let key = |p: &Path| {
            let s = p.to_string_lossy().trim_end_matches(['/', '\\']).to_owned();
            if cfg!(windows) {
                s.to_lowercase()
            } else {
                s
            }
        };
        let mut a: Vec<String> = self.roots.iter().map(|p| key(p)).collect();
        let mut b: Vec<String> = other.iter().map(|p| key(p)).collect();
        a.sort();
        b.sort();
        a.dedup();
        b.dedup();
        a == b
    }
}

/// The folder roots of a scan, or None when it is not a plain folder scan (portable
/// devices, "all local drives") that history can repeat as typed.
pub fn folder_roots(roots: &[Root]) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    for r in roots {
        match r {
            Root::Path(p) => out.push(p.clone()),
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Puts `entry` first; an older scan of the same roots is dropped, and the list is cut to
/// [`MAX`].
pub fn remember(list: &mut Vec<Entry>, entry: Entry) {
    list.retain(|e| !e.same_roots(&entry.roots));
    list.insert(0, entry);
    list.truncate(MAX);
}

/// Drops the scans whose folders are all gone (`exists` says), and any beyond [`MAX`].
/// True when something was dropped.
pub fn prune(list: &mut Vec<Entry>, exists: impl Fn(&Path) -> bool) -> bool {
    let before = list.len();
    list.retain(|e| !e.roots.is_empty() && e.roots.iter().any(|p| exists(p)));
    list.truncate(MAX);
    list.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(roots: &[&str], at: i64) -> Entry {
        Entry {
            roots: roots.iter().map(PathBuf::from).collect(),
            scanned: at,
            bytes: 0,
            files: 0,
        }
    }

    #[test]
    fn a_rescan_moves_first_and_the_list_is_capped() {
        let mut list = Vec::new();
        for i in 0..20 {
            remember(&mut list, entry(&[&format!("/data/{i}")], i));
        }
        assert_eq!(list.len(), MAX);
        assert_eq!(list[0].scanned, 19, "newest first");
        remember(&mut list, entry(&["/data/15"], 99));
        assert_eq!(list.len(), MAX);
        assert_eq!((list[0].scanned, list[0].label()), (99, "/data/15".into()));
        assert_eq!(list.iter().filter(|e| e.label() == "/data/15").count(), 1);
        // The same roots in another order are the same scan.
        remember(&mut list, entry(&["/a", "/b"], 100));
        remember(&mut list, entry(&["/b/", "/a"], 101));
        assert_eq!(list[0].scanned, 101);
        assert_ne!(list[1].roots.len(), 2);
    }

    #[test]
    fn prune_drops_scans_whose_folders_are_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let here = tmp.path().to_string_lossy().into_owned();
        let gone = tmp.path().join("gone").to_string_lossy().into_owned();
        let mut list = vec![
            entry(&[&gone], 3),
            entry(&[&here], 2),
            entry(&[&gone, &here], 1),
            entry(&[], 0),
        ];
        assert!(prune(&mut list, |p| p.exists()));
        let left: Vec<i64> = list.iter().map(|e| e.scanned).collect();
        assert_eq!(left, [2, 1], "one root left is enough");
        assert!(!prune(&mut list, |p| p.exists()), "nothing more to drop");
    }

    #[test]
    fn only_folder_scans_are_remembered() {
        assert!(folder_roots(&[Root::AllLocalDrives]).is_none());
        assert!(folder_roots(&[]).is_none());
        let roots = folder_roots(&[Root::Path("/x".into())]).unwrap();
        assert_eq!(roots, [PathBuf::from("/x")]);
    }

    #[test]
    fn history_round_trips_through_the_settings() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct S {
            history: Vec<Entry>,
        }
        let s = S {
            history: vec![entry(&["/data", "/more"], 5)],
        };
        let text = toml::to_string(&s).unwrap();
        assert_eq!(toml::from_str::<S>(&text).unwrap(), s);
    }
}
