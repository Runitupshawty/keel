//! What the scan does not keep, asked of the OS on a worker: an item's owner (named through
//! [`Owners`]' cache, once per SID or user id) and its last access and creation times
//! (metadata only, never the file's contents), and the description the OS gives a file
//! extension (Windows `AssocQueryStringW`, macOS UTType, Linux shared-mime-info).

use super::model::Metadata;
use keel_vfs::du::owner::Owners;
use std::collections::HashMap;
use std::path::Path;

/// The owner and times of `path` (the link itself for a link). Empty when it cannot be
/// read.
pub fn metadata(path: &Path, owners: &Owners) -> Metadata {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Metadata::default();
    };
    let time = |t: std::io::Result<std::time::SystemTime>| {
        (t.ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_nanos()).ok())
    };
    Metadata {
        owner: owners.owner(path),
        accessed: time(meta.accessed()),
        created: time(meta.created()),
    }
}

/// The OS's descriptions of `exts` (lowercase, without the dot); extensions it does not
/// know are left out.
pub fn descriptions<'a>(exts: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let exts: Vec<&str> = exts.into_iter().filter(|e| !e.is_empty()).collect();
    describe_all(&exts)
}

#[cfg(windows)]
mod win {
    #[link(name = "shlwapi")]
    extern "system" {
        fn AssocQueryStringW(
            flags: u32,
            string: u32,
            assoc: *const u16,
            extra: *const u16,
            out: *mut u16,
            len: *mut u32,
        ) -> i32;
    }


    const ASSOCF_NOTRUNCATE: u32 = 0x20;
    const ASSOCSTR_FRIENDLYDOCNAME: u32 = 3;

    /// The registered type name of `.ext` ("Text Document"), None when unregistered.
    pub fn description(ext: &str) -> Option<String> {
        let ext: Vec<u16> = format!(".{ext}").encode_utf16().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `ext` is NUL-terminated; `buf` is writable for `len` u16s.
        let hr = unsafe {
            AssocQueryStringW(
                ASSOCF_NOTRUNCATE,
                ASSOCSTR_FRIENDLYDOCNAME,
                ext.as_ptr(),
                std::ptr::null(),
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        if hr != 0 || len < 2 || len as usize > buf.len() {
            return None;
        }
        let text = String::from_utf16_lossy(&buf[..len as usize - 1]);
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }
}

#[cfg(windows)]
fn describe_all(exts: &[&str]) -> HashMap<String, String> {
    (exts.iter())
        .filter_map(|e| win::description(e).map(|d| (e.to_string(), d)))
        .collect()
}

/// shared-mime-info: the MIME type a literal `*.ext` glob gives the extension (the
/// heaviest), then that type's English comment.
#[cfg(target_os = "linux")]
fn describe_all(exts: &[&str]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for base in ["/usr/local/share/mime", "/usr/share/mime"] {
        let Ok(globs) = std::fs::read_to_string(format!("{base}/globs2")) else {
            continue;
        };
        let mut best: HashMap<String, (u32, String)> = HashMap::new();
        for line in globs.lines().filter(|l| !l.starts_with('#')) {
            let mut parts = line.split(':');
            let (Some(weight), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(weight), Some(ext)) = (weight.parse::<u32>(), glob.strip_prefix("*.")) else {
                continue;
            };
            if ext.contains(['*', '?', '[']) {
                continue;
            }
            let ext = ext.to_lowercase();
            if best.get(&ext).is_none_or(|(w, _)| weight > *w) {
                best.insert(ext, (weight, mime.to_owned()));
            }
        }
        let mut comments: HashMap<String, Option<String>> = HashMap::new();
        for ext in exts {
            if out.contains_key(*ext) {
                continue;
            }
            let Some((_, mime)) = best.get(*ext) else {
                continue;
            };
            let comment = comments
                .entry(mime.clone())
                .or_insert_with(|| mime_comment(base, mime));
            if let Some(c) = comment {
                out.insert(ext.to_string(), c.clone());
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn mime_comment(base: &str, mime: &str) -> Option<String> {
    // "type/subtype" only: never a path outside the mime folder.
    let (kind, sub) = mime.split_once('/')?;
    let safe = |s: &str| !s.is_empty() && !s.contains(['/', '\\']) && s != ".." && s != ".";
    if !safe(kind) || !safe(sub) {
        return None;
    }
    let xml = std::fs::read_to_string(format!("{base}/{kind}/{sub}.xml")).ok()?;
    let doc = roxmltree::Document::parse(&xml).ok()?;
    let lang = ("http://www.w3.org/XML/1998/namespace", "lang");
    (doc.descendants())
        .find(|n| n.has_tag_name("comment") && n.attribute(lang).is_none())
        .and_then(|n| n.text())
        .map(str::to_owned)
}

#[cfg(target_os = "macos")]
fn describe_all(exts: &[&str]) -> HashMap<String, String> {
    (exts.iter())
        .filter_map(|e| mac::description(e).map(|d| (e.to_string(), d)))
        .collect()
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void, CString};
    type Ref = *const c_void;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: Ref, text: *const c_char, encoding: u32) -> Ref;
        fn CFStringGetCString(text: Ref, out: *mut c_char, size: isize, encoding: u32) -> bool;
        fn CFRelease(value: Ref);
    }

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        static kUTTagClassFilenameExtension: Ref;
        fn UTTypeCreatePreferredIdentifierForTag(class: Ref, tag: Ref, parent: Ref) -> Ref;
        fn UTTypeCopyDescription(id: Ref) -> Ref;
    }

    pub fn description(ext: &str) -> Option<String> {
        let text = CString::new(ext).ok()?;
        // SAFETY: every Create/Copy result is null-checked and released exactly once.
        unsafe {
            let tag = CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8);
            if tag.is_null() {
                return None;
            }
            let id = UTTypeCreatePreferredIdentifierForTag(
                kUTTagClassFilenameExtension,
                tag,
                std::ptr::null(),
            );
            CFRelease(tag);
            if id.is_null() {
                return None;
            }
            let desc = UTTypeCopyDescription(id);
            CFRelease(id);
            if desc.is_null() {
                return None;
            }
            let mut buf = vec![0u8; 1024];
            let ok = CFStringGetCString(desc, buf.as_mut_ptr().cast(), buf.len() as isize, UTF8);
            CFRelease(desc);
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            ok.then(|| String::from_utf8_lossy(&buf[..end]).into_owned())
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn describe_all(_: &[&str]) -> HashMap<String, String> {
    HashMap::new()
}
