//! Windows file clipboard: `CF_HDROP` plus `Preferred DropEffect` (cut = move), so files
//! copied here paste in Explorer and the other way round. Other OSes live in keel-app.
use anyhow::{Context, Result};
use std::{
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
        Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE},
    },
    UI::Shell::{DragQueryFileW, CFSTR_PREFERREDDROPEFFECT, DROPFILES, HDROP},
};

/// Open clipboard; closed on drop.
struct Open;
impl Open {
    /// Another app (a clipboard manager, Office) often holds the clipboard for a few ms:
    /// retry for about half a second before giving up. Workers only.
    fn new() -> Result<Self> {
        for _ in 0..24 {
            if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
                return Ok(Open);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        unsafe { OpenClipboard(HWND::default()) }.context("clipboard is busy")?;
        Ok(Open)
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// Copies `bytes` into a movable global block and hands it to the clipboard.
unsafe fn set(format: u32, bytes: &[u8]) -> Result<()> {
    let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let ptr = GlobalLock(mem) as *mut u8;
    if ptr.is_null() {
        let _ = GlobalFree(mem);
        anyhow::bail!("GlobalLock failed");
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = GlobalUnlock(mem);
    if let Err(e) = SetClipboardData(format, HANDLE(mem.0)) {
        // Ownership only passes to the system on success.
        let _ = GlobalFree(mem);
        return Err(e.into());
    }
    Ok(())
}

fn drop_effect_format() -> u32 {
    unsafe { RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) }
}

/// Puts `paths` on the clipboard as files (Explorer-compatible). Empty `paths` clears it.
pub fn write_files(paths: &[PathBuf], cut: bool) -> Result<()> {
    let _open = Open::new()?;
    unsafe { EmptyClipboard() }?;
    if paths.is_empty() {
        return Ok(());
    }
    let header = std::mem::size_of::<DROPFILES>();
    let mut bytes = vec![0u8; header];
    bytes[..4].copy_from_slice(&(header as u32).to_le_bytes()); // pFiles
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        // Plain absolute paths: Explorer does not take `\?\` names here.
        let units: Vec<u16> = p.as_os_str().encode_wide().collect();
        anyhow::ensure!(!units.contains(&0), "path contains NUL");
        for u in units.into_iter().chain([0]) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    let effect = if cut {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY
    };
    unsafe {
        set(u32::from(CF_HDROP.0), &bytes)?;
        set(drop_effect_format(), &effect.0.to_le_bytes())?;
    }
    Ok(())
}

/// Files on the clipboard and whether they were cut. `None` when it holds no files.
pub fn read_files() -> Result<Option<(Vec<PathBuf>, bool)>> {
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_HDROP.0)) }.is_err() {
        return Ok(None);
    }
    let _open = Open::new()?;
    let drop = HDROP(unsafe { GetClipboardData(u32::from(CF_HDROP.0)) }?.0);
    let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
    let mut paths = Vec::with_capacity(count as usize);
    for i in 0..count {
        let len = unsafe { DragQueryFileW(drop, i, None) } as usize;
        let mut buf = vec![0u16; len + 1];
        let got = unsafe { DragQueryFileW(drop, i, Some(&mut buf)) } as usize;
        paths.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..got])));
    }
    let cut = unsafe { GetClipboardData(drop_effect_format()) }
        .ok()
        .and_then(|h| unsafe {
            let mem = HGLOBAL(h.0);
            let ptr = GlobalLock(mem) as *const u8;
            let effect = (!ptr.is_null() && GlobalSize(mem) >= 4)
                .then(|| u32::from_le_bytes(std::ptr::read_unaligned(ptr as *const [u8; 4])));
            let _ = GlobalUnlock(mem);
            effect
        })
        // Explorer writes MOVE for cut and COPY|LINK for copy.
        .is_some_and(|e| e & DROPEFFECT_MOVE.0 != 0 && e & DROPEFFECT_COPY.0 == 0);
    Ok((!paths.is_empty()).then_some((paths, cut)))
}

/// Changes whenever any app writes the clipboard.
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}
