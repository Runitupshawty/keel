//! Scan filters: what a scan leaves out of its tree.

use super::{attr, UNKNOWN_TIME};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Items a scan leaves out (not walked, not listed, not counted). Path, name, regex,
/// hidden and system filters apply to files and folders (a folder left out takes its
/// subtree with it); size and age filters to files. Names and paths compare without case
/// on Windows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Filters {
    /// These paths (and what is below them).
    pub exclude_paths: Vec<PathBuf>,
    /// Names matching any of these globs (`*` any run of characters, `?` one).
    pub exclude_names: Vec<String>,
    /// Items whose full path matches this regular expression (`regex` syntax; Windows
    /// paths have backslashes).
    pub exclude_path_regex: Option<String>,
    /// Files smaller than this many bytes.
    pub min_size: Option<u64>,
    /// Files last written more than this many seconds before the scan started.
    pub exclude_older_than: Option<u64>,
    /// Files last written less than this many seconds before the scan started.
    pub exclude_newer_than: Option<u64>,
    /// Hidden items (Windows: the hidden attribute; elsewhere a name starting with a dot,
    /// or macOS's hidden flag).
    pub exclude_hidden: bool,
    /// Items with the Windows system attribute (none elsewhere).
    pub exclude_system: bool,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        *self == Filters::default()
    }

    /// Fails when a filter does not compile (the path regular expression), as a scan with
    /// these filters would before walking.
    pub fn check(&self) -> Result<()> {
        Compiled::new(self).map(|_| ())
    }
}

pub(super) struct Compiled {
    paths: Vec<String>,
    names: Vec<Vec<char>>,
    regex: Option<regex::Regex>,
    min_size: u64,
    older_cut: Option<i64>,
    newer_cut: Option<i64>,
    hidden: bool,
    system: bool,
}

impl Compiled {
    pub(super) fn new(f: &Filters) -> Result<Self> {
        let regex = match &f.exclude_path_regex {
            Some(r) if !r.is_empty() => Some(
                regex::RegexBuilder::new(r)
                    .size_limit(1 << 22)
                    .build()
                    .with_context(|| format!("path filter {r:?}"))?,
            ),
            _ => None,
        };
        let now = super::walk::now_ns();
        let ago = |secs: u64| {
            now.saturating_sub(
                i64::try_from(secs)
                    .unwrap_or(i64::MAX / 2)
                    .saturating_mul(1_000_000_000),
            )
        };
        Ok(Compiled {
            paths: (f.exclude_paths.iter())
                .map(|p| key(&std::path::absolute(p).unwrap_or_else(|_| p.clone())))
                .collect(),
            names: (f.exclude_names.iter())
                .filter(|n| !n.is_empty())
                .map(|n| name_key(n).chars().collect())
                .collect(),
            regex,
            min_size: f.min_size.unwrap_or(0),
            older_cut: f.exclude_older_than.map(ago),
            newer_cut: f.exclude_newer_than.map(ago),
            hidden: f.exclude_hidden,
            system: f.exclude_system,
        })
    }

    /// Whether the item named `name` (with `attrs`) at `path()` is left out; `path` is
    /// called only when a path filter needs it.
    pub(super) fn excludes(&self, name: &str, attrs: u32, path: impl FnOnce() -> PathBuf) -> bool {
        if (self.hidden && attrs & attr::HIDDEN != 0) || (self.system && attrs & attr::SYSTEM != 0)
        {
            return true;
        }
        if !self.names.is_empty() {
            let name: Vec<char> = name_key(name).chars().collect();
            if self.names.iter().any(|g| glob(g, &name)) {
                return true;
            }
        }
        if self.paths.is_empty() && self.regex.is_none() {
            return false;
        }
        let path = path();
        let full = path.to_string_lossy();
        if self.regex.as_ref().is_some_and(|r| r.is_match(&full)) {
            return true;
        }
        !self.paths.is_empty() && self.paths.contains(&key(&path))
    }

    /// Whether a file of `size` bytes last written at `mtime` is left out.
    pub(super) fn excludes_file(&self, size: u64, mtime: i64) -> bool {
        if size < self.min_size {
            return true;
        }
        if mtime == UNKNOWN_TIME {
            return false;
        }
        self.older_cut.is_some_and(|cut| mtime < cut)
            || self.newer_cut.is_some_and(|cut| mtime > cut)
    }
}

/// A path as compared: `/` as `\` and lowercase on Windows; no trailing separator (except
/// a root's own).
pub(super) fn key(p: &Path) -> String {
    let mut s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s = s.replace('/', "\\").to_lowercase();
    }
    while s.len() > 1 && s.ends_with(['/', std::path::MAIN_SEPARATOR]) && !s.ends_with(":\\") {
        s.pop();
    }
    s
}

/// A name as compared (lowercase on Windows).
pub(super) fn name_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

/// `*` matches any run of characters, `?` one; everything else itself.
pub(super) fn glob(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || (p[pi] != '*' && p[pi] == s[si])) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str, s: &str) -> bool {
        glob(
            &p.chars().collect::<Vec<_>>(),
            &s.chars().collect::<Vec<_>>(),
        )
    }

    #[test]
    fn globs() {
        assert!(g("*.tmp", "a.tmp"));
        assert!(g("*.tmp", ".tmp"));
        assert!(!g("*.tmp", "a.tmp.txt"));
        assert!(g("node_modules", "node_modules"));
        assert!(!g("node_modules", "node_modules2"));
        assert!(g("~$*", "~$report.docx"));
        assert!(g("a?c", "abc"));
        assert!(!g("a?c", "ac"));
        assert!(g("*", ""));
        assert!(g("**a*b", "xxaxxb"));
        assert!(!g("", "a"));
    }

    #[test]
    fn keys_drop_trailing_separators_but_not_roots() {
        if cfg!(windows) {
            assert_eq!(key(Path::new("C:\\")), "c:\\");
            assert_eq!(key(Path::new("C:/Data/")), "c:\\data");
            assert_eq!(key(Path::new("\\\\server\\share\\")), "\\\\server\\share");
        } else {
            assert_eq!(key(Path::new("/")), "/");
            assert_eq!(key(Path::new("/data/")), "/data");
        }
    }

    #[test]
    fn a_bad_regex_is_an_error() {
        let f = Filters {
            exclude_path_regex: Some("(".into()),
            ..Filters::default()
        };
        assert!(Compiled::new(&f).is_err());
    }
}
