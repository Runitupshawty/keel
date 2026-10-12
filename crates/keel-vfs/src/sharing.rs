//! Sharing & Permissions (Task 67, Get Info): who owns a local file or folder and who may do
//! what with it. Windows reads the owner and the access control list (each account's
//! allowed or denied access in Explorer's words, inherited entries marked); Unix reads the
//! owner, the group and the mode bits (in Finder's words: Read & Write, Read only, ...).
//! Read only: nothing here changes a permission.

use std::{io, path::Path};

/// The owner and the access entries of one item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sharing {
    /// An account name (`DOMAIN\user` on Windows), or the raw id when it has no name.
    pub owner: Option<String>,
    /// Unix: the group.
    pub group: Option<String>,
    /// Unix: the permission bits (`0o755`, with setuid/setgid/sticky).
    pub mode: Option<u32>,
    /// One per account (Windows: allowed and denied separately), in the order the system
    /// lists them.
    pub entries: Vec<Access>,
}

/// One account's access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub who: String,
    /// "Full control", "Read & write", "Read only", ...
    pub access: String,
    /// A Windows deny entry.
    pub deny: bool,
    /// A Windows entry inherited from a parent folder.
    pub inherited: bool,
}

/// The owner and access entries of a local path.
pub fn sharing(path: &Path) -> io::Result<Sharing> {
    imp::sharing(path)
}

/// Finder's words for one rwx triplet.
pub fn unix_access(bits: u32) -> &'static str {
    match (bits & 4 != 0, bits & 2 != 0) {
        (true, true) => "Read & Write",
        (true, false) => "Read only",
        (false, true) => "Write only (Drop Box)",
        (false, false) => "No Access",
    }
}

/// Owner, group and everyone else with their access, as Finder lists them.
pub fn unix_entries(owner: &str, group: &str, mode: u32) -> Vec<Access> {
    let entry = |who: String, bits: u32| Access {
        who,
        access: unix_access(bits).to_owned(),
        deny: false,
        inherited: false,
    };
    vec![
        entry(format!("{owner} (Me)"), (mode >> 6) & 7),
        entry(group.to_owned(), (mode >> 3) & 7),
        entry("everyone".to_owned(), mode & 7),
    ]
}

/// `drwxr-xr-x`, as `ls -l` writes it (setuid, setgid and sticky included).
pub fn mode_text(mode: u32, dir: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if dir { 'd' } else { '-' });
    for (shift, special, set, unset) in [
        (6, 0o4000, 's', 'S'),
        (3, 0o2000, 's', 'S'),
        (0, 0o1000, 't', 'T'),
    ] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => set,
            (false, true) => unset,
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    s
}

// Windows access masks (winnt.h).
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const FULL: u32 = 0x001F_01FF;
const MODIFY: u32 = 0x0013_01BF;
const READ_WRITE: u32 = 0x0012_01BF;
const READ_EXECUTE: u32 = 0x0012_00A9;
const READ: u32 = 0x0012_0089;
const WRITE: u32 = 0x0010_0116;

/// Explorer's words for a file access mask (generic rights mapped to file rights first).
pub fn windows_access(mask: u32) -> &'static str {
    let mut m = mask & 0x0FFF_FFFF;
    if mask & GENERIC_ALL != 0 {
        m |= FULL;
    }
    if mask & GENERIC_READ != 0 {
        m |= READ;
    }
    if mask & GENERIC_WRITE != 0 {
        m |= WRITE;
    }
    if mask & GENERIC_EXECUTE != 0 {
        m |= 0x0012_00A0;
    }
    let has = |bits: u32| m & bits == bits;
    if has(FULL) {
        "Full control"
    } else if has(MODIFY) {
        "Modify"
    } else if has(READ_WRITE) {
        "Read & write"
    } else if has(READ_EXECUTE) {
        "Read & execute"
    } else if has(READ) {
        "Read only"
    } else if has(WRITE) {
        "Write only"
    } else {
        "Special"
    }
}

/// One ACL entry as read: (account, deny, mask, inherited).
pub type Ace = (String, bool, u32, bool);

