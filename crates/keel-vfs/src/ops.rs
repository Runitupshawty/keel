use crate::local::long;
use crate::sys;
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub current: String,
    pub done_items: usize,
    pub total_items: usize,
    /// Items left alone because they already existed (`Conflict::Skip`).
    pub skipped: usize,
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
    transfer_local(src, dst_dir, on_conflict, progress, cancel, false, None)
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
    transfer_local(src, dst_dir, on_conflict, progress, cancel, true, None)
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

/// A resumable transfer records its progress after this many placed files...
pub const RECORD_FILES: usize = 256;
/// ...or this many bytes (placed, or written to the file in progress).
pub const RECORD_BYTES: u64 = 4 << 20;

/// A file's size and modified time (nanoseconds since 1970), as a resumable transfer
/// records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub mtime: Option<i64>,
}

impl Stamp {
    pub fn new(size: u64, modified: Option<std::time::SystemTime>) -> Stamp {
        let mtime = modified.and_then(|t| match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).ok(),
            Err(e) => i64::try_from(e.duration().as_nanos()).ok().map(|n| -n),
        });
        Stamp { size, mtime }
    }
    fn of(m: &fs::Metadata) -> Stamp {
        Stamp::new(m.len(), m.modified().ok())
    }
    fn entry(e: &crate::Entry) -> Stamp {
        Stamp::new(e.size, e.modified)
    }
}

/// What a resumable transfer placed for one source path.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Placed {
    pub target: crate::VPath,
    /// The placed file as it was then. None: a folder the transfer made, so everything in
    /// it is the transfer's own.
    pub file: Option<Stamp>,
}

/// The file a resumable transfer was writing when it last recorded.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Unfinished {
    pub source: crate::VPath,
    pub source_stamp: Stamp,
    pub target: crate::VPath,
    /// Where the bytes go until the file is placed at `target`.
    pub staging: crate::VPath,
    /// Bytes of `staging` written (and flushed) by then, and its modified time then.
    pub bytes: u64,
    pub staging_mtime: Option<i64>,
}

/// Whether `staging` (now `now`) can be continued at `u.bytes`: it holds at least those
/// bytes and was last written at the record (exactly that long) or after it (what came
/// after the record is cut off). Anything else is not the file recorded: start over.
fn continues(u: &Unfinished, now: Stamp) -> bool {
    match now.size.cmp(&u.bytes) {
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => now.mtime == u.staging_mtime,
        std::cmp::Ordering::Greater => now.mtime >= u.staging_mtime,
    }
}

/// A resumed transfer found a target it had placed (or a file inside a folder it made)
/// different from what it placed: it never overwrites it.
#[derive(Debug)]
pub struct TargetChanged(pub String);

impl std::fmt::Display for TargetChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} changed since the preview (this operation had copied it there before it \
             stopped): confirm a new preview",
            self.0
        )
    }
}

impl std::error::Error for TargetChanged {}

type Record<'j> =
    Box<dyn FnMut(Vec<(crate::VPath, Placed)>, Option<&Unfinished>) -> Result<()> + 'j>;

/// A resumable transfer's memory (`transfer_resumable`): what earlier runs placed, the file
/// they were writing, and where this run records its own progress: after [`RECORD_FILES`]
/// files or [`RECORD_BYTES`] bytes, before it makes a folder or a renamed file at a new
/// name, and when it stops.
pub struct Journal<'j> {
    /// Placed by earlier runs, by source path.
    pub placed: HashMap<crate::VPath, Placed>,
    pub unfinished: Option<Unfinished>,
    /// Files placed so far, by earlier runs too.
    pub files: u64,
    /// For the job log: what became of an unfinished file.
    pub notes: Vec<String>,
    record: Record<'j>,
    fresh: Vec<(crate::VPath, Placed)>,
    bytes: u64,
}

impl<'j> Journal<'j> {
    /// `record` gets what was placed since its last call and the file being written (if
    /// any), and must make both durable before it returns.
    pub fn new(
        placed: HashMap<crate::VPath, Placed>,
        unfinished: Option<Unfinished>,
        record: impl FnMut(Vec<(crate::VPath, Placed)>, Option<&Unfinished>) -> Result<()> + 'j,
    ) -> Journal<'j> {
        let files = placed.values().filter(|p| p.file.is_some()).count() as u64;
        Journal {
            placed,
            unfinished,
            files,
            notes: Vec::new(),
            record: Box::new(record),
            fresh: Vec::new(),
            bytes: 0,
        }
    }

    /// Records what is not recorded yet.
    pub fn flush(&mut self) -> Result<()> {
        let fresh = std::mem::take(&mut self.fresh);
        self.bytes = 0;
        (self.record)(fresh, self.unfinished.as_ref())
    }

    /// `source` is placed at `placed`; `now`: record it before going on.
    fn placed(&mut self, source: crate::VPath, placed: Placed, now: bool) -> Result<()> {
        if let Some(stamp) = placed.file {
            self.files += 1;
            self.bytes += stamp.size;
        }
        self.fresh.push((source, placed));
        if now || self.fresh.len() >= RECORD_FILES || self.bytes >= RECORD_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    /// The unfinished record of `source`, taken out.
    fn take_unfinished(&mut self, source: &crate::VPath) -> Option<Unfinished> {
        self.unfinished.take_if(|u| u.source == *source)
    }
}

/// How a journaled transfer takes one source before its conflict policy.
enum Settle {
    /// Placed already, at this target: counted, never written again.
    Done(crate::VPath),
    /// Goes to this target, which is the transfer's own (no conflict policy): a folder it
    /// made, or the target of the file it was writing.
    Into(crate::VPath),
    Normal,
}

/// Placed files are known from the record, and from what is in a folder the transfer made
/// (`fresh`) or at the target of the file it was writing: an earlier run placed those, so
/// they must match the source or the transfer stops ([`TargetChanged`]). Any other target
/// that exists gets the conflict policy, as in a first run. `stat` gives a target's (is a
/// folder, stamp), None when it is missing. `exact`: targets keep the source's modified
/// time (local copies), so it is compared too (to the 2 s of FAT); else sizes only.
#[allow(clippy::too_many_arguments)]
fn settle(
    j: &Journal,
    key: &crate::VPath,
    source: Stamp,
    source_dir: bool,
    proposed: &crate::VPath,
    fresh: bool,
    exact: bool,
    stat: impl Fn(&crate::VPath) -> Result<Option<(bool, Stamp)>>,
) -> Result<Settle> {
    match j.placed.get(key) {
        Some(Placed {
            target,
            file: Some(stamp),
        }) => {
            return match stat(target)? {
                Some((false, now)) if now == *stamp => Ok(Settle::Done(target.clone())),
                _ => Err(TargetChanged(target.display()).into()),
            }
        }
        Some(Placed { target, file: None }) => return Ok(Settle::Into(target.clone())),
        None => {}
    }
    let unfinished = j
        .unfinished
        .as_ref()
        .filter(|u| u.source == *key)
        .map(|u| u.target.clone());
    let ours = fresh || unfinished.is_some();
    let target = unfinished.clone().unwrap_or_else(|| proposed.clone());
    // FAT keeps modified times to 2 s.
    let close = |a: Option<i64>, b: Option<i64>| match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= 2_000_000_000,
        (a, b) => a == b,
    };
    let same = |now: Stamp| now.size == source.size && (!exact || close(now.mtime, source.mtime));
    Ok(match stat(&target)? {
        None if unfinished.is_some() => Settle::Into(target),
        None => Settle::Normal,
        Some((true, _)) if source_dir && ours => Settle::Into(target),
        Some((false, now)) if !source_dir && ours => {
            anyhow::ensure!(same(now), TargetChanged(target.display()));
            Settle::Done(target)
        }
        Some(_) => Settle::Normal,
    })
}

