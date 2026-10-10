//! Small display helpers (tested natively).

/// `1536` -> `1.5 KB`.
pub fn human(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < units.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

/// The parent of a daemon path (`library://src/a/b`, `D:\x\y`, `/home/x`); None at a root.
pub fn parent(path: &str) -> Option<String> {
    // A URI's root is `scheme://authority/`.
    let root_len = match path.find("://") {
        Some(i) => path[i + 3..]
            .find('/')
            .map_or(path.len(), |j| i + 3 + j + 1),
        None => 0,
    };
    let (root, rest) = path.split_at(root_len);
    let rest = rest.trim_end_matches(['/', '\\']);
    if rest.is_empty() {
        return None;
    }
    match rest.rfind(['/', '\\']) {
        Some(0) => Some(format!("{root}{}", &rest[..1])),
        Some(i) if rest[..i].ends_with(':') => Some(format!("{root}{}", &rest[..=i])),
        Some(i) => Some(format!("{root}{}", &rest[..i])),
        None if !root.is_empty() => Some(root.to_owned()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents() {
        let p = |s: &str| parent(s);
        assert_eq!(p("library://s1/a/b").as_deref(), Some("library://s1/a"));
        assert_eq!(p("library://s1/a").as_deref(), Some("library://s1/"));
        assert_eq!(p("library://s1/"), None);
        assert_eq!(p("library://s1"), None);
        assert_eq!(p(r"D:\x\y").as_deref(), Some(r"D:\x"));
        assert_eq!(p(r"D:\x").as_deref(), Some(r"D:\"));
        assert_eq!(p(r"D:\"), None);
        assert_eq!(p("/home/x/").as_deref(), Some("/home"));
        assert_eq!(p("/home").as_deref(), Some("/"));
        assert_eq!(p("/"), None);
        assert_eq!(p("sftp://host/a/b").as_deref(), Some("sftp://host/a"));
    }

    #[test]
    fn sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(5 << 30), "5.0 GB");
    }
}
