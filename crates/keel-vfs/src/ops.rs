use crate::local::long;
use crate::sys;
use anyhow::{Context, Result};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub current: String,
    pub done_items: usize,
    pub total_items: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Skip,
    Overwrite,
    RenameNew,
}

/// Includes directories in the item count. Links and junctions are refused, not followed.
pub fn plan_size(src: &[PathBuf]) -> Result<(u64, usize)> {
    let (mut bytes, mut items) = (0u64, 0usize);
    for root in src {
        for item in walkdir::WalkDir::new(long(root)?)
            .follow_links(false)
            .follow_root_links(false)
        {
            let item = item.with_context(|| format!("scan {}", root.display()))?;
            let metadata = fs::symlink_metadata(item.path())?;
            ensure_regular(item.path(), &metadata)?;
            items = items.checked_add(1).context("too many items")?;
            if metadata.is_file() {
                bytes = bytes.checked_add(metadata.len()).context("size overflow")?;
            }
        }
    }
    Ok((bytes, items))
}

fn ensure_regular(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    // On Windows `is_symlink` covers symlinks and junctions (name-surrogate reparse points).
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "copy/move of links is unsupported: {}",
        path.display()
    );
    Ok(())
}

pub fn copy_local(
    src: &[PathBuf],
    dst_dir: &Path,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    transfer(src, dst_dir, on_conflict, progress, cancel, false)
}

/// Same volume: rename. Other volume: per file, copy with progress, then delete that source
/// only after its copy succeeded. Skipped or failed sources (and their folders) are kept.
pub fn move_local(
    src: &[PathBuf],
    dst_dir: &Path,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    transfer(src, dst_dir, on_conflict, progress, cancel, true)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
    Ok(())
}

/// Canonical path, case-folded where the filesystem is case-insensitive (Windows).
fn canonical_key(path: &Path) -> Result<PathBuf> {
    let p = fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?;
    #[cfg(windows)]
    let p = PathBuf::from(p.to_string_lossy().to_lowercase());
    Ok(p)
}

fn transfer(
    src: &[PathBuf],
    dst_dir: &Path,
    conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    moving: bool,
) -> Result<()> {
    check_cancel(cancel)?;
    let dst = long(dst_dir)?;
    anyhow::ensure!(
        dst.is_dir(),
        "destination is not a directory: {}",
        dst_dir.display()
    );
    let dst_key = canonical_key(&dst)?;
    // Preflight the entire selection before any writes.
    let sources = src.iter().map(|p| long(p)).collect::<Result<Vec<_>>>()?;
    let mut keys: Vec<PathBuf> = Vec::new();
    for path in &sources {
        let metadata =
            fs::symlink_metadata(path).with_context(|| format!("source {}", path.display()))?;
        ensure_regular(path, &metadata)?;
        let key = canonical_key(path)?;
        if metadata.is_dir() {
            anyhow::ensure!(
                !dst_key.starts_with(&key),
                "cannot copy a folder into itself: {}",
                path.display()
            );
        }
        for previous in &keys {
            anyhow::ensure!(
                !key.starts_with(previous) && !previous.starts_with(&key),
                "overlapping source selection: {}",
                path.display()
            );
        }
        keys.push(key);
        let target = dst.join(path.file_name().context("cannot copy a root")?);
        if target.try_exists()? {
            anyhow::ensure!(
                !same_file::is_same_file(path, &target)?,
                "cannot copy a file onto itself: {}",
                path.display()
            );
        }
    }
    let (total_bytes, total_items) = plan_size(&sources)?;
    let mut job = Job {
        state: Progress {
            done_bytes: 0,
            total_bytes,
            current: String::new(),
            done_items: 0,
            total_items,
        },
        conflict,
        progress,
        cancel,
        moving,
    };
    for source in sources {
        check_cancel(cancel)?;
        let destination = dst.join(source.file_name().context("source has no name")?);
        job.node(&source, &destination)?;
    }
    Ok(())
}

struct Job<'a> {
    state: Progress,
    conflict: Conflict,
    progress: &'a dyn Fn(Progress),
    cancel: &'a AtomicBool,
    moving: bool,
}

