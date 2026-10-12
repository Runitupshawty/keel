//! The Exclude tab compiled for the walk: hidden and system entries, excluded
//! folders, include-only folders, include-only and excluded file wildcards. Rules are
//! checked while indexing, so what they leave out never reaches the index. Matching
//! ignores case, like Everything.

use std::path::Path;

use crate::index_settings::ExcludeSettings;

/// File attribute bits stored with every walk-index entry: the Windows values; on
/// other systems Keel sets `READONLY`, `HIDDEN` (a leading dot), `DIRECTORY` and
/// `REPARSE_POINT` (a symlink) itself.
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const REPARSE_POINT: u32 = 0x400;
}

/// Compiled exclude rules.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExcludeRules {
    hidden: bool,
    system: bool,
    /// Excluded folders by path (normalised, see [`norm`]).
    folder_paths: Vec<String>,
    /// Wildcards on an excluded folder's name.
    folder_names: Vec<String>,
    /// Wildcards on an excluded folder's whole path.
    folder_globs: Vec<String>,
    include_only: Vec<String>,
    only_files: Vec<String>,
    files: Vec<String>,
}

/// Lowercase, `/` separators, no trailing separator (`C:\` is `c:`, `/` is ``).
fn norm(path: &str) -> String {
    let mut s = path.replace('\\', "/").to_lowercase();
    while s.ends_with('/') {
        s.pop();
    }
    s
}

fn has_wildcard(s: &str) -> bool {
    s.contains(['*', '?'])
}

impl ExcludeRules {
    pub fn new(s: &ExcludeSettings) -> Self {
        let mut r = Self {
            hidden: s.hidden,
            system: s.system,
            ..Self::default()
        };
        for f in s.folders.iter().map(|f| f.trim()).filter(|f| !f.is_empty()) {
            let n = norm(f);
            if !f.contains(['/', '\\']) {
                r.folder_names.push(n);
            } else if has_wildcard(f) {
                r.folder_globs.push(n);
            } else {
                r.folder_paths.push(n);
            }
        }
        let globs = |list: &[String]| -> Vec<String> {
            list.iter()
                .map(|g| g.trim().to_lowercase())
                .filter(|g| !g.is_empty())
                .collect()
        };
        r.include_only = (s.include_only_folders.iter())
            .map(|f| norm(f.trim()))
            .filter(|f| !f.is_empty())
            .collect();
        r.only_files = globs(&s.include_only_files);
        r.files = globs(&s.files);
        r
    }

    /// True when nothing is excluded.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// True when [`admit`](Self::admit) looks at attributes (a stat per entry).
    pub fn needs_attrs(&self) -> bool {
        self.hidden || self.system
    }

    /// A stable text of the rules, saved with an index so a change rebuilds it.
    pub fn key(&self) -> String {
        format!("{self:?}")
    }

    /// Whether the entry at `path` belongs in the index. `attrs` are [`attr`] bits.
    /// A folder that is not admitted is left out with everything below it.
    pub fn admit(&self, path: &Path, is_dir: bool, attrs: u32) -> bool {
        if (self.hidden && attrs & attr::HIDDEN != 0) || (self.system && attrs & attr::SYSTEM != 0)
        {
            return false;
        }
        let full = norm(&path.to_string_lossy());
        let name = full.rsplit('/').next().unwrap_or(&full);
        let under = |dir: &str, p: &str| {
            p == dir || (p.len() > dir.len() && p.starts_with(dir) && p[dir.len()..].starts_with('/'))
        };
        if is_dir
            && (self.folder_paths.iter().any(|f| under(f, &full))
                || self.folder_names.iter().any(|g| wildcard(g, name))
                || self.folder_globs.iter().any(|g| wildcard(g, &full)))
        {
            return false;
        }
        if !self.include_only.is_empty() {
            let inside = self.include_only.iter().any(|f| under(f, &full));
            // A folder above an include-only folder is kept so the path leads there.
            let above = is_dir && self.include_only.iter().any(|f| under(&full, f));
            if !inside && !above {
                return false;
            }
        }
        if is_dir {
            return true;
        }
        if !self.only_files.is_empty() && !self.only_files.iter().any(|g| wildcard(g, name)) {
            return false;
        }
        !self.files.iter().any(|g| wildcard(g, name))
    }
}

