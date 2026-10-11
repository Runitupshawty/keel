//! Whether the running binary is code-signed, for `keel --version` and crash reports: on
//! Windows `WinVerifyTrust` checks the embedded Authenticode signature (no revocation check,
//! no network); on macOS the Mach-O code signature is read (a certificate signature, or the
//! ad-hoc one the linker adds, which is not signed in any useful sense). Linux binaries carry
//! no signature: `SHA256SUMS.asc` signs the downloads instead (docs/releasing.md).

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signature {
    /// Windows: an Authenticode signature Windows trusts. macOS: a certificate signature
    /// (Developer ID in a release; `codesign -dvv` names the signer).
    Signed,
    /// Windows: a signature Windows does not trust; the `WinVerifyTrust` HRESULT.
    Untrusted(i32),
    /// macOS: only the linker's or `codesign -s -` ad-hoc signature.
    AdHoc,
    Unsigned,
    /// Linux.
    NotApplicable,
    /// The binary could not be read or parsed.
    Unknown,
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signed => f.write_str("signed"),
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
    use windows::Win32::Foundation::{HWND, TRUST_E_NOSIGNATURE};
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
    match status {
        0 => Signature::Signed,
        s if s == TRUST_E_NOSIGNATURE.0 => Signature::Unsigned,
        s => Signature::Untrusted(s),
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
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CS_ADHOC: u32 = 0x2;

/// The code signature of a thin 64-bit little-endian Mach-O file (each macOS release
/// binary is one); `None` for anything else or a damaged file. The load commands are in
/// little endian, the signature blobs in big endian.
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
    for i in 0..be(8)? as usize {
        let (slot, at) = (be(12 + 8 * i)?, be(16 + 8 * i)? as usize);
        if slot == CSSLOT_CODEDIRECTORY && be(at)? == CSMAGIC_CODEDIRECTORY {
            let adhoc = be(at + 12)? & CS_ADHOC != 0;
            return Some(if adhoc {
                Signature::AdHoc
            } else {
                Signature::Signed
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A Mach-O header, one unrelated load command and, with `flags`, an
    /// LC_CODE_SIGNATURE pointing at a SuperBlob whose CodeDirectory has those flags.
    fn macho_file(flags: Option<u32>) -> Vec<u8> {
        let mut cmds = Vec::new();
        cmds.extend(0x19u32.to_le_bytes()); // LC_SEGMENT_64, contents irrelevant
        cmds.extend(16u32.to_le_bytes());
        cmds.extend([0; 8]);
        if flags.is_some() {
            cmds.extend(LC_CODE_SIGNATURE.to_le_bytes());
            cmds.extend(16u32.to_le_bytes());
            cmds.extend(256u32.to_le_bytes()); // dataoff
            cmds.extend(48u32.to_le_bytes()); // datasize
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
            // SuperBlob: magic, length, count, then {slot, offset}; a requirements slot first.
            for word in [CSMAGIC_EMBEDDED_SIGNATURE, 48, 2, 2, 40, 0, 28] {
                f.extend(word.to_be_bytes());
            }
            // CodeDirectory at 28: magic, length, version, flags.
            for word in [CSMAGIC_CODEDIRECTORY, 16, 0x20400, flags] {
                f.extend(word.to_be_bytes());
            }
            f.extend([0; 4]);
        }
        f
    }

    #[test]
    fn macho_signatures() {
        let read = |b: Vec<u8>| macho(&mut Cursor::new(b));
        assert_eq!(read(macho_file(None)), Some(Signature::Unsigned));
        // The arm64 linker's signature: CS_ADHOC | CS_LINKER_SIGNED.
        assert_eq!(read(macho_file(Some(0x2_0002))), Some(Signature::AdHoc));
        // Developer ID with the hardened runtime: CS_RUNTIME.
        assert_eq!(read(macho_file(Some(0x1_0000))), Some(Signature::Signed));
        assert_eq!(read(b"MZ not a Mach-O file at all......".to_vec()), None);
        let mut cut = macho_file(Some(0));
        cut.truncate(270);
        assert_eq!(read(cut), None, "a damaged file is not a guess");
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