fn local_target(t: &crate::VPath) -> Result<PathBuf> {
    long(
        &t.to_local_path()
            .with_context(|| format!("not a local path: {}", t.display()))?,
    )
}

fn local_stat(t: &crate::VPath) -> Result<Option<(bool, Stamp)>> {
    match fs::symlink_metadata(local_target(t)?) {
        Ok(m) => Ok(Some((m.is_dir(), Stamp::of(&m)))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn transfer_local(
    src: &[PathBuf],
    dst_dir: &Path,
    conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    moving: bool,
    journal: Option<&mut Journal>,
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
            skipped: 0,
        },
        conflict,
        progress,
        cancel,
        moving,
        swept: Default::default(),
        journal,
    };
    let mut result = Ok(());
    for source in sources {
        result = check_cancel(cancel).and_then(|()| {
            let destination = dst.join(source.file_name().context("source has no name")?);
            job.node(&source, &destination, false).map(drop)
        });
        if result.is_err() {
            break;
        }
    }
    if let (Err(_), Some(j)) = (&result, job.journal) {
        if let Err(e) = j.flush() {
            tracing::warn!("recording a transfer's progress: {e:#}");
        }
    }
    result
}

struct Job<'a, 'j> {
    state: Progress,
    conflict: Conflict,
    progress: &'a dyn Fn(Progress),
    cancel: &'a AtomicBool,
    moving: bool,
    /// Folders already swept for stale staging files.
    swept: std::collections::HashSet<PathBuf>,
    journal: Option<&'a mut Journal<'j>>,
}

impl Job<'_, '_> {
    /// Returns false when something under `source` was skipped (so a move keeps it).
    /// `fresh`: `proposed`'s folder was made by this transfer (journaled runs).
    fn node(&mut self, source: &Path, proposed: &Path, fresh: bool) -> Result<bool> {
        check_cancel(self.cancel)?;
        let metadata = fs::symlink_metadata(source)?;
        ensure_regular(source, &metadata)?;
        self.state.current = source.to_string_lossy().into_owned();
        let key = crate::VPath::local(source);
        let settled = match self.journal.as_deref() {
            Some(j) => settle(
                j,
                &key,
                Stamp::of(&metadata),
                metadata.is_dir(),
                &crate::VPath::local(proposed),
                fresh,
                true,
                local_stat,
            )?,
            None => Settle::Normal,
        };
        let mut ours = fresh;
        let target = match settled {
            Settle::Done(target) => return self.already(source, &metadata, key, target),
            Settle::Into(target) => {
                ours = true;
                local_target(&target)?
            }
            Settle::Normal => match destination(metadata.is_dir(), proposed, self.conflict)? {
                Some(target) => target,
                None => {
                    self.state.skipped += 1;
                    (self.progress)(self.state.clone());
                    return Ok(false);
                }
            },
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
                if let Some(j) = self.journal.as_deref_mut().filter(|_| !ours) {
                    // Recorded before it exists: a resumed run knows the folder is its own.
                    let placed = Placed {
                        target: crate::VPath::local(&target),
                        file: None,
                    };
                    j.placed(key, placed, true)?;
                }
                fs::create_dir(&target)?;
                ours = true;
            }
            let mut complete = true;
            // Stable traversal makes progress and cancellation deterministic.
            let mut children = fs::read_dir(source)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                complete &= self.node(&child.path(), &target.join(child.file_name()), ours)?;
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
        if let Some(parent) = target.parent() {
            if self.swept.insert(parent.to_path_buf()) {
                sweep_local(parent);
            }
        }
        let picked = target != proposed;
        let resumable = self.journal.as_deref().is_some_and(|j| {
            picked
                || metadata.len() > RECORD_BYTES
                || j.unfinished.as_ref().is_some_and(|u| u.source == key)
        });
        if resumable {
            self.copy_resumable(source, &target, &metadata, &key, picked)?;
        } else {
            self.copy_file(source, &target, metadata.len())?;
        }
        if self.moving {
            // The verified copy is in place at `target`; only now drop the source.
            fs::remove_file(source)
                .with_context(|| format!("remove moved file {}", source.display()))?;
        }
        if let Some(j) = self.journal.as_deref_mut() {
            let file = Some(Stamp::of(&fs::symlink_metadata(&target)?));
            let target = crate::VPath::local(&target);
            j.placed(key, Placed { target, file }, false)?;
        }
        self.state.done_bytes += metadata.len();
        self.state.done_items += 1;
        (self.progress)(self.state.clone());
        Ok(true)
    }

    /// A file an earlier run placed at `target`: counted, and a move drops its source now.
    fn already(
        &mut self,
        source: &Path,
        metadata: &fs::Metadata,
        key: crate::VPath,
        target: crate::VPath,
    ) -> Result<bool> {
        if self.moving {
            fs::remove_file(source)
                .with_context(|| format!("remove moved file {}", source.display()))?;
        }
        if let Some(j) = self.journal.as_deref_mut() {
            j.take_unfinished(&key);
            if !j.placed.contains_key(&key) {
                let file = local_stat(&target)?.map(|(_, stamp)| stamp);
                j.placed(key, Placed { target, file }, false)?;
            }
        }
        self.state.done_bytes += metadata.len();
        self.state.done_items += 1;
        (self.progress)(self.state.clone());
        Ok(true)
    }

    /// Copies to a unique `<target>.keel-partial-<pid>-<n>`, then renames into place. On
    /// cancel or error the partial is deleted and an existing `target` is untouched.
    fn copy_file(&self, source: &Path, target: &Path, size: u64) -> Result<()> {
        let name = target.file_name().context("target has no name")?;
        let partial = target.with_file_name(partial_name(&name.to_string_lossy()));
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
        place(self.conflict, source, &partial.0, target)
    }

    /// `copy_file` for a journaled transfer: the staging file is recorded every
    /// [`RECORD_BYTES`] and kept when the transfer stops, and a later run continues it
    /// while it still matches the record (else starts over). `force`: record before the
    /// first byte (the target is a name `RenameNew` picked).
    fn copy_resumable(
        &mut self,
        source: &Path,
        target: &Path,
        metadata: &fs::Metadata,
        key: &crate::VPath,
        force: bool,
    ) -> Result<()> {
        use std::io::{Seek, SeekFrom};
        let j = self.journal.as_deref_mut().context("not journaled")?;
        let stamp = Stamp::of(metadata);
        let name = target.file_name().context("target has no name")?;
        let mut staging = target.with_file_name(partial_name(&name.to_string_lossy()));
        let mut offset = 0;
        if let Some(u) = j.take_unfinished(key) {
            let at = local_target(&u.staging)?;
            let now = fs::symlink_metadata(&at)
                .ok()
                .filter(|m| m.is_file())
                .map(|m| Stamp::of(&m));
            let target_key = crate::VPath::local(target);
            if u.source_stamp == stamp
                && u.target == target_key
                && now.is_some_and(|now| continues(&u, now))
            {
                j.notes.push(format!(
                    "continued {} at {} of {} bytes",
                    source.display(),
                    u.bytes,
                    stamp.size
                ));
                (staging, offset) = (at, u.bytes);
            } else {
                let _ = fs::remove_file(&at);
                j.notes.push(format!(
                    "restarted {}: its partial copy no longer matched",
                    source.display()
                ));
            }
        }
        let guard = Partial(staging.clone());
        let mut unfinished = Unfinished {
            source: key.clone(),
            source_stamp: stamp,
            target: crate::VPath::local(target),
            staging: crate::VPath::local(&staging),
            bytes: offset,
            staging_mtime: None,
        };
        let result = (|| -> Result<bool> {
            let mut out = crate::local::open_at(&staging, offset)?;
            let mut input = fs::File::open(source)?;
            input.seek(SeekFrom::Start(offset))?;
            if force {
                record_local(j, &mut unfinished, &out, offset)?;
            }
            let mut buf = vec![0; 1 << 20];
            let (mut done, mut recorded) = (offset, offset);
            loop {
                if self.cancel.load(Ordering::Relaxed) {
                    // Kept for the next run (or removed by whoever cancelled).
                    record_local(j, &mut unfinished, &out, done)?;
                    return Ok(true);
                }
                let n = match input.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e).with_context(|| format!("read {}", source.display())),
                };
                out.write_all(&buf[..n])?;
                done += n as u64;
                let mut state = self.state.clone();
                state.done_bytes += done;
                (self.progress)(state);
                if done - recorded >= RECORD_BYTES {
                    record_local(j, &mut unfinished, &out, done)?;
                    recorded = done;
                }
            }
            out.set_modified(metadata.modified()?)?;
            out.set_permissions(metadata.permissions())?;
            drop(out);
            anyhow::ensure!(
                fs::metadata(&staging)?.len() == metadata.len(),
                "source changed during copy: {}",
                source.display()
            );
            place(self.conflict, source, &staging, target)?;
            Ok(false)
        })();
        match result {
            Ok(true) => {
                guard.keep();
                anyhow::bail!("operation cancelled")
            }
            Ok(false) => {
                j.unfinished = None;
                Ok(())
            }
            Err(e) => {
                j.unfinished = None;
                Err(e)
            }
        }
    }
}

