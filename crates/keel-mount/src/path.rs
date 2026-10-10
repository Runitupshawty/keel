//! Mount paths: what the OS asks for (WinFsp's `\a\b.txt`, FUSE's names) and the source
//! VPath behind it, with the naming rules of the side that serves the mount.

use keel_vfs::VPath;
use std::io;

/// A path inside a mount: plain names joined with `/`, "" for the mount root. Built only
/// by [`MountPath::parse`] and [`MountPath::join`], so no component is empty, `.` or `..`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct MountPath(String);

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_owned())
}

/// A single name: not empty, `.` or `..`, and without separators or NUL.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// Whether Windows can show `name`: none of `<>:"/\|?*` or control characters, no
/// trailing dot or space, and not a device name (`CON`, `nul.txt`, `COM1`, `LPT¹`, ...).
pub fn windows_name(name: &str) -> bool {
    if !plain_name(name)
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let upper = stem.to_uppercase();
    let device = match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => match upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
        {
            Some(n) => {
                matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                    || matches!(n, "\u{b9}" | "\u{b2}" | "\u{b3}")
            }
            None => false,
        },
    };
    !device
}

impl MountPath {
    pub fn root() -> Self {
        Self::default()
    }

    /// `\a\b`, `/a/b` or `a\b` (either separator, extra ones ignored).
    pub fn parse(raw: &str) -> io::Result<Self> {
        let mut out = Self::root();
        for part in raw.split(['/', '\\']).filter(|p| !p.is_empty()) {
            out = out.join(part)?;
        }
        Ok(out)
    }

    pub fn join(&self, name: &str) -> io::Result<Self> {
        if !plain_name(name) {
            return Err(invalid("not a plain file name"));
        }
        Ok(Self(if self.0.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.0)
        }))
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last component ("" for the root).
    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    pub fn parent(&self) -> Option<MountPath> {
        (!self.is_root()).then(|| Self(self.0.rsplit_once('/').map_or("", |(p, _)| p).into()))
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// `self` is `base` or below it (whole components).
    pub fn within(&self, base: &MountPath) -> bool {
        base.is_root()
            || self.0 == base.0
            || self.0.starts_with(&base.0) && self.0[base.0.len()..].starts_with('/')
    }

    /// `self` with its `from` prefix replaced by `to` (for renames of a folder).
    pub fn rebase(&self, from: &MountPath, to: &MountPath) -> Option<MountPath> {
        if !self.within(from) {
            return None;
        }
        let rest = self.0[from.0.len()..].trim_start_matches('/');
        Some(match (to.is_root(), rest.is_empty()) {
            (_, true) => to.clone(),
            (true, false) => Self(rest.into()),
            (false, false) => Self(format!("{}/{rest}", to.0)),
        })
    }
}

/// Maps mount paths onto the source and filters what the OS gets to see.
#[derive(Clone, Debug)]
pub struct PathMap {
    /// The source root joined with the mounted subtree.
    pub root: VPath,
    /// The mounted subtree relative to the source root ("" for all of it), for the index.
    pub subtree: String,
    /// The mount ignores case (WinFsp) but the source does not (SFTP, cloud, Unix):
    /// names that differ only in case collide.
    pub fold_case: bool,
    /// Names Windows cannot show are left out (the mount is served on Windows).
    pub windows_names: bool,
}

impl PathMap {
    /// `windows`: served on Windows (case-insensitive, Windows names); `source_nocase`: the
    /// source compares names case-insensitively itself (a local Windows folder).
    pub fn new(root: VPath, subtree: &str, windows: bool, source_nocase: bool) -> Self {
        Self {
            root,
            subtree: subtree.trim_matches('/').to_owned(),
            fold_case: windows && !source_nocase,
            windows_names: windows,
        }
    }

    pub fn vpath(&self, p: &MountPath) -> VPath {
        p.components().fold(self.root.clone(), |v, c| v.join(c))
    }

    /// `p` relative to the source root, as the library index stores it.
    pub fn index_rel(&self, p: &MountPath) -> String {
        match (self.subtree.is_empty(), p.is_root()) {
            (true, _) => p.as_str().to_owned(),
            (false, true) => self.subtree.clone(),
            (false, false) => format!("{}/{}", self.subtree, p.as_str()),
        }
    }

    /// The key two mount paths share when the mount takes them for the same file.
    pub fn key(&self, p: &MountPath) -> String {
        if self.windows_names {
            p.as_str().to_lowercase()
        } else {
            p.as_str().to_owned()
        }
    }

    /// Whether the name asked for (`asked`) means the listed `name`.
    pub fn same_name(&self, name: &str, asked: &str) -> bool {
        name == asked || self.windows_names && name.to_lowercase() == asked.to_lowercase()
    }

    /// A listed name the mount shows: never staging files, on Windows only names Windows
    /// can show.
    pub fn shown(&self, name: &str) -> bool {
        plain_name(name)
            && !keel_vfs::ops::is_partial(name)
            && (!self.windows_names || windows_name(name))
    }

