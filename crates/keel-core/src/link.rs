//! `keel:` links, Keel's URL protocol: `keel:search?q=<query>[&path=<folder>]` opens a search
//! (in that folder), `keel:open?path=<folder>` opens a folder. Links come from web pages,
//! mail and other apps, so a link can only search or show a folder: there is no action that
//! runs, changes or deletes anything, a file's path opens its folder with the file selected
//! (never the file), and only local absolute paths are taken. A network path
//! (`\\host\share`, `//host/share`) is refused, because opening one makes the system
//! contact that host and offer it the user's credentials.

use std::fmt;
use std::path::PathBuf;

/// The URL scheme (`keel:`).
pub const SCHEME: &str = "keel";
/// Longest link taken (characters), and longest search after decoding.
pub const MAX_LEN: usize = 4096;

/// What a `keel:` link asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// Open a search tab with `query`, in `path` when given.
    Search {
        query: String,
        path: Option<PathBuf>,
    },
    /// Show the folder `path` (a file: its folder, with it selected).
    Open { path: PathBuf },
}

/// Why a link was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    NotKeel,
    TooLong,
    UnknownAction(String),
    Missing(&'static str),
    Repeated(String),
    BadEncoding,
    BadPath(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotKeel => write!(f, "not a keel: link"),
            Self::TooLong => write!(f, "the link is longer than {MAX_LEN} characters"),
            Self::UnknownAction(a) => {
                write!(f, "unknown keel: action {a:?} (search and open are known)")
            }
            Self::Missing(k) => write!(f, "the link has no {k}"),
            Self::Repeated(k) => write!(f, "the link gives {k} more than once"),
            Self::BadEncoding => write!(f, "the link is not valid percent-encoded UTF-8"),
            Self::BadPath(why) => write!(f, "the link's path is refused: {why}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// Parses a `keel:` link (`keel:`, `keel:/` or `keel://` before the action; the action and
/// the scheme in any case; `+` is a space in values; a `#fragment` is ignored, as are
/// unknown parameters). Paths are checked for this platform ([`parse_for`]).
pub fn parse(url: &str) -> Result<Link, LinkError> {
    parse_for(url, cfg!(windows))
}

/// As [`parse`], with Windows (`windows`) or Unix path rules.
pub fn parse_for(url: &str, windows: bool) -> Result<Link, LinkError> {
    let url = url.trim();
    if url.chars().count() > MAX_LEN {
        return Err(LinkError::TooLong);
    }
    let rest = url
        .get(..SCHEME.len() + 1)
        .filter(|head| head.eq_ignore_ascii_case("keel:"))
        .map(|_| &url[SCHEME.len() + 1..])
        .ok_or(LinkError::NotKeel)?;
    let rest = rest.split('#').next().unwrap_or("");
    let (action, query) = rest.split_once('?').unwrap_or((rest, ""));
    let action = action.trim_matches('/').to_ascii_lowercase();
    let params = params(query)?;
    let get =
        |key: &str| -> Option<&String> { params.iter().find(|(k, _)| k == key).map(|(_, v)| v) };
    match action.as_str() {
        "search" => {
            let query = get("q").ok_or(LinkError::Missing("q (the search)"))?;
            let query: String = query
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            let query = query.trim().to_owned();
            if query.is_empty() {
                return Err(LinkError::Missing("q (the search)"));
            }
            let path = get("path").map(|p| local_path(p, windows)).transpose()?;
            Ok(Link::Search { query, path })
        }
        "open" => {
            let path = get("path").ok_or(LinkError::Missing("path"))?;
            Ok(Link::Open {
                path: local_path(path, windows)?,
            })
        }
        _ => Err(LinkError::UnknownAction(action.chars().take(40).collect())),
    }
}

/// `key=value` pairs, decoded; a key given twice is refused (which one would win is
/// ambiguous).
fn params(query: &str) -> Result<Vec<(String, String)>, LinkError> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = (decode(k)?.to_ascii_lowercase(), decode(v)?);
        if out.iter().any(|(seen, _)| *seen == k) {
            return Err(LinkError::Repeated(k.chars().take(40).collect()));
        }
        out.push((k, v));
    }
    Ok(out)
}

/// Percent-decoding with `+` as a space.
fn decode(s: &str) -> Result<String, LinkError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = (bytes.get(i + 1..i + 3))
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                    .ok_or(LinkError::BadEncoding)?;
                let hex = std::str::from_utf8(hex).map_err(|_| LinkError::BadEncoding)?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| LinkError::BadEncoding)?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| LinkError::BadEncoding)
}

/// A local absolute path: a drive path (`C:\x`, `C:/x`) on Windows, `/x` elsewhere. No
/// network or device paths, no `..`, no control characters.
fn local_path(p: &str, windows: bool) -> Result<PathBuf, LinkError> {
    let bad = |why: &str| Err(LinkError::BadPath(why.into()));
    if p.is_empty() {
        return bad("it is empty");
    }
    if p.chars().any(char::is_control) {
        return bad("it holds control characters");
    }
    let sep = |c: char| c == '/' || (windows && c == '\\');
    if p.starts_with(&sep) && p[1..].starts_with(&sep) {
        return bad("network and device paths are not opened from links");
    }
    if p.split(sep).any(|part| part == "..") {
        return bad("it holds ..");
    }
    if windows {
        let b = p.as_bytes();
        let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(b[2] as char);
        if !drive {
            return bad("it is not a full path with a drive letter");
        }
        // A colon past the drive names a stream or device.
        if p[2..].contains(':') {
            return bad("it holds a colon");
        }
    } else if !p.starts_with('/') {
        return bad("it is not a full path");
    }
    Ok(PathBuf::from(p))
}

