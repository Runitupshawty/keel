//! Files only this user may read or write: the plan store (`api-plans.json`), the
//! WebSocket token, the socket-name salt; and the data folder. Windows: a protected DACL
//! with one entry (the user's SID, full access; inherited by a folder's contents), set as
//! the file or folder is created; Unix: mode 0600 (0700 for folders). `read` refuses
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

/// Creates `dir` and its missing parents; `dir` itself (when this creates it) is for this
/// user only, and on Windows what is created inside inherits that. An existing folder is
/// left as it is.
pub fn create_dir_all(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    };
    #[cfg(windows)]
    let created = win::create_dir(dir);
    match created {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        r => r,
    }
}

/// `dir` is a folder only this user may access (an owner-only DACL its contents inherit
/// on Windows, mode 0700 on Unix).
pub fn is_private_dir(dir: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(dir)?;
        Ok(meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS: needed to open a folder.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0200_0000)
            .open(dir)?;
        Ok(f.metadata()?.is_dir() && win::owner_only(&f, true)?)
    }
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
    Ok(file.metadata()?.is_file() && win::owner_only(file, false)?)
}

#[cfg(windows)]
mod win {
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        LocalFree, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        EqualSid, GetAce, GetSecurityDescriptorControl, GetTokenInformation, IsWellKnownSid,
        TokenOwner, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE,
        ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, INHERITED_ACE,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, SE_DACL_PROTECTED, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn win32(e: windows::core::Error) -> io::Error {
        let code = e.code().0 as u32;
        match code >> 16 {
            0x8007 => io::Error::from_raw_os_error((code & 0xFFFF) as i32),
            _ => io::Error::other(e),
        }
    }

