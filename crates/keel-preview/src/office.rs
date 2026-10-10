//! Shared helpers for the zip + XML document previewers (pptx, OpenDocument, docx).

use quick_xml::events::{BytesRef, BytesStart};
use std::io::Read;
use std::path::Path;

/// Total decompressed bytes one preview may read out of a package.
pub(crate) const MAX_UNZIPPED: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_SLIDES: usize = 200;
/// 500 pages at about 40 paragraphs each.
pub(crate) const MAX_BLOCKS: usize = 500 * 40;
pub(crate) const TRUNCATED: &str = "… (truncated)";

pub(crate) struct Package {
    zip: zip::ZipArchive<std::fs::File>,
    budget: u64,
}

impl Package {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let zip =
            zip::ZipArchive::new(file).map_err(|e| format!("not a valid Office/zip file: {e}"))?;
        Ok(Self {
            zip,
            budget: MAX_UNZIPPED,
        })
    }

    /// Sum of the sizes the archive claims for all entries (a lying header is still
    /// bounded by `read`).
    pub(crate) fn declared_size(&mut self) -> u64 {
        (0..self.zip.len())
            .filter_map(|i| self.zip.by_index_raw(i).ok().map(|f| f.size()))
            .fold(0, u64::saturating_add)
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.zip.file_names().map(str::to_owned).collect()
    }

    /// `Ok(None)` when the entry does not exist. Never allocates past the budget.
    pub(crate) fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let file = match self.zip.by_name(name) {
            Ok(file) => file,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(e) => return Err(format!("{name}: {e}")),
        };
        let limit = format!(
            "{name} exceeds the {} MiB decompressed limit",
            MAX_UNZIPPED >> 20
        );
        if file.size() > self.budget {
            return Err(limit);
        }
        let mut out = Vec::new();
        file.take(self.budget + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("{name}: {e}"))?;
        if out.len() as u64 > self.budget {
            return Err(limit);
        }
        self.budget -= out.len() as u64;
        Ok(Some(out))
    }
}

/// Value of the attribute whose qualified name is `key` (e.g. `text:outline-level`).
pub(crate) fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// `&amp;`, `&#10;` and friends.
pub(crate) fn entity(r: &BytesRef) -> Option<String> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c.to_string());
    }
    let name: &str = r;
    quick_xml::escape::resolve_predefined_entity(name).map(str::to_owned)
}
