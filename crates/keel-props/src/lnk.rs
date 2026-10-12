//! Windows shell links (`.lnk`, [MS-SHLLINK]): the header, the LinkTargetIDList (skipped),
//! LinkInfo (local base path, network share and common path suffix), the string data
//! (name, relative path, working directory, arguments, icon location) and the
//! EnvironmentVariableDataBlock. Every offset and count is checked against the bytes there.
//!
//! [MS-SHLLINK]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-shllink/

use std::fmt;

/// Largest `.lnk` file read (links are a few KiB; extra data blocks can add some).
pub const MAX_LNK_BYTES: u64 = 1 << 20;

const HEADER_SIZE: usize = 0x4c;
const LINK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

const HAS_ID_LIST: u32 = 0x1;
const HAS_LINK_INFO: u32 = 0x2;
const HAS_NAME: u32 = 0x4;
const HAS_RELATIVE_PATH: u32 = 0x8;
const HAS_WORKING_DIR: u32 = 0x10;
const HAS_ARGUMENTS: u32 = 0x20;
const HAS_ICON_LOCATION: u32 = 0x40;
const IS_UNICODE: u32 = 0x80;
const FORCE_NO_LINK_INFO: u32 = 0x100;

/// Longest string-data entry kept (the format allows 65,535 characters).
const MAX_STRING_CHARS: usize = 32 * 1024;
/// Most extra data blocks walked.
const MAX_EXTRA_BLOCKS: usize = 64;

/// Why a `.lnk` file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnkError {
    /// Not a shell link (wrong header size or CLSID).
    NotALink,
    /// A structure runs past the end of the file or contradicts itself.
    Malformed(&'static str),
}

impl fmt::Display for LnkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LnkError::NotALink => f.write_str("not a shell link"),
            LnkError::Malformed(what) => write!(f, "malformed shell link: {what}"),
        }
    }
}

impl std::error::Error for LnkError {}

/// What a shell link holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shortcut {
    /// LinkInfo's local base path (`C:\Tools\app.exe`).
    pub local_base_path: Option<String>,
    /// LinkInfo's network share name (`\\server\share`).
    pub network_path: Option<String>,
    /// LinkInfo's common path suffix, appended to the base path or share.
    pub common_path_suffix: Option<String>,
    /// The EnvironmentVariableDataBlock's target (`%windir%\notepad.exe`).
    pub env_target: Option<String>,
    pub name: Option<String>,
    pub relative_path: Option<String>,
    pub working_dir: Option<String>,
    pub arguments: Option<String>,
    pub icon_location: Option<String>,
    /// The target's size as recorded when the link was made.
    pub target_size: u32,
}

impl Shortcut {
    /// The best target path the link records: LinkInfo's local path plus suffix, else the
    /// share plus suffix, else the environment-variable target, else the relative path.
    pub fn target(&self) -> Option<String> {
        let suffix = self.common_path_suffix.as_deref().unwrap_or("");
        if let Some(base) = self.local_base_path.as_deref().filter(|s| !s.is_empty()) {
            return Some(join(base, suffix));
        }
        if let Some(share) = self.network_path.as_deref().filter(|s| !s.is_empty()) {
            return Some(join(share, suffix));
        }
        self.env_target
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.relative_path.clone().filter(|s| !s.is_empty()))
    }
}

fn join(base: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        base.to_owned()
    } else if base.ends_with('\\') {
        format!("{base}{suffix}")
    } else {
        format!("{base}\\{suffix}")
    }
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// A NUL-terminated ANSI (Windows-1252) string starting at `off`, within `b`.
fn ansi_z(b: &[u8], off: usize) -> Option<String> {
    let rest = b.get(off..)?;
    let end = rest.iter().position(|&c| c == 0)?;
    Some(decode_ansi(&rest[..end]))
}

/// A NUL-terminated UTF-16LE string starting at `off`, within `b`.
fn utf16_z(b: &[u8], off: usize) -> Option<String> {
    let rest = b.get(off..)?;
    let units: Vec<u16> = rest
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    if units.len() * 2 + 2 > rest.len() {
        return None; // no terminator
    }
    Some(String::from_utf16_lossy(&units))
}