/// Syncs a local staging file, then records it as `u` at `bytes`.
fn record_local(j: &mut Journal, u: &mut Unfinished, out: &fs::File, bytes: u64) -> Result<()> {
    out.sync_data()?;
    u.bytes = bytes;
    u.staging_mtime = Stamp::of(&out.metadata()?).mtime;
    j.unfinished = Some(u.clone());
    j.flush()
}

/// Renames a complete `staging` copy of `source` into place at `target`: over an existing
/// file with `Overwrite` (never a folder, a link or the source itself), else only while
/// `target` is free.
fn place(conflict: Conflict, source: &Path, staging: &Path, target: &Path) -> Result<()> {
    if conflict == Conflict::Overwrite {
        if let Ok(metadata) = fs::symlink_metadata(target) {
            anyhow::ensure!(
                metadata.is_file() && !same_file::is_same_file(source, target)?,
                "unsafe overwrite: {}",
                target.display()
            );
        }
        fs::rename(staging, target)
    } else {
        // A file that appeared at `target` during the copy is never replaced.
        sys::rename_noreplace(staging, target)
    }
    .with_context(|| format!("place {}", target.display()))
}

/// This job's own incomplete copy; removed on every exit path (a no-op after the rename)
/// unless kept.
struct Partial(PathBuf);
impl Partial {
    /// Leaves the file for a later run.
    fn keep(mut self) {
        self.0 = PathBuf::new();
    }
}
impl Drop for Partial {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
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
    transfer_with(
        src,
        dst_dir,
        mv,
        on_conflict,
        progress,
        cancel,
        router,
        None,
    )
}

/// `transfer` that records its progress in `journal` and, run again with what an earlier
/// run recorded there (after a crash or a close), skips the files that run placed, refuses
/// to overwrite one changed since, and continues the file it was writing where the target
/// can be written in place (local folders, SFTP), else writes that file again.
#[allow(clippy::too_many_arguments)]
pub fn transfer_resumable(
    src: &[crate::VPath],
    dst_dir: &crate::VPath,
    mv: bool,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
    journal: &mut Journal,
) -> Result<()> {
    transfer_with(
        src,
        dst_dir,
        mv,
        on_conflict,
        progress,
        cancel,
        router,
        Some(journal),
    )
}

