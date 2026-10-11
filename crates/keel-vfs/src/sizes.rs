//! Folder sizes by walking (Task 46): Calculate sizes in the app and `keel du` outside the
//! library index. Apparent sizes (a file's length); a hard link counts once per path, a
//! cloud placeholder by its stated size; links are neither counted nor followed.

use crate::{Kind, Router, VPath};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

/// Folders deeper than this below the walked one are not entered (counted as unreadable).
const MAX_DEPTH: usize = 512;

/// A folder's total, as `folder_total` reports each one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Walked {
    pub bytes: u64,
    pub files: u64,
    /// Folders below that could not be listed (counted as empty).
    pub unreadable: u64,
}

/// The total below `dir` (whose modified time is `mtime`). `seen` gets every folder below
/// it, and `dir` last, with its modified time and total once its subtree is done. Stops
/// with "operation cancelled" when `cancel` is set; a listing error of `dir` itself is
/// returned, one below it is counted in `unreadable`.
pub fn folder_total(
    router: &Router,
    dir: &VPath,
    mtime: Option<SystemTime>,
    cancel: &AtomicBool,
    seen: &mut dyn FnMut(&VPath, Option<SystemTime>, Walked),
) -> Result<Walked> {
    let provider = router
        .provider_for(dir)
        .with_context(|| format!("no provider for {}", dir.display()))?;
    let entries = provider.list(dir)?;
    walk(&*provider, dir, mtime, entries, 0, cancel, seen)
}

fn walk(
    provider: &dyn crate::Provider,
    dir: &VPath,
    mtime: Option<SystemTime>,
    entries: Vec<crate::Entry>,
    depth: usize,
    cancel: &AtomicBool,
    seen: &mut dyn FnMut(&VPath, Option<SystemTime>, Walked),
) -> Result<Walked> {
    let mut total = Walked::default();
    for e in entries {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
        if e.is_link {
            continue;
        }
        match e.kind {
            Kind::Dir if depth < MAX_DEPTH => match provider.list(&e.path) {
                Ok(below) => {
                    let sub = walk(
                        provider,
                        &e.path,
                        e.modified,
                        below,
                        depth + 1,
                        cancel,
                        seen,
                    )?;
                    total.bytes += sub.bytes;
                    total.files += sub.files;
                    total.unreadable += sub.unreadable;
                }
                Err(_) => total.unreadable += 1,
            },
            Kind::Dir => total.unreadable += 1,
            _ => {
                total.bytes += e.size;
                total.files += 1;
            }
        }
    }
    seen(dir, mtime, total);
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_every_folder_below_and_stops_on_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("a/one.txt"), "1").unwrap();
        std::fs::write(root.join("a/b/two.txt"), "22").unwrap();
        std::fs::write(root.join("top.txt"), "333").unwrap();
        let router = Router::new();
        let dir = VPath::local(root);
        let mut all = Vec::new();
        let total = folder_total(
            &router,
            &dir,
            None,
            &AtomicBool::new(false),
            &mut |p, _, w| {
                all.push((p.name().to_owned(), w.bytes, w.files));
            },
        )
        .unwrap();
        assert_eq!((total.bytes, total.files, total.unreadable), (6, 3, 0));
        all.sort();
        let names: Vec<_> = all.iter().map(|a| (a.0.as_str(), a.1, a.2)).collect();
        let top = dir.name().to_owned();
        assert!(names.contains(&("a", 3, 2)), "{names:?}");
        assert!(names.contains(&("b", 2, 1)), "{names:?}");
        assert!(names.contains(&("empty", 0, 0)), "{names:?}");
        assert!(names.contains(&(top.as_str(), 6, 3)), "{names:?}");
        let cancelled = folder_total(
            &router,
            &dir,
            None,
            &AtomicBool::new(true),
            &mut |_, _, _| {},
        );
        assert!(format!("{:#}", cancelled.unwrap_err()).contains("operation cancelled"));
    }
}