    /// Keeps the shown names; with `fold_case` the first of names that differ only in case
    /// (by byte order, so the same one every time).
    pub fn filter<T>(&self, mut items: Vec<T>, name: impl Fn(&T) -> &str) -> Vec<T> {
        items.retain(|i| self.shown(name(i)));
        if self.fold_case {
            items.sort_by(|a, b| name(a).cmp(name(b)));
            let mut seen = std::collections::HashSet::new();
            items.retain(|i| {
                let lower = name(i).to_lowercase();
                let first = seen.insert(lower);
                if !first {
                    tracing::debug!("mount: hiding {} (differs only in case)", name(i));
                }
                first
            });
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_either_separator_and_refuses_dot_dot() {
        let p = MountPath::parse(r"\Photos\2026/a.jpg").unwrap();
        assert_eq!(p.as_str(), "Photos/2026/a.jpg");
        assert_eq!(p.name(), "a.jpg");
        assert_eq!(p.parent().unwrap().as_str(), "Photos/2026");
        assert!(MountPath::parse(r"\").unwrap().is_root());
        assert!(MountPath::root().parent().is_none());
        assert!(MountPath::parse(r"\a\..\b").is_err());
        assert!(MountPath::parse("a/./b").is_err());
        assert!(MountPath::root().join("a/b").is_err());
        assert!(MountPath::root().join("a\0").is_err());
    }

    #[test]
    fn within_and_rebase_use_whole_components() {
        let a = MountPath::parse("a").unwrap();
        let ab = MountPath::parse("a/b").unwrap();
        let ax = MountPath::parse("ax/b").unwrap();
        assert!(ab.within(&a) && a.within(&a) && ab.within(&MountPath::root()));
        assert!(!ax.within(&a));
        let to = MountPath::parse("z/y").unwrap();
        assert_eq!(ab.rebase(&a, &to).unwrap().as_str(), "z/y/b");
        assert_eq!(a.rebase(&a, &to).unwrap(), to);
        assert_eq!(ax.rebase(&a, &to), None);
        assert_eq!(ab.rebase(&a, &MountPath::root()).unwrap().as_str(), "b");
    }

    #[test]
    fn windows_reserved_names() {
        for bad in [
            "CON",
            "con.txt",
            "Nul",
            "aux.tar.gz",
            "COM1",
            "lpt9.log",
            "COM\u{b9}",
            "PRN ",
            "a.",
            "a ",
            "a:b",
            "a?b",
            "a*",
            "a|b",
            "a\"b",
            "<a>",
            "tab\there",
            "conin$",
        ] {
            assert!(!windows_name(bad), "{bad:?} should be refused");
        }
        for ok in [
            "CONSOLE",
            "com10",
            "com0",
            "lpt",
            "a.b",
            ".hidden",
            "x con",
            "naïve.txt",
        ] {
            assert!(windows_name(ok), "{ok:?} should be allowed");
        }
    }

    #[test]
    fn map_joins_the_subtree_and_the_index_path() {
        let root = VPath::parse("sftp://box/home/me").unwrap();
        let map = PathMap::new(root.join("Photos"), "Photos/", true, false);
        let p = MountPath::parse(r"\2026\a.jpg").unwrap();
        assert_eq!(
            map.vpath(&p).display(),
            "sftp://box/home/me/Photos/2026/a.jpg"
        );
        assert_eq!(map.index_rel(&p), "Photos/2026/a.jpg");
        assert_eq!(map.index_rel(&MountPath::root()), "Photos");
        assert!(map.fold_case);
        let whole = PathMap::new(root, "", false, false);
        assert_eq!(whole.index_rel(&p), "2026/a.jpg");
        assert!(!whole.fold_case);
    }

    #[test]
    fn listings_hide_partials_bad_names_and_case_twins() {
        let root = VPath::parse("sftp://box/x").unwrap();
        let names = vec![
            "b.txt",
            "B.txt",
            "a.txt",
            "a.txt.keel-partial-12-3",
            "aux.c",
            "ok:no",
            "notes.keel-partial-draft.txt",
        ];
        let win = PathMap::new(root.clone(), "", true, false);
        assert_eq!(
            win.filter(names.clone(), |n| n),
            vec!["B.txt", "a.txt", "notes.keel-partial-draft.txt"]
        );
        assert!(win.same_name("B.txt", "b.TXT"));
        assert_eq!(win.key(&MountPath::parse("A/B.txt").unwrap()), "a/b.txt");
        // A Windows-local source folds case itself; a FUSE mount shows every name.
        let local = PathMap::new(root.clone(), "", true, true);
        assert_eq!(local.filter(names.clone(), |n| n).len(), 4);
        let fuse = PathMap::new(root, "", false, false);
        assert_eq!(fuse.filter(names, |n| n).len(), 6);
        assert!(!fuse.same_name("B.txt", "b.txt"));
    }
}
