use crate::local::{long, recycle, wide};
use anyhow::{Context, Result};
use std::{
    cell::RefCell,
    ffi::c_void,
    fs,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use windows::Win32::System::WindowsProgramming::{PROGRESS_CANCEL, PROGRESS_CONTINUE};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            CopyFileExW, MoveFileExW, FILE_ATTRIBUTE_REPARSE_POINT,
            LPPROGRESS_ROUTINE_CALLBACK_REASON, MOVEFILE_COPY_ALLOWED, MOVE_FILE_FLAGS,
        },
    },
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

/// Includes directories in the item count. Reparse points are deliberately not followed.
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
    anyhow::ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "copy/move of reparse points is unsupported: {}",
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

fn canonical_key(path: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        fs::canonicalize(path)?.to_string_lossy().to_lowercase(),
    ))
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
    // Preflight the entire selection before any writes, including alias/case resolution.
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
    fn node(&mut self, source: &Path, proposed: &Path) -> Result<bool> {
        check_cancel(self.cancel)?;
        let metadata = fs::symlink_metadata(source)?;
        ensure_regular(source, &metadata)?;
        self.state.current = source.to_string_lossy().into_owned();
        let target = match destination(source, proposed, self.conflict)? {
            Some(p) => p,
            None => return Ok(false),
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
        }
        if self.moving && !target.try_exists()? && same_volume(source, &target)? {
            let (bytes, items) = plan_size(&[source.to_path_buf()])?;
            check_cancel(self.cancel)?;
            move_file(source, &target, MOVEFILE_COPY_ALLOWED)?;
            self.state.done_bytes += bytes;
            self.state.done_items += items;
            (self.progress)(self.state.clone());
            return Ok(true);
        }
        if metadata.is_dir() {
            if !target.try_exists()? {
                fs::create_dir(&target)?;
            }
            let mut complete = true;
            // Stable traversal makes progress and cancellation deterministic.
            let mut children = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                complete &= self.node(&child.path(), &target.join(child.file_name()))?;
            }
            if self.moving && complete {
                check_cancel(self.cancel)?;
                recycle(source)?;
            }
            self.state.current = source.to_string_lossy().into_owned();
            self.state.done_items += 1;
            (self.progress)(self.state.clone());
            return Ok(complete);
        }
        self.copy_file(source, &target, metadata.len())?;
        if self.moving {
            recycle(source)?;
        }
        self.state.done_bytes += metadata.len();
        self.state.done_items += 1;
        (self.progress)(self.state.clone());
        Ok(true)
    }

    fn copy_file(&self, source: &Path, target: &Path, size: u64) -> Result<()> {
        // Stage in the destination directory: cancel/failure cannot truncate an existing file.
        let staged = Staged::new(target.parent().context("target has no parent")?)?;
        let src_name = wide(source)?;
        let dst_name = wide(&staged.0)?;
        let context = CopyContext {
            state: &self.state,
            progress: self.progress,
            cancel: self.cancel,
            transferred: AtomicU64::new(0),
            panic: RefCell::new(None),
        };
        // CopyFileEx invokes callbacks synchronously; all borrowed state outlives the call.
        let result = unsafe {
            CopyFileExW(
                PCWSTR(src_name.as_ptr()),
                PCWSTR(dst_name.as_ptr()),
                Some(copy_progress),
                Some((&context as *const CopyContext<'_>).cast()),
                None,
                0,
            )
        };
        if let Some(panic) = context.panic.into_inner() {
            std::panic::resume_unwind(panic);
        }
        check_cancel(self.cancel)?;
        result.with_context(|| format!("copy {} to {}", source.display(), target.display()))?;
        anyhow::ensure!(
            fs::metadata(&staged.0)?.len() == size,
            "source changed during copy: {}",
            source.display()
        );
        // Recheck races: only Overwrite may displace a target that appeared during copying.
        if target.try_exists()? {
            anyhow::ensure!(
                self.conflict == Conflict::Overwrite,
                "destination appeared during copy: {}",
                target.display()
            );
            let metadata = fs::symlink_metadata(target)?;
            ensure_regular(target, &metadata)?;
            anyhow::ensure!(
                metadata.is_file() && !same_file::is_same_file(source, target)?,
                "unsafe overwrite: {}",
                target.display()
            );
            recycle(target)
                .with_context(|| format!("recycle overwritten file {}", target.display()))?;
        }
        // No REPLACE_EXISTING: a racing new file must not be permanently destroyed.
        move_file(&staged.0, target, MOVE_FILE_FLAGS(0))?;
        Ok(())
    }
}

fn destination(source: &Path, proposed: &Path, conflict: Conflict) -> Result<Option<PathBuf>> {
    if fs::symlink_metadata(proposed).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Ok(Some(proposed.to_path_buf()));
    }
    // Propagate access errors, and count dangling links as collisions.
    fs::symlink_metadata(proposed)?;
    match conflict {
        Conflict::Skip => Ok(None),
        Conflict::Overwrite => Ok(Some(proposed.to_path_buf())),
        Conflict::RenameNew => {
            let metadata = fs::symlink_metadata(source)?;
            let stem = if metadata.is_dir() {
                proposed.file_name()
            } else {
                proposed.file_stem()
            }
            .context("target has no name")?;
            for n in 2u64.. {
                let mut name = stem.to_os_string();
                name.push(format!(" ({n})"));
                if !metadata.is_dir() {
                    if let Some(ext) = proposed.extension() {
                        name.push(".");
                        name.push(ext);
                    }
                }
                let candidate = proposed.with_file_name(name);
                match fs::symlink_metadata(&candidate) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Some(candidate))
                    }
                    Err(e) => return Err(e.into()),
                    Ok(_) => {}
                }
            }
            unreachable!()
        }
    }
}

