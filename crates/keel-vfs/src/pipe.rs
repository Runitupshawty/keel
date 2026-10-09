//! Per-user named pipes for keel-app's single instance (which may not use the `windows`
//! crate): the listener's DACL, a client connect that refuses impersonation and checks who
//! serves the pipe, and a read cancel for the server's per-client timeout.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::time::{Duration, Instant};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    LocalFree, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// SDDL for a pipe only the current user may open: a protected DACL with one entry, full
/// access for the user's SID (no Everyone / Anonymous read that the default DACL grants).
pub fn user_only_sddl() -> io::Result<String> {
    Ok(format!("D:P(A;;GA;;;{})", user_sid()?))
}

/// The current user's SID (`S-1-5-21-…`).
pub fn user_sid() -> io::Result<String> {
    // The pseudo handle needs no closing.
    process_user(unsafe { GetCurrentProcess() })
}

/// Whether process `pid` runs as the current user.
pub fn same_user(pid: u32) -> io::Result<bool> {
    let me = user_sid()?;
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
    let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
    Ok(process_user(HANDLE(process.as_raw_handle()))? == me)
}

fn process_user(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
        let token = OwnedHandle::from_raw_handle(token.0);
        let token = HANDLE(token.as_raw_handle());
        let mut len = 0;
        // Sizing call: fails with ERROR_INSUFFICIENT_BUFFER and sets `len`.
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        // u64s: TOKEN_USER holds a pointer, so the buffer must be pointer-aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text)?;
        let sid = text.to_string();
        let _ = LocalFree(HLOCAL(text.0.cast()));
        sid.map_err(io::Error::other)
    }
}

/// Opens `\\.\pipe\<name>` for reading and writing, waiting up to `timeout` while every
/// instance is busy. The server may only identify this client, never impersonate it
/// (`SECURITY_IDENTIFICATION`), and it must run as the current user (PermissionDenied
/// otherwise: someone else holds the name). Returns the pipe and the server's process id.
pub fn connect(name: &str, timeout: Duration) -> io::Result<(File, u32)> {
    let path: Vec<u16> = format!(r"\\.\pipe\{name}")
        .encode_utf16()
        .chain([0])
        .collect();
    let path = PCWSTR(path.as_ptr());
    let deadline = Instant::now() + timeout;
    let pipe = loop {
        let opened = unsafe {
            CreateFileW(
                path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                HANDLE::default(),
            )
        };
        match opened {
            Ok(h) => break unsafe { OwnedHandle::from_raw_handle(h.0) },
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                let ms = left.as_millis().clamp(1, u32::MAX as u128) as u32;
                let _ = unsafe { WaitNamedPipeW(path, ms) };
            }
            Err(e) => return Err(win32_error(e)),
        }
    };
    let mut server = 0;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server)? };
    if !same_user(server)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("pipe {name} is served by another user's process {server}"),
        ));
    }
    Ok((File::from(pipe), server))
}

/// The Win32 error behind `e` as an `io::Error`, so its kind (NotFound, …) survives.
fn win32_error(e: windows::core::Error) -> io::Error {
    let code = e.code().0 as u32;
    match code >> 16 {
        0x8007 => io::Error::from_raw_os_error((code & 0xFFFF) as i32),
        _ => e.into(),
    }
}

/// Cancels the pending reads and writes on `pipe`, from any thread: they fail with
/// `ERROR_OPERATION_ABORTED`.
pub fn cancel_io(pipe: BorrowedHandle<'_>) {
    let _ = unsafe { windows::Win32::System::IO::CancelIoEx(HANDLE(pipe.as_raw_handle()), None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_and_sddl() {
        let sid = user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(user_only_sddl().unwrap(), format!("D:P(A;;GA;;;{sid})"));
        assert!(same_user(std::process::id()).unwrap());
        // System (pid 4) is not this user (or not even openable).
        assert!(!same_user(4).unwrap_or(false));
    }

    #[test]
    fn missing_pipe_is_not_found() {
        let name = format!("keel-vfs-test-none-{}", std::process::id());
        let err = connect(&name, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