/// The entries of an ACL with one line per account and kind (allow or deny): the masks of
/// an account's entries are combined (an explicit and an inherited one, or a folder's
/// separate entries for itself and its contents); inherited only when all of them are.
pub fn merge_aces(aces: &[Ace]) -> Vec<Access> {
    let mut merged: Vec<Ace> = Vec::new();
    for (who, deny, mask, inherited) in aces {
        match merged.iter_mut().find(|m| m.0 == *who && m.1 == *deny) {
            Some(m) => {
                m.2 |= mask;
                m.3 &= inherited;
            }
            None => merged.push((who.clone(), *deny, *mask, *inherited)),
        }
    }
    merged
        .into_iter()
        .map(|(who, deny, mask, inherited)| Access {
            who,
            access: windows_access(mask).to_owned(),
            deny,
            inherited,
        })
        .collect()
}

#[cfg(windows)]
mod imp {
    use super::{merge_aces, Access, Ace, Sharing};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::{io, path::Path};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetAce, LookupAccountSidW, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
        INHERITED_ACE, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SID_NAME_USE,
    };

    /// READ_CONTROL: enough to read the owner and the DACL.
    const READ_CONTROL: u32 = 0x0002_0000;
    /// Opens folders too.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    pub fn sharing(path: &Path) -> io::Result<Sharing> {
        let file = std::fs::OpenOptions::new()
            .access_mode(READ_CONTROL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        // SAFETY: the handle is open for READ_CONTROL; the descriptor GetSecurityInfo
        // allocates holds `owner` and `dacl` and is freed after they are read.
        unsafe {
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
            let out = read(owner, dacl);
            let _ = LocalFree(HLOCAL(sd.0));
            Ok(out)
        }
    }

    unsafe fn read(owner: PSID, dacl: *mut ACL) -> Sharing {
        let mut s = Sharing {
            owner: (!owner.0.is_null()).then(|| account(owner)),
            ..Sharing::default()
        };
        // A null DACL grants everyone everything.
        if dacl.is_null() {
            s.entries.push(Access {
                who: "Everyone".into(),
                access: "Full control".into(),
                deny: false,
                inherited: false,
            });
            return s;
        }
        let mut aces: Vec<Ace> = Vec::new();
        for i in 0..u32::from((*dacl).AceCount) {
            let mut ace = std::ptr::null_mut();
            if GetAce(dacl, i, &mut ace).is_err() {
                continue;
            }
            let header = &*(ace as *const ACE_HEADER);
            let deny = match header.AceType {
                0 => false,
                1 => true,
                // Object and callback entries: not shown.
                _ => continue,
            };
            let flags = u32::from(header.AceFlags);
            // Applies only to what the folder contains, not to the folder itself.
            if flags & INHERIT_ONLY_ACE.0 != 0 {
                continue;
            }
            // Allowed and denied entries share this layout.
            let body = &*(ace as *const ACCESS_ALLOWED_ACE);
            let sid = PSID(std::ptr::addr_of!(body.SidStart) as *mut _);
            let inherited = flags & INHERITED_ACE.0 != 0;
            aces.push((account(sid), deny, body.Mask, inherited));
        }
        s.entries = merge_aces(&aces);
        s
    }

    /// `DOMAIN\name`, the bare name for built-in accounts, else the SID as text.
    unsafe fn account(sid: PSID) -> String {
        let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        let found = LookupAccountSidW(
            PCWSTR::null(),
            sid,
            PWSTR(name.as_mut_ptr()),
            &mut name_len,
            PWSTR(domain.as_mut_ptr()),
            &mut domain_len,
            &mut kind,
        );
        if found.is_ok() {
            let name = String::from_utf16_lossy(&name[..name_len as usize]);
            let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
            return match domain.is_empty() || domain == "BUILTIN" || domain == "NT AUTHORITY" {
                true => name,
                false => format!("{domain}\\{name}"),
            };
        }
        let mut text = PWSTR::null();
        if ConvertSidToStringSidW(sid, &mut text).is_ok() {
            let s = text.to_string().unwrap_or_default();
            let _ = LocalFree(HLOCAL(text.0.cast()));
            return s;
        }
        "Unknown account".into()
    }
}

#[cfg(unix)]
mod imp {
    use super::{unix_entries, Sharing};
    use std::ffi::CStr;
    use std::os::unix::fs::MetadataExt;
    use std::{io, path::Path};

    pub fn sharing(path: &Path) -> io::Result<Sharing> {
        let m = std::fs::metadata(path)?;
        let owner = user_name(m.uid()).unwrap_or_else(|| m.uid().to_string());
        let group = group_name(m.gid()).unwrap_or_else(|| m.gid().to_string());
        let mode = m.mode() & 0o7777;
        Ok(Sharing {
            entries: unix_entries(&owner, &group, mode),
            owner: Some(owner),
            group: Some(group),
            mode: Some(mode),
        })
    }

