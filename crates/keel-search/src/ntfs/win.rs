//! Win32 side of the NTFS indexer: fixed NTFS volumes, MFT enumeration
//! (`FSCTL_ENUM_USN_DATA`), the USN journal, elevation.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::{
    FileIdType, FileNameInfo, GetDriveTypeW, GetFileAttributesExW, GetFileExInfoStandard,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetLogicalDrives,
    GetVolumeInformationW, OpenFileById, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, WIN32_FILE_ATTRIBUTE_DATA,
};
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::usn::{parse_buffer, Record};

const DRIVE_FIXED: u32 = 3;
const GENERIC_READ: u32 = 0x8000_0000;
const BUFFER: usize = 1 << 20;

/// A fixed NTFS volume by drive letter and serial number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VolumeId {
    pub letter: char,
    pub serial: u32,
}

impl VolumeId {
    /// "C:".
    pub fn drive(&self) -> String {
        format!("{}:", self.letter)
    }
}

/// Every fixed drive formatted NTFS.
pub(crate) fn fixed_ntfs_volumes() -> Vec<VolumeId> {
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let letter = char::from(b'A' + bit);
            let root = HSTRING::from(format!("{letter}:\\"));
            // SAFETY: `root` is a NUL-terminated wide string that outlives the call.
            if unsafe { GetDriveTypeW(&root) } != DRIVE_FIXED {
                return None;
            }
            let mut serial = 0u32;
            let mut fs = [0u16; 32];
            // SAFETY: the out pointers and buffer outlive the call.
            unsafe {
                GetVolumeInformationW(&root, None, Some(&mut serial), None, None, Some(&mut fs))
            }
            .ok()?;
            let end = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
            (String::from_utf16_lossy(&fs[..end]) == "NTFS").then_some(VolumeId { letter, serial })
        })
        .collect()
}

/// Size (0 for a folder) and last-write time of `path` from one GetFileAttributesExW
/// call. Unlike `std::fs::metadata` it opens no handle: about 2x faster warm, and no
/// open for antivirus filters to inspect. None when it fails (gone, or a path too long
/// for the API).
pub(crate) fn file_meta(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    // SAFETY: `data` is the out struct GetFileExInfoStandard fills; the name outlives
    // the call.
    unsafe {
        GetFileAttributesExW(
            &HSTRING::from(path.as_os_str()),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut c_void,
        )
    }
    .ok()?;
    let size = if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        0
    } else {
        u64::from(data.nFileSizeHigh) << 32 | u64::from(data.nFileSizeLow)
    };
    let written = data.ftLastWriteTime;
    let filetime = u64::from(written.dwHighDateTime) << 32 | u64::from(written.dwLowDateTime);
    Some((size, crate::everything::filetime_to_system_time(filetime)))
}

/// The root directory's file reference number (`C:\`).
pub(crate) fn root_frn(letter: char) -> io::Result<u64> {
    let dir = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(format!("{letter}:\\"))?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid handle and out struct.
    unsafe { GetFileInformationByHandle(handle(&dir), &mut info) }?;
    Ok(u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow))
}

/// `USN_JOURNAL_DATA_V0`, the fields the indexer needs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Journal {
    pub id: u64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
}

/// A handle to `\\.\X:`. Reading the MFT needs an administrator; the journal read
/// falls back to `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` on a no-access handle.
pub(crate) struct Volume {
    file: File,
    privileged: bool,
}

fn handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

