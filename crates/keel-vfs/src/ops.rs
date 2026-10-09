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
    transfer_local(src, dst_dir, on_conflict, progress, cancel, false)
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
    transfer_local(src, dst_dir, on_conflict, progress, cancel, true)
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

fn transfer_local(
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

/// Worker-thread transfer with a 1 MiB buffer, staged writes and verified moves.
pub fn transfer(
    src: &[crate::VPath],
    dst_dir: &crate::VPath,
    mv: bool,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
) -> Result<()> {
    if dst_dir.scheme == "file" && src.iter().all(|p| p.scheme == "file") {
        let paths = src
            .iter()
            .map(|p| p.to_local_path().context("invalid local source"))
            .collect::<Result<Vec<_>>>()?;
        let dst = dst_dir
            .to_local_path()
            .context("invalid local destination")?;
        return if mv {
            move_local(&paths, &dst, on_conflict, progress, cancel)
        } else {
            copy_local(&paths, &dst, on_conflict, progress, cancel)
        };
    }
    check_cancel(cancel)?;
    let target_provider = routed(router, dst_dir)?;
    let dst = target_provider.stat(dst_dir)?;
    anyhow::ensure!(
        dst.kind == crate::Kind::Dir && !dst.is_link,
        "destination must be a real directory: {}",
        dst_dir.display()
    );
    let dst_key = target_provider.canonicalize(dst_dir)?;
    let mut roots = Vec::new();
    let mut state = Progress {
        done_bytes: 0,
        total_bytes: 0,
        current: String::new(),
        done_items: 0,
        total_items: 0,
    };
    for source in src {
        let provider = routed(router, source)?;
        let entry = provider.stat(source)?;
        anyhow::ensure!(
            !source.name().is_empty() && !entry.is_link && entry.kind != crate::Kind::Symlink,
            "unsupported source: {}",
            source.display()
        );
        let key = provider.canonicalize(source)?;
        anyhow::ensure!(
            !within(&dst_key, &key),
            "cannot transfer a folder into itself: {}",
            source.display()
        );
        for previous in &roots {
            anyhow::ensure!(
                !within(&key, previous) && !within(previous, &key),
                "overlapping source selection"
            );
        }
        roots.push(key);
        scan_remote(&*provider, source, &mut state, cancel, 0)?;
    }
    let mut job = ProviderJob {
        router,
        state,
        conflict: on_conflict,
        progress,
        cancel,
        moving: mv,
    };
    for source in src {
        job.node(source, &dst_dir.join(source.name()), 0)?;
    }
    Ok(())
}

fn routed(
    router: &crate::Router,
    path: &crate::VPath,
) -> Result<std::sync::Arc<dyn crate::Provider>> {
    router
        .provider_for(path)
        .with_context(|| format!("no provider: {}", path.display()))
}
fn within(path: &crate::VPath, root: &crate::VPath) -> bool {
    if path.scheme != root.scheme || path.authority != root.authority {
        return false;
    }
    let (mut p, mut r) = (
        path.path.clone(),
        root.path.trim_end_matches('/').to_owned(),
    );
    if cfg!(windows) && path.scheme == "file" {
        p.make_ascii_lowercase();
        r.make_ascii_lowercase();
    }
    p == r || p.starts_with(&format!("{r}/"))
}
fn scan_remote(
    provider: &dyn crate::Provider,
    p: &crate::VPath,
    state: &mut Progress,
    cancel: &AtomicBool,
    depth: usize,
) -> Result<()> {
    check_cancel(cancel)?;
    anyhow::ensure!(depth < 256, "directory nesting limit: {}", p.display());
    let e = provider.stat(p)?;
    anyhow::ensure!(
        !e.is_link && e.kind != crate::Kind::Symlink,
        "copy/move of links unsupported: {}",
        p.display()
    );
    state.total_items = state.total_items.checked_add(1).context("too many items")?;
    if e.kind == crate::Kind::Dir {
        for child in provider.list(p)? {
            scan_remote(provider, &child.path, state, cancel, depth + 1)?;
        }
    } else {
        state.total_bytes = state
            .total_bytes
            .checked_add(e.size)
            .context("size overflow")?;
    }
    Ok(())
}
fn maybe_stat(provider: &dyn crate::Provider, p: &crate::VPath) -> Result<Option<crate::Entry>> {
    match provider.stat(p) {
        Ok(e) => Ok(Some(e)),
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
struct ProviderJob<'a> {
    router: &'a crate::Router,
    state: Progress,
    conflict: Conflict,
    progress: &'a dyn Fn(Progress),
    cancel: &'a AtomicBool,
    moving: bool,
}
impl ProviderJob<'_> {
    fn node(
        &mut self,
        source: &crate::VPath,
        proposed: &crate::VPath,
        depth: usize,
    ) -> Result<bool> {
        use crate::Kind;
        check_cancel(self.cancel)?;
        anyhow::ensure!(depth < 256, "directory nesting limit");
        let src = routed(self.router, source)?;
        let dst = routed(self.router, proposed)?;
        let before = src.stat(source)?;
        anyhow::ensure!(
            !before.is_link && before.kind != Kind::Symlink,
            "source is a link: {}",
            source.display()
        );
        let mut target = proposed.clone();
        let existing = maybe_stat(&*dst, &target)?;
        if let Some(e) = &existing {
            let key = src.canonicalize(source)?;
            let target_key = dst.canonicalize(&target)?;
            anyhow::ensure!(
                key != target_key,
                "cannot copy onto itself: {}",
                source.display()
            );
            if self.conflict == Conflict::Skip
                && !(before.kind == Kind::Dir && e.kind == Kind::Dir && !e.is_link)
            {
                return Ok(false);
            }
            if self.conflict == Conflict::RenameNew {
                let parent = target.parent().context("target has no parent")?;
                let (stem, ext) = if before.kind == Kind::Dir {
                    (target.name(), None)
                } else {
                    target
                        .name()
                        .rsplit_once('.')
                        .filter(|(s, _)| !s.is_empty())
                        .map(|(s, e)| (s, Some(e)))
                        .unwrap_or((target.name(), None))
                };
                for n in 2u64.. {
                    let name = match ext {
                        Some(ext) => format!("{stem} ({n}).{ext}"),
                        None => format!("{stem} ({n})"),
                    };
                    let candidate = parent.join(&name);
                    if maybe_stat(&*dst, &candidate)?.is_none() {
                        target = candidate;
                        break;
                    }
                }
            } else {
                anyhow::ensure!(
                    !e.is_link && (e.kind == Kind::Dir) == (before.kind == Kind::Dir),
                    "unsafe destination: {}",
                    target.display()
                );
            }
        }
        if before.kind == Kind::Dir {
            if maybe_stat(&*dst, &target)?.is_none() {
                dst.mkdir(&target)?;
            }
            let mut complete = true;
            for child in src.list(source)? {
                complete &= self.node(&child.path, &target.join(&child.name), depth + 1)?;
            }
            if self.moving && complete {
                src.remove_empty_dir(source)?;
            }
            self.state.done_items += 1;
            (self.progress)(self.state.clone());
            return Ok(complete);
        }
        let partial = target
            .parent()
            .context("missing parent")?
            .join(&format!("{}.keel-partial", target.name()));
        let mut reader = src.read(source)?;
        let mut writer = dst.create_new(&partial)?;
        let mut guard = ProviderPartial {
            provider: dst.clone(),
            path: Some(partial.clone()),
        };
        let mut buffer = vec![0; 1024 * 1024];
        let mut copied = 0u64;
        loop {
            check_cancel(self.cancel)?;
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buffer[..n])?;
            copied += n as u64;
            let mut progress = self.state.clone();
            progress.done_bytes += copied;
            progress.current = source.display();
            (self.progress)(progress);
        }
        check_cancel(self.cancel)?;
        writer.flush()?;
        drop(writer);
        let after = src.stat(source)?;
        anyhow::ensure!(
            copied == before.size
                && after.size == before.size
                && after.modified == before.modified
                && !after.is_link
                && dst.stat(&partial)?.size == copied,
            "source changed or copy size mismatch: {}",
            source.display()
        );
        check_cancel(self.cancel)?;
        if self.conflict == Conflict::Overwrite {
            if let Some(e) = maybe_stat(&*dst, &target)? {
                anyhow::ensure!(
                    !e.is_link && e.kind == Kind::File,
                    "unsafe overwrite: {}",
                    target.display()
                );
            }
            dst.rename_replace(&partial, &target)?;
        } else {
            dst.rename_noreplace(&partial, &target)?;
        }
        guard.path = None;
        anyhow::ensure!(
            dst.stat(&target)?.size == copied,
            "destination verification failed: {}",
            target.display()
        );
        if self.moving {
            check_cancel(self.cancel)?;
            src.remove(source)?;
        }
        self.state.done_bytes += copied;
        self.state.done_items += 1;
        self.state.current = source.display();
        (self.progress)(self.state.clone());
        Ok(true)
    }
}
/// This job's own staged file; removed on every early exit, disarmed once renamed into place.
struct ProviderPartial {
    provider: std::sync::Arc<dyn crate::Provider>,
    path: Option<crate::VPath>,
}
impl Drop for ProviderPartial {
    fn drop(&mut self) {
        match self.path.as_ref().map(|p| (p, p.to_local_path())) {
            // A local partial is permanently deleted, not sent to the trash.
            Some((_, Some(local))) => drop(fs::remove_file(local)),
            Some((p, None)) => drop(self.provider.remove(p)),
            None => {}
        }
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
