pub(crate) fn valid_path(path: &str) -> bool {
    path.is_empty()
        || (path.len() <= 16 * 1024
            && path.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.ends_with([' ', '.'])
                    && !part
                        .chars()
                        .any(|c| c.is_control() || matches!(c, '\\' | ':' | '%'))
            }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_or_escaping_paths() {
        for path in [
            "../file",
            "a/../file",
            "/file",
            "a\\file",
            "a//file",
            "a/./file",
            "a/",
            "disk:file",
            "a\0b",
        ] {
            assert!(!valid_path(path), "accepted {path:?}");
        }
        for path in ["", "photos", "photos/file", "space here/file", "café/file"] {
            assert!(valid_path(path));
        }
    }

    #[test]
    fn subtree_matches_components_and_never_ancestors_or_siblings() {
        assert!(contains("", "photos/file"));
        assert!(contains("photos", "photos"));
        assert!(contains("photos", "photos/file"));
        assert!(!contains("photos", "photos-private/file"));
        assert!(!contains("photos", ""));
        assert!(!contains("photos", "photos/../private"));
    }
}