fn ioctl<T>(file: &File, code: u32, input: &T, out: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0u32;
    // SAFETY: `input` and `out` are live buffers of the sizes passed.
    unsafe {
        DeviceIoControl(
            handle(file),
            code,
            Some(input as *const T as *const c_void),
            size_of::<T>() as u32,
            Some(out.as_mut_ptr() as *mut c_void),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
    Ok(returned as usize)
}

#[repr(C)]
struct MftEnumDataV0 {
    start_frn: u64,
    low_usn: i64,
    high_usn: i64,
}

#[repr(C)]
struct ReadUsnJournalDataV0 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    journal_id: u64,
}

impl Volume {
    /// Opens `\\.\X:` for reading (administrator), else with no access rights
    /// (enough for the unprivileged journal read).
    pub fn open(letter: char) -> io::Result<Self> {
        let path = format!(r"\\.\{letter}:");
        let open = |access: u32| {
            std::fs::OpenOptions::new()
                .access_mode(access)
                .share_mode(7)
                .open(&path)
        };
        match open(GENERIC_READ) {
            Ok(file) => Ok(Self {
                file,
                privileged: true,
            }),
            // Not elevated: a handle to the root folder is enough for the
            // unprivileged journal read.
            Err(_) => Ok(Self {
                file: std::fs::OpenOptions::new()
                    .access_mode(GENERIC_READ)
                    .share_mode(7)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
                    .open(format!("{letter}:\\"))?,
                privileged: false,
            }),
        }
    }

    pub fn privileged(&self) -> bool {
        self.privileged
    }

    /// The current name of a file by its reference number (the unprivileged journal
    /// read leaves names out). None when it is gone or this user cannot open it.
    pub fn name_of(&self, frn: u64) -> Option<String> {
        let id = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
        };
        // SAFETY: `id` outlives the call; the returned handle is closed below.
        let file = unsafe {
            OpenFileById(
                handle(&self.file),
                &id,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        }
        .ok()?;
        // FILE_NAME_INFO: a u32 byte length, then the volume-relative path.
        let mut buf = vec![0u32; 8 * 1024];
        // SAFETY: `buf` is a live, u32-aligned buffer of the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file,
                FileNameInfo,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 4) as u32,
            )
        }
        .is_ok();
        // SAFETY: opened above.
        let _ = unsafe { CloseHandle(file) };
        if !ok {
            return None;
        }
        let len = (buf[0] as usize / 2).min((buf.len() - 1) * 2);
        // SAFETY: the u32 buffer viewed as the u16 path that follows the length.
        let wide = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(1) as *const u16, len) };
        let path = String::from_utf16_lossy(wide);
        path.rsplit('\\')
            .next()
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
    }

    pub fn query_journal(&self) -> io::Result<Journal> {
        let mut out = [0u8; 64];
        ioctl(&self.file, FSCTL_QUERY_USN_JOURNAL, &(), &mut out)?;
        let at = |i: usize| u64::from_le_bytes(out[i..i + 8].try_into().unwrap());
        Ok(Journal {
            id: at(0),
            next_usn: at(16) as i64,
            lowest_valid_usn: at(24) as i64,
        })
    }

    /// Streams every MFT entry with a name (`FSCTL_ENUM_USN_DATA`; administrator).
    pub fn enumerate(&self, mut each: impl FnMut(Vec<Record>)) -> io::Result<()> {
        let mut out = vec![0u8; BUFFER];
        let mut input = MftEnumDataV0 {
            start_frn: 0,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        loop {
            let n = match ioctl(&self.file, FSCTL_ENUM_USN_DATA, &input, &mut out) {
                Ok(n) => n,
                // ERROR_HANDLE_EOF: past the last MFT record.
                Err(e) if e.raw_os_error() == Some(38) => return Ok(()),
                Err(e) => return Err(e),
            };
            let Some((next, records)) = parse_buffer(&out[..n]) else {
                return Ok(());
            };
            if records.is_empty() && next == input.start_frn {
                return Ok(());
            }
            each(records);
            input.start_frn = next;
        }
    }

    /// Reads journal records from `start` up to the current end. Returns the records
    /// and the USN to resume from. Errors with ERROR_JOURNAL_ENTRY_DELETED (1181)
    /// when `start` was already truncated away.
    pub fn read_journal(&self, journal_id: u64, start: i64) -> io::Result<(Vec<Record>, i64)> {
        let code = if self.privileged {
            FSCTL_READ_USN_JOURNAL
        } else {
            FSCTL_READ_UNPRIVILEGED_USN_JOURNAL
        };
        let mut out = vec![0u8; BUFFER];
        let mut input = ReadUsnJournalDataV0 {
            start_usn: start,
            reason_mask: u32::MAX,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            journal_id,
        };
        let mut all = Vec::new();
        loop {
            let n = ioctl(&self.file, code, &input, &mut out)?;
            let Some((next, records)) = parse_buffer(&out[..n]) else {
                return Ok((all, input.start_usn));
            };
            let done = records.is_empty();
            all.extend(records);
            input.start_usn = next as i64;
            if done {
                return Ok((all, input.start_usn));
            }
        }
    }
}

#[cfg(test)]
impl Volume {
    /// Hex of the first journal record at `start` (test capture).
    pub fn raw_journal_hex(&self, journal_id: u64, start: i64) -> String {
        let mut out = vec![0u8; 4096];
        let input = ReadUsnJournalDataV0 {
            start_usn: start,
            reason_mask: u32::MAX,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            journal_id,
        };
        let n = ioctl(&self.file, FSCTL_READ_USN_JOURNAL, &input, &mut out).unwrap_or(0);
        let len = out
            .get(8..12)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
        out[8..(8 + len).min(n)]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Whether this process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo process handle needs no closing; `token` is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: `elevation` is the documented out struct for TokenElevation.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .is_ok();
    // SAFETY: opened above.
    let _ = unsafe { CloseHandle(token) };
    ok && elevation.TokenIsElevated != 0
}

/// Runs `exe args` through the UAC prompt ("runas"), waits, returns its exit code.
pub(crate) fn run_elevated(exe: &Path, args: &str) -> io::Result<u32> {
    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: the strings outlive the call; hProcess is closed below.
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| io::Error::other(e.message()))?;
    let mut code = 1u32;
    // SAFETY: SEE_MASK_NOCLOSEPROCESS hands back a process handle we own.
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
    }
    Ok(code)
}
