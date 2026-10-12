//! File signatures: the file's type from its first bytes (magic numbers), not its name.
//!
//! [`detect`] looks only at the head it is given ([`HEAD_LEN`] bytes is enough for every
//! format here) and never allocates by a size the input declares.

/// Bytes of the file [`detect`] needs to see (the tar magic sits at offset 257; zip-based
/// formats are told apart by the names of their first entries).
pub const HEAD_LEN: usize = 4096;

/// A detected file type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature {
    /// Short id, usually the format's common extension (`png`, `docx`, `exe`).
    pub id: &'static str,
    /// MIME type (`image/png`).
    pub content_type: &'static str,
    /// Windows perceived type: `image`, `audio`, `video`, `text`, `compressed`, `document`,
    /// `system`, `application`, `font` or `unknown`.
    pub perceived: &'static str,
}

const fn sig(id: &'static str, content_type: &'static str, perceived: &'static str) -> Signature {
    Signature {
        id,
        content_type,
        perceived,
    }
}

fn at(head: &[u8], off: usize, magic: &[u8]) -> bool {
    off.checked_add(magic.len())
        .and_then(|end| head.get(off..end))
        == Some(magic)
}

fn u32be(head: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        head.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u32le(head: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        head.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u16le(head: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        head.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn contains(head: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && head.windows(needle.len()).any(|w| w == needle)
}

/// The file type the head of a file shows, or `None` when no signature matches (plain text
/// has none; see [`looks_like_text`]).
pub fn detect(head: &[u8]) -> Option<Signature> {
    let h = &head[..head.len().min(HEAD_LEN)];
    // Images.
    if at(h, 0, b"\x89PNG\r\n\x1a\n") {
        return Some(sig("png", "image/png", "image"));
    }
    if at(h, 0, b"\xff\xd8\xff") {
        return Some(sig("jpg", "image/jpeg", "image"));
    }
    if at(h, 0, b"GIF87a") || at(h, 0, b"GIF89a") {
        return Some(sig("gif", "image/gif", "image"));
    }
    if at(h, 0, b"II*\0") || at(h, 0, b"MM\0*") {
        return Some(sig("tif", "image/tiff", "image"));
    }
    if at(h, 0, b"8BPS") {
        return Some(sig("psd", "image/vnd.adobe.photoshop", "image"));
    }
    if at(h, 0, b"\xff\x0a") || at(h, 0, b"\0\0\0\x0cJXL \r\n\x87\n") {
        return Some(sig("jxl", "image/jxl", "image"));
    }
    if at(h, 0, b"BM") && h.len() >= 18 {
        if let Some(dib) = u32le(h, 14) {
            if matches!(dib, 12 | 40 | 52 | 56 | 64 | 108 | 124) {
                return Some(sig("bmp", "image/bmp", "image"));
            }
        }
    }
    if (at(h, 0, b"\0\0\x01\0") || at(h, 0, b"\0\0\x02\0")) && h.len() >= 22 {
        // ICONDIR: count 1..=256, then the first entry's reserved byte (0) and planes.
        let count = u16le(h, 4).unwrap_or(0);
        if (1..=256).contains(&count) && h[9] == 0 {
            return Some(if h[2] == 1 {
                sig("ico", "image/vnd.microsoft.icon", "image")
            } else {
                sig("cur", "image/vnd.microsoft.icon", "image")
            });
        }
    }
    // RIFF containers.
    if at(h, 0, b"RIFF") {
        if at(h, 8, b"WEBP") {
            return Some(sig("webp", "image/webp", "image"));
        }
        if at(h, 8, b"WAVE") {
            return Some(sig("wav", "audio/wav", "audio"));
        }
        if at(h, 8, b"AVI ") {
            return Some(sig("avi", "video/x-msvideo", "video"));
        }
    }
    if at(h, 0, b"FORM") && (at(h, 8, b"AIFF") || at(h, 8, b"AIFC")) {
        return Some(sig("aiff", "audio/aiff", "audio"));
    }
    // ISO base media (MP4, QuickTime, HEIF, AVIF).
    if at(h, 4, b"ftyp") {
        let brand = h.get(8..12).unwrap_or(b"");
        return Some(match brand {
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" => {
                sig("heic", "image/heic", "image")
            }
            b"mif1" | b"msf1" => sig("heif", "image/heif", "image"),
            b"avif" | b"avis" => sig("avif", "image/avif", "image"),
            b"M4A " | b"M4B " | b"M4P " => sig("m4a", "audio/mp4", "audio"),
            b"qt  " => sig("mov", "video/quicktime", "video"),
            b"crx " => sig("cr3", "image/x-canon-cr3", "image"),
            _ if brand.starts_with(b"3g") => sig("3gp", "video/3gpp", "video"),
            _ => sig("mp4", "video/mp4", "video"),
        });
    }
    // Audio.
    if at(h, 0, b"ID3") {
        return Some(sig("mp3", "audio/mpeg", "audio"));
    }
    if at(h, 0, b"fLaC") {
        return Some(sig("flac", "audio/flac", "audio"));
    }
    if at(h, 0, b"OggS") {
        return Some(if contains(h, b"OpusHead") {
            sig("opus", "audio/opus", "audio")
        } else if contains(h, b"\x80theora") {
            sig("ogv", "video/ogg", "video")
        } else if contains(h, b"\x01vorbis") {
            sig("ogg", "audio/ogg", "audio")
        } else {
            sig("ogg", "application/ogg", "audio")
        });
    }
    if at(h, 0, b"MThd") {
        return Some(sig("mid", "audio/midi", "audio"));
    }
    if at(h, 0, b"#!AMR") {
        return Some(sig("amr", "audio/amr", "audio"));
    }
    if at(h, 0, b"MAC ") {
        return Some(sig("ape", "audio/ape", "audio"));
    }
    if at(h, 0, b"wvpk") {
        return Some(sig("wv", "audio/wavpack", "audio"));
    }
    // Video.
    if at(h, 0, b"\x1a\x45\xdf\xa3") {
        return Some(if contains(h, b"webm") {
            sig("webm", "video/webm", "video")
        } else {
            sig("mkv", "video/x-matroska", "video")
        });
    }
    if at(h, 0, b"FLV\x01") {
        return Some(sig("flv", "video/x-flv", "video"));
    }
    if at(h, 0, b"\x30\x26\xb2\x75\x8e\x66\xcf\x11") {
        return Some(sig("asf", "video/x-ms-asf", "video"));
    }
    if at(h, 0, b"\0\0\x01\xba") || at(h, 0, b"\0\0\x01\xb3") {
        return Some(sig("mpg", "video/mpeg", "video"));
    }
    if h.len() > 376 && h[0] == 0x47 && h[188] == 0x47 && h[376] == 0x47 {
        return Some(sig("ts", "video/mp2t", "video"));
    }
    // Documents.
    if at(h, 0, b"%PDF-") {
        return Some(sig("pdf", "application/pdf", "document"));
    }
    if at(h, 0, b"{\\rtf") {
        return Some(sig("rtf", "application/rtf", "document"));
    }
    if at(h, 0, b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
        // Compound File Binary: .doc, .xls, .ppt, .msi, .msg all share it.
        return Some(sig("cfb", "application/x-ole-storage", "document"));
    }
    if at(h, 0, b"AT&TFORM") {
        return Some(sig("djvu", "image/vnd.djvu", "document"));
    }
    if at(h, 0, b"SQLite format 3\0") {
        return Some(sig("sqlite", "application/vnd.sqlite3", "unknown"));
    }
    // Archives.
    if at(h, 0, b"PK\x03\x04") || at(h, 0, b"PK\x05\x06") || at(h, 0, b"PK\x07\x08") {
        return Some(zip_kind(h));
    }
    if at(h, 0, b"Rar!\x1a\x07") {
        return Some(sig("rar", "application/vnd.rar", "compressed"));
    }
    if at(h, 0, b"7z\xbc\xaf\x27\x1c") {
        return Some(sig("7z", "application/x-7z-compressed", "compressed"));
    }
    if at(h, 0, b"\x1f\x8b\x08") {
        return Some(sig("gz", "application/gzip", "compressed"));
    }
    if at(h, 0, b"BZh") && h.get(3).is_some_and(|b| (b'1'..=b'9').contains(b)) {
        return Some(sig("bz2", "application/x-bzip2", "compressed"));
    }
    if at(h, 0, b"\xfd7zXZ\0") {
        return Some(sig("xz", "application/x-xz", "compressed"));
    }
    if at(h, 0, b"\x28\xb5\x2f\xfd") {
        return Some(sig("zst", "application/zstd", "compressed"));
    }
    if at(h, 0, b"\x04\x22\x4d\x18") {
        return Some(sig("lz4", "application/x-lz4", "compressed"));
    }
    if at(h, 0, b"MSCF") {
        return Some(sig(
            "cab",
            "application/vnd.ms-cab-compressed",
            "compressed",
        ));
    }
    if at(h, 257, b"ustar") {
        return Some(sig("tar", "application/x-tar", "compressed"));
    }
    // Executables and system files.
    if at(h, 0, b"MZ") {
        return Some(pe_kind(h));
    }
    if at(h, 0, b"\x7fELF") {
        return Some(sig("elf", "application/x-executable", "application"));
    }
    if at(h, 0, b"\xfe\xed\xfa\xce")
        || at(h, 0, b"\xce\xfa\xed\xfe")
        || at(h, 0, b"\xfe\xed\xfa\xcf")
        || at(h, 0, b"\xcf\xfa\xed\xfe")
    {
        return Some(sig("macho", "application/x-mach-binary", "application"));
    }
    if at(h, 0, b"\xca\xfe\xba\xbe") {
        // A fat Mach-O names a few architectures here; a Java class its version (45+).
        return Some(match u32be(h, 4) {
            Some(n) if n < 45 => sig("macho", "application/x-mach-binary", "application"),
            _ => sig("class", "application/java-vm", "application"),
        });
    }
    if at(h, 0, b"\0asm") {
        return Some(sig("wasm", "application/wasm", "application"));
    }
    if at(h, 0, b"dex\n") {
        return Some(sig("dex", "application/vnd.android.dex", "application"));
    }
    if at(
        h,
        0,
        b"\x4c\0\0\0\x01\x14\x02\0\0\0\0\0\xc0\0\0\0\0\0\0\x46",
    ) {
        return Some(sig("lnk", "application/x-ms-shortcut", "system"));
    }
    if at(h, 0, b"regf") {
        return Some(sig("dat", "application/x-ms-registry", "system"));
    }
    // Fonts.
    if at(h, 0, b"wOFF") {
        return Some(sig("woff", "font/woff", "font"));
    }
    if at(h, 0, b"wOF2") {
        return Some(sig("woff2", "font/woff2", "font"));
    }
    if at(h, 0, b"OTTO") {
        return Some(sig("otf", "font/otf", "font"));
    }
    if at(h, 0, b"ttcf") {
        return Some(sig("ttc", "font/collection", "font"));
    }
    if (at(h, 0, b"\0\x01\0\0") || at(h, 0, b"true")) && h.len() >= 12 {
        // sfnt: numTables, then searchRange = 16 * 2^floor(log2(numTables)).
        let tables = u16::from_be_bytes([h[4], h[5]]);
        let range = u16::from_be_bytes([h[6], h[7]]);
        if (1..=64).contains(&tables) && range == 16 * (1u16 << (15 - tables.leading_zeros())) {
            return Some(sig("ttf", "font/ttf", "font"));
        }
    }
    // MPEG audio without an ID3 tag: a frame header (sync, layer, valid rates).
    if h.len() >= 4 && h[0] == 0xff {
        if h[1] & 0xf6 == 0xf0 {
            return Some(sig("aac", "audio/aac", "audio"));
        }
        let layer = (h[1] >> 1) & 3;
        let version = (h[1] >> 3) & 3;
        if h[1] & 0xe0 == 0xe0
            && layer != 0
            && version != 1
            && h[2] >> 4 != 0xf
            && (h[2] >> 2) & 3 != 3
        {
            return Some(sig("mp3", "audio/mpeg", "audio"));
        }
    }
    // Markup and text with a byte order mark.
    let text = strip_bom(h);
    let lead = trim_ascii_start(text);
    if starts_with_ci(lead, b"<?xml") {
        return Some(if contains(&lead[..lead.len().min(1024)], b"<svg") {
            sig("svg", "image/svg+xml", "image")
        } else {
            sig("xml", "application/xml", "text")
        });
    }
    if starts_with_ci(lead, b"<svg") {
        return Some(sig("svg", "image/svg+xml", "image"));
    }
    if starts_with_ci(lead, b"<!doctype html") || starts_with_ci(lead, b"<html") {
        return Some(sig("html", "text/html", "text"));
    }
    if at(text, 0, b"#!") {
        return Some(sig("sh", "text/x-shellscript", "text"));
    }
    None
}

fn strip_bom(h: &[u8]) -> &[u8] {
    h.strip_prefix(b"\xef\xbb\xbf").unwrap_or(h)
}

fn trim_ascii_start(h: &[u8]) -> &[u8] {
    let n = h.iter().take_while(|b| b.is_ascii_whitespace()).count();
    &h[n..]
}

fn starts_with_ci(h: &[u8], prefix: &[u8]) -> bool {
    h.len() >= prefix.len() && h[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Zip-based formats by their first entries: OpenDocument and EPUB store a `mimetype` entry
/// first; Office Open XML has `[Content_Types].xml` and a `word/`, `xl/` or `ppt/` folder;
/// Java and Android archives a `META-INF/` or `AndroidManifest.xml`.
fn zip_kind(h: &[u8]) -> Signature {
    if at(h, 30, b"mimetype") && u16le(h, 26) == Some(8) {
        let extra = usize::from(u16le(h, 28).unwrap_or(0));
        let body = h.get(38 + extra..).unwrap_or(b"");
        let mime: &[u8] = &body[..body.len().min(80)];
        for (prefix, s) in [
            (
                &b"application/vnd.oasis.opendocument.text"[..],
                sig("odt", "application/vnd.oasis.opendocument.text", "document"),
            ),
            (
                b"application/vnd.oasis.opendocument.spreadsheet",
                sig(
                    "ods",
                    "application/vnd.oasis.opendocument.spreadsheet",
                    "document",
                ),
            ),
            (
                b"application/vnd.oasis.opendocument.presentation",
                sig(
                    "odp",
                    "application/vnd.oasis.opendocument.presentation",
                    "document",
                ),
            ),
            (
                b"application/epub+zip",
                sig("epub", "application/epub+zip", "document"),
            ),
        ] {
            if mime.starts_with(prefix) {
                return s;
            }
        }
    }
    if contains(h, b"word/") {
        return sig(
            "docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "document",
        );
    }
    if contains(h, b"xl/") {
        return sig(
            "xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "document",
        );
    }
    if contains(h, b"ppt/") {
        return sig(
            "pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "document",
        );
    }
    if contains(h, b"AndroidManifest.xml") {
        return sig(
            "apk",
            "application/vnd.android.package-archive",
            "application",
        );
    }
    if contains(h, b"META-INF/") {
        return sig("jar", "application/java-archive", "application");
    }
    sig("zip", "application/zip", "compressed")
}

/// `MZ`: a PE image (`exe`, or `dll` when the COFF header says so) or a plain DOS program.
fn pe_kind(h: &[u8]) -> Signature {
    if let Some(off) = u32le(h, 0x3c).map(|o| o as usize) {
        if at(h, off, b"PE\0\0") {
            let chars = off.checked_add(22).and_then(|o| u16le(h, o)).unwrap_or(0);
            if chars & 0x2000 != 0 {
                return sig(
                    "dll",
                    "application/vnd.microsoft.portable-executable",
                    "application",
                );
            }
            return sig(
                "exe",
                "application/vnd.microsoft.portable-executable",
                "application",
            );
        }
    }
    sig("exe", "application/x-msdownload", "application")
}

/// True when the head reads as text: a UTF-16/UTF-8 byte order mark, or no NUL bytes and
/// few control characters. Used for `perceived-type` and `content-type` of files without a
/// signature.
pub fn looks_like_text(head: &[u8]) -> bool {
    if head.starts_with(b"\xef\xbb\xbf")
        || head.starts_with(b"\xff\xfe")
        || head.starts_with(b"\xfe\xff")
    {
        return true;
    }
    if head.is_empty() || head.contains(&0) {
        return false;
    }
    let controls = head
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0c | 0x1b))
        .count();
    controls * 100 <= head.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(h: &[u8]) -> Option<&'static str> {
        detect(h).map(|s| s.id)
    }

    #[test]
    fn images() {
        assert_eq!(id(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), Some("png"));
        assert_eq!(id(b"\xff\xd8\xff\xe0\0\x10JFIF"), Some("jpg"));
        assert_eq!(id(b"GIF89a\x01\0\x01\0"), Some("gif"));
        let mut bmp = b"BM\0\0\0\0\0\0\0\0\x36\0\0\0\x28\0\0\0".to_vec();
        bmp.resize(54, 0);
        assert_eq!(id(&bmp), Some("bmp"));
        assert_eq!(id(b"BM plain text that starts with BM"), None);
        assert_eq!(id(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(id(b"\0\0\0\x18ftypheic\0\0\0\0"), Some("heic"));
        assert_eq!(id(b"\0\0\0\x18ftypavif\0\0\0\0"), Some("avif"));
        let ico = [
            0u8, 0, 1, 0, 1, 0, 16, 16, 0, 0, 1, 0, 32, 0, 0, 0, 0, 0, 22, 0, 0, 0,
        ];
        assert_eq!(id(&ico), Some("ico"));
        let s = detect(b"  <?xml version='1.0'?><svg xmlns='x'/>").unwrap();
        assert_eq!((s.id, s.perceived), ("svg", "image"));
    }

    #[test]
    fn media() {
        assert_eq!(id(b"ID3\x04\0\0\0\0\0\0"), Some("mp3"));
        assert_eq!(id(&[0xff, 0xfb, 0x90, 0x64]), Some("mp3"));
        assert_eq!(id(&[0xff, 0xf1, 0x50, 0x80]), Some("aac"));
        assert_eq!(id(&[0xff, 0xff, 0xff, 0xff]), None);
        assert_eq!(id(b"fLaC\0\0\0\x22"), Some("flac"));
        assert_eq!(id(b"RIFF\0\0\0\0WAVEfmt "), Some("wav"));
        assert_eq!(id(b"\0\0\0\x20ftypisom\0\0\x02\0"), Some("mp4"));
        assert_eq!(id(b"\0\0\0\x20ftypM4A \0\0\x02\0"), Some("m4a"));
        assert_eq!(id(b"\x1a\x45\xdf\xa3\x9f\x42\x82\x84webm"), Some("webm"));
        assert_eq!(id(b"OggS\0\x02\0\0\0\0\0\0\0\0\x01vorbis"), Some("ogg"));
    }

    #[test]
    fn documents_and_archives() {
        assert_eq!(id(b"%PDF-1.7\n"), Some("pdf"));
        assert_eq!(id(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"), Some("cfb"));
        assert_eq!(id(b"7z\xbc\xaf\x27\x1c\0\x04"), Some("7z"));
        assert_eq!(id(b"Rar!\x1a\x07\x01\0"), Some("rar"));
        assert_eq!(id(b"\x1f\x8b\x08\0\0\0"), Some("gz"));
        assert_eq!(id(b"PK\x05\x06\0\0\0\0"), Some("zip"));
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(id(&tar), Some("tar"));
        // An OpenDocument text: a stored `mimetype` entry first.
        let mut odt = b"PK\x03\x04".to_vec();
        odt.resize(26, 0);
        odt.extend_from_slice(&8u16.to_le_bytes());
        odt.extend_from_slice(&0u16.to_le_bytes());
        odt.extend_from_slice(b"mimetypeapplication/vnd.oasis.opendocument.text");
        assert_eq!(id(&odt), Some("odt"));
        let mut docx = b"PK\x03\x04".to_vec();
        docx.resize(30, 0);
        docx.extend_from_slice(b"[Content_Types].xml....PK\x03\x04word/document.xml");
        assert_eq!(id(&docx), Some("docx"));
    }

    #[test]
    fn executables() {
        let mut pe = vec![0u8; 0x100];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c] = 0x80;
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        assert_eq!(id(&pe), Some("exe"));
        pe[0x80 + 22..0x80 + 24].copy_from_slice(&0x2102u16.to_le_bytes());
        assert_eq!(id(&pe), Some("dll"));
        assert_eq!(id(b"MZ"), Some("exe"));
        assert_eq!(id(b"\x7fELF\x02\x01\x01"), Some("elf"));
        assert_eq!(id(b"\xca\xfe\xba\xbe\0\0\0\x02"), Some("macho"));
        assert_eq!(id(b"\xca\xfe\xba\xbe\0\0\0\x34"), Some("class"));
        assert_eq!(id(b"\0asm\x01\0\0\0"), Some("wasm"));
    }

    #[test]
    fn text() {
        assert_eq!(id(b"plain words\n"), None);
        assert!(looks_like_text(b"plain words\n"));
        assert!(looks_like_text("caf\u{e9}\n".as_bytes()));
        assert!(!looks_like_text(b"a\0b"));
        assert!(!looks_like_text(b""));
        assert!(looks_like_text(b"\xff\xfea\0"));
        assert_eq!(id(b"<!DOCTYPE html><html>"), Some("html"));
        assert_eq!(id(b"#!/bin/sh\necho"), Some("sh"));
    }

    #[test]
    fn short_and_hostile_heads_do_not_panic() {
        for n in 0..64 {
            let h = vec![0xffu8; n];
            let _ = detect(&h);
            let mut z = b"PK\x03\x04".to_vec();
            z.resize(n, 0xff);
            let _ = detect(&z);
            let mut m = b"MZ".to_vec();
            m.resize(n, 0xff);
            let _ = detect(&m);
        }
    }
}
