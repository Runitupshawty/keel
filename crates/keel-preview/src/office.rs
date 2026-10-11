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
/// The end of a part's error when the package inflates past `MAX_UNZIPPED`.
pub(crate) const TOO_BIG: &str = "exceeds the 64 MiB decompressed limit";
/// The end of a part's error when its XML nests past `MAX_DEPTH` (or `MAX_TABLES`).
pub(crate) const TOO_DEEP: &str = "nests too deeply";
/// XML elements one inside another, at most; docx-rs reads nested tables recursively, and
/// 20,000 of them overflow any thread's stack (an abort no panic hook sees).
const MAX_DEPTH: usize = 256;
/// Word tables one inside another, at most.
const MAX_TABLES: usize = 64;

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
        let limit = format!("{name} {TOO_BIG}");
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

    /// The package again as an in-memory zip, each part inflated through `read` (so a
    /// central directory that lies about a size cannot get past the budget) and stored.
    /// `word`: every XML part is also checked for nesting (docx-rs). The readers that take
    /// a whole zip (docx-rs, calamine) read this copy, never the file.
    pub(crate) fn repack(&mut self, word: bool) -> Result<Vec<u8>, String> {
        use std::io::Write;
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for name in self.names() {
            if name.ends_with('/') {
                continue;
            }
            let bytes = self.read(&name)?.unwrap_or_default();
            let lower = name.replace('\\', "/").to_ascii_lowercase();
            // docx-rs reads whatever part the relationships name as XML: every part that
            // looks like markup is checked (images do not start with `<`; HTML chunks
            // docx-rs never reads are left alone).
            let html = lower.ends_with(".htm") || lower.ends_with(".html");
            if word && !html && markup(&bytes) {
                nesting(&bytes).map_err(|e| format!("{name} {e}"))?;
            }
            // calamine finds parts by lowercase name.
            if lower.ends_with("sharedstrings.xml") {
                shared_strings(&bytes).map_err(|e| format!("{name}: {e}"))?;
            }
            out.start_file(name.as_str(), options)
                .and_then(|()| out.write_all(&bytes).map_err(Into::into))
                .map_err(|e| format!("{name}: {e}"))?;
        }
        let out = out.finish().map_err(|e| e.to_string())?;
        Ok(out.into_inner())
    }
}

/// Whether `bytes` start like XML (after a byte order mark and white space).
fn markup(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let bytes = (bytes.strip_prefix(b"\xFF\xFE"))
        .or_else(|| bytes.strip_prefix(b"\xFE\xFF"))
        .unwrap_or(bytes);
    bytes
        .iter()
        .find(|b| !b.is_ascii_whitespace() && **b != 0)
        .is_some_and(|b| *b == b'<')
}

/// `TOO_DEEP` past `MAX_DEPTH` elements or `MAX_TABLES` Word tables one inside another (an
/// XML error is a damaged part too: the check must see what docx-rs would).
fn nesting(xml: &[u8]) -> Result<(), String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    // Whether each open element is a table.
    let mut open: Vec<bool> = Vec::new();
    let mut tables = 0;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let table = e.local_name().as_ref() == "tbl";
                tables += usize::from(table);
                open.push(table);
                if open.len() > MAX_DEPTH || tables > MAX_TABLES {
                    return Err(TOO_DEEP.into());
                }
            }
            Ok(Event::End(_)) => tables -= usize::from(open.pop().unwrap_or(false)),
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Err("is not valid XML".into()),
        }
    }
}

/// calamine reserves room for the strings an xlsx says it has (`uniqueCount`) before
/// reading them: a count no part of this size can hold is a damaged file, not an
/// allocation that aborts the process.
fn shared_strings(xml: &[u8]) -> Result<(), String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "sst" => {
                let count = attr(&e, "uniqueCount").unwrap_or_default();
                // `<si></si>` is 9 bytes.
                let fits =
                    (count.trim().parse::<u64>()).map_or(true, |n| n <= xml.len() as u64 / 9);
                return if fits {
                    Ok(())
                } else {
                    Err("more strings than the part can hold".into())
                };
            }
            Ok(Event::Eof) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
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
