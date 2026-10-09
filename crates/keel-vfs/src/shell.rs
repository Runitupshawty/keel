//! Windows shell dialogs keel-app may open without using the `windows` crate itself.
use anyhow::Result;
use std::{os::windows::ffi::OsStrExt, path::Path};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HWND,
        System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
        UI::Shell::{SHObjectProperties, SHOP_FILEPATH},
    },
};

/// Opens Explorer's Properties sheet for `path`. The sheet runs on its own shell thread
/// and lives as long as this process; the caller returns at once. Call off the UI thread
/// (the shell may touch a slow network path).
pub fn properties(path: &Path) -> Result<()> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    anyhow::ensure!(!wide[..wide.len() - 1].contains(&0), "path contains NUL");
    // SAFETY: COM init on this thread (an S_FALSE/RPC_E_CHANGED_MODE result is harmless);
    // `wide` is NUL-terminated and outlives the call.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        SHObjectProperties(
            HWND::default(),
            SHOP_FILEPATH,
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
        )
        .ok()?;
    }
    Ok(())
}
