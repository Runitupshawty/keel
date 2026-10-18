//! Digital signatures (`digital-signature-name:`, `digital-signature-timestamp:`) through
//! `WinVerifyTrust`, as Explorer's Digital Signatures tab and `signtool verify` read them:
//! the file's own (embedded) Authenticode signature, else the system catalog that signs
//! it (most of Windows' own files). The signer is reported whether or not its chain is
//! trusted on this machine; revocation is not checked and nothing is fetched from the
//! network.

use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::SystemTime;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{BOOL, FILETIME, HANDLE, HWND};
use windows::Win32::Security::Cryptography::Catalog::{
    CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2,
    CryptCATAdminEnumCatalogFromHash, CryptCATAdminReleaseCatalogContext,
    CryptCATAdminReleaseContext, CryptCATCatalogInfoFromContext, CATALOG_INFO,
};
use windows::Win32::Security::Cryptography::{
    CertGetNameStringW, BCRYPT_SHA256_ALGORITHM, CERT_NAME_SIMPLE_DISPLAY_TYPE,
};
use windows::Win32::Security::WinTrust::{
    WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA,
    WINTRUST_DATA_0, WINTRUST_DATA_PROVIDER_FLAGS, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL,
    WTD_CHOICE_CATALOG, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};

use crate::value::filetime_to_system_time;

/// Who signed a file and when the signature was timestamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    /// The signing certificate's display name (`Microsoft Windows`).
    pub name: String,
    /// The countersignature's time; `None` for a signature without a timestamp.
    pub timestamp: Option<SystemTime>,
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn time_of(ft: FILETIME) -> Option<SystemTime> {
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    (ticks != 0).then(|| filetime_to_system_time(ticks)).flatten()
}

/// The signer of `path`: its embedded signature, else its catalog's; `None` when it is
/// not signed.
pub fn read(path: &Path) -> std::io::Result<Option<Signer>> {
    let file = std::fs::File::open(path)?;
    let path_w = wide(path.as_os_str());
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path_w.as_ptr()),
        hFile: HANDLE(file.as_raw_handle() as _),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let union = WINTRUST_DATA_0 {
        pFile: &mut info,
    };
    // SAFETY: `info` and `path_w` outlive the call; `verify` closes its state.
    if let Some(s) = unsafe { verify(WTD_CHOICE_FILE, union) } {
        return Ok(Some(s));
    }
    // SAFETY: as above, with the open file's handle.
    Ok(unsafe { from_catalog(&file, &path_w) })
}

