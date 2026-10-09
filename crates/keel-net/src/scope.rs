/// Paths are validated by the host, so Windows aliasing rules (`:` streams,
/// trailing dots/spaces, DOS device names) apply only when the host is Windows.
pub(crate) fn valid_path(path: &str) -> bool {
    valid_path_for(path, cfg!(windows))
}

fn valid_path_for(path: &str, windows: bool) -> bool {
    path.is_empty()
        || (path.len() <= 16 * 1024
            && path.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.chars().any(|c| c.is_control() || c == '\\' || bidi(c))
                    && !(windows
                        && (part.contains(':') || part.ends_with([' ', '.']) || device(part)))
            }))
}

/// Bidirectional embedding/override/isolate controls (U+202A–202E, U+2066–2069)
/// can make a name display differently from what it is.
fn bidi(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Reserved DOS device names, with or without an extension, any case.
fn device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(prefix).is_some_and(|n| {
            matches!(n, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                || matches!(n, "\u{B9}" | "\u{B2}" | "\u{B3}")
        })
    })
}

/// Device and peer labels: at most 256 bytes, no control or bidi characters.
pub(crate) fn valid_label(label: &str) -> bool {
    label.len() <= 256 && !label.chars().any(|c| c.is_control() || bidi(c))
}

pub(crate) fn contains(subtree: &str, path: &str) -> bool {
    valid_path(path)
        && valid_path(subtree)
        && (subtree.is_empty()
            || path == subtree
            || path
                .strip_prefix(subtree)
                .is_some_and(|s| s.starts_with('/')))
}

/// Strictly below `subtree`: the granted root itself cannot be removed,
/// overwritten or renamed.
pub(crate) fn inside(subtree: &str, path: &str) -> bool {
    path != subtree && contains(subtree, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_or_escaping_paths() {
        for windows in [false, true] {
            for path in [
                "../file",
                "a/../file",
                "/file",
                "a\\file",
                "a//file",
                "a/./file",
                "a/",
                "a\0b",
                "evil\u{202E}txt.exe",
                "a/\u{2066}b",
            ] {
                assert!(!valid_path_for(path, windows), "accepted {path:?}");
            }
            for path in [
                "",
                "photos",
                "photos/file",
                "space here/file",
                "café/file",
                "100% done.txt",
                "a%2e%2e/b",
                "console",
                "com10",
                "nullable.txt",
            ] {
                assert!(valid_path_for(path, windows), "rejected {path:?}");
            }
        }
    }

    #[test]
    fn windows_only_rules_cover_streams_trailing_dots_and_devices() {
        for path in [
            "disk:file",
            "a/file.",
            "a/file ",
            "CON",
            "nul.txt",
            "dir/NUL",
            "Com1",
            "lpt9.log",
            "aux .txt",
            "CONIN$",
            "conout$.x",
            "COM\u{B9}",
            "prn.tar.gz",
        ] {
            assert!(!valid_path_for(path, true), "accepted {path:?}");
            assert!(valid_path_for(path, false), "rejected {path:?}");
        }
    }

    #[test]
    fn labels_reject_long_control_and_bidi() {
        assert!(valid_label("Laptop"));
        assert!(valid_label(""));
        assert!(!valid_label(&"x".repeat(257)));
        assert!(!valid_label("a\nb"));
        assert!(!valid_label("abc\u{202E}gpj.exe"));
        assert!(!valid_label("\u{2068}x"));
    }

    #[test]
    fn subtree_matches_components_and_never_ancestors_or_siblings() {
        assert!(contains("", "photos/file"));
        assert!(contains("photos", "photos"));
        assert!(contains("photos", "photos/file"));
        assert!(!contains("photos", "photos-private/file"));
        assert!(!contains("photos", ""));
        assert!(!contains("photos", "photos/../private"));
        assert!(inside("photos", "photos/file"));
        assert!(inside("", "photos"));
        assert!(!inside("photos", "photos"));
        assert!(!inside("", ""));
    }
}
