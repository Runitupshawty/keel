//! Windows window and console helpers for keel-app (which may not use the `windows` crate):
//! bringing Keel's window forward (global hotkey, second instance) and showing `--help`
//! output from a GUI-subsystem build.

use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, GetWindow, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindowAsync,
    ASFW_ANY, GW_OWNER, SW_RESTORE,
};

/// Restores (if minimized) and activates this process's main window: the first visible,
/// unowned, titled top-level window. False when there is none or Windows refused.
pub fn focus_own_window() -> bool {
    unsafe extern "system" fn find(hwnd: HWND, found: LPARAM) -> BOOL {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let main = pid == GetCurrentProcessId()
            && IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_OWNER).is_err()
            && GetWindowTextLengthW(hwnd) > 0;
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

/// Lets the running instance take the foreground (this process was just started by the
/// user, so it may hand that right over).
pub fn allow_foreground_any() {
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
}

/// Attaches to the console of the shell that started a GUI-subsystem build, so `--help`
/// and `--version` text shows there. Does nothing when there is none.
pub fn attach_parent_console() {
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}
