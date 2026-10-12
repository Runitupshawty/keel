//! The folders the ETP server serves, SUBST-style: each configured root appears as a top
//! folder under its name (`/Media/...` for a root named `Media`; a root named `C:` for
//! `C:\` gives Everything's own layout), and nothing outside the roots can be named, found
//! or read. Every path a client names is resolved without the file system first (`..`
//! never climbs above `/`), then canonicalized and checked to be inside its root's
//! canonical folder (a link inside a root that points out of it is refused).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One served folder.
#[derive(Clone, Debug)]
pub struct Root {
    /// Its top folder's name (unique, any case).
    pub name: String,
    /// As configured (and as the index names it).
    pub path: PathBuf,
    /// Canonical, without Windows' `\\?\` prefix.
    canonical: PathBuf,
}

/// The served folders.
#[derive(Clone, Debug, Default)]
pub struct Roots(Vec<Root>);

/// The name a root gets when none is configured: `C:` for a drive's root, else its folder
/// name.
pub fn default_name(path: &Path) -> String {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        let s = s.trim_end_matches(['\\', '/']);
        if s.len() == 2 && s.as_bytes()[1] == b':' {
            return s.to_ascii_uppercase();
        }
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Windows' `\\?\` (and `\\?\UNC\`) prefix off a canonical path.
fn plain(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p
}

fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map(plain)
}

/// A path as text for prefix checks: `/` separators, lowercase on Windows (whose names
/// ignore case), no trailing separator.
fn key(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = if cfg!(windows) {
        s.replace('\\', "/").to_lowercase()
    } else {
        s.into_owned()
    };
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        "/".to_owned()
    } else {
        t.to_owned()
    }
}

/// `p` relative to `base` (both as [`key`]s; `p` original case kept), when inside it.
fn relative<'a>(p: &'a str, p_key: &str, base: &str) -> Option<&'a str> {
    if p_key == base {
        return Some("");
    }
    let prefix = if base.ends_with('/') {
        base.to_owned()
    } else {
        format!("{base}/")
    };
    // Lowercasing keeps char boundaries for the text it compares only when lengths agree.
    (p_key.starts_with(&prefix) && p_key.len() == p.len()).then(|| &p[prefix.len()..])
}

/// A name a top folder can have: nonempty, no separators or control characters, not `.`
/// or `..`.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && n != "."
        && n != ".."
        && !n.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}

impl Roots {
    /// Checks the roots: each an existing folder, names unique (any case) and valid.
    pub fn new(list: Vec<(String, PathBuf)>) -> Result<Roots> {
        let mut roots: Vec<Root> = Vec::new();
        for (name, path) in list {
            let name = if name.trim().is_empty() {
                default_name(&path)
            } else {
                name.trim().to_owned()
            };
            anyhow::ensure!(
                valid_name(&name),
                "[[etp.roots]] {}: give it a `name` (no '/' or '\\')",
                path.display()
            );
            anyhow::ensure!(
                path.is_absolute(),
                "[[etp.roots]] {}: the path must be absolute",
                path.display()
            );
            let canonical =
                canonical(&path).with_context(|| format!("[[etp.roots]] {}", path.display()))?;
            anyhow::ensure!(
                canonical.is_dir(),
                "[[etp.roots]] {}: not a folder",
                path.display()
            );
            if roots.iter().any(|r| r.name.eq_ignore_ascii_case(&name)) {
                bail!("[[etp.roots]]: two roots are named {name}");
            }
            roots.push(Root {
                name,
                path,
                canonical,
            });
        }
        Ok(Roots(roots))
    }

    pub fn list(&self) -> &[Root] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn by_name(&self, name: &str) -> Option<&Root> {
        self.0.iter().find(|r| r.name.eq_ignore_ascii_case(name))
    }

    /// A found path (as the index names it) in the server's names: (its folder as `QUERY`
    /// writes it, `Media\Music`; its name). None outside every root, or when a name could
    /// not be sent (a line break) or named back (a `\` on a system whose names may hold
    /// one).
    pub fn locate(&self, real: &Path) -> Option<(String, String)> {
        let text = real.to_string_lossy();
        if text.contains(['\r', '\n', '\0']) {
            return None;
        }
        let text = if cfg!(windows) {
            text.replace('\\', "/")
        } else {
            text.into_owned()
        };
        let text = text.trim_end_matches('/');
        let text_key = key(Path::new(text));
        for root in &self.0 {
            for base in [key(&root.path), key(&root.canonical)] {
                let Some(rel) = relative(text, &text_key, &base) else {
                    continue;
                };
                if !cfg!(windows) && rel.contains('\\') {
                    return None;
                }
                let mut parts: Vec<&str> = std::iter::once(root.name.as_str())
                    .chain(rel.split('/').filter(|p| !p.is_empty()))
                    .collect();
                let name = parts.pop().unwrap_or_default().to_owned();
                return Some((parts.join("\\"), name));
            }
        }
        None
    }
}