pub(crate) fn decode_ansi(b: &[u8]) -> String {
    encoding_rs::WINDOWS_1252
        .decode_without_bom_handling(b)
        .0
        .into_owned()
}

/// Parses a `.lnk` file's bytes.
pub fn parse(b: &[u8]) -> Result<Shortcut, LnkError> {
    if u32_at(b, 0) != Some(HEADER_SIZE as u32) || b.get(4..20) != Some(&LINK_CLSID[..]) {
        return Err(LnkError::NotALink);
    }
    if b.len() < HEADER_SIZE {
        return Err(LnkError::Malformed("short header"));
    }
    let flags = u32_at(b, 0x14).unwrap_or(0);
    let mut out = Shortcut {
        target_size: u32_at(b, 0x34).unwrap_or(0),
        ..Shortcut::default()
    };
    let mut pos = HEADER_SIZE;
    if flags & HAS_ID_LIST != 0 {
        let size = u16_at(b, pos).ok_or(LnkError::Malformed("IDList size"))?;
        pos += 2 + usize::from(size);
        if pos > b.len() {
            return Err(LnkError::Malformed("IDList past the end"));
        }
    }
    if flags & HAS_LINK_INFO != 0 && flags & FORCE_NO_LINK_INFO == 0 {
        let size = u32_at(b, pos).ok_or(LnkError::Malformed("LinkInfo size"))? as usize;
        let end = pos
            .checked_add(size)
            .filter(|&e| e <= b.len() && size >= 0x1c)
            .ok_or(LnkError::Malformed("LinkInfo size"))?;
        link_info(&b[pos..end], &mut out)?;
        pos = end;
    } else if flags & HAS_LINK_INFO != 0 {
        // ForceNoLinkInfo: the structure is there but must be ignored; skip it.
        let size = u32_at(b, pos).ok_or(LnkError::Malformed("LinkInfo size"))? as usize;
        pos = pos
            .checked_add(size)
            .filter(|&e| e <= b.len())
            .ok_or(LnkError::Malformed("LinkInfo size"))?;
    }
    let unicode = flags & IS_UNICODE != 0;
    for (flag, slot) in [
        (HAS_NAME, &mut out.name),
        (HAS_RELATIVE_PATH, &mut out.relative_path),
        (HAS_WORKING_DIR, &mut out.working_dir),
        (HAS_ARGUMENTS, &mut out.arguments),
        (HAS_ICON_LOCATION, &mut out.icon_location),
    ] {
        if flags & flag == 0 {
            continue;
        }
        let chars = usize::from(u16_at(b, pos).ok_or(LnkError::Malformed("string data"))?);
        pos += 2;
        let len = if unicode { chars * 2 } else { chars };
        let raw = b
            .get(pos..pos + len)
            .ok_or(LnkError::Malformed("string data past the end"))?;
        pos += len;
        let s = if unicode {
            let units: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .take(MAX_STRING_CHARS)
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            decode_ansi(&raw[..raw.len().min(MAX_STRING_CHARS)])
        };
        *slot = Some(s);
    }
    extra_data(b, pos, &mut out);
    Ok(out)
}

fn link_info(li: &[u8], out: &mut Shortcut) -> Result<(), LnkError> {
    let header = u32_at(li, 4).ok_or(LnkError::Malformed("LinkInfo header"))? as usize;
    if header < 0x1c || header > li.len() {
        return Err(LnkError::Malformed("LinkInfo header size"));
    }
    let flags = u32_at(li, 8).unwrap_or(0);
    let local_off = u32_at(li, 16).unwrap_or(0) as usize;
    let net_off = u32_at(li, 20).unwrap_or(0) as usize;
    let suffix_off = u32_at(li, 24).unwrap_or(0) as usize;
    let (local_u, suffix_u) = if header >= 0x24 {
        (
            u32_at(li, 28).unwrap_or(0) as usize,
            u32_at(li, 32).unwrap_or(0) as usize,
        )
    } else {
        (0, 0)
    };
    if flags & 1 != 0 {
        out.local_base_path = if local_u != 0 {
            utf16_z(li, local_u)
        } else {
            None
        }
        .or_else(|| (local_off != 0).then(|| ansi_z(li, local_off)).flatten());
    }
    if flags & 2 != 0 && net_off != 0 {
        out.network_path = net_name(li, net_off);
    }
    out.common_path_suffix = if suffix_u != 0 {
        utf16_z(li, suffix_u)
    } else {
        None
    }
    .or_else(|| (suffix_off != 0).then(|| ansi_z(li, suffix_off)).flatten());
    Ok(())
}

