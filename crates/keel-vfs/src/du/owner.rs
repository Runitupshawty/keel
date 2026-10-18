//! Item owners for the Disk Usage window's Owner column, read lazily (only for the rows
//! shown) and named through a cache: Windows reads the owner SID (`READ_CONTROL` only, no
//! recall, a final link itself) and asks `LookupAccountSidW` once per SID; Unix takes the
//! user id from the item's metadata and asks `getpwuid_r` once per id. A domain lookup can
//! take seconds, so the cache is what keeps a long column fast.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Names by SID or user id, each looked up once (also the ones that did not resolve).
#[derive(Default)]
pub struct NameCache {
    names: Mutex<HashMap<String, Option<String>>>,
    lookups: AtomicU64,
}

impl NameCache {
    /// The name for `key`, from the cache or from `lookup` (called at most once per key).
    pub fn get(&self, key: &str, lookup: impl FnOnce() -> Option<String>) -> Option<String> {
        if let Some(n) = self.names.lock().get(key) {
            return n.clone();
        }
        self.lookups.fetch_add(1, Ordering::Relaxed);
        let name = lookup();
        self.names.lock().insert(key.to_owned(), name.clone());
        name
    }

    /// Lookups made so far (cache misses).
    pub fn lookups(&self) -> u64 {
        self.lookups.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.names.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Reads owners; share one per scan (or longer: account names rarely change).
#[derive(Default)]
pub struct Owners {
    pub cache: NameCache,
}

impl Owners {
    pub fn new() -> Self {
        Self::default()
    }

    /// `DOMAIN\user` (Windows) or the login name (Unix) owning `path`; the SID or the user
    /// id when it has no name; None when it cannot be read.
    pub fn owner(&self, path: &Path) -> Option<String> {
        let (key, id) = owner_id(path)?;
        Some(self.cache.get(&key, || account(&id)).unwrap_or(key))
    }
}

#[cfg(unix)]
fn owner_id(path: &Path) -> Option<(String, u32)> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::symlink_metadata(path).ok()?.uid();
    Some((uid.to_string(), uid))
}

#[cfg(unix)]
fn account(uid: &u32) -> Option<String> {
    super::permissions::unix::user(*uid)
}

/// The owner's SID as text and as bytes.
#[cfg(windows)]
fn owner_id(path: &Path) -> Option<(String, Vec<u8>)> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetLengthSid, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };
    // READ_CONTROL; folders, a link itself, no recall (as the Permissions tab).
    let file = std::fs::OpenOptions::new()
        .access_mode(0x0002_0000)
        .share_mode(7)
        .custom_flags(0x0200_0000 | 0x0020_0000 | 0x0010_0000)
        .open(path)
        .ok()?;
    struct Local(*mut std::ffi::c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            // SAFETY: memory LocalAlloc'ed by the API that returned it, freed once.
            unsafe {
                let _ = LocalFree(HLOCAL(self.0));
            }
        }
    }
    // SAFETY: GetSecurityInfo allocates one descriptor (freed by `_sd`); `owner` points
    // inside it and is checked before it is read; the SID text is freed by `_text`.
    unsafe {
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let err = GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        );
        if err != ERROR_SUCCESS {
            return None;
        }
        let _sd = Local(sd.0);
        if owner.0.is_null() || !IsValidSid(owner).as_bool() {
            return None;
        }
        let len = GetLengthSid(owner) as usize;
        let bytes = std::slice::from_raw_parts(owner.0 as *const u8, len).to_vec();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(owner, &mut text).ok()?;
        let _text = Local(text.0.cast());
        Some((text.to_string().ok()?, bytes))
    }
}

/// `DOMAIN\user` for a SID's bytes.
#[cfg(windows)]
fn account(sid: &[u8]) -> Option<String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Security::{LookupAccountSidW, PSID, SID_NAME_USE};
    let mut sid = sid.to_vec();
    let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
    let (mut n, mut d) = (name.len() as u32, domain.len() as u32);
    let mut use_ = SID_NAME_USE::default();
    // SAFETY: `sid` holds a whole valid SID (copied from the descriptor); the buffers
    // match their lengths.
    unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            PSID(sid.as_mut_ptr().cast()),
            PWSTR(name.as_mut_ptr()),
            &mut n,
            PWSTR(domain.as_mut_ptr()),
            &mut d,
            &mut use_,
        )
        .ok()?;
    }
    let name = String::from_utf16_lossy(name.get(..n as usize)?);
    let domain = String::from_utf16_lossy(domain.get(..d as usize)?);
    Some(if domain.is_empty() {
        name
    } else {
        format!("{domain}\\{name}")
    })
}

#[cfg(not(any(windows, unix)))]
fn owner_id(_: &Path) -> Option<(String, ())> {
    None
}

#[cfg(not(any(windows, unix)))]
fn account(_: &()) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_looked_up_once_per_id() {
        let c = NameCache::default();
        let mut calls = 0;
        for _ in 0..3 {
            let n = c.get("S-1-5-18", || {
                calls += 1;
                Some("NT AUTHORITY\\SYSTEM".into())
            });
            assert_eq!(n.as_deref(), Some("NT AUTHORITY\\SYSTEM"));
        }
        // An id without a name is remembered too.
        for _ in 0..2 {
            assert_eq!(
                c.get("1234567", || {
                    calls += 1;
                    None
                }),
                None
            );
        }
        assert_eq!((calls, c.lookups(), c.len()), (2, 2, 2));
    }

    /// Two files of one owner: one name lookup.
    #[test]
    fn owners_of_one_account_share_a_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let owners = Owners::new();
        let first = owners.owner(&a);
        if cfg!(any(windows, unix)) {
            assert!(first.as_deref().is_some_and(|o| !o.is_empty()), "{first:?}");
        }
        assert_eq!(owners.owner(&b), first);
        assert_eq!(owners.owner(tmp.path()), first, "the folder is ours too");
        assert_eq!(owners.cache.lookups(), u64::from(first.is_some()));
        assert_eq!(owners.owner(&tmp.path().join("missing")), None);
    }
}
