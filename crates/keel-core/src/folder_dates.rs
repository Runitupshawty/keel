//! Set folder dates to the newest change below (WinDirStat's cleanup): each folder at and
//! under a chosen one gets the modified time of the newest file below it, at any depth. The
//! preview (`plan::preview`) reads the times from the disk; the plan's job sets them one
//! folder at a time (in any order: setting a folder's time leaves its parent's alone).
//! Links are never followed and do not count; a folder with no file below it keeps its time.

use anyhow::{Context, Result};
use keel_vfs::VPath;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One folder a folder dates plan changes: its modified time now (`was`) and the one it gets
/// (`to`), in nanoseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderDate {
    pub path: VPath,
    pub was: i64,
    pub to: i64,
}

/// A time in nanoseconds since the Unix epoch (`unix_ns`) as a `SystemTime`.
pub fn ns_time(ns: i64) -> SystemTime {
    let d = Duration::from_nanos(ns.unsigned_abs());
    match ns >= 0 {
        true => UNIX_EPOCH + d,
        false => UNIX_EPOCH.checked_sub(d).unwrap_or(UNIX_EPOCH),
    }
}

/// The folders at and under `root` (a local folder, `local` its path) whose modified time
/// is not the newest file time below them, sorted by path (the job finds a folder's time by
/// binary search). An unreadable folder fails the preview: the folders above it could not
/// know their newest change.
pub(crate) fn generate(root: &VPath, local: &Path) -> Result<Vec<FolderDate>> {
    let meta =
        std::fs::metadata(local).with_context(|| format!("{} does not exist", root.display()))?;
    anyhow::ensure!(meta.is_dir(), "{} is not a folder", root.display());
    let mut out = Vec::new();
    walk(root, local, mtime(&meta), &mut out)?;
    out.sort_by(|a, b| crate::arrange::key(&a.path).cmp(&crate::arrange::key(&b.path)));
    Ok(out)
}

fn mtime(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok().map(crate::unix_ns)
}

/// The time `dates` (sorted by path) sets on `folder`.
pub(crate) fn date_of(dates: &[FolderDate], folder: &VPath) -> Option<i64> {
    let k = crate::arrange::key(folder);
    dates
        .binary_search_by(|d| crate::arrange::key(&d.path).cmp(&k))
        .ok()
        .map(|i| dates[i].to)
}

/// The newest file time under `dir` (None: no file below it), pushing the folders to change.
fn walk(
    vdir: &VPath,
    dir: &Path,
    own: Option<i64>,
    out: &mut Vec<FolderDate>,
) -> Result<Option<i64>> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", vdir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot read {}", vdir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut newest: Option<i64> = None;
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        // Not followed: a symbolic link or junction (and what it points to) does not count.
        let kind = e
            .file_type()
            .with_context(|| format!("cannot read {}", vdir.join(&name).display()))?;
        if kind.is_symlink() {
            continue;
        }
        // A folder's own time from its handle: on Windows the copy in its parent's listing
        // can lag behind.
        let meta = match kind.is_dir() {
            true => std::fs::symlink_metadata(e.path()),
            false => e.metadata(),
        }
        .with_context(|| format!("cannot read {}", vdir.join(&name).display()))?;
        let t = if kind.is_dir() {
            walk(&vdir.join(&name), &e.path(), mtime(&meta), out)?
        } else {
            mtime(&meta)
        };
        newest = newest.max(t);
    }
    if let Some(to) = newest.filter(|to| own != Some(*to)) {
        out.push(FolderDate {
            path: vdir.clone(),
            was: own.unwrap_or(0),
            to,
        });
    }
    Ok(newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn file(p: &Path, t: SystemTime) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let f = std::fs::File::create(p).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn each_folder_gets_its_newest_file_below() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        file(&root.join("a/old.txt"), at(1_000_000_000));
        file(&root.join("a/deep/new.txt"), at(1_500_000_000));
        file(&root.join("b/one.txt"), at(1_200_000_000));
        std::fs::create_dir_all(root.join("empty/inner")).unwrap();
        let v = VPath::local(&root);
        let plan = generate(&v, &root).unwrap();
        let to = |rel: &str| {
            let p = if rel.is_empty() {
                v.clone()
            } else {
                rel.split('/').fold(v.clone(), |p, n| p.join(n))
            };
            plan.iter().find(|d| d.path == p).map(|d| d.to)
        };
        let ns = |s: u64| crate::unix_ns(at(s));
        assert_eq!(to("a/deep"), Some(ns(1_500_000_000)));
        assert_eq!(to("a"), Some(ns(1_500_000_000)));
        assert_eq!(to("b"), Some(ns(1_200_000_000)));
        assert_eq!(to(""), Some(ns(1_500_000_000)));
        assert_eq!(to("empty"), None, "no file below: keeps its time");
        assert_eq!(to("empty/inner"), None);
        assert_eq!(plan[0].path, v, "sorted by path");
        assert_eq!(date_of(&plan, &v.join("b")), Some(ns(1_200_000_000)));
        assert_eq!(date_of(&plan, &v.join("empty")), None);
        // Set as planned, a fresh preview has nothing left to change.
        for d in &plan {
            let local = d.path.to_local_path().unwrap();
            keel_vfs::fsattr::set_folder_modified(&local, ns_time(d.to)).unwrap();
        }
        assert_eq!(generate(&v, &root).unwrap(), Vec::new());
    }

    #[test]
    fn times_round_trip_both_sides_of_the_epoch() {
        for t in [at(0), at(1_700_000_000), UNIX_EPOCH - Duration::from_secs(86_400)] {
            assert_eq!(ns_time(crate::unix_ns(t)), t);
        }
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, "x").unwrap();
        let err = generate(&VPath::local(&f), &f).unwrap_err();
        assert!(err.to_string().contains("is not a folder"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn links_do_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        file(&root.join("a.txt"), at(1_000_000_000));
        file(&tmp.path().join("elsewhere/new.txt"), at(1_600_000_000));
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), root.join("link")).unwrap();
        let plan = generate(&VPath::local(&root), &root).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].to, crate::unix_ns(at(1_000_000_000)));
    }
}