/// A path in the server's names: its parts below `/`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Virtual(pub Vec<String>);

impl Virtual {
    /// `arg` from the current folder `cwd`: absolute when it starts with `/` (or `\`),
    /// `.` and empty parts skipped, `..` one up (never above `/`). `\` separates too on
    /// Windows, whose names cannot hold one.
    pub fn resolve(cwd: &Virtual, arg: &str) -> Virtual {
        let sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
        let mut parts = if arg.starts_with(sep) {
            Vec::new()
        } else {
            cwd.0.clone()
        };
        for p in arg.split(sep) {
            match p {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                p => parts.push(p.to_owned()),
            }
        }
        Virtual(parts)
    }

    /// `/Media/Music`.
    pub fn display(&self) -> String {
        format!("/{}", self.0.join("/"))
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The last part (`/` for the top).
    pub fn name(&self) -> &str {
        self.0.last().map_or("/", String::as_str)
    }
}

/// What a server path is.
#[derive(Debug)]
pub enum Target {
    /// `/`: the roots.
    Top,
    /// A file or folder inside a root: its canonical path.
    Real(PathBuf),
}

impl Roots {
    /// The file or folder `v` names, if it exists inside its root.
    pub fn real(&self, v: &Virtual) -> Result<Target> {
        let Some((first, rest)) = v.0.split_first() else {
            return Ok(Target::Top);
        };
        let root = self.by_name(first).context("not found")?;
        let mut path = root.canonical.clone();
        for part in rest {
            // A drive, a stream (`name:stream`) or a separator smuggled into one part.
            anyhow::ensure!(
                !part.contains(['/', '\0']) && !(cfg!(windows) && part.contains([':', '\\'])),
                "not found"
            );
            path.push(part);
        }
        let real = canonical(&path).map_err(|_| anyhow::anyhow!("not found"))?;
        anyhow::ensure!(
            key(&real) == key(&root.canonical)
                || key(&real)
                    .starts_with(&format!("{}/", key(&root.canonical).trim_end_matches('/'))),
            "not found"
        );
        Ok(Target::Real(real))
    }
}

/// An entry of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl Entry {
    pub fn of(name: String, meta: &std::fs::Metadata) -> Entry {
        Entry {
            name,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
        }
    }
}

/// What `/` lists: the roots, as folders.
pub fn top(roots: &Roots) -> Vec<Entry> {
    roots
        .list()
        .iter()
        .map(|r| Entry {
            name: r.name.clone(),
            is_dir: true,
            size: 0,
            modified: std::fs::metadata(&r.canonical)
                .and_then(|m| m.modified())
                .ok(),
        })
        .collect()
}

/// A folder's entries (names that cannot be sent, with a line break, left out), sorted.
pub fn read_dir(path: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(path)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.contains(['\r', '\n']) || (!cfg!(windows) && name.contains('\\')) {
            continue;
        }
        // Links followed for their kind and size, as a client opening them would see.
        let Ok(meta) = std::fs::metadata(e.path()) else {
            continue;
        };
        out.push(Entry::of(name, &meta));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Seconds since 1970 (0 when unknown or before).
fn unix(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

fn utc(t: Option<SystemTime>) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(unix(t), 0).unwrap_or_default()
}

/// `YYYYMMDDHHMMSS` (UTC), as `MDTM` and `MLSD` give times.
pub fn mdtm(t: Option<SystemTime>) -> String {
    utc(t).format("%Y%m%d%H%M%S").to_string()
}

/// A `LIST` line, UNIX `ls -l` style (UTC; the year for times over six months away).
pub fn list_line(e: &Entry, now: SystemTime) -> String {
    let perm = if e.is_dir { "drwxr-xr-x" } else { "-rw-r--r--" };
    let t = utc(e.modified);
    let age = unix(Some(now)) - unix(e.modified);
    let date = if age.abs() < 180 * 86_400 {
        t.format("%b %d %H:%M").to_string()
    } else {
        t.format("%b %d  %Y").to_string()
    };
    format!("{perm} 1 owner group {:>13} {date} {}", e.size, e.name)
}

/// An `MLSD` line (and `MLST`'s fact line without its leading space).
pub fn mlsd_line(e: &Entry) -> String {
    let kind = if e.is_dir { "dir" } else { "file" };
    let size = if e.is_dir {
        String::new()
    } else {
        format!("size={};", e.size)
    };
    format!("type={kind};modify={};{size} {}", mdtm(e.modified), e.name)
}

/// Windows file attributes (`FILE_ATTRIBUTE_*`), as Everything's `ATTRIBUTES` column;
/// elsewhere folder (16), archive (32) and read-only (1).
pub fn attributes(meta: &std::fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let mut a = if meta.is_dir() { 16 } else { 32 };
        if meta.permissions().readonly() {
            a |= 1;
        }
        a
    }
}

