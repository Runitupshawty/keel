//! Files only this user may read or write: the plan store (`api-plans.json`), the
//! WebSocket token, the socket-name salt. Windows: a protected DACL with one entry (the
//! user's SID, full access), set as the file is created; Unix: mode 0600. `read` refuses
//! (returns None for) a file anybody else may access.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Longest file `read` returns.
const MAX_READ: u64 = 1 << 20;

/// Creates `path`, which must not exist, readable and writable by this user only.
pub fn create_new(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(windows)]
    win::create_new(path)
}

/// Replaces `path` with `bytes`: a private temporary file renamed over it (the rename
/// carries the private permissions along).
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut f = create_new(&tmp)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// The contents of `path` (at most 1 MiB) when it is a plain file only this user may
/// access; None when it is not (someone else may read or change it, or it is a link).
pub fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = open(path)?;
    if !is_private(&file)? {
        return Ok(None);
    }
    let mut buf = Vec::new();
    file.take(MAX_READ).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

#[cfg(unix)]
fn open(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(unix)]
fn is_private(file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok(meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0)
}

#[cfg(windows)]
fn open(path: &Path) -> io::Result<File> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a link", path.display()),
        ));
    }
    File::open(path)
}

#[cfg(windows)]
fn is_private(file: &File) -> io::Result<bool> {
    Ok(file.metadata()?.is_file() && win::owner_only(file)?)
}

#[cfg(windows)]
mod win {
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        LocalFree, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };

    fn win32(e: windows::core::Error) -> io::Error {
        let code = e.code().0 as u32;
        match code >> 16 {
            0x8007 => io::Error::from_raw_os_error((code & 0xFFFF) as i32),
            _ => io::Error::other(e),
        }
    }

    pub fn create_new(path: &Path) -> io::Result<File> {
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{})", keel_vfs::pipe::user_sid()?)
            .encode_utf16()
            .chain([0])
            .collect();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .map_err(win32)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let created = CreateFileW(
                PCWSTR(wide.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ,
                Some(&sa),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(),
            );
            let _ = LocalFree(HLOCAL(sd.0));
            let handle = created.map_err(win32)?;
            Ok(File::from(OwnedHandle::from_raw_handle(handle.0)))
        }
    }

    /// The file's owner is this user (or Administrators, for an elevated process) and its
    /// DACL is protected with exactly one entry: full access for this user.
    pub fn owner_only(file: &File) -> io::Result<bool> {
        let sid = keel_vfs::pipe::user_sid()?;
        let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let text = unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let err = GetSecurityInfo(
                HANDLE(file.as_raw_handle()),
                SE_FILE_OBJECT,
                what,
                None,
                None,
                None,
                None,
                Some(&mut sd),
            );
            if err != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(err.0 as i32));
            }
            let mut text = PWSTR::null();
            let converted = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                what,
                &mut text,
                None,
            );
            let _ = LocalFree(HLOCAL(sd.0));
            converted.map_err(win32)?;
            let s = text.to_string();
            let _ = LocalFree(HLOCAL(text.0.cast()));
            s.map_err(io::Error::other)?
        };
        Ok(owner_only_sddl(&text, &sid))
    }

    /// `O:<owner>D:<flags>(<ace>)`: owner `sid` or `BA`, flags with `P`, one ACE.
    pub(super) fn owner_only_sddl(text: &str, sid: &str) -> bool {
        let Some((owner, dacl)) = text
            .strip_prefix("O:")
            .and_then(|rest| rest.split_once("D:"))
        else {
            return false;
        };
        let Some((flags, aces)) = dacl.split_once('(') else {
            return false;
        };
        (owner == sid || owner == "BA") && flags.contains('P') && aces == format!("A;;FA;;;{sid})")
    }

    #[cfg(test)]
    #[test]
    fn parses_owner_only_descriptors() {
        let sid = "S-1-5-21-1-2-3-1001";
        for ok in [
            format!("O:{sid}D:P(A;;FA;;;{sid})"),
            format!("O:BAD:PAI(A;;FA;;;{sid})"),
        ] {
            assert!(owner_only_sddl(&ok, sid), "{ok}");
        }
        for bad in [
            format!("O:{sid}D:(A;;FA;;;{sid})"),
            format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FR;;;AU)"),
            format!("O:S-1-5-21-9D:P(A;;FA;;;{sid})"),
            format!("O:{sid}D:AI(A;ID;FA;;;{sid})(A;ID;0x1301bf;;;AU)"),
            String::new(),
        ] {
            assert!(!owner_only_sddl(&bad, sid), "{bad}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_files_are_private_and_shared_ones_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        write(&path, b"one").unwrap();
        assert_eq!(read(&path).unwrap().as_deref(), Some(&b"one"[..]));
        write(&path, b"two").unwrap();
        assert_eq!(read(&path).unwrap().as_deref(), Some(&b"two"[..]));
        assert!(create_new(&path).is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists));
        // A file made the ordinary way inherits the folder's permissions (Windows; the
        // folder grants others access) or here is opened to others (Unix).
        let shared = dir.path().join("shared.txt");
        std::fs::write(&shared, b"planted").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(read(&shared).unwrap(), None);
    }
}
