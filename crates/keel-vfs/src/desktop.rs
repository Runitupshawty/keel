//! Windows helpers for keel-app (which may not use the `windows` crate): bringing Keel's
//! window forward (global hotkey, second instance), showing `--help` output from a
//! GUI-subsystem build, and which volume a path is on (one transfer per drive).

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::Storage::FileSystem::{GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, GetClassNameW, GetWindow, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindowAsync, GW_OWNER, SW_RESTORE,
};

/// The class winit registers its windows under (its default; eframe keeps it). Other
/// top-level windows of the process, like Explorer's Properties sheet (`#32770`), differ.
const WINIT_CLASS: &str = "Window Class";

/// Restores (if minimized) and activates this process's main window: the first visible,
/// unowned top-level window of winit's class. False when there is none or Windows refused.
pub fn focus_own_window() -> bool {
    unsafe extern "system" fn find(hwnd: HWND, found: LPARAM) -> BOOL {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let main = pid == GetCurrentProcessId()
            && IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_OWNER).is_err()
            && is_winit(hwnd);
        if main {
            *(found.0 as *mut Option<HWND>) = Some(hwnd);
        }
        BOOL::from(!main) // stop at the first match
    }
    let mut found: Option<HWND> = None;
    unsafe {
        let _ = EnumWindows(Some(find), LPARAM(&mut found as *mut _ as isize));
        let Some(hwnd) = found else { return false };
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

fn is_winit(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n]) == WINIT_CLASS
}

/// Lets process `pid` (the running instance this one hands its request to) take the
/// foreground: this process was just started by the user, so it may pass that right on.
pub fn allow_foreground(pid: u32) {
    let _ = unsafe { AllowSetForegroundWindow(pid) };
}

/// Lets any process take the foreground (superseded by `allow_foreground`).
pub fn allow_foreground_any() {
    let _ = unsafe { AllowSetForegroundWindow(windows::Win32::UI::WindowsAndMessaging::ASFW_ANY) };
}

/// Attaches to the console of the shell that started a GUI-subsystem build, so `--help`
/// and `--version` text shows there. Does nothing when there is none.
pub fn attach_parent_console() {
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// The volume `path` (which must exist) is on, as `\?\Volume{GUID}\`, lowercase: the same
/// for every way to reach it (drive letters, `subst` drives, junctions, folder mount
/// points). For a network share, its root (`\server\share\`). None when Windows can't
/// tell. Touches the disk.
pub fn volume_id(path: &Path) -> Option<String> {
    // Resolves subst drives and junctions to the real path first.
    let real = std::fs::canonicalize(path).ok()?;
    // Without the `\\?\` prefix canonicalize adds, which the volume calls do not take.
    let real = real.to_string_lossy();
    let plain = match real.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => real.strip_prefix(r"\\?\").unwrap_or(&real).to_owned(),
    };
    let wide: Vec<u16> = std::ffi::OsStr::new(&plain)
        .encode_wide()
        .chain([0])
        .collect();
    let mut root = [0u16; 1024];
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut root) }.ok()?;
    let mut guid = [0u16; 64];
    let id = match unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root.as_ptr()), &mut guid) } {
        Ok(()) => &guid[..],
        Err(_) => &root[..], // shares have no volume GUID
    };
    let n = id.iter().position(|&c| c == 0).unwrap_or(id.len());
    Some(String::from_utf16_lossy(&id[..n]).to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_ids() {
        let tmp = std::env::temp_dir();
        let id = volume_id(&tmp).unwrap();
        assert!(id.starts_with(r"\\?\volume{"), "{id}");
        assert_eq!(volume_id(&tmp.join(".")), Some(id.clone()));
        let drive = tmp.ancestors().last().unwrap();
        assert_eq!(volume_id(drive), Some(id), "same volume as its drive root");
        assert_eq!(volume_id(Path::new(r"C:\no\such\keel\path")), None);
    }

    #[test]
    fn no_window_in_tests() {
        assert!(!focus_own_window(), "the test process has no winit window");
    }
}