    fn user_name(uid: libc::uid_t) -> Option<String> {
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        // SAFETY: zeroed is a valid passwd; getpwuid_r writes into it and `buf`.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let rc =
            unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc != 0 || found.is_null() || pwd.pw_name.is_null() {
            return None;
        }
        // SAFETY: a found entry's name is a NUL-terminated string in `buf`.
        let name = unsafe { CStr::from_ptr(pwd.pw_name) };
        Some(name.to_string_lossy().into_owned())
    }

    fn group_name(gid: libc::gid_t) -> Option<String> {
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        // SAFETY: as in `user_name`.
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        let rc =
            unsafe { libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc != 0 || found.is_null() || grp.gr_name.is_null() {
            return None;
        }
        // SAFETY: as in `user_name`.
        let name = unsafe { CStr::from_ptr(grp.gr_name) };
        Some(name.to_string_lossy().into_owned())
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    pub fn sharing(_: &std::path::Path) -> std::io::Result<super::Sharing> {
        Ok(super::Sharing::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_words_and_mode_text() {
        assert_eq!(unix_access(7), "Read & Write");
        assert_eq!(unix_access(5), "Read only");
        assert_eq!(unix_access(3), "Write only (Drop Box)");
        assert_eq!(unix_access(1), "No Access");
        let e = unix_entries("ana", "staff", 0o640);
        let words: Vec<(&str, &str)> = e
            .iter()
            .map(|a| (a.who.as_str(), a.access.as_str()))
            .collect();
        assert_eq!(
            words,
            [
                ("ana (Me)", "Read & Write"),
                ("staff", "Read only"),
                ("everyone", "No Access")
            ]
        );
        assert_eq!(mode_text(0o755, true), "drwxr-xr-x");
        assert_eq!(mode_text(0o644, false), "-rw-r--r--");
        assert_eq!(mode_text(0o4755, false), "-rwsr-xr-x");
        assert_eq!(mode_text(0o1777, true), "drwxrwxrwt");
        assert_eq!(mode_text(0o2640, false), "-rw-r-S---");
    }

    #[test]
    fn windows_masks_in_explorer_words() {
        assert_eq!(windows_access(0x001F_01FF), "Full control");
        assert_eq!(windows_access(GENERIC_ALL), "Full control");
        assert_eq!(windows_access(0x0013_01BF), "Modify");
        assert_eq!(windows_access(0x0012_01BF), "Read & write");
        assert_eq!(windows_access(0x0012_00A9), "Read & execute");
        assert_eq!(
            windows_access(GENERIC_READ | GENERIC_EXECUTE),
            "Read & execute"
        );
        assert_eq!(windows_access(0x0012_0089), "Read only");
        assert_eq!(windows_access(GENERIC_READ), "Read only");
        assert_eq!(windows_access(0x0010_0116), "Write only");
        assert_eq!(windows_access(0x0000_0001), "Special");
    }

    #[test]
    fn aces_merge_per_account_and_kind() {
        let aces: Vec<Ace> = vec![
            ("SYSTEM".into(), false, 0x001F_01FF, true),
            ("PC\\ana".into(), false, 0x0012_0089, false),
            ("PC\\ana".into(), false, 0x0010_0116 | 0x0001_0040, true),
            ("Users".into(), true, 0x0010_0116, false),
            ("Users".into(), false, 0x0012_00A9, true),
        ];
        let m = merge_aces(&aces);
        let rows: Vec<(&str, &str, bool, bool)> = m
            .iter()
            .map(|a| (a.who.as_str(), a.access.as_str(), a.deny, a.inherited))
            .collect();
        assert_eq!(
            rows,
            [
                ("SYSTEM", "Full control", false, true),
                ("PC\\ana", "Modify", false, false),
                ("Users", "Write only", true, false),
                ("Users", "Read & execute", false, true),
            ]
        );
    }

    #[test]
    fn a_temp_file_has_an_owner_and_entries() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.txt");
        std::fs::write(&file, "x").unwrap();
        for p in [file.as_path(), dir.path()] {
            let s = sharing(p).unwrap();
            assert!(s.owner.as_deref().is_some_and(|o| !o.is_empty()), "{s:?}");
            assert!(!s.entries.is_empty(), "{s:?}");
            if cfg!(unix) {
                assert!(s.mode.is_some() && s.group.is_some(), "{s:?}");
            }
        }
        assert!(sharing(&dir.path().join("missing")).is_err());
    }
}
