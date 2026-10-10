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

/// How long ago unix second `then` was, at `now` (seconds): `5 min ago`, `3 h ago`,
/// `2 d ago`; `never` for None.
pub fn ago(then: Option<i64>, now: f64) -> String {
    let Some(then) = then else {
        return "never".into();
    };
    let secs = (now as i64 - then).max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
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

/// `node://<device>/<source>/rest` for people: "<device> / <source label> / rest", with
/// the device names from `devices.list` (id, label) and the labels of the library's sources
/// (root, label). None for other paths and for a device not in `devices` (forgotten: its
/// id is all there is). A device without a name shows the start of its id.
pub fn node_title(
    path: &str,
    devices: &[(&str, &str)],
    sources: &[(&str, &str)],
) -> Option<String> {
    let rest = path.strip_prefix("node://")?;
    let (device, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let label = devices.iter().find(|(id, _)| *id == device)?.1.trim();
    let name = match label {
        "" => device.chars().take(8).collect(),
        label => label.to_owned(),
    };
    let rest = rest.trim_matches('/');
    let (source, rest) = rest.split_once('/').unwrap_or((rest, ""));
    if source.is_empty() {
        return Some(name);
    }
    let root = format!("node://{device}/{source}");
    let source = (sources.iter())
        .find(|(r, _)| r.trim_end_matches('/') == root)
        .map_or(source, |(_, l)| l);
    Some(match rest {
        "" => format!("{name} / {source}"),
        rest => format!("{name} / {source} / {rest}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_paths_read_as_device_and_source_names() {
        let devices = [("abc123def456", "Laptop"), ("0123456789ab", " ")];
        let sources = [("node://abc123def456/s1", "Photos")];
        let t = |p: &str| node_title(p, &devices, &sources);
        assert_eq!(t("node://abc123def456/").as_deref(), Some("Laptop"));
        assert_eq!(
            t("node://abc123def456/s1").as_deref(),
            Some("Laptop / Photos")
        );
        assert_eq!(
            t("node://abc123def456/s1/2026/june").as_deref(),
            Some("Laptop / Photos / 2026/june")
        );
        assert_eq!(t("node://abc123def456/s2").as_deref(), Some("Laptop / s2"));
        assert_eq!(t("node://0123456789ab/").as_deref(), Some("01234567"));
        // A forgotten device keeps its id; other paths are not node paths.
        assert_eq!(t("node://ffff/s1"), None);
        assert_eq!(t("library://s1/a"), None);
    }

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
    fn ages() {
        let now = 1_000_000.0;
        assert_eq!(ago(None, now), "never");
        assert_eq!(ago(Some(999_990), now), "just now");
        assert_eq!(ago(Some(1_000_100), now), "just now", "clock skew");
        assert_eq!(ago(Some(1_000_000 - 300), now), "5 min ago");
        assert_eq!(ago(Some(1_000_000 - 3 * 3600), now), "3 h ago");
        assert_eq!(ago(Some(1_000_000 - 2 * 86400), now), "2 d ago");
    }

    #[test]
    fn sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(5 << 30), "5.0 GB");
    }
}
