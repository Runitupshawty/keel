//! Whether the running binary is code-signed, for `keel --version` and crash reports: on
//! Windows `WinVerifyTrust` checks the embedded Authenticode signature (no revocation check,
//! no network); on macOS the Mach-O code signature is read, not verified (a certificate
//! signature, or the ad-hoc one the linker adds, which is not signed in any useful sense;
//! `codesign --verify` checks the hashes and the certificate). Linux binaries carry
//! no signature: `SHA256SUMS.asc` signs the downloads instead (docs/releasing.md).

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signature {
    /// Windows: an Authenticode signature Windows trusts.
    Signed,
    /// macOS: a certificate signature is there (a non-empty CMS blob: Developer ID in a
    /// release; `codesign -dvv` names the signer), but neither it nor the code hashes are
    /// checked.
    SignedNotVerified,
    /// Windows: a signature Windows does not trust (its certificate, chain, digest or
    /// timestamp); the `WinVerifyTrust` HRESULT.
    Untrusted(i32),
    /// macOS: only the linker's or `codesign -s -` ad-hoc signature.
    AdHoc,
    Unsigned,
    /// Linux.
    NotApplicable,
    /// The binary could not be read, parsed or checked.
    Unknown,
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signed => f.write_str("signed"),
            Self::SignedNotVerified => f.write_str("signed (not verified)"),
            Self::Untrusted(hr) => write!(f, "signed, not trusted (0x{hr:08X})"),
            Self::AdHoc => f.write_str("unsigned (ad-hoc)"),
            Self::Unsigned => f.write_str("unsigned"),
            Self::NotApplicable => f.write_str("not signed (Linux)"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// The running binary's signature, read once per process.
pub fn current() -> Signature {
    static CURRENT: OnceLock<Signature> = OnceLock::new();
    *CURRENT.get_or_init(|| match std::env::current_exe() {
        Ok(exe) => of_file(&exe),
        Err(_) => Signature::Unknown,
    })
}

#[cfg(windows)]
fn of_file(path: &std::path::Path) -> Signature {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Security::WinTrust::{
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
        WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE,
        WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data`, `file` and `wide` outlive both calls; the second call releases the
    // state the first one allocated.
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            std::ptr::addr_of_mut!(data).cast(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            std::ptr::addr_of_mut!(data).cast(),
        )
    };
    from_win_verify_trust(status)
}

/// `WinVerifyTrust`'s answer. Only a signature that is there but not trusted is
/// `Untrusted`; a file Windows could not check at all (not a PE file, unreadable, no
/// provider: `TRUST_E_SUBJECT_FORM_UNKNOWN`, `CRYPT_E_FILE_ERROR`, ...) is `Unknown`.
#[cfg_attr(not(windows), allow(dead_code))]
fn from_win_verify_trust(status: i32) -> Signature {
    const TRUST_E_NOSIGNATURE: u32 = 0x800B_0100;
    const TRUST_E_SUBJECT_NOT_TRUSTED: u32 = 0x800B_0004;
    const CRYPT_E_SECURITY_SETTINGS: u32 = 0x8009_2026;
    match status as u32 {
        0 => Signature::Signed,
        TRUST_E_NOSIGNATURE => Signature::Unsigned,
        // CERT_E_* (expired, revoked, untrusted root, chaining...), TRUST_E_EXPLICIT_DISTRUST.
        0x800B_0101..=0x800B_01FF
        | TRUST_E_SUBJECT_NOT_TRUSTED
        | CRYPT_E_SECURITY_SETTINGS
        // TRUST_E_NO_SIGNER_CERT, _COUNTER_SIGNER, _CERT_SIGNATURE, _TIME_STAMP, _BAD_DIGEST
        // (changed after signing), _BASIC_CONSTRAINTS, _FINANCIAL_CRITERIA.
        | 0x8009_6002..=0x8009_6005
        | 0x8009_6010
        | 0x8009_6019
        | 0x8009_601E => Signature::Untrusted(status),
        _ => Signature::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn of_file(path: &std::path::Path) -> Signature {
    std::fs::File::open(path)
        .ok()
        .and_then(|mut f| macho(&mut f))
        .unwrap_or(Signature::Unknown)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn of_file(_: &std::path::Path) -> Signature {
    Signature::NotApplicable
}

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSMAGIC_BLOBWRAPPER: u32 = 0xfade_0b01;
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CSSLOT_SIGNATURESLOT: u32 = 0x10000;
const CS_ADHOC: u32 = 0x2;

/// The code signature of a thin 64-bit little-endian Mach-O file (each macOS release
/// binary is one); `None` for anything else or a damaged file, including a CodeDirectory
/// that is not ad-hoc without a certificate signature (CMS blob) next to it. The load
/// commands are in little endian, the signature blobs in big endian.
pub fn macho<R: Read + Seek>(r: &mut R) -> Option<Signature> {
    let le = |b: &[u8], at: usize| Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?));
    let mut head = [0; 32];
    r.read_exact(&mut head).ok()?;
    if le(&head, 0)? != MH_MAGIC_64 {
        return None;
    }
    let (ncmds, size) = (le(&head, 16)?, le(&head, 20)? as usize);
    let mut cmds = vec![0; size.min(1 << 20)];
    r.read_exact(&mut cmds).ok()?;
    let mut at = 0;
    let mut found = None;
    for _ in 0..ncmds {
        let (cmd, len) = (le(&cmds, at)?, le(&cmds, at + 4)? as usize);
        if cmd == LC_CODE_SIGNATURE {
            found = Some((le(&cmds, at + 8)?, le(&cmds, at + 12)?));
            break;
        }
        if len < 8 {
            return None;
        }
        at += len;
    }
    let Some((offset, len)) = found else {
        return Some(Signature::Unsigned);
    };
    r.seek(SeekFrom::Start(offset.into())).ok()?;
    let mut blob = vec![0; (len as usize).min(1 << 20)];
    r.read_exact(&mut blob).ok()?;
    let be = |at: usize| Some(u32::from_be_bytes(blob.get(at..at + 4)?.try_into().ok()?));
    if be(0)? != CSMAGIC_EMBEDDED_SIGNATURE {
        return None;
    }
    let (mut adhoc, mut cms) = (None, false);
    for i in 0..be(8)? as usize {
        let (slot, at) = (be(12 + 8 * i)?, be(16 + 8 * i)? as usize);
        match slot {
            CSSLOT_CODEDIRECTORY if be(at)? == CSMAGIC_CODEDIRECTORY => {
                adhoc = Some(be(at + 12)? & CS_ADHOC != 0);
            }
            // An ad-hoc signature carries an empty wrapper (its 8-byte header only).
            CSSLOT_SIGNATURESLOT if be(at)? == CSMAGIC_BLOBWRAPPER => cms = be(at + 4)? > 8,
            _ => {}
        }
    }
    match adhoc? {
        true => Some(Signature::AdHoc),
        false if cms => Some(Signature::SignedNotVerified),
        false => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A Mach-O header, one unrelated load command and, with `flags`, an
    /// LC_CODE_SIGNATURE pointing at a SuperBlob whose CodeDirectory has those flags and,
    /// with `cms`, a CMS wrapper of that many bytes after its header.
    fn macho_file(flags: Option<u32>, cms: Option<u32>) -> Vec<u8> {
        let mut cmds = Vec::new();
        cmds.extend(0x19u32.to_le_bytes()); // LC_SEGMENT_64, contents irrelevant
        cmds.extend(16u32.to_le_bytes());
        cmds.extend([0; 8]);
        if flags.is_some() {
            cmds.extend(LC_CODE_SIGNATURE.to_le_bytes());
            cmds.extend(16u32.to_le_bytes());
            cmds.extend(256u32.to_le_bytes()); // dataoff
            cmds.extend(64u32.to_le_bytes()); // datasize
        }
        let mut f = Vec::new();
        f.extend(MH_MAGIC_64.to_le_bytes());
        f.extend([0; 12]); // cputype, cpusubtype, filetype
        f.extend((1 + u32::from(flags.is_some())).to_le_bytes());
        f.extend((cmds.len() as u32).to_le_bytes());
        f.extend([0; 8]); // flags, reserved
        f.extend(cmds);
        if let Some(flags) = flags {
            f.resize(256, 0);
            // SuperBlob: magic, length, count, then {slot, offset}: requirements, the
            // CodeDirectory at 36 and, with `cms`, the CMS wrapper at 52.
            let slots = 2 + u32::from(cms.is_some());
            for word in [CSMAGIC_EMBEDDED_SIGNATURE, 64, slots, 2, 60, 0, 36] {
                f.extend(word.to_be_bytes());
            }
            for word in [CSSLOT_SIGNATURESLOT, 52] {
                f.extend(word.to_be_bytes());
            }
            // CodeDirectory: magic, length, version, flags.
            for word in [CSMAGIC_CODEDIRECTORY, 16, 0x20400, flags] {
                f.extend(word.to_be_bytes());
            }
            // CMS wrapper: magic, length (its header and `cms` bytes; at most 4 here).
            for word in [CSMAGIC_BLOBWRAPPER, 8 + cms.unwrap_or(0)] {
                f.extend(word.to_be_bytes());
            }
            f.extend([0; 4]);
        }
        f
    }

    #[test]
    fn macho_signatures() {
        let read = |b: Vec<u8>| macho(&mut Cursor::new(b));
        assert_eq!(read(macho_file(None, None)), Some(Signature::Unsigned));
        // The arm64 linker's signature: CS_ADHOC | CS_LINKER_SIGNED, no CMS slot.
        assert_eq!(
            read(macho_file(Some(0x2_0002), None)),
            Some(Signature::AdHoc)
        );
        // `codesign -s -`: ad-hoc with an empty CMS wrapper.
        assert_eq!(read(macho_file(Some(0x2), Some(0))), Some(Signature::AdHoc));
        // Developer ID with the hardened runtime: CS_RUNTIME and a CMS signature.
        assert_eq!(
            read(macho_file(Some(0x1_0000), Some(4))),
            Some(Signature::SignedNotVerified)
        );
        // Not ad-hoc, yet no certificate signature, or an empty one: broken, not "signed".
        assert_eq!(read(macho_file(Some(0x1_0000), None)), None);
        assert_eq!(read(macho_file(Some(0x1_0000), Some(0))), None);
        assert_eq!(read(b"MZ not a Mach-O file at all......".to_vec()), None);
        let mut cut = macho_file(Some(0), Some(4));
        cut.truncate(270);
        assert_eq!(read(cut), None, "a damaged file is not a guess");
    }

    #[test]
    fn win_verify_trust_answers() {
        let hr = |code: u32| code as i32;
        assert_eq!(from_win_verify_trust(0), Signature::Signed);
        assert_eq!(from_win_verify_trust(hr(0x800B_0100)), Signature::Unsigned);
        // Signed, but: untrusted root, explicit distrust, changed after signing, revoked.
        for code in [0x800B_0109, 0x800B_0111, 0x8009_6010, 0x800B_010C] {
            assert_eq!(
                from_win_verify_trust(hr(code)),
                Signature::Untrusted(hr(code))
            );
        }
        // Not checked at all: not a PE file, unreadable, unknown provider, a system error.
        for code in [0x800B_0003, 0x8009_2003, 0x800B_0001, 0x8009_6001] {
            assert_eq!(from_win_verify_trust(hr(code)), Signature::Unknown);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_file_windows_cannot_check_is_unknown_not_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "not a program").unwrap();
        assert_eq!(of_file(&text), Signature::Unknown);
        assert_eq!(of_file(&dir.path().join("missing.exe")), Signature::Unknown);
    }

    #[test]
    fn this_test_binary_is_not_signed() {
        let sig = current();
        if cfg!(windows) {
            assert_eq!(sig, Signature::Unsigned);
        } else if cfg!(target_os = "macos") {
            assert!(
                matches!(sig, Signature::AdHoc | Signature::Unsigned),
                "{sig}"
            );
        } else {
            assert_eq!(sig, Signature::NotApplicable);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_signed_windows_file_is_recognised() {
        // Any binary with an embedded Authenticode signature (most of System32 is
        // catalog-signed, which this check does not look at; drivers are embedded).
        let dir = std::path::Path::new(r"C:\Windows\System32\drivers");
        let signed = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sys"))
            .any(|p| of_file(&p) == Signature::Signed);
        assert!(
            signed,
            "no embedded-signed driver found in {}",
            dir.display()
        );
    }
}