    /// Runs `f` with security attributes holding `D:P(A;<flags>;FA;;;<user>)`.
    fn with_owner_only<T>(
        flags: &str,
        f: impl FnOnce(&SECURITY_ATTRIBUTES) -> io::Result<T>,
    ) -> io::Result<T> {
        let sddl: Vec<u16> = format!("D:P(A;{flags};FA;;;{})", keel_vfs::pipe::user_sid()?)
            .encode_utf16()
            .chain([0])
            .collect();
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
            let out = f(&sa);
            let _ = LocalFree(HLOCAL(sd.0));
            out
        }
    }

    pub fn create_dir(dir: &Path) -> io::Result<()> {
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        with_owner_only("OICI", |sa| unsafe {
            CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(sa)).map_err(win32)
        })
    }

    pub fn create_new(path: &Path) -> io::Result<File> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        with_owner_only("", |sa| unsafe {
            let handle = CreateFileW(
                PCWSTR(wide.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ,
                Some(sa),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(),
            )
            .map_err(win32)?;
            Ok(File::from(OwnedHandle::from_raw_handle(handle.0)))
        })
    }

    /// The process token's user and default owner SIDs (the owner is Administrators for an
    /// elevated process, or for an administrator account without UAC), as raw bytes.
    fn token_sids() -> io::Result<(Vec<u64>, Vec<u64>)> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(win32)?;
            let token = OwnedHandle::from_raw_handle(token.0);
            let token = HANDLE(token.as_raw_handle());
            let info = |class| -> io::Result<Vec<u64>> {
                let mut len = 0;
                // Sizing call: fails with ERROR_INSUFFICIENT_BUFFER and sets `len`.
                let _ = GetTokenInformation(token, class, None, 0, &mut len);
                // u64s: the structures hold pointers, so the buffer must be aligned.
                let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
                GetTokenInformation(token, class, Some(buf.as_mut_ptr().cast()), len, &mut len)
                    .map_err(win32)?;
                Ok(buf)
            };
            Ok((info(TokenUser)?, info(TokenOwner)?))
        }
    }

    /// Whether `file` is this user's alone: owned by the token's user, its default owner
    /// or Administrators; a protected DACL (nothing inherited from the folder) whose
    /// allow entries name only the user, the token's owner, the file's owner, SYSTEM or
    /// Administrators (`dir`: each one inherited by the folder's contents). SIDs are
    /// compared as SIDs, never as SDDL text (which writes some accounts as aliases).
    pub fn owner_only(file: &File, dir: bool) -> io::Result<bool> {
        let (user_buf, owner_buf) = token_sids()?;
        unsafe {
            let user = (*(user_buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            let token_owner = (*(owner_buf.as_ptr() as *const TOKEN_OWNER)).Owner;
            let mut owner = PSID::default();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let err = GetSecurityInfo(
                HANDLE(file.as_raw_handle()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut dacl),
                None,
                Some(&mut sd),
            );
            if err != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(err.0 as i32));
            }
            let ok = check(sd, owner, dacl, user, token_owner, dir);
            let _ = LocalFree(HLOCAL(sd.0));
            ok
        }
    }

    unsafe fn check(
        sd: PSECURITY_DESCRIPTOR,
        owner: PSID,
        dacl: *mut ACL,
        user: PSID,
        token_owner: PSID,
        dir: bool,
    ) -> io::Result<bool> {
        let is = |a: PSID, b: PSID| EqualSid(a, b).is_ok();
        let admins = |s: PSID| IsWellKnownSid(s, WinBuiltinAdministratorsSid).as_bool();
        if !(is(owner, user) || is(owner, token_owner) || admins(owner)) {
            return Ok(false);
        }
        // A null DACL grants everyone everything.
        if dacl.is_null() {
            return Ok(false);
        }
        let (mut control, mut revision) = (0u16, 0u32);
        GetSecurityDescriptorControl(sd, &mut control, &mut revision).map_err(win32)?;
        if control & SE_DACL_PROTECTED.0 == 0 {
            return Ok(false);
        }
        let mut allows = 0;
        for i in 0..u32::from((*dacl).AceCount) {
            let mut ace = std::ptr::null_mut();
            GetAce(dacl, i, &mut ace).map_err(win32)?;
            let header = &*(ace as *const ACE_HEADER);
            match header.AceType {
                // Deny entries only take access away.
                1 => continue,
                0 => {}
                // Object and callback entries: not something Keel writes.
                _ => return Ok(false),
            }
            let flags = u32::from(header.AceFlags);
            let inherit = OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0;
            if flags & INHERITED_ACE.0 != 0 || (dir && flags & inherit != inherit) {
                return Ok(false);
            }
            let sid =
                PSID(std::ptr::addr_of!((*(ace as *const ACCESS_ALLOWED_ACE)).SidStart) as *mut _);
            let trusted = is(sid, user)
                || is(sid, token_owner)
                || is(sid, owner)
                || admins(sid)
                || IsWellKnownSid(sid, WinLocalSystemSid).as_bool();
            if !trusted {
                return Ok(false);
            }
            allows += 1;
        }
        Ok(allows > 0)
    }

    #[cfg(test)]
    #[test]
    fn an_administrators_owned_file_is_still_private() {
        use windows::Win32::Security::Authorization::SetNamedSecurityInfoW;
        use windows::Win32::Security::{CreateWellKnownSid, SECURITY_MAX_SID_SIZE};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin-owned.txt");
        drop(create_new(&path).unwrap());
        let mut buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let admins = PSID(buf.as_mut_ptr().cast());
        let mut len = SECURITY_MAX_SID_SIZE;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let set = unsafe {
            CreateWellKnownSid(WinBuiltinAdministratorsSid, None, admins, &mut len).unwrap();
            SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                admins,
                None,
                None,
                None,
            )
        };
        if set != ERROR_SUCCESS {
            eprintln!("not elevated: cannot give the file to Administrators; skipped");
            return;
        }
        assert!(super::read(&path).unwrap().is_some());
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
        // A file made the ordinary way inherits the folder's permissions (Windows: not a
        // protected DACL) or here is opened to others (Unix).
        let shared = dir.path().join("shared.txt");
        std::fs::write(&shared, b"planted").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(read(&shared).unwrap(), None);
    }

    #[test]
    fn created_folders_are_private_and_existing_ones_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("a").join("data");
        create_dir_all(&data).unwrap();
        assert!(is_private_dir(&data).unwrap());
        create_dir_all(&data).unwrap();
        // An ordinary folder (here the temp folder's permissions) is not private.
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(!is_private_dir(&plain).unwrap());
        create_dir_all(&plain).unwrap();
        assert!(!is_private_dir(&plain).unwrap(), "left as it was");
    }
}