/// Seconds since 1601 in 100 ns steps (a Windows FILETIME).
pub fn filetime(t: Option<SystemTime>) -> Option<u64> {
    t.map(crate::everything::filetime)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> (tempfile::TempDir, Roots) {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir_all(media.join("Music")).unwrap();
        std::fs::write(media.join("Music/a.mp3"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("secret")).unwrap();
        std::fs::write(dir.path().join("secret/key.txt"), b"k").unwrap();
        let roots = Roots::new(vec![(String::new(), media)]).unwrap();
        (dir, roots)
    }

    #[test]
    fn paths_stay_inside_the_roots() {
        let (dir, roots) = roots();
        assert_eq!(roots.list()[0].name, "media");
        let v = |s: &str| Virtual::resolve(&Virtual::default(), s);
        assert!(matches!(roots.real(&v("/")).unwrap(), Target::Top));
        assert!(matches!(
            roots.real(&v("/MEDIA/Music/a.mp3")).unwrap(),
            Target::Real(_)
        ));
        // `..` never climbs above `/`.
        assert_eq!(v("/media/../../../secret"), v("/secret"));
        assert!(roots.real(&v("/media/../../secret/key.txt")).is_err());
        assert!(roots.real(&v("/secret/key.txt")).is_err());
        assert!(roots.real(&v("/media/nope")).is_err());
        let cwd = v("/media/Music");
        assert_eq!(
            Virtual::resolve(&cwd, "a.mp3").display(),
            "/media/Music/a.mp3"
        );
        assert_eq!(Virtual::resolve(&cwd, "..").display(), "/media");
        assert_eq!(Virtual::resolve(&cwd, "/").display(), "/");
        // A link inside the root to a folder outside it is refused.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("secret"), dir.path().join("media/out"))
                .unwrap();
            assert!(roots.real(&v("/media/out/key.txt")).is_err());
        }
        // Found paths in the server's names.
        let found = dir.path().join("media").join("Music").join("a.mp3");
        assert_eq!(
            roots.locate(&found),
            Some(("media\\Music".to_owned(), "a.mp3".to_owned()))
        );
        assert_eq!(
            roots.locate(&dir.path().join("media")),
            Some((String::new(), "media".to_owned()))
        );
        assert_eq!(roots.locate(&dir.path().join("secret/key.txt")), None);
        assert_eq!(roots.locate(&dir.path().join("media2/x")), None);
        assert_eq!(roots.locate(&dir.path().join("media/a\nb")), None);
    }

    #[test]
    fn roots_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir(&a).unwrap();
        assert!(Roots::new(vec![("x".into(), dir.path().join("missing"))]).is_err());
        assert!(Roots::new(vec![("x/y".into(), a.clone())]).is_err());
        assert!(Roots::new(vec![("..".into(), a.clone())]).is_err());
        assert!(Roots::new(vec![("x".into(), "relative".into())]).is_err());
        assert!(Roots::new(vec![("X".into(), a.clone()), ("x".into(), a.clone())]).is_err());
        std::fs::write(dir.path().join("f"), b"").unwrap();
        assert!(Roots::new(vec![("f".into(), dir.path().join("f"))]).is_err());
        assert!(Roots::new(vec![("ok".into(), a)]).is_ok());
    }

    #[test]
    fn listing_lines() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let f = Entry {
            name: "a b.txt".into(),
            is_dir: false,
            size: 12,
            modified: Some(t),
        };
        assert_eq!(
            list_line(&f, t),
            "-rw-r--r-- 1 owner group            12 Nov 14 22:13 a b.txt"
        );
        let later = t + std::time::Duration::from_secs(400 * 86_400);
        assert!(list_line(&f, later).contains("Nov 14  2023"));
        assert_eq!(
            mlsd_line(&f),
            "type=file;modify=20231114221320;size=12; a b.txt"
        );
        let d = Entry {
            name: "d".into(),
            is_dir: true,
            size: 0,
            modified: None,
        };
        assert_eq!(mlsd_line(&d), "type=dir;modify=19700101000000; d");
    }
}