/// Runs `WinVerifyTrust` on what `union` names and reads the first signer; closes the
/// verification state again.
unsafe fn verify(
    choice: windows::Win32::Security::WinTrust::WINTRUST_DATA_UNION_CHOICE,
    union: WINTRUST_DATA_0,
) -> Option<Signer> {
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: choice,
        Anonymous: union,
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WINTRUST_DATA_PROVIDER_FLAGS(
            WTD_CACHE_ONLY_URL_RETRIEVAL.0 | WTD_REVOCATION_CHECK_NONE.0,
        ),
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // The result says whether the chain is trusted; the signer is read either way.
    let _status = WinVerifyTrust(
        HWND::default(),
        &mut action,
        (&mut data as *mut WINTRUST_DATA).cast(),
    );
    let signer = if data.hWVTStateData.is_invalid() {
        None
    } else {
        signer_of(data.hWVTStateData)
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    let _ = WinVerifyTrust(
        HWND::default(),
        &mut action,
        (&mut data as *mut WINTRUST_DATA).cast(),
    );
    signer
}

/// The first signer in a verification state: its certificate's name and the first
/// countersignature's time.
unsafe fn signer_of(state: HANDLE) -> Option<Signer> {
    let prov = WTHelperProvDataFromStateData(state);
    if prov.is_null() {
        return None;
    }
    let sgnr = WTHelperGetProvSignerFromChain(prov, 0, BOOL(0), 0);
    if sgnr.is_null() {
        return None;
    }
    let cert = WTHelperGetProvCertFromChain(sgnr, 0);
    if cert.is_null() || (*cert).pCert.is_null() {
        return None;
    }
    let mut buf = [0u16; 512];
    let n = CertGetNameStringW(
        (*cert).pCert,
        CERT_NAME_SIMPLE_DISPLAY_TYPE,
        0,
        None,
        Some(&mut buf[..]),
    ) as usize;
    // `n` counts the terminating NUL.
    let name = String::from_utf16_lossy(&buf[..n.saturating_sub(1).min(buf.len())]);
    if name.trim().is_empty() {
        return None;
    }
    let s = &*sgnr;
    let timestamp = if s.csCounterSigners > 0 && !s.pasCounterSigners.is_null() {
        time_of((*s.pasCounterSigners).sftVerifyAsOf)
    } else {
        None
    };
    Some(Signer {
        name: name.trim().to_owned(),
        timestamp,
    })
}

/// The signer of the system catalog that holds the file's hash, if one does.
unsafe fn from_catalog(file: &std::fs::File, path_w: &[u16]) -> Option<Signer> {
    let mut admin: isize = 0;
    CryptCATAdminAcquireContext2(&mut admin, None, BCRYPT_SHA256_ALGORITHM, None, 0).ok()?;
    let handle = HANDLE(file.as_raw_handle() as _);
    let result = (|| {
        let mut len: u32 = 0;
        // The first call asks for the hash's length.
        let _ = CryptCATAdminCalcHashFromFileHandle2(admin, handle, &mut len, None, 0);
        if len == 0 || len > 256 {
            return None;
        }
        let mut hash = vec![0u8; len as usize];
        CryptCATAdminCalcHashFromFileHandle2(admin, handle, &mut len, Some(hash.as_mut_ptr()), 0)
            .ok()?;
        hash.truncate(len as usize);
        let catalog = CryptCATAdminEnumCatalogFromHash(admin, &hash, 0, None);
        if catalog == 0 {
            return None;
        }
        let mut cat = CATALOG_INFO {
            cbStruct: std::mem::size_of::<CATALOG_INFO>() as u32,
            wszCatalogFile: [0; 260],
        };
        let signer = if CryptCATCatalogInfoFromContext(catalog, &mut cat, 0).is_ok() {
            let tag: Vec<u16> = hash
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<String>()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut info = WINTRUST_CATALOG_INFO {
                cbStruct: std::mem::size_of::<WINTRUST_CATALOG_INFO>() as u32,
                dwCatalogVersion: 0,
                pcwszCatalogFilePath: PCWSTR(cat.wszCatalogFile.as_ptr()),
                pcwszMemberTag: PCWSTR(tag.as_ptr()),
                pcwszMemberFilePath: PCWSTR(path_w.as_ptr()),
                hMemberFile: handle,
                pbCalculatedFileHash: hash.as_mut_ptr(),
                cbCalculatedFileHash: hash.len() as u32,
                pcCatalogContext: std::ptr::null_mut(),
                hCatAdmin: admin,
            };
            verify(
                WTD_CHOICE_CATALOG,
                WINTRUST_DATA_0 {
                    pCatalog: &mut info,
                },
            )
        } else {
            None
        };
        let _ = CryptCATAdminReleaseCatalogContext(admin, catalog, 0);
        signer
    })();
    let _ = CryptCATAdminReleaseContext(admin, 0);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system32(name: &str) -> Option<std::path::PathBuf> {
        let root = std::env::var_os("SystemRoot")?;
        let p = Path::new(&root).join("System32").join(name);
        p.is_file().then_some(p)
    }

    #[test]
    fn an_unsigned_file_has_no_signer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plain.exe");
        std::fs::write(&p, b"MZ not really a program").unwrap();
        assert_eq!(read(&p).unwrap(), None);
        let t = dir.path().join("notes.txt");
        std::fs::write(&t, b"hello").unwrap();
        assert_eq!(read(&t).unwrap(), None);
        assert!(read(&dir.path().join("missing.exe")).is_err());
    }

    /// Windows' own programs are signed by their catalog (`Microsoft Windows`); a few
    /// carry their own signature. Either way a signer and a timestamp come back.
    #[test]
    fn windows_programs_have_a_signer() {
        let notepad = system32("notepad.exe").expect("notepad.exe in System32");
        let s = read(&notepad).unwrap().expect("notepad.exe is signed");
        assert!(s.name.contains("Microsoft"), "{s:?}");
        // Embedded signatures, when one of these is installed.
        for name in ["MpSigStub.exe", "mrt.exe"] {
            if let Some(p) = system32(name) {
                let s = read(&p).unwrap().expect("signed");
                assert!(s.name.contains("Microsoft"), "{name}: {s:?}");
                assert!(s.timestamp.is_some(), "{name}: {s:?}");
            }
        }
    }
}