#[allow(clippy::too_many_arguments)]
fn transfer_with(
    src: &[crate::VPath],
    dst_dir: &crate::VPath,
    mv: bool,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
    journal: Option<&mut Journal>,
) -> Result<()> {
    // Into a folder inside a zip: one rewrite of the archive (atomic, so nothing for a
    // journal to record).
    #[cfg(feature = "zip")]
    if dst_dir.split_archive().is_some() {
        return crate::archive::zipedit::transfer_into(
            src,
            dst_dir,
            mv,
            on_conflict,
            progress,
            cancel,
            router,
        );
    }
    // Paths inside a local archive are `file:` too, but have no local path of their own.
    let local_paths = src
        .iter()
        .map(crate::VPath::to_local_path)
        .collect::<Option<Vec<_>>>();
    if let (Some(paths), Some(dst)) = (local_paths, dst_dir.to_local_path()) {
        return transfer_local(&paths, &dst, on_conflict, progress, cancel, mv, journal);
    }
    check_cancel(cancel)?;
    if mv {
        for source in src {
            anyhow::ensure!(
                routed(router, source)?.caps().delete,
                "cannot move out of a read-only location (copy instead): {}",
                source.display()
            );
        }
    }
    let target_provider = routed(router, dst_dir)?;
    let dst = target_provider.stat(dst_dir)?;
    anyhow::ensure!(
        dst.kind == crate::Kind::Dir && !dst.is_link,
        "destination must be a real directory: {}",
        dst_dir.display()
    );
    let dst_key = target_provider.canonicalize(dst_dir)?;
    // Providers are resolved once: re-registering a host mid-job (new settings) must not
    // switch the remaining files of this job to another connection.
    let mut sources = Vec::with_capacity(src.len());
    let mut roots = Vec::new();
    let mut state = Progress {
        done_bytes: 0,
        total_bytes: 0,
        current: String::new(),
        done_items: 0,
        total_items: 0,
        skipped: 0,
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
        sources.push(provider);
    }
    let mut job = ProviderJob {
        dst: target_provider,
        state,
        conflict: on_conflict,
        progress,
        cancel,
        moving: mv,
        swept: Default::default(),
        journal,
    };
    let mut result = Ok(());
    for (source, provider) in src.iter().zip(&sources) {
        result = job
            .node(provider, source, &dst_dir.join(source.name()), 0, false)
            .map(drop);
        if result.is_err() {
            break;
        }
    }
    if let (Err(_), Some(j)) = (&result, job.journal) {
        if let Err(e) = j.flush() {
            tracing::warn!("recording a transfer's progress: {e:#}");
        }
    }
    result
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
struct ProviderJob<'a, 'j> {
    dst: std::sync::Arc<dyn crate::Provider>,
    state: Progress,
    conflict: Conflict,
    progress: &'a dyn Fn(Progress),
    cancel: &'a AtomicBool,
    moving: bool,
    /// Destination folders already swept for stale staging files.
    swept: std::collections::HashSet<String>,
    journal: Option<&'a mut Journal<'j>>,
}
fn provider_stat(
    provider: &dyn crate::Provider,
    p: &crate::VPath,
) -> Result<Option<(bool, Stamp)>> {
    Ok(maybe_stat(provider, p)?.map(|e| (e.kind == crate::Kind::Dir, Stamp::entry(&e))))
}
impl ProviderJob<'_, '_> {
    /// `fresh`: `proposed`'s folder was made by this transfer (journaled runs).
    fn node(
        &mut self,
        src: &std::sync::Arc<dyn crate::Provider>,
        source: &crate::VPath,
        proposed: &crate::VPath,
        depth: usize,
        fresh: bool,
    ) -> Result<bool> {
        use crate::Kind;
        check_cancel(self.cancel)?;
        anyhow::ensure!(depth < 256, "directory nesting limit");
        let dst = self.dst.clone();
        let before = src.stat(source)?;
        anyhow::ensure!(
            !before.is_link && before.kind != Kind::Symlink,
            "source is a link: {}",
            source.display()
        );
        let settled = match self.journal.as_deref() {
            // Targets get a new modified time: only sizes compare.
            Some(j) => settle(
                j,
                source,
                Stamp::entry(&before),
                before.kind == Kind::Dir,
                proposed,
                fresh,
                false,
                |t| provider_stat(&*dst, t),
            )?,
            None => Settle::Normal,
        };
        let mut ours = fresh;
        let mut target = proposed.clone();
        // What occupies the final target (None once `RenameNew` picked a free name).
        let mut occupied;
        match settled {
            Settle::Done(target) => return self.already(src, source, &before, target),
            Settle::Into(into) => {
                ours = true;
                target = into;
                occupied = maybe_stat(&*dst, &target)?;
            }
            Settle::Normal => {
                let existing = maybe_stat(&*dst, &target)?;
                occupied = existing.clone();
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
                        self.state.skipped += 1;
                        (self.progress)(self.state.clone());
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
                                occupied = None;
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
            }
        }
        if self.moving && same_remote(source, &target) {
            // Merging into an existing folder still walks its children; everything else
            // is one rename on the server.
            let merge = before.kind == Kind::Dir && occupied.is_some();
            if !merge && self.rename_on_server(src, &dst, source, &target, occupied.is_some())? {
                return Ok(true);
            }
        }
        if before.kind == Kind::Dir {
            if maybe_stat(&*dst, &target)?.is_none() {
                if let Some(j) = self.journal.as_deref_mut().filter(|_| !ours) {
                    // Recorded before it exists: a resumed run knows the folder is its own.
                    let placed = Placed {
                        target: target.clone(),
                        file: None,
                    };
                    j.placed(source.clone(), placed, true)?;
                }
                dst.mkdir(&target)?;
                ours = true;
            }
            let mut complete = true;
            for child in src.list(source)? {
                let to = target.join(&child.name);
                complete &= self.node(src, &child.path, &to, depth + 1, ours)?;
            }
            if self.moving && complete {
                src.remove_empty_dir(source)?;
            }
            self.state.done_items += 1;
            (self.progress)(self.state.clone());
            return Ok(complete);
        }
        let folder = target.parent().context("missing parent")?;
        if self.swept.insert(folder.display()) {
            sweep_provider(&*dst, &folder);
        }
        let picked = target != *proposed;
        let placed = self.copy(src, source, &before, &target, picked)?;
        if self.moving {
            check_cancel(self.cancel)?;
            // The copy is verified: delete the source permanently, like `move_local` (the
            // local provider's `remove` would only send it to the Recycle Bin).
            match source.to_local_path() {
                Some(local) => fs::remove_file(&local)
                    .with_context(|| format!("remove moved file {}", local.display()))?,
                None => src.remove(source)?,
            }
        }
        if let Some(j) = self.journal.as_deref_mut() {
            let file = Some(Stamp::entry(&placed));
            j.placed(source.clone(), Placed { target, file }, false)?;
        }
        self.state.done_bytes += placed.size;
        self.state.done_items += 1;
        self.state.current = source.display();
        (self.progress)(self.state.clone());
        Ok(true)
    }

    /// A file an earlier run placed at `target`: counted, and a move drops its source now.
    fn already(
        &mut self,
        src: &std::sync::Arc<dyn crate::Provider>,
        source: &crate::VPath,
        before: &crate::Entry,
        target: crate::VPath,
    ) -> Result<bool> {
        if self.moving {
            match source.to_local_path() {
                Some(local) => fs::remove_file(&local)
                    .with_context(|| format!("remove moved file {}", local.display()))?,
                None => src.remove(source)?,
            }
        }
        if let Some(j) = self.journal.as_deref_mut() {
            j.take_unfinished(source);
            if !j.placed.contains_key(source) {
                let file = provider_stat(&*self.dst, &target)?.map(|(_, stamp)| stamp);
                j.placed(source.clone(), Placed { target, file }, false)?;
            }
        }
        self.state.done_bytes += before.size;
        self.state.done_items += 1;
        self.state.current = source.display();
        (self.progress)(self.state.clone());
        Ok(true)
    }

    /// Streams `source` into a staging file beside `target`, checks it and places it;
    /// returns the placed file. Journaled, a file over [`RECORD_BYTES`] (or the one an
    /// earlier run was writing, or one at a name `RenameNew` picked: `picked`, recorded
    /// before the first byte) is written in place where the target allows it
    /// (`Provider::write_at`): recorded every [`RECORD_BYTES`], kept when the transfer
    /// stops, and continued by a later run while it still matches the record.
    fn copy(
        &mut self,
        src: &std::sync::Arc<dyn crate::Provider>,
        source: &crate::VPath,
        before: &crate::Entry,
        target: &crate::VPath,
        picked: bool,
    ) -> Result<crate::Entry> {
        use crate::Kind;
        let dst = self.dst.clone();
        let folder = target.parent().context("missing parent")?;
        let stamp = Stamp::entry(before);
        let resumable = self.journal.as_deref().is_some_and(|j| {
            picked
                || before.size > RECORD_BYTES
                || j.unfinished.as_ref().is_some_and(|u| u.source == *source)
        });
        // (staging, offset, writer, written in place)
        let mut start = None;
        if let Some(j) = self.journal.as_deref_mut() {
            if let Some(u) = j.take_unfinished(source) {
                let now = maybe_stat(&*dst, &u.staging)?
                    .filter(|e| e.kind == Kind::File && !e.is_link)
                    .map(|e| Stamp::entry(&e));
                let matches = u.source_stamp == stamp
                    && u.target == *target
                    && now.is_some_and(|now| continues(&u, now));
                let writer = if matches {
                    dst.write_at(&u.staging, u.bytes)?
                } else {
                    None
                };
                match writer {
                    Some(w) => {
                        j.notes.push(format!(
                            "continued {} at {} of {} bytes",
                            source.display(),
                            u.bytes,
                            stamp.size
                        ));
                        start = Some((u.staging.clone(), u.bytes, w, true));
                    }
                    None => {
                        if now.is_some() {
                            let _ = dst.remove(&u.staging);
                        }
                        j.notes.push(format!(
                            "restarted {}: its partial copy no longer matched",
                            source.display()
                        ));
                    }
                }
            }
        }
        let (staging, offset, mut writer, in_place) = match start {
            Some(start) => start,
            None => {
                let staging = folder.join(&partial_name(target.name()));
                let direct = if resumable {
                    dst.write_at(&staging, 0)?
                } else {
                    None
                };
                match direct {
                    Some(w) => (staging, 0, w, true),
                    None => {
                        let w = dst.create_new_cancellable(&staging, self.cancel)?;
                        (staging, 0, w, false)
                    }
                }
            }
        };
        let mut guard = ProviderPartial {
            provider: dst.clone(),
            path: Some(staging.clone()),
        };
        let mut unfinished = Unfinished {
            source: source.clone(),
            source_stamp: stamp,
            target: target.clone(),
            staging: staging.clone(),
            bytes: offset,
            staging_mtime: None,
        };
        let result = (|| -> Result<Option<crate::Entry>> {
            if picked {
                if let Some(j) = self.journal.as_deref_mut() {
                    // A staged upload is never flushed early: that would place it.
                    let writer = in_place.then_some(&mut *writer);
                    record_provider(j, &mut unfinished, writer, &*dst, offset)?;
                }
            }
            let mut reader = match offset {
                0 => src.read(source)?,
                at => match src.read_range(source, at, before.size - at)? {
                    Some(r) => r,
                    None => {
                        let mut r = src.read(source)?;
                        io::copy(&mut (&mut r).take(at), &mut io::sink())?;
                        r
                    }
                },
            };
            let uploading = dst.uploads_on_flush();
            let mut buffer = vec![0; 1024 * 1024];
            let (mut copied, mut recorded) = (offset, offset);
            loop {
                if self.cancel.load(Ordering::Relaxed) {
                    if let Some(j) = self.journal.as_deref_mut().filter(|_| in_place) {
                        // Kept for the next run (or removed by whoever cancelled).
                        record_provider(j, &mut unfinished, Some(&mut *writer), &*dst, copied)?;
                        return Ok(None);
                    }
                    check_cancel(self.cancel)?;
                }
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                writer.write_all(&buffer[..n])?;
                copied += n as u64;
                let mut progress = self.state.clone();
                match uploading {
                    // Only buffered so far: the bytes count once `flush()` has sent them.
                    Some(service) => {
                        progress.current = format!("Uploading to {service}… {}", source.display())
                    }
                    None => {
                        progress.done_bytes += copied;
                        progress.current = source.display();
                    }
                }
                (self.progress)(progress);
                if in_place && copied - recorded >= RECORD_BYTES {
                    if let Some(j) = self.journal.as_deref_mut() {
                        record_provider(j, &mut unfinished, Some(&mut *writer), &*dst, copied)?;
                        recorded = copied;
                    }
                }
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
                    && dst.stat(&staging)?.size == copied,
                "source changed or copy size mismatch: {}",
                source.display()
            );
            check_cancel(self.cancel)?;
            if self.conflict == Conflict::Overwrite {
                if let Some(e) = maybe_stat(&*dst, target)? {
                    anyhow::ensure!(
                        !e.is_link && e.kind == Kind::File,
                        "unsafe overwrite: {}",
                        target.display()
                    );
                }
                dst.rename_replace(&staging, target)?;
            } else {
                dst.rename_noreplace(&staging, target)?;
            }
            guard.path = None;
            let placed = dst.stat(target)?;
            anyhow::ensure!(
                placed.size == copied,
                "destination verification failed: {}",
                target.display()
            );
            Ok(Some(placed))
        })();
        let ours = |u: &Unfinished| u.source == *source;
        match result {
            Ok(Some(placed)) => {
                if let Some(j) = self.journal.as_deref_mut() {
                    j.unfinished.take_if(|u| ours(u));
                }
                Ok(placed)
            }
            Ok(None) => {
                guard.path = None;
                anyhow::bail!("operation cancelled")
            }
            Err(e) => {
                if let Some(j) = self.journal.as_deref_mut() {
                    j.unfinished.take_if(|u| ours(u));
                }
                Err(e)
            }
        }
    }
}
/// Flushes a staging file written in place (`writer`), then records it as `u` at `bytes`.
fn record_provider(
    j: &mut Journal,
    u: &mut Unfinished,
    writer: Option<&mut (dyn Write + Send + '_)>,
    dst: &dyn crate::Provider,
    bytes: u64,
) -> Result<()> {
    if let Some(writer) = writer {
        writer.flush()?;
        u.staging_mtime = maybe_stat(dst, &u.staging)?.and_then(|e| Stamp::entry(&e).mtime);
    }
    u.bytes = bytes;
    j.unfinished = Some(u.clone());
    j.flush()
}
/// Both paths on the same `sftp://<id>` host (one server, so a rename can move the data).
fn same_remote(a: &crate::VPath, b: &crate::VPath) -> bool {
    a.scheme == "sftp"
        && b.scheme == "sftp"
        && a.authority == b.authority
        && a.split_archive().is_none()
        && b.split_archive().is_none()
}
impl ProviderJob<'_, '_> {
    /// Moves `source` to `target` with a server-side rename. `Ok(false)`: the rename was
    /// refused (e.g. across devices) and nothing changed, so the caller streams instead.
    fn rename_on_server(
        &mut self,
        src: &std::sync::Arc<dyn crate::Provider>,
        dst: &std::sync::Arc<dyn crate::Provider>,
        source: &crate::VPath,
        target: &crate::VPath,
        replace: bool,
    ) -> Result<bool> {
        let mut counted = Progress {
            total_items: 0,
            total_bytes: 0,
            ..self.state.clone()
        };
        scan_remote(&**src, source, &mut counted, self.cancel, 0)?;
        check_cancel(self.cancel)?;
        let renamed = if replace {
            dst.rename_replace(source, target)
        } else {
            dst.rename_noreplace(source, target)
        };
        if let Err(e) = renamed {
            // Unchanged source and no new target: a cross-device style refusal.
            if src.stat(source).is_ok() && maybe_stat(&**dst, target)?.is_none() {
                return Ok(false);
            }
            return Err(e);
        }
        self.state.done_bytes += counted.total_bytes;
        self.state.done_items += counted.total_items;
        self.state.current = source.display();
        (self.progress)(self.state.clone());
        Ok(true)
    }
}
/// Staging files are `<name>.keel-partial-<pid>-<n>` (the SFTP provider stages uploads the
/// same way): unique per attempt, so a crashed run's leftover never blocks a retry.
const PARTIAL: &str = ".keel-partial";
/// Staging files older than this are leftovers of a crash or a lost connection.
const STALE_PARTIAL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// A staging name for `name`, unique to this attempt and at most 255 bytes (NAME_MAX).
pub fn partial_name(name: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let suffix = format!(
        "{PARTIAL}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut keep = name.len().min(255 - suffix.len());
    while !name.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}{suffix}", &name[..keep])
}
/// `<stem>.keel-partial` (v0.1 builds) or exactly `<stem>.keel-partial-<pid>-<n>`, both
/// numbers written as `partial_name` writes them (no leading zeros), so a user's
/// `report.keel-partial-2024-05` is never taken for a staging file and swept. Also used by
/// the SFTP provider, which stages under the same names.
pub fn is_partial(name: &str) -> bool {
    name.rsplit_once(PARTIAL).is_some_and(|(stem, rest)| {
        let number = |n: &str| {
            n.bytes().all(|c| c.is_ascii_digit())
                && (n == "0" || !n.is_empty() && !n.starts_with('0'))
        };
        !stem.is_empty()
            && (rest.is_empty()
                || rest
                    .strip_prefix('-')
                    .and_then(|r| r.split_once('-'))
                    .is_some_and(|(pid, n)| number(pid) && number(n)))
    })
}
fn stale(modified: Option<std::time::SystemTime>) -> bool {
    modified
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > STALE_PARTIAL)
}
/// Deletes day-old staging files (never links) from the local folder `dir`. Best effort.
pub(crate) fn sweep_local(dir: &Path) {
    for item in fs::read_dir(dir).into_iter().flatten().flatten() {
        // `DirEntry::metadata` does not follow links.
        let Ok(meta) = item.metadata() else { continue };
        let name = item.file_name();
        if meta.is_file() && name.to_str().is_some_and(is_partial) && stale(meta.modified().ok()) {
            let _ = fs::remove_file(item.path());
        }
    }
}
/// `sweep_local` for any provider; a local folder is swept directly (no trash).
fn sweep_provider(provider: &dyn crate::Provider, dir: &crate::VPath) {
    if let Some(local) = dir.to_local_path() {
        return sweep_local(&local);
    }
    for item in provider.list(dir).into_iter().flatten() {
        if item.kind == crate::Kind::File
            && !item.is_link
            && is_partial(&item.name)
            && stale(item.modified)
        {
            let _ = provider.remove(&item.path);
        }
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

/// Deletes entries or folders inside zips on this computer, rewriting each archive once
/// (`archive::editable` says which can be changed). The folders holding them stay.
pub fn remove_entries(
    paths: &[crate::VPath],
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    #[cfg(feature = "zip")]
    return crate::archive::zipedit::remove_entries(paths, progress, cancel);
    #[cfg(not(feature = "zip"))]
    {
        let _ = (progress, cancel);
        anyhow::bail!(
            "archives are read-only in this build: {}",
            paths.first().map(crate::VPath::display).unwrap_or_default()
        )
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
    extract_under(
        archive,
        "",
        entries,
        dst_dir,
        on_conflict,
        progress,
        cancel,
        router,
    )
}

/// `extract` with names taken relative to the archive folder `base` (empty = the root):
/// `base/a.txt` lands at `dst_dir/a.txt`; names outside `base` are left out. This is what a
/// copy out of a folder inside an archive does.
#[allow(clippy::too_many_arguments)]
pub fn extract_under(
    archive: &crate::VPath,
    base: &str,
    entries: &[String],
    dst_dir: &Path,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
) -> Result<()> {
    check_cancel(cancel)?;
    let (mut reader, chosen) = open_selection(archive, base, entries, progress, cancel, router)?;
    let Selection {
        dirs,
        files,
        total_bytes,
    } = chosen;
    let dst = long(dst_dir)?;
    anyhow::ensure!(
        dst.is_dir(),
        "destination is not a directory: {}",
        dst_dir.display()
    );
    if let Some(free) = free_space(&dst) {
        anyhow::ensure!(
            total_bytes <= free,
            "not enough free space in {}: extracting needs {total_bytes} bytes, {free} are free",
            dst_dir.display()
        );
    }
    let mut state = Progress {
        done_bytes: 0,
        total_bytes,
        current: String::new(),
        done_items: 0,
        total_items: dirs.len() + files.len(),
        skipped: 0,
    };
    progress(state.clone());
    for name in &dirs {
        check_cancel(cancel)?;
        let target = below(&dst, name)?;
        fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;
        state.done_items += 1;
    }
    // Parent folders already created and re-checked during this extraction.
    let mut ready = std::collections::HashSet::new();
    reader.visit(&|raw| files.contains_key(raw), &mut |raw, input| {
        check_cancel(cancel)?;
        let (name, size) = &files[raw];
        state.current = name.clone();
        let proposed = below(&dst, name)?;
        let target = destination(false, &proposed, on_conflict)?;
        state.skipped += usize::from(target.is_none());
        if let Some(target) = target {
            let parent = target.parent().context("destination has no parent")?;
            if !ready.contains(parent) {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
                // Re-check: nothing on the way down may have become a link meanwhile.
                below(&dst, name)?;
                ready.insert(parent.to_path_buf());
            }
            let mut partial = tempfile::NamedTempFile::new_in(parent)?;
            let copied = copy_archive_bytes(input, &mut partial, &mut state, progress, cancel)?;
            // Readers already hold bodies to their declared size; never persist a short file.
            anyhow::ensure!(copied == *size, "archive entry is truncated: {name}");
            // A file that appeared at `target` meanwhile is replaced only under Overwrite.
            if let Err(e) = partial.persist_noclobber(&target) {
                anyhow::ensure!(
                    on_conflict == Conflict::Overwrite
                        && e.error.kind() == io::ErrorKind::AlreadyExists
                        && fs::symlink_metadata(&target)?.is_file(),
                    "refusing to replace {}: {}",
                    target.display(),
                    e.error
                );
                e.file.persist(&target)?;
            }
        }
        state.done_items += 1;
        progress(state.clone());
        Ok(())
    })?;
    progress(state);
    Ok(())
}

/// `extract_under` into any writable folder: a local one, or a folder on another provider
/// (an SFTP host, a cloud account), where each entry is streamed
/// through the provider (one pass over the archive, no copy of the extracted tree on disk):
/// written as a staging file beside its target (`<name>.keel-partial-<pid>-<n>`), its size
/// checked against the archive's, then renamed into place, so cancel or an error never
/// leaves a half-written file. Clashes follow `on_conflict` (folders merge). Not into a
/// folder inside an archive: `transfer` copies entries into a zip.
#[allow(clippy::too_many_arguments)]
pub fn extract_to(
    archive: &crate::VPath,
    base: &str,
    entries: &[String],
    dst_dir: &crate::VPath,
    on_conflict: Conflict,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
) -> Result<()> {
    if let Some(local) = dst_dir.to_local_path() {
        return extract_under(
            archive,
            base,
            entries,
            &local,
            on_conflict,
            progress,
            cancel,
            router,
        );
    }
    check_cancel(cancel)?;
    anyhow::ensure!(
        dst_dir.split_archive().is_none(),
        "cannot extract into a folder inside an archive (paste copies entries into a zip): {}",
        dst_dir.display()
    );
    let dst = routed(router, dst_dir)?;
    let folder = dst.stat(dst_dir)?;
    anyhow::ensure!(
        folder.kind == crate::Kind::Dir && !folder.is_link,
        "destination is not a directory: {}",
        dst_dir.display()
    );
    let (mut reader, chosen) = open_selection(archive, base, entries, progress, cancel, router)?;
    let Selection {
        dirs,
        files,
        total_bytes,
    } = chosen;
    let mut state = Progress {
        done_bytes: 0,
        total_bytes,
        current: String::new(),
        done_items: 0,
        total_items: dirs.len() + files.len(),
        skipped: 0,
    };
    progress(state.clone());
    let mut job = RemoteExtract {
        dst,
        root: dst_dir.clone(),
        ready: Default::default(),
        swept: Default::default(),
    };
    for name in &dirs {
        check_cancel(cancel)?;
        job.folder(name)?;
        state.done_items += 1;
    }
    reader.visit(&|raw| files.contains_key(raw), &mut |raw, input| {
        check_cancel(cancel)?;
        let (name, size) = &files[raw];
        state.current = name.clone();
        job.file(
            name,
            *size,
            input,
            on_conflict,
            &mut state,
            progress,
            cancel,
        )?;
        state.done_items += 1;
        progress(state.clone());
        Ok(())
    })?;
    progress(state);
    Ok(())
}

/// An extraction into a folder on another provider.
struct RemoteExtract {
    dst: std::sync::Arc<dyn crate::Provider>,
    root: crate::VPath,
    /// Folders known to exist (made or found) during this extraction.
    ready: std::collections::HashSet<String>,
    /// Folders already swept for stale staging files.
    swept: std::collections::HashSet<String>,
}
impl RemoteExtract {
    fn path(&self, name: &str) -> crate::VPath {
        name.split('/')
            .fold(self.root.clone(), |p, part| p.join(part))
    }
    /// `name` and the folders above it, made where missing; a file in the way fails.
    fn folder(&mut self, name: &str) -> Result<()> {
        let mut done = String::new();
        for part in name.split('/') {
            done = if done.is_empty() {
                part.to_owned()
            } else {
                format!("{done}/{part}")
            };
            if self.ready.contains(&done) {
                continue;
            }
            let path = self.path(&done);
            match maybe_stat(&*self.dst, &path)? {
                Some(e) => anyhow::ensure!(
                    e.kind == crate::Kind::Dir && !e.is_link,
                    "not a folder: {}",
                    path.display()
                ),
                None => self.dst.mkdir(&path)?,
            }
            self.ready.insert(done.clone());
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn file(
        &mut self,
        name: &str,
        size: u64,
        input: &mut dyn io::Read,
        conflict: Conflict,
        state: &mut Progress,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<()> {
        let (parent, leaf) = name.rsplit_once('/').unwrap_or(("", name));
        if !parent.is_empty() {
            self.folder(parent)?;
        }
        let folder = if parent.is_empty() {
            self.root.clone()
        } else {
            self.path(parent)
        };
        let mut target = folder.join(leaf);
        let mut replace = false;
        if let Some(existing) = maybe_stat(&*self.dst, &target)? {
            match conflict {
                Conflict::Skip => {
                    state.skipped += 1;
                    state.done_bytes = state.done_bytes.saturating_add(size);
                    return Ok(());
                }
                Conflict::Overwrite => {
                    anyhow::ensure!(
                        existing.kind == crate::Kind::File && !existing.is_link,
                        "refusing to replace {}",
                        target.display()
                    );
                    replace = true;
                }
                Conflict::RenameNew => {
                    let (stem, ext) = leaf
                        .rsplit_once('.')
                        .filter(|(s, _)| !s.is_empty())
                        .map_or((leaf, None), |(s, e)| (s, Some(e)));
                    for n in 2u64.. {
                        let candidate = folder.join(&match ext {
                            Some(ext) => format!("{stem} ({n}).{ext}"),
                            None => format!("{stem} ({n})"),
                        });
                        if maybe_stat(&*self.dst, &candidate)?.is_none() {
                            target = candidate;
                            break;
                        }
                    }
                }
            }
        }
        if self.swept.insert(folder.display()) {
            sweep_provider(&*self.dst, &folder);
        }
        let partial = folder.join(&partial_name(target.name()));
        let mut writer = self.dst.create_new_cancellable(&partial, cancel)?;
        let mut guard = ProviderPartial {
            provider: self.dst.clone(),
            path: Some(partial.clone()),
        };
        let copied = copy_archive_bytes(input, &mut writer, state, progress, cancel)?;
        anyhow::ensure!(copied == size, "archive entry is truncated: {name}");
        check_cancel(cancel)?;
        writer.flush()?;
        drop(writer);
        anyhow::ensure!(
            self.dst.stat(&partial)?.size == copied,
            "upload size mismatch: {}",
            target.display()
        );
        if replace {
            self.dst.rename_replace(&partial, &target)?;
        } else {
            self.dst.rename_noreplace(&partial, &target)?;
        }
        guard.path = None;
        Ok(())
    }
}

/// What an extraction writes: folders and files (by raw name: normalised name relative
/// to the base, declared size) and their total size.
struct Selection {
    dirs: Vec<String>,
    files: std::collections::HashMap<String, (String, u64)>,
    total_bytes: u64,
}

/// Opens the archive (fetching or materialising it when it is not a local file) and picks
/// the entries to extract. Every name is checked first: one unsafe name refuses the whole
/// archive, as does a password-protected entry or a selected name that is not there.
fn open_selection(
    archive: &crate::VPath,
    base: &str,
    entries: &[String],
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    router: &crate::Router,
) -> Result<(Box<dyn crate::archive::ArchiveReader>, Selection)> {
    use crate::archive::safe_name;
    let base = if base.trim_matches('/').is_empty() {
        String::new()
    } else {
        safe_name(base)?
    };
    let relative = |name: &str| -> Option<String> {
        if base.is_empty() {
            return Some(name.to_owned());
        }
        let rest = name.strip_prefix(&base)?.strip_prefix('/')?;
        Some(rest.to_owned())
    };
    let archive = match archive.split_archive() {
        Some((outer, inner)) if inner.trim_matches('/').is_empty() => outer,
        _ => archive.clone(),
    };
    let local = router
        .provider_for(&archive)
        .with_context(|| format!("no provider for {}", archive.display()))?
        .local_copy_cancellable(&archive, progress, cancel)?;
    let mut reader = crate::archive::open_archive(&local)?;
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
        let Some(name) = relative(&name) else {
            continue;
        };
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
            files.insert(entry.inner, (name, entry.size));
        }
    }
    for wanted in &selection {
        anyhow::ensure!(
            relative(wanted).is_some_and(|w| dirs
                .iter()
                .chain(files.values().map(|(n, _)| n))
                .any(|n| under(n, &w))),
            "not in the archive: {wanted}"
        );
    }
    Ok((
        reader,
        Selection {
            dirs,
            files,
            total_bytes,
        },
    ))
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

/// Free bytes on the volume holding `dir`, if it can be told (`statvfs` / `GetDiskFreeSpaceExW`
/// on the path itself, so tmpfs and bind mounts count too).
fn free_space(dir: &Path) -> Option<u64> {
    let dir = fs::canonicalize(dir).ok()?;
    fs4::available_space(&dir).ok()
}

/// Copies `input` to `output`, returning the byte count.
fn copy_archive_bytes(
    input: &mut dyn io::Read,
    output: &mut dyn io::Write,
    state: &mut Progress,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<u64> {
    let mut buffer = vec![0; 64 << 10];
    let mut copied = 0u64;
    loop {
        check_cancel(cancel)?;
        let n = match input.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            return Ok(copied);
        }
        output.write_all(&buffer[..n])?;
        copied += n as u64;
        state.done_bytes = state.done_bytes.saturating_add(n as u64);
        progress(state.clone());
    }
}

/// What `add_*` writes: archive entry name (folders end in `/`) -> source path and metadata.
#[cfg(any(feature = "zip", feature = "sevenz", feature = "tar"))]
pub(crate) type AddFiles = std::collections::BTreeMap<String, (PathBuf, fs::Metadata)>;

/// Walks `src` into `<inner_dir>/<name>/...` entries, refusing links and `archive` itself.
#[cfg(any(feature = "zip", feature = "sevenz", feature = "tar"))]
fn collect_add(archive: &Path, exists: bool, src: &[PathBuf], inner_dir: &str) -> Result<AddFiles> {
    let prefix = if inner_dir.trim_matches('/').is_empty() {
        String::new()
    } else {
        format!("{}/", crate::archive::safe_name(inner_dir)?)
    };
    let mut files = AddFiles::new();
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
                !(exists && same_file::is_same_file(item.path(), archive)?),
                "cannot add an archive to itself"
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
                "two sources map to the same archive entry: {name}"
            );
        }
    }
    Ok(files)
}

#[cfg(any(feature = "zip", feature = "sevenz", feature = "tar"))]
fn add_progress(files: &AddFiles) -> Result<Progress> {
    Ok(Progress {
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
        skipped: 0,
    })
}

/// Reads `inner` while counting bytes into `state` and failing once `cancel` is set.
#[cfg(any(feature = "sevenz", feature = "tar"))]
pub(crate) struct Feed<'a> {
    pub inner: &'a mut dyn io::Read,
    pub state: &'a mut Progress,
    pub progress: &'a dyn Fn(Progress),
    pub cancel: &'a AtomicBool,
}
#[cfg(any(feature = "sevenz", feature = "tar"))]
impl io::Read for Feed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("operation cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.state.done_bytes = self.state.done_bytes.saturating_add(n as u64);
        (self.progress)(self.state.clone());
        Ok(n)
    }
}

/// A staging file path; the file is removed on drop unless committed.
#[cfg(any(feature = "sevenz", feature = "tar"))]
struct Staging(Option<PathBuf>);
#[cfg(any(feature = "sevenz", feature = "tar"))]
impl Drop for Staging {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Adds files and folders to the archive at `archive` (zip, 7z, `.tar`, `.tar.gz`/`.tgz`),
/// creating it if missing. Same-named entries are replaced. The new archive is staged beside
/// the old one (`.keel-partial-<pid>-<n>`), fsynced and renamed over it, so cancel or an
/// error leaves the original alone and no staging file behind. 7z entries are re-encoded
/// (the writer has no raw copy); tar entries are streamed through.
pub fn add_to_archive(
    archive: &Path,
    src: &[PathBuf],
    inner_dir: &str,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    #[cfg(feature = "zip")]
    if name.ends_with(".zip") || name.ends_with(".jar") {
        return add_to_zip(archive, src, inner_dir, progress, cancel);
    }
    #[cfg(feature = "sevenz")]
    if name.ends_with(".7z") {
        return rewrite_archive(archive, src, inner_dir, progress, cancel, |job| {
            crate::archive::sevenz_rewrite(job)
        });
    }
    #[cfg(feature = "tar")]
    if name.ends_with(".tar") || name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let gzip = !name.ends_with(".tar");
        return rewrite_archive(archive, src, inner_dir, progress, cancel, move |job| {
            crate::archive::tar_rewrite(job, gzip)
        });
    }
    let _ = (src, inner_dir, progress, cancel);
    if name.ends_with(".rar") {
        anyhow::bail!("RAR archives are read-only; cannot add to {name}");
    }
    anyhow::bail!("cannot add to {name}: only zip, 7z, tar and tar.gz archives can be changed")
}

/// Everything a format writer needs to rewrite one archive.
#[cfg(any(feature = "sevenz", feature = "tar"))]
pub(crate) struct Rewrite<'a> {
    /// The archive being replaced, if it exists.
    pub old: Option<&'a Path>,
    pub files: AddFiles,
    pub out: &'a mut fs::File,
    pub state: &'a mut Progress,
    pub progress: &'a dyn Fn(Progress),
    pub cancel: &'a AtomicBool,
}