impl Job<'_> {
    /// Returns false when something under `source` was skipped (so a move keeps it).
    fn node(&mut self, source: &Path, proposed: &Path) -> Result<bool> {
        check_cancel(self.cancel)?;
        let metadata = fs::symlink_metadata(source)?;
        ensure_regular(source, &metadata)?;
        self.state.current = source.to_string_lossy().into_owned();
        let Some(target) = destination(source, proposed, self.conflict)? else {
            return Ok(false);
        };
        if let Ok(target_metadata) = fs::symlink_metadata(&target) {
            ensure_regular(&target, &target_metadata)?;
            anyhow::ensure!(
                !same_file::is_same_file(source, &target)?,
                "cannot copy a file onto itself: {}",
                source.display()
            );
            anyhow::ensure!(
                metadata.is_dir() == target_metadata.is_dir(),
                "file/directory conflict: {}",
                target.display()
            );
        } else if self.moving {
            // Same volume: one rename. Another volume: fall through to copy + delete.
            match sys::rename_noreplace(source, &target) {
                Ok(()) => {
                    let (bytes, items) = plan_size(std::slice::from_ref(&target))?;
                    self.state.done_bytes += bytes;
                    self.state.done_items += items;
                    (self.progress)(self.state.clone());
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("move {} to {}", source.display(), target.display())
                    })
                }
            }
        }
        if metadata.is_dir() {
            if !target.try_exists()? {
                fs::create_dir(&target)?;
            }
            let mut complete = true;
            // Stable traversal makes progress and cancellation deterministic.
            let mut children = fs::read_dir(source)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                complete &= self.node(&child.path(), &target.join(child.file_name()))?;
            }
            if self.moving && complete {
                // remove_dir only removes an empty folder: every child has been moved.
                fs::remove_dir(source)
                    .with_context(|| format!("remove moved folder {}", source.display()))?;
            }
            self.state.current = source.to_string_lossy().into_owned();
            self.state.done_items += 1;
            (self.progress)(self.state.clone());
            return Ok(complete);
        }
        self.copy_file(source, &target, metadata.len())?;
        if self.moving {
            // The verified copy is in place at `target`; only now drop the source.
            fs::remove_file(source)
                .with_context(|| format!("remove moved file {}", source.display()))?;
        }
        self.state.done_bytes += metadata.len();
        self.state.done_items += 1;
        (self.progress)(self.state.clone());
        Ok(true)
    }

    /// Copies to `<target>.keel-partial`, then renames into place. On cancel or error the
    /// partial is deleted and an existing `target` is untouched.
    fn copy_file(&self, source: &Path, target: &Path, size: u64) -> Result<()> {
        let mut partial = target.as_os_str().to_owned();
        partial.push(".keel-partial");
        let partial = PathBuf::from(partial);
        anyhow::ensure!(
            fs::symlink_metadata(&partial).is_err(),
            "leftover partial copy exists: {}",
            partial.display()
        );
        let partial = Partial(partial);
        let base = &self.state;
        let result = sys::copy_file(
            source,
            &partial.0,
            &|bytes| {
                let mut state = base.clone();
                state.done_bytes += bytes;
                (self.progress)(state);
            },
            self.cancel,
        );
        check_cancel(self.cancel)?;
        result?;
        anyhow::ensure!(
            fs::metadata(&partial.0)?.len() == size,
            "source changed during copy: {}",
            source.display()
        );
        if self.conflict == Conflict::Overwrite {
            if let Ok(metadata) = fs::symlink_metadata(target) {
                anyhow::ensure!(
                    metadata.is_file() && !same_file::is_same_file(source, target)?,
                    "unsafe overwrite: {}",
                    target.display()
                );
            }
            fs::rename(&partial.0, target)
        } else {
            // A file that appeared at `target` during the copy is never replaced.
            sys::rename_noreplace(&partial.0, target)
        }
        .with_context(|| format!("place {}", target.display()))
    }
}

/// This job's own incomplete copy; removed on every exit path (a no-op after the rename).
struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn destination(source: &Path, proposed: &Path, conflict: Conflict) -> Result<Option<PathBuf>> {
    let existing = match fs::symlink_metadata(proposed) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(proposed.to_path_buf())),
        // Propagate access errors, and count dangling links as collisions.
        Err(e) => return Err(e.into()),
        Ok(m) => m,
    };
    let is_dir = fs::symlink_metadata(source)?.is_dir();
    match conflict {
        // Folders merge (as in Explorer); the policy applies to the files inside.
        Conflict::Skip if is_dir && existing.is_dir() => Ok(Some(proposed.to_path_buf())),
        Conflict::Skip => Ok(None),
        Conflict::Overwrite => Ok(Some(proposed.to_path_buf())),
        Conflict::RenameNew => {
            let stem = if is_dir {
                proposed.file_name()
            } else {
                proposed.file_stem()
            }
            .context("target has no name")?;
            for n in 2u64.. {
                let mut name = stem.to_os_string();
                name.push(format!(" ({n})"));
                if !is_dir {
                    if let Some(ext) = proposed.extension() {
                        name.push(".");
                        name.push(ext);
                    }
                }
                let candidate = proposed.with_file_name(name);
                match fs::symlink_metadata(&candidate) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(candidate)),
                    Err(e) => return Err(e.into()),
                    Ok(_) => {}
                }
            }
            unreachable!()
        }
    }
}
