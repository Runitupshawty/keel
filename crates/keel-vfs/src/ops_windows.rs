//! Win32 file primitives behind the shared `copy_file` / `rename_noreplace` interface.
use anyhow::{Context, Result};
use std::{
    cell::RefCell,
    ffi::c_void,
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            CopyFileExW, MoveFileExW, LPPROGRESS_ROUTINE_CALLBACK_REASON, MOVE_FILE_FLAGS,
        },
        System::WindowsProgramming::{PROGRESS_CANCEL, PROGRESS_CONTINUE},
    },
};

fn wide(p: &Path) -> io::Result<Vec<u16>> {
    let mut units: Vec<_> = p.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    units.push(0);
    Ok(units)
}

/// Rename that fails instead of replacing an existing `to`. Cross-volume moves fail with
/// `ErrorKind::CrossesDevices` (no MOVEFILE_COPY_ALLOWED) so callers can copy with progress.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (src, dst) = (wide(from)?, wide(to)?);
    unsafe {
        MoveFileExW(
            PCWSTR(src.as_ptr()),
            PCWSTR(dst.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    }
    // HRESULT_FROM_WIN32 keeps the Win32 code in the low word.
    .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

struct CopyContext<'a> {
    on_bytes: &'a dyn Fn(u64),
    cancel: &'a AtomicBool,
    panic: RefCell<Option<Box<dyn std::any::Any + Send>>>,
}

/// Copies `src` over `dst` with CopyFileExW, calling `on_bytes(copied_so_far)`.
pub(crate) fn copy_file(
    src: &Path,
    dst: &Path,
    on_bytes: &dyn Fn(u64),
    cancel: &AtomicBool,
) -> Result<()> {
    let (from, to) = (wide(src)?, wide(dst)?);
    let context = CopyContext {
        on_bytes,
        cancel,
        panic: RefCell::new(None),
    };
    // CopyFileExW invokes the callback synchronously; `context` outlives the call.
    let result = unsafe {
        CopyFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            Some(copy_progress),
            Some((&context as *const CopyContext<'_>).cast()),
            None,
            0,
        )
    };
    if let Some(panic) = context.panic.into_inner() {
        std::panic::resume_unwind(panic);
    }
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
    result.with_context(|| format!("copy {} to {}", src.display(), dst.display()))
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
    let context = &*data.cast::<CopyContext<'_>>();
    if context.cancel.load(Ordering::Relaxed) {
        return PROGRESS_CANCEL;
    }
    // Never unwind through a Windows ABI frame; resume after CopyFileExW returns.
    let bytes = transferred.max(0) as u64;
    if let Err(panic) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (context.on_bytes)(bytes)))
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