fn same_volume(source: &Path, target: &Path) -> Result<bool> {
    // Prefix equality is conservative for ordinary volumes. Mount points may still cause
    // a cross-volume move; reject reparse destinations in the fast path below.
    let src = fs::canonicalize(source)?;
    let parent = target.parent().context("target has no parent")?;
    let dst = fs::canonicalize(parent)?;
    Ok(src.components().next() == dst.components().next())
}

fn move_file(source: &Path, target: &Path, flags: MOVE_FILE_FLAGS) -> Result<()> {
    let from = wide(source)?;
    let to = wide(target)?;
    unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) }
        .with_context(|| format!("move {} to {}", source.display(), target.display()))
}

struct Staged(PathBuf);
impl Staged {
    fn new(dir: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = dir.join(format!(
                ".keel-copy-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("create staged copy"),
            }
        }
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        // Only this job's private incomplete copy is removed permanently.
        let _ = fs::remove_file(&self.0);
    }
}

struct CopyContext<'a> {
    state: &'a Progress,
    progress: &'a dyn Fn(Progress),
    cancel: &'a AtomicBool,
    transferred: AtomicU64,
    panic: RefCell<Option<Box<dyn std::any::Any + Send>>>,
}

unsafe extern "system" fn copy_progress(
    _total: i64,
    transferred: i64,
    _stream_size: i64,
    _stream_transferred: i64,
    _stream: u32,
    _reason: LPPROGRESS_ROUTINE_CALLBACK_REASON,
    _source: HANDLE,
    _destination: HANDLE,
    data: *const c_void,
) -> u32 {
    // The caller supplies a valid context for the synchronous duration of CopyFileExW.
    let context = &*data.cast::<CopyContext<'_>>();
    if context.cancel.load(Ordering::Relaxed) {
        return PROGRESS_CANCEL;
    }
    let transferred = transferred.max(0) as u64;
    context
        .transferred
        .fetch_max(transferred, Ordering::Relaxed);
    let mut state = context.state.clone();
    state.done_bytes += context.transferred.load(Ordering::Relaxed);
    // Never unwind through a Windows ABI frame. Resume on the Rust side after CopyFileExW.
    if let Err(panic) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (context.progress)(state)))
    {
        *context.panic.borrow_mut() = Some(panic);
        return PROGRESS_CANCEL;
    }
    if context.cancel.load(Ordering::Relaxed) {
        PROGRESS_CANCEL
    } else {
        PROGRESS_CONTINUE
    }
}
