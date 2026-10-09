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
        let Some(target) = destination(metadata.is_dir(), proposed, self.conflict)? else {
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

fn destination(is_dir: bool, proposed: &Path, conflict: Conflict) -> Result<Option<PathBuf>> {
    let existing = match fs::symlink_metadata(proposed) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Some(proposed.to_path_buf())),
        // Propagate access errors, and count dangling links as collisions.
        Err(e) => return Err(e.into()),
        Ok(m) => m,
    };
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

/// Extracts `entries` (normalised inner names, files or folders; empty = everything) of the
/// archive at `archive` into `dst_dir`. Zip-slip safe: every name in the archive is checked
/// before the first write and one unsafe name refuses the whole archive; links already in the
/// destination are never followed; links inside the archive are not extracted. Each file is
/// written to a temp file beside its target and renamed into place, so cancel or an error
/// never leaves a half-written file or touches an existing one it was not told to replace.
pub fn extract(
    archive: &crate::VPath,
    entries: &[String],
    dst_dir: &Path,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
) -> Result<()> {
    use crate::archive::safe_name;
    check_cancel(cancel)?;
    let archive = match archive.split_archive() {
        Some((outer, inner)) if inner.trim_matches('/').is_empty() => outer,
        _ => archive.clone(),
    };
    let local = router
        .provider_for(&archive)
        .with_context(|| format!("no provider for {}", archive.display()))?
        .local_copy(&archive)?;
    let mut reader = crate::archive::open_archive(&local)?;
    let dst = long(dst_dir)?;
    anyhow::ensure!(
        dst.is_dir(),
        "destination is not a directory: {}",
        dst_dir.display()
    );
    let selection = entries
        .iter()
        .map(|s| safe_name(s))
        .collect::<Result<Vec<_>>>()?;
    let under = |name: &str, root: &str| {
        name.strip_prefix(root)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    let (mut dirs, mut files) = (Vec::new(), std::collections::HashMap::new());
    let mut total_bytes = 0u64;
    for entry in reader.entries()? {
        // A tarball's own `./` entry names the destination itself.
        if entry.is_dir
            && entry
                .inner
                .split(['/', '\\'])
                .all(|p| p.is_empty() || p == ".")
        {
            continue;
        }
        let name = safe_name(&entry.inner)?;
        if !selection.is_empty() && !selection.iter().any(|s| under(&name, s)) {
            continue;
        }
        anyhow::ensure!(
            !entry.encrypted,
            "password-protected archive entry: {}",
            entry.inner
        );
        if entry.is_dir {
            dirs.push(name);
        } else {
            total_bytes = total_bytes
                .checked_add(entry.size)
                .context("archive size overflow")?;
            files.insert(entry.inner, name);
        }
    }
    for wanted in &selection {
        anyhow::ensure!(
            dirs.iter().chain(files.values()).any(|n| under(n, wanted)),
            "not in the archive: {wanted}"
        );
    }
    let mut state = Progress {
        done_bytes: 0,
        total_bytes,
        current: String::new(),
        done_items: 0,
        total_items: dirs.len() + files.len(),
    };
    progress(state.clone());
    for name in &dirs {
        check_cancel(cancel)?;
        let target = below(&dst, name)?;
        fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;
        state.done_items += 1;
    }
    reader.visit(&|raw| files.contains_key(raw), &mut |raw, input| {
        check_cancel(cancel)?;
        let name = &files[raw];
        state.current = name.clone();
        let proposed = below(&dst, name)?;
        if let Some(target) = destination(false, &proposed, on_conflict)? {
            let parent = target.parent().context("destination has no parent")?;
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
            // Re-check: nothing on the way down may have become a link meanwhile.
            below(&dst, name)?;
            let mut partial = tempfile::NamedTempFile::new_in(parent)?;
            copy_archive_bytes(input, &mut partial, &mut state, progress, cancel)?;
            if let Ok(existing) = fs::symlink_metadata(&target) {
                anyhow::ensure!(
                    on_conflict == Conflict::Overwrite && existing.is_file(),
                    "refusing to replace {}",
                    target.display()
                );
                partial.persist(&target)?;
            } else {
                // A file that appeared at `target` meanwhile is never replaced.
                partial.persist_noclobber(&target)?;
            }
        }
        state.done_items += 1;
        progress(state.clone());
        Ok(())
    })?;
    progress(state);
    Ok(())
}

/// `dst` joined with the `/`-separated `name`, refusing any existing link (or junction) on
/// the way down from `dst`. `dst` itself and its ancestors are the user's choice.
fn below(dst: &Path, name: &str) -> Result<PathBuf> {
    let mut path = dst.to_path_buf();
    for part in name.split('/') {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) => anyhow::ensure!(
                !meta.file_type().is_symlink(),
                "archive destination contains a link: {}",
                path.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

fn copy_archive_bytes(
    input: &mut dyn io::Read,
    output: &mut dyn io::Write,
    state: &mut Progress,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    let mut buffer = vec![0; 64 << 10];
    loop {
        check_cancel(cancel)?;
        let n = match input.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..n])?;
        state.done_bytes = state.done_bytes.saturating_add(n as u64);
        progress(state.clone());
    }
}

/// Adds files and folders (recursively, as `<inner_dir>/<name>/...`) to the zip at
/// `zip_path`, creating it if missing. Entries with the same name are replaced, all others
/// are copied over without recompression. The new zip is written beside the old one and
/// renamed over it, so cancel or an error leaves the original untouched.
#[cfg(feature = "zip")]
pub fn add_to_zip(
    zip_path: &Path,
    src: &[PathBuf],
    inner_dir: &str,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    use std::collections::BTreeMap;
    check_cancel(cancel)?;
    let zip_path = long(zip_path)?;
    let exists = match fs::symlink_metadata(&zip_path) {
        Ok(meta) => {
            anyhow::ensure!(meta.is_file(), "not a zip file: {}", zip_path.display());
            true
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    let prefix = if inner_dir.trim_matches('/').is_empty() {
        String::new()
    } else {
        format!("{}/", crate::archive::safe_name(inner_dir)?)
    };
    let mut files = BTreeMap::new();
    for root in src {
        let root = long(root)?;
        let parent = root.parent().context("cannot add a root")?;
        for item in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .follow_root_links(false)
        {
            let item = item?;
            let meta = fs::symlink_metadata(item.path())?;
            ensure_regular(item.path(), &meta)?;
            anyhow::ensure!(
                !(exists && same_file::is_same_file(item.path(), &zip_path)?),
                "cannot add a zip to itself"
            );
            let relative = item
                .path()
                .strip_prefix(parent)?
                .to_str()
                .context("file name is not valid UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            let name = crate::archive::safe_name(&format!("{prefix}{relative}"))?;
            let name = if meta.is_dir() {
                format!("{name}/")
            } else {
                name
            };
            anyhow::ensure!(
                files
                    .insert(name.clone(), (item.path().to_path_buf(), meta))
                    .is_none(),
                "two sources map to the same zip entry: {name}"
            );
        }
    }
    let mut state = Progress {
        done_bytes: 0,
        total_bytes: files
            .values()
            .filter(|(_, m)| m.is_file())
            .try_fold(0u64, |s, (_, m)| {
                s.checked_add(m.len()).context("size overflow")
            })?,
        current: String::new(),
        done_items: 0,
        total_items: files.len(),
    };
    progress(state.clone());
    let parent = zip_path.parent().context("zip has no parent folder")?;
    let mut partial = tempfile::NamedTempFile::new_in(parent)?;
    let mut output = zip::ZipWriter::new(io::BufWriter::new(partial.as_file_mut()));
    if exists {
        let mut old = zip::ZipArchive::new(io::BufReader::new(fs::File::open(&zip_path)?))?;
        for index in 0..old.len() {
            check_cancel(cancel)?;
            let entry = old.by_index_raw(index)?;
            if !files.contains_key(entry.name()) {
                output.raw_copy_file(entry)?;
            }
        }
    }
    for (name, (path, meta)) in files {
        check_cancel(cancel)?;
        state.current = name.clone();
        let mut options =
            zip::write::SimpleFileOptions::default().large_file(meta.len() >= u32::MAX as u64);
        if let Some(time) = meta
            .modified()
            .ok()
            .and_then(|t| zip::DateTime::try_from(time::OffsetDateTime::from(t)).ok())
        {
            options = options.last_modified_time(time);
        }
        if meta.is_dir() {
            output.add_directory(name.as_str(), options)?;
        } else {
            output.start_file(name.as_str(), options)?;
            copy_archive_bytes(
                &mut fs::File::open(&path)?,
                &mut output,
                &mut state,
                progress,
                cancel,
            )?;
        }
        state.done_items += 1;
        progress(state.clone());
    }
    output.finish()?.into_inner().map_err(|e| e.into_error())?;
    check_cancel(cancel)?;
    partial.as_file().sync_all()?;
    partial.persist(&zip_path)?;
    Ok(())
}