impl Link {
    /// The link as a URL that [`parse`] reads back as this link.
    pub fn to_url(&self) -> String {
        match self {
            Link::Search { query, path } => {
                let mut url = format!("{SCHEME}:search?q={}", encode(query));
                if let Some(path) = path {
                    url.push_str("&path=");
                    url.push_str(&encode(&path.to_string_lossy()));
                }
                url
            }
            Link::Open { path } => {
                format!("{SCHEME}:open?path={}", encode(&path.to_string_lossy()))
            }
        }
    }
}

/// Percent-encodes everything but unreserved characters and `/`, `\`, `:`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b'\\'
            | b':' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(url: &str) -> Result<Link, LinkError> {
        parse_for(url, true)
    }
    fn unix(url: &str) -> Result<Link, LinkError> {
        parse_for(url, false)
    }

    #[test]
    fn parses_search_links() {
        assert_eq!(
            win("keel:search?q=invoice+2026"),
            Ok(Link::Search {
                query: "invoice 2026".into(),
                path: None
            })
        );
        assert_eq!(
            win("KEEL://Search/?q=%22two%20words%22%20ext%3Apdf&path=C%3A%5CUsers%5Cx#top"),
            Ok(Link::Search {
                query: "\"two words\" ext:pdf".into(),
                path: Some(PathBuf::from(r"C:\Users\x"))
            })
        );
        assert_eq!(
            unix("keel:search?path=/home/x&q=r%C3%A9sum%C3%A9&utm=ignored"),
            Ok(Link::Search {
                query: "résumé".into(),
                path: Some(PathBuf::from("/home/x"))
            })
        );
        // Control characters become spaces.
        assert_eq!(
            unix("keel:search?q=a%0Ab%00c"),
            Ok(Link::Search {
                query: "a b c".into(),
                path: None
            })
        );
    }

    #[test]
    fn parses_open_links() {
        assert_eq!(
            win("keel:open?path=D:/Photos/2026"),
            Ok(Link::Open {
                path: PathBuf::from("D:/Photos/2026")
            })
        );
        assert_eq!(
            unix("keel:open?path=%2Ftmp%2Fa%20b"),
            Ok(Link::Open {
                path: PathBuf::from("/tmp/a b")
            })
        );
    }

    #[test]
    fn refuses_bad_links() {
        assert_eq!(win("http://x"), Err(LinkError::NotKeel));
        assert_eq!(win("keelx:search?q=a"), Err(LinkError::NotKeel));
        assert_eq!(win(""), Err(LinkError::NotKeel));
        assert!(matches!(
            win("keel:run?path=C:\\x.exe"),
            Err(LinkError::UnknownAction(a)) if a == "run"
        ));
        assert_eq!(
            win("keel:search"),
            Err(LinkError::Missing("q (the search)"))
        );
        assert_eq!(
            win("keel:search?q=+%20+"),
            Err(LinkError::Missing("q (the search)"))
        );
        assert_eq!(win("keel:open"), Err(LinkError::Missing("path")));
        assert_eq!(
            win("keel:search?q=a&Q=b"),
            Err(LinkError::Repeated("q".into()))
        );
        for bad in ["%", "%4", "%zz", "%+1", "%C3%28", "%FF"] {
            assert_eq!(
                win(&format!("keel:search?q={bad}")),
                Err(LinkError::BadEncoding),
                "{bad}"
            );
        }
        assert_eq!(
            win(&format!("keel:search?q={}", "a".repeat(MAX_LEN))),
            Err(LinkError::TooLong)
        );
    }

    #[test]
    fn refuses_network_relative_and_device_paths() {
        for bad in [
            r"\\host\share",
            "//host/share",
            r"\/host/share",
            r"\\?\C:\x",
            r"\\.\PhysicalDrive0",
            "relative",
            r"\rooted",
            "C:relative",
            r"C:\a\..\..\Windows",
            r"C:\x:stream",
            "",
            "C:\\a\u{7}b",
        ] {
            let url = format!("keel:open?path={}", encode(bad));
            assert!(
                matches!(
                    win(&url),
                    Err(LinkError::BadPath(_)) | Err(LinkError::Missing(_))
                ),
                "{bad}: {:?}",
                win(&url)
            );
            let url = format!("keel:search?q=x&path={}", encode(bad));
            assert!(win(&url).is_err(), "{bad}");
        }
        for bad in ["//host/share", "relative", "/a/../etc", "C:/x"] {
            let url = format!("keel:open?path={}", encode(bad));
            assert!(unix(&url).is_err(), "{bad}");
        }
        // A backslash is an ordinary character in a Unix name.
        assert!(unix(r"keel:open?path=/a\b").is_ok());
    }

    #[test]
    fn links_round_trip() {
        let links = [
            Link::Search {
                query: "a & b = c % d + e#f?g".into(),
                path: Some(PathBuf::from(r"C:\Users\x y")),
            },
            Link::Search {
                query: "日本語 ext:pdf".into(),
                path: None,
            },
            Link::Open {
                path: PathBuf::from(r"D:\a\b"),
            },
        ];
        for link in links {
            assert_eq!(win(&link.to_url()), Ok(link.clone()), "{}", link.to_url());
        }
    }
}
