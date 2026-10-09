//! Portable file primitives for macOS/Linux behind the shared `copy_file` / `rename_noreplace`.
use anyhow::{Context, Result};
use std::{
    fs, io,
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// Rename that fails instead of replacing an existing `to`; EXDEV maps to `CrossesDevices`.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    // ponytail: check-then-rename can race; use renameat2(RENAME_NOREPLACE) if that matters.
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(from, to)
}

/// Chunked copy (1 MiB) of `src` over `dst`, calling `on_bytes(copied_so_far)` per chunk.
pub(crate) fn copy_file(
    src: &Path,
    dst: &Path,
    on_bytes: &dyn Fn(u64),
    cancel: &AtomicBool,
) -> Result<()> {
    let ctx = || format!("copy {} to {}", src.display(), dst.display());
    let mut input = fs::File::open(src).with_context(ctx)?;
    let mut output = fs::File::create(dst).with_context(ctx)?;
    let mut buf = vec![0; 1 << 20];
    let mut done = 0u64;
    loop {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(ctx),
        };
        output.write_all(&buf[..n]).with_context(ctx)?;
        done += n as u64;
        on_bytes(done);
    }
    let meta = input.metadata()?;
    output.set_permissions(meta.permissions())?;
    output.set_modified(meta.modified()?)?;
    Ok(())
}