/// CommonNetworkRelativeLink: its NetName (ANSI, or Unicode when the offsets say so).
fn net_name(li: &[u8], off: usize) -> Option<String> {
    let cnrl = li.get(off..)?;
    let size = u32_at(cnrl, 0)? as usize;
    let cnrl = cnrl.get(..size)?;
    let name_off = u32_at(cnrl, 8)? as usize;
    if name_off > 0x14 {
        if let Some(uoff) = u32_at(cnrl, 20).filter(|&o| o != 0) {
            if let Some(s) = utf16_z(cnrl, uoff as usize) {
                return Some(s);
            }
        }
    }
    ansi_z(cnrl, name_off)
}

/// ExtraData: walks the blocks for the EnvironmentVariableDataBlock (0xA0000001).
fn extra_data(b: &[u8], mut pos: usize, out: &mut Shortcut) {
    for _ in 0..MAX_EXTRA_BLOCKS {
        let Some(size) = u32_at(b, pos).map(|s| s as usize) else {
            return;
        };
        if size < 8 {
            return; // TerminalBlock
        }
        let Some(block) = pos.checked_add(size).and_then(|end| b.get(pos..end)) else {
            return;
        };
        if u32_at(block, 4) == Some(0xa000_0001) && size >= 0x314 {
            let unicode = utf16_z(&block[0x10c..0x314], 0);
            let ansi = ansi_z(&block[8..0x10c], 0);
            out.env_target = unicode.filter(|s| !s.is_empty()).or(ansi);
        }
        pos += size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn local_target_and_strings() {
        let b = fixtures::lnk(&fixtures::LnkSpec {
            local_base_path: Some(r"C:\Tools\"),
            suffix: "app.exe",
            unicode_paths: false,
            arguments: Some("--flag value"),
            working_dir: Some(r"C:\Tools"),
            name: Some("My app"),
            id_list: true,
            env_target: None,
        });
        let s = parse(&b).unwrap();
        assert_eq!(s.target().as_deref(), Some(r"C:\Tools\app.exe"));
        assert_eq!(s.arguments.as_deref(), Some("--flag value"));
        assert_eq!(s.working_dir.as_deref(), Some(r"C:\Tools"));
        assert_eq!(s.name.as_deref(), Some("My app"));
    }

    #[test]
    fn unicode_paths_win() {
        let b = fixtures::lnk(&fixtures::LnkSpec {
            local_base_path: Some("C:\\Donn\u{e9}es\\\u{65e5}\u{672c}"),
            suffix: "",
            unicode_paths: true,
            arguments: None,
            working_dir: None,
            name: None,
            id_list: false,
            env_target: None,
        });
        let s = parse(&b).unwrap();
        assert_eq!(
            s.target().as_deref(),
            Some("C:\\Donn\u{e9}es\\\u{65e5}\u{672c}")
        );
    }

    #[test]
    fn env_target_without_link_info() {
        let b = fixtures::lnk(&fixtures::LnkSpec {
            local_base_path: None,
            suffix: "",
            unicode_paths: false,
            arguments: None,
            working_dir: None,
            name: None,
            id_list: true,
            env_target: Some(r"%windir%\notepad.exe"),
        });
        let s = parse(&b).unwrap();
        assert_eq!(s.target().as_deref(), Some(r"%windir%\notepad.exe"));
    }

    #[test]
    fn rejects_other_files_and_truncation() {
        assert_eq!(parse(b"not a link"), Err(LnkError::NotALink));
        let b = fixtures::lnk(&fixtures::LnkSpec {
            local_base_path: Some(r"C:\x\"),
            suffix: "y.txt",
            unicode_paths: true,
            arguments: Some("a"),
            working_dir: None,
            name: None,
            id_list: true,
            env_target: Some("z"),
        });
        for n in 0..b.len() {
            let _ = parse(&b[..n]);
        }
        assert!(parse(&b).is_ok());
    }
}
