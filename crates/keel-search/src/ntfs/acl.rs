//! Review focus 2: the index never shows a file the user could not list in Explorer.
//! A volume index read from the MFT (elevated, or by the index service) holds every
//! name on the volume, including those in other users' profiles and in system
//! folders. Before results are shown, each hit's folders, from the drive root down,
//! are listed (`FindFirstFileExW`) as the user Keel runs as; a hit under a
//! folder the user may not list is left out. Verdicts are cached per folder for
//! [`TTL`]. On by default; Settings → General turns it off
//! ([`set_hide_unlistable`]).
//!
//! This is what Explorer would show, not a security boundary of its own: the service
//! only answers the user who installed it, who was an administrator at that time
//! (`docs/index-service.md`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::core::HSTRING;
use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FIND_FIRST_EX_FLAGS,
    WIN32_FIND_DATAW,
};

use super::index::Found;

static HIDE: AtomicBool = AtomicBool::new(true);

/// Hide index entries in folders this user cannot list (on by default).
pub fn set_hide_unlistable(on: bool) {
    HIDE.store(on, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    HIDE.load(Ordering::Relaxed)
}

/// How long a folder's verdict is kept.
const TTL: Duration = Duration::from_secs(300);
/// Most folders cached; past it the cache starts over.
const MAX_CACHED: usize = 100_000;

/// Which folders this user may list, cached (see the module docs).
pub(crate) struct FolderAccess {
    check: fn(&str) -> bool,
    cache: Mutex<HashMap<String, (bool, Instant)>>,
}

impl FolderAccess {
    /// `check` answers for one folder ([`can_list`]; a fake in tests).
    pub fn new(check: fn(&str) -> bool) -> Self {
        Self {
            check,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Whether every folder above `path` can be listed.
    pub fn visible(&self, path: &str) -> bool {
        let mut cache = self.cache.lock();
        let now = Instant::now();
        for folder in ancestors(path) {
            let key = folder.to_lowercase();
            let ok = match cache.get(&key) {
                Some(&(ok, at)) if now.duration_since(at) < TTL => ok,
                _ => {
                    let ok = (self.check)(folder);
                    if cache.len() >= MAX_CACHED {
                        cache.clear();
                    }
                    cache.insert(key, (ok, now));
                    ok
                }
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// Keeps the first `max` visible hits of `found` (in order) and drops the rest;
    /// hits past the first `max` visible ones are not checked.
    pub fn retain(&self, found: &mut Vec<Found>, max: usize) {
        let mut kept = 0;
        found.retain(|f| {
            if kept >= max || !self.visible(&f.path) {
                return false;
            }
            kept += 1;
            true
        });
    }
}

/// The folders above `path`, from the drive root down: `C:\a\b\c.txt` gives `C:\`,
/// `C:\a`, `C:\a\b`.
pub(crate) fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let path = path.trim_end_matches('\\');
    path.match_indices('\\').map(move |(i, _)| {
        let folder = &path[..i];
        // "C:" alone would mean the current folder on C:, not its root.
        if folder.ends_with(':') {
            &path[..=i]
        } else {
            folder
        }
    })
}

/// Whether this process may list `folder`: the first listing batch succeeds. Only
/// "access denied" hides; a folder that is gone (a stale entry), a drive root with no
/// entries, or any other error does not. Listing (not opening with backup semantics)
/// is what Explorer does, and it never uses a backup privilege an elevated token may
/// hold, so an administrator is held to the folder's ACL too.
pub(crate) fn can_list(folder: &str) -> bool {
    let base = folder.trim_end_matches('\\');
    let pattern = if folder.starts_with(r"\\") {
        format!(r"{base}\*")
    } else {
        format!(r"\\?\{base}\*")
    };
    let mut found = WIN32_FIND_DATAW::default();
    // SAFETY: the name outlives the call; `found` is the WIN32_FIND_DATAW that
    // `FindExInfoBasic` fills.
    let opened = unsafe {
        FindFirstFileExW(
            &HSTRING::from(pattern),
            FindExInfoBasic,
            std::ptr::from_mut(&mut found).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_FLAGS(0),
        )
    };
    match opened {
        Ok(handle) => {
            // SAFETY: opened above.
            let _ = unsafe { FindClose(handle) };
            true
        }
        Err(e) => e.code() != ERROR_ACCESS_DENIED.to_hresult(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// For this thread only, a copy of the process token with every privilege
    /// disabled. An administrator's enabled backup privilege (a build or SSH session
    /// has it on) would otherwise let it list any folder, which is not what a user's
    /// Keel runs with.
    struct NoPrivileges(windows::Win32::Foundation::HANDLE);

    impl NoPrivileges {
        fn enter() -> Self {
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::Security::{
                AdjustTokenPrivileges, ImpersonateSelf, SecurityImpersonation,
                TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
            };
            use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
            let mut token = HANDLE::default();
            // SAFETY: plain Win32 calls on this thread's own token; the handle is
            // closed in `drop`.
            unsafe {
                ImpersonateSelf(SecurityImpersonation).unwrap();
                OpenThreadToken(
                    GetCurrentThread(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    true,
                    &mut token,
                )
                .unwrap();
                AdjustTokenPrivileges(token, true, None, 0, None, None).unwrap();
            }
            Self(token)
        }
    }

    impl Drop for NoPrivileges {
        fn drop(&mut self) {
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::Security::RevertToSelf;
            // SAFETY: undoes `enter` on the same thread.
            unsafe {
                let _ = RevertToSelf();
                let _ = CloseHandle(self.0);
            }
        }
    }

    #[test]
    fn ancestors_from_the_root_down() {
        let all: Vec<&str> = ancestors(r"C:\a\b\c.txt").collect();
        assert_eq!(all, [r"C:\", r"C:\a", r"C:\a\b"]);
        assert_eq!(ancestors(r"C:\a").collect::<Vec<_>>(), [r"C:\"]);
        assert_eq!(ancestors(r"C:\a\b\").collect::<Vec<_>>(), [r"C:\", r"C:\a"]);
        assert!(ancestors(r"C:\").next().is_none());
    }

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn fake(folder: &str) -> bool {
        CALLS.fetch_add(1, Ordering::Relaxed);
        !folder.to_lowercase().starts_with(r"c:\users\other")
    }

    fn found(path: &str) -> Found {
        Found {
            quality: 0,
            path: path.into(),
            is_dir: false,
        }
    }

    #[test]
    fn hides_entries_below_a_folder_the_user_cannot_list_and_caches() {
        let access = FolderAccess::new(fake);
        let before = CALLS.load(Ordering::Relaxed);
        let mut hits = vec![
            found(r"C:\Users\me\a.txt"),
            found(r"C:\Users\Other\secret.txt"),
            found(r"C:\Users\other\Desktop\deep.txt"),
            // The folder itself shows (in a listable parent); its contents do not.
            found(r"C:\Users\Other"),
            found(r"C:\Users\me\b.txt"),
        ];
        access.retain(&mut hits, 10);
        let paths: Vec<&str> = hits.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                r"C:\Users\me\a.txt",
                r"C:\Users\Other",
                r"C:\Users\me\b.txt"
            ]
        );
        // C:\, C:\Users, C:\Users\me, C:\Users\Other: once each (case-insensitive);
        // the denied folder stops the walk before C:\Users\other\Desktop.
        assert_eq!(CALLS.load(Ordering::Relaxed) - before, 4);

        // At most `max` kept, and nothing past them checked.
        let mut hits = vec![found(r"C:\x\1"), found(r"C:\y\2"), found(r"C:\z\3")];
        let before = CALLS.load(Ordering::Relaxed);
        access.retain(&mut hits, 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(CALLS.load(Ordering::Relaxed) - before, 1, "only C:\\x");
    }

    #[test]
    fn the_switch_defaults_on() {
        assert!(enabled());
    }

    /// A real folder whose DACL grants this user nothing is hidden; an ordinary one
    /// is not; a missing one is not (a stale entry).
    #[test]
    fn can_list_reads_the_folder_acl() {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{
            SetFileSecurityW, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        };
        use windows::Win32::Storage::FileSystem::CreateDirectoryW;

        fn with_sd<T>(sddl: &str, f: impl FnOnce(PSECURITY_DESCRIPTOR) -> T) -> T {
            let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            // SAFETY: a NUL-terminated string; `sd` is freed below.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(wide.as_ptr()),
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                )
            }
            .unwrap();
            let out = f(sd);
            // SAFETY: allocated by the conversion above.
            unsafe {
                let _ = LocalFree(HLOCAL(sd.0));
            }
            out
        }

        let base = std::env::temp_dir().join(format!("keel-acl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("open")).unwrap();
        let locked = base.join("locked");
        let name = HSTRING::from(locked.as_os_str());
        // Only SYSTEM: this user (even an elevated administrator, who has not
        // enabled the backup privilege) may not list it.
        with_sd("D:P(A;OICI;FA;;;SY)", |sd| {
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            // SAFETY: the name and attributes outlive the call.
            unsafe { CreateDirectoryW(&name, Some(&sa)) }.unwrap();
        });
        let unprivileged = NoPrivileges::enter();
        let s = |p: &std::path::Path| p.to_str().unwrap().to_owned();
        let denied = !can_list(&s(&locked));
        let open = can_list(&s(&base.join("open")));
        let missing = can_list(&s(&base.join("missing")));
        let access = FolderAccess::new(can_list);
        let hidden = !access.visible(&format!(r"{}\file.txt", s(&locked)));
        let shown = access.visible(&format!(r"{}\file.txt", s(&base.join("open"))));
        drop(unprivileged);
        // Give the folder back to this user (the owner may always rewrite the DACL)
        // so it can be removed.
        let me = keel_vfs::pipe::user_sid().unwrap();
        let _ = with_sd(&format!("D:P(A;OICI;FA;;;{me})"), |sd| {
            // SAFETY: the name and descriptor outlive the call.
            unsafe { SetFileSecurityW(&name, DACL_SECURITY_INFORMATION, sd) }
        });
        let _ = std::fs::remove_dir_all(&base);
        assert!(denied, "a folder only SYSTEM may list is hidden");
        assert!(open);
        assert!(missing, "a folder that is gone does not hide");
        assert!(hidden && shown);
    }
}
