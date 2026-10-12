//! The Windows Recycle Bin's `$I` files: each deleted item's original path, size and
//! deletion time (the item itself is the `$R` file of the same suffix). Version 1 (Vista
//! to 8.1) holds a fixed 260-character path; version 2 (Windows 10 and later) a counted one.

use super::{filetime, u32_at, u64_at, utf16_until_nul, RecoverError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Longest `$I` file read (a version 2 path is at most 32,767 characters).
const MAX_INFO: u64 = 70 << 10;

/// What a `$I` file says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    /// The full path the item had.
    pub original: String,
    pub size: u64,
    /// Seconds since 1970, UTC.
    pub deleted: Option<i64>,
}

impl Info {
    /// The item's original name (the last part of its path).
    pub fn name(&self) -> &str {
        self.original
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&self.original)
    }
}

/// An item in a Recycle Bin folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub info: Info,
    /// The `$I` file.
    pub info_file: PathBuf,
    /// The `$R` file (the item), when it is still there.
    pub data_file: Option<PathBuf>,
}

/// Parses a `$I` file.
pub fn parse_info(b: &[u8]) -> Result<Info> {
    let bad = |why: &str| RecoverError::Corrupt("Recycle Bin", why.into());
    if b.len() < 24 {
        return Err(bad("$I file too short"));
    }
    let size = u64_at(b, 8);
    let deleted = filetime(u64_at(b, 16));
    let original = match u64_at(b, 0) {
        1 => {
            let path = b
                .get(24..24 + 520)
                .ok_or_else(|| bad("$I file too short"))?;
            utf16_until_nul(path)
        }
        2 => {
            let chars = u32_at(b, 24) as usize;
            if chars == 0 || chars > 32_768 {
                return Err(bad("bad path length"));
            }
            let path = b
                .get(28..28 + 2 * chars)
                .ok_or_else(|| bad("$I file too short"))?;
            utf16_until_nul(path)
        }
        v => return Err(bad(&format!("unknown $I version {v}"))),
    };
    if original.is_empty() {
        return Err(bad("no original path"));
    }
    Ok(Info {
        original,
        size,
        deleted,
    })
}

/// The `$I` files in `dir` (one user's Recycle Bin folder, `X:\$Recycle.Bin\<SID>`), or
/// in each of its subfolders when it holds none itself (`X:\$Recycle.Bin`; folders this
/// user may not read are skipped). Files that do not parse are skipped.
pub fn list(dir: &Path) -> std::io::Result<Vec<Item>> {
    let mut items = list_one(dir)?;
    if items.is_empty() {
        let mut subs: Vec<PathBuf> = std::fs::read_dir(dir)?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .collect();
        subs.sort();
        for sub in subs {
            items.extend(list_one(&sub).unwrap_or_default());
        }
    }
    Ok(items)
}

fn list_one(dir: &Path) -> std::io::Result<Vec<Item>> {
    use std::io::Read;
    let mut names: Vec<(PathBuf, String)> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.get(..2)
                .is_some_and(|p| p.eq_ignore_ascii_case("$I"))
                .then(|| (e.path(), name))
        })
        .collect();
    names.sort();
    let mut items = Vec::new();
    for (path, name) in names {
        let mut bytes = Vec::new();
        let read = std::fs::File::open(&path)
            .and_then(|f| f.take(MAX_INFO).read_to_end(&mut bytes));
        let Ok(info) = read.map_err(RecoverError::from).and_then(|_| parse_info(&bytes)) else {
            continue;
        };
        let data = dir.join(format!("$R{}", &name[2..]));
        items.push(Item {
            info,
            info_file: path,
            data_file: data.exists().then_some(data),
        });
    }
    Ok(items)
}
