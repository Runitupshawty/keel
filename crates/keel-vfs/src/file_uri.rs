//! `file://` URIs and `text/uri-list` (RFC 2483): what the Linux clipboard and drag-out
//! offer other apps, and how Keel reads the lists other apps offer.

use anyhow::{ensure, Result};
use std::path::{Path, PathBuf};

/// `file://` URI of an absolute path, every byte but `A-Z a-z 0-9 - . _ ~ /` escaped (so a
/// line break in a name is `%0A` and cannot split a list).
pub fn file_uri(p: &Path) -> String {
    let mut out = String::from("file://");
    for b in path_bytes(p) {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A `text/uri-list` of `paths`: one URI per line, each line ending in CRLF. Refuses an
/// empty list and relative paths (they have no `file://` URI).
pub fn uri_list(paths: &[PathBuf]) -> Result<String> {
    ensure!(!paths.is_empty(), "no files");
    let mut out = String::new();
    for p in paths {
        ensure!(
            path_bytes(p).first() == Some(&b'/'),
            "not an absolute path: {}",
            p.display()
        );
        out.push_str(&file_uri(p));
        out.push_str("\r\n");
    }
    Ok(out)
}

/// The local path of a `file://` URI (`file:///p` or `file://localhost/p`).
pub fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.trim().strip_prefix("file://")?;
    let (host, rest) = rest.split_at(rest.find('/')?);
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    if !rest.starts_with('/') {
        return None;
    }
    let (mut bytes, mut it) = (Vec::new(), rest.bytes());
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    Some(bytes_path(bytes))
}

/// The local paths in URI lines; comments, other schemes and other hosts are skipped.
pub fn uri_paths<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<PathBuf> {
    lines
        .filter(|l| !l.starts_with('#'))
        .filter_map(uri_path)
        .collect()
}

#[cfg(unix)]
fn path_bytes(p: &Path) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()).to_vec()
}
#[cfg(unix)]
fn bytes_path(b: Vec<u8>) -> PathBuf {
    <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b).into()
}
// Tests on other systems: Unix-style paths as text.
#[cfg(not(unix))]
fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().replace('\\', "/").into_bytes()
}
#[cfg(not(unix))]
fn bytes_path(b: Vec<u8>) -> PathBuf {
    String::from_utf8_lossy(&b).into_owned().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_is_case_insensitive() {
        for host in ["localhost", "LOCALHOST", "LocalHost"] {
            assert_eq!(
                uri_path(&format!("file://{host}/tmp/a%20b")),
                Some(PathBuf::from("/tmp/a b"))
            );
        }
        assert_eq!(uri_path("file://localhost.example/tmp/a"), None);
    }

    #[test]
    fn uri_list_escapes_and_round_trips() {
        let paths = [
            PathBuf::from("/home/u/a b.txt"),
            PathBuf::from("/tmp/été/日本#1 100%.md"),
            PathBuf::from("/tmp/two\nlines"),
        ];
        let list = uri_list(&paths).unwrap();
        assert_eq!(
            list,
            "file:///home/u/a%20b.txt\r\n\
             file:///tmp/%C3%A9t%C3%A9/%E6%97%A5%E6%9C%AC%231%20100%25.md\r\n\
             file:///tmp/two%0Alines\r\n"
        );
        // A line break in a name stays inside its URI: three lines, three paths back.
        assert_eq!(list.lines().count(), 3);
        assert_eq!(uri_paths(list.lines()), paths);
        assert!(uri_list(&[PathBuf::from("relative/a")]).is_err());
        assert!(uri_list(&[]).is_err());
    }
}