#[cfg(any(feature = "sevenz", feature = "tar"))]
fn rewrite_archive(
    archive: &Path,
    src: &[PathBuf],
    inner_dir: &str,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
    write: impl FnOnce(Rewrite<'_>) -> Result<()>,
) -> Result<()> {
    check_cancel(cancel)?;
    let archive = long(archive)?;
    let exists = match fs::symlink_metadata(&archive) {
        Ok(meta) => {
            anyhow::ensure!(meta.is_file(), "not an archive file: {}", archive.display());
            true
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    let files = collect_add(&archive, exists, src, inner_dir)?;
    let mut state = add_progress(&files)?;
    progress(state.clone());
    let parent = archive.parent().context("archive has no parent folder")?;
    let leaf = archive.file_name().context("archive has no name")?;
    let staged = parent.join(partial_name(&leaf.to_string_lossy()));
    // Declared before `out` so the handle closes before the file is removed (Windows).
    let mut staging = Staging(None);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .with_context(|| format!("create {}", staged.display()))?;
    staging.0 = Some(staged.clone());
    if exists {
        // Keeps the mode bits of the old archive (a new file gets the umask default).
        out.set_permissions(fs::metadata(&archive)?.permissions())?;
    }
    write(Rewrite {
        old: exists.then_some(archive.as_path()),
        files,
        out: &mut out,
        state: &mut state,
        progress,
        cancel,
    })?;
    check_cancel(cancel)?;
    out.sync_all()?;
    drop(out);
    fs::rename(&staged, &archive).with_context(|| format!("replace {}", archive.display()))?;
    staging.0 = None;
    progress(state);
    Ok(())
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
    let files = collect_add(&zip_path, exists, src, inner_dir)?;
    let mut state = add_progress(&files)?;
    progress(state.clone());
    let parent = zip_path.parent().context("zip has no parent folder")?;
    // A new zip gets the usual umask-filtered mode, not the temp file's private 0600.
    #[cfg(unix)]
    let partial = tempfile::Builder::new()
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666))
        .tempfile_in(parent);
    #[cfg(not(unix))]
    let partial = tempfile::NamedTempFile::new_in(parent);
    let mut partial = partial?;
    // An existing zip keeps its mode bits (only those: ACLs and xattrs are not copied).
    #[cfg(unix)]
    if exists {
        let mode = fs::metadata(&zip_path)?.permissions();
        partial.as_file().set_permissions(mode)?;
    }
    let mut output = zip::ZipWriter::new(io::BufWriter::new(partial.as_file_mut()));
    if exists {
        let mut old = zip::ZipArchive::new(io::BufReader::new(fs::File::open(&zip_path)?))?;
        output.set_raw_comment(old.comment().into());
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
        if let Some(time) = meta.modified().ok().and_then(zip_time) {
            options = options.last_modified_time(time);
        }
        #[cfg(unix)]
        {
            options = options
                .unix_permissions(std::os::unix::fs::PermissionsExt::mode(&meta.permissions()));
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

/// Zip timestamps are local wall-clock time (two-second resolution, 1980 onwards).
#[cfg(feature = "zip")]
fn zip_time(time: std::time::SystemTime) -> Option<zip::DateTime> {
    use chrono::{Datelike, Timelike};
    let local = chrono::DateTime::<chrono::Local>::from(time);
    zip::DateTime::from_date_and_time(
        u16::try_from(local.year()).ok()?,
        local.month() as u8,
        local.day() as u8,
        local.hour() as u8,
        local.minute() as u8,
        local.second() as u8,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::{is_partial, partial_name};
    #[test]
    fn partial_names_are_unique_bounded_and_recognised() {
        let (a, b) = (partial_name("report.pdf"), partial_name("report.pdf"));
        assert_ne!(a, b);
        assert!(a.starts_with("report.pdf.keel-partial-") && is_partial(&a));
        let long = partial_name(&"\u{e9}".repeat(200));
        assert!(long.len() <= 255 && is_partial(&long), "{}", long.len());
        assert!(is_partial("x.bin.keel-partial"));
        assert!(is_partial("x.keel-partial-1234-0") && is_partial("x.keel-partial-7-10"));
        for not in [
            "notes.keel-partial-draft.txt",
            ".keel-partial",
            "x.keel-partial-1-",
            "x.keel-partial-1",
            "report.keel-partial-2024-05",
            "x.keel-partial-01-2",
            "x.keel-partial-1-2-3",
            "x.keel-partial-1-2.txt",
            "x.keel-partial--1-2",
            "x.keel-partial-+1-2",
        ] {
            assert!(!is_partial(not), "{not}");
        }
    }
}

#[cfg(all(test, feature = "zip"))]
#[path = "extract_tests.rs"]
mod extract_tests;