/// `*` (any run) and `?` (one character) over the whole of `text`; both already
/// lowercase.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rules(f: impl FnOnce(&mut ExcludeSettings)) -> ExcludeRules {
        let mut s = ExcludeSettings::default();
        f(&mut s);
        ExcludeRules::new(&s)
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn wildcards_match_the_whole_text() {
        assert!(wildcard("*.pdf", "report.pdf"));
        assert!(!wildcard("*.pdf", "report.pdf.txt"));
        assert!(wildcard("re?ort*", "report.pdf"));
        assert!(wildcard("*", ""));
        assert!(wildcard("a*b*c", "axxbyyc"));
        assert!(!wildcard("a*b*c", "axxbyy"));
        assert!(wildcard("**x", "x"));
        assert!(!wildcard("", "x"));
        assert!(wildcard("é*", "été"));
    }

    #[test]
    fn hidden_and_system_attributes() {
        let r = rules(|s| s.hidden = true);
        assert!(!r.admit(&p("/r/.git"), true, attr::HIDDEN | attr::DIRECTORY));
        assert!(r.admit(&p("/r/a.txt"), false, attr::SYSTEM));
        let r = rules(|s| s.system = true);
        assert!(!r.admit(&p("/r/pagefile.sys"), false, attr::SYSTEM | attr::HIDDEN));
        assert!(r.admit(&p("/r/.git"), true, attr::HIDDEN));
        assert!(rules(|_| {}).is_empty());
        assert!(!r.is_empty());
    }

    #[test]
    fn excluded_folders_by_path_name_or_wildcard() {
        let r = rules(|s| {
            s.folders = vec![
                r"C:\Data\Cache\".into(),
                "node_modules".into(),
                "*.tmpdir".into(),
                r"*\build\out".into(),
            ]
        });
        assert!(!r.admit(&p(r"c:\data\cache"), true, 0));
        assert!(!r.admit(&p(r"C:\DATA\Cache\deep"), true, 0), "below it too");
        assert!(r.admit(&p(r"C:\Data\Cached"), true, 0), "not a prefix of a name");
        assert!(!r.admit(&p("/src/app/node_modules"), true, 0));
        assert!(r.admit(&p("/src/app/node_modules"), false, 0), "a file of that name");
        assert!(!r.admit(&p("/x/Y.TmpDir"), true, 0));
        assert!(!r.admit(&p(r"D:\proj\build\out"), true, 0));
        assert!(r.admit(&p(r"D:\proj\build"), true, 0));
    }

    #[test]
    fn include_only_folders_keep_the_way_there() {
        let r = rules(|s| s.include_only_folders = vec!["/vol/Photos".into(), "/vol/b/c".into()]);
        assert!(r.admit(&p("/vol/photos"), true, 0));
        assert!(r.admit(&p("/vol/Photos/2024/a.jpg"), false, 0));
        assert!(r.admit(&p("/vol/b"), true, 0), "above an include-only folder");
        assert!(!r.admit(&p("/vol/b/x.txt"), false, 0), "a file beside it");
        assert!(!r.admit(&p("/vol/music"), true, 0));
        assert!(!r.admit(&p("/vol/photosx"), true, 0));
    }

    #[test]
    fn file_wildcards_include_only_then_exclude() {
        let r = rules(|s| {
            s.include_only_files = vec!["*.JPG".into(), "*.png".into()];
            s.files = vec!["thumb*".into()];
        });
        assert!(r.admit(&p("/a/IMG_1.jpg"), false, 0));
        assert!(!r.admit(&p("/a/notes.txt"), false, 0));
        assert!(!r.admit(&p("/a/thumb1.png"), false, 0));
        assert!(r.admit(&p("/a/thumbs"), true, 0), "folders are kept");
        assert_ne!(r.key(), rules(|_| {}).key());
    }
}
