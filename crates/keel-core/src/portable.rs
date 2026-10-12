//! Portable mode: a `keel.portable` file beside the executable keeps every setting, library,
//! cache and index in a folder beside it (`keel-data` unless the file names another), so Keel
//! runs from a USB stick or an unzipped folder without touching the user's profile.
//!
//! It works through the folder variables every resolver already reads (`KEEL_CONFIG_DIR`:
//! settings, session, window state, profiles; `KEEL_DATA_DIR`: libraries, crash reports and
//! the file cache; `KEEL_INDEX_DIR`: the search index), set by [`apply`] at the start of
//! `main` before any thread runs, so child processes (keel-daemon, the elevated index
//! service) get them too. A variable that is already set wins (tests, scripted setups).
//! Passwords stay in the system's credential store; they are not files.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The marker file's name, beside the executable.
pub const MARKER: &str = "keel.portable";
/// The portable folder when the marker names none.
pub const DEFAULT_FOLDER: &str = "keel-data";
/// Only the start of the marker is read (it names a folder at most).
const MARKER_READ: usize = 4096;

/// The portable folders: everything lives under `root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    pub root: PathBuf,
    /// `KEEL_CONFIG_DIR`: settings, session, window state, profiles.
    pub config: PathBuf,
    /// `KEEL_DATA_DIR`: libraries, crash reports, the file cache (`data/cache`).
    pub data: PathBuf,
    /// `KEEL_INDEX_DIR`: the NTFS and folder-walk search indexes.
    pub index: PathBuf,
}

impl Dirs {
    /// The folders under `root`.
    pub fn under(root: PathBuf) -> Self {
        Self {
            config: root.join("config"),
            data: root.join("data"),
            index: root.join("index"),
            root,
        }
    }

    /// Each folder variable with its folder.
    pub fn vars(&self) -> [(&'static str, &Path); 3] {
        [
            ("KEEL_CONFIG_DIR", &self.config),
            ("KEEL_DATA_DIR", &self.data),
            ("KEEL_INDEX_DIR", &self.index),
        ]
    }
}

/// The portable folders of the executable at `exe`, or None when no `keel.portable` file
/// sits beside it. The marker's first line that is neither empty nor a `#` comment names
/// the folder (relative to the executable's folder, or absolute); without one it is
/// `keel-data`.
pub fn dirs_for(exe: &Path) -> Option<Dirs> {
    let dir = exe.parent()?;
    let marker = dir.join(MARKER);
    if !std::fs::metadata(&marker).ok()?.is_file() {
        return None;
    }
    let text = read_start(&marker);
    let name = text
        .lines()
        .map(|l| l.trim_start_matches('\u{feff}').trim())
        .find(|l| !l.is_empty() && !l.starts_with('#'));
    Some(Dirs::under(dir.join(name.unwrap_or(DEFAULT_FOLDER))))
}

/// The first [`MARKER_READ`] bytes of `path` as text (empty when unreadable).
fn read_start(path: &Path) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(MARKER_READ as u64).read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Sets each folder variable of `dirs` that `is_set` says is unset, through `set`, and
/// creates its folder (best effort: a read-only medium fails later, where it is written).
/// Returns the variables it set.
pub fn apply_dirs(
    dirs: &Dirs,
    is_set: impl Fn(&str) -> bool,
    mut set: impl FnMut(&str, &Path),
) -> Vec<&'static str> {
    let mut done = Vec::new();
    for (var, dir) in dirs.vars() {
        if is_set(var) {
            continue;
        }
        let _ = std::fs::create_dir_all(dir);
        set(var, dir);
        done.push(var);
    }
    done
}

static ACTIVE: OnceLock<Option<Dirs>> = OnceLock::new();

/// Portable mode for this process: when `keel.portable` sits beside the executable, sets
/// the folder variables (those not already set). Call it first in `main`, before any
/// thread starts (it changes the environment). Returns the portable folders, or None.
pub fn apply() -> Option<&'static Dirs> {
    ACTIVE
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            let dirs = dirs_for(&exe)?;
            apply_dirs(
                &dirs,
                |var| std::env::var_os(var).is_some_and(|v| !v.is_empty()),
                |var, dir| std::env::set_var(var, dir),
            );
            Some(dirs)
        })
        .as_ref()
}

/// The portable folders [`apply`] found, if any (None before it ran).
pub fn active() -> Option<&'static Dirs> {
    ACTIVE.get().and_then(Option::as_ref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn no_marker_no_portable_mode() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(dirs_for(&dir.path().join("keel.exe")), None);
        // A folder named like the marker is not one.
        std::fs::create_dir(dir.path().join(MARKER)).unwrap();
        assert_eq!(dirs_for(&dir.path().join("keel.exe")), None);
    }

    #[test]
    fn empty_marker_uses_keel_data() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MARKER), "").unwrap();
        let dirs = dirs_for(&dir.path().join("keel.exe")).unwrap();
        let root = dir.path().join(DEFAULT_FOLDER);
        assert_eq!(dirs.root, root);
        assert_eq!(dirs.config, root.join("config"));
        assert_eq!(dirs.data, root.join("data"));
        assert_eq!(dirs.index, root.join("index"));
    }

    #[test]
    fn marker_names_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MARKER),
            "\u{feff}# Keel portable settings\n\n  My Keel  \nignored\n",
        )
        .unwrap();
        let dirs = dirs_for(&dir.path().join("keel")).unwrap();
        assert_eq!(dirs.root, dir.path().join("My Keel"));
        // An absolute folder is taken as it is.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MARKER),
            elsewhere.path().display().to_string(),
        )
        .unwrap();
        let dirs = dirs_for(&dir.path().join("keel")).unwrap();
        assert_eq!(dirs.root, elsewhere.path());
        // Only comments: the default folder.
        std::fs::write(dir.path().join(MARKER), "# nothing\n").unwrap();
        let dirs = dirs_for(&dir.path().join("keel")).unwrap();
        assert_eq!(dirs.root, dir.path().join(DEFAULT_FOLDER));
    }

    #[test]
    fn a_huge_marker_is_read_only_at_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut text = "x".repeat(MARKER_READ * 4);
        text.push_str("\nlater");
        std::fs::write(dir.path().join(MARKER), text).unwrap();
        let dirs = dirs_for(&dir.path().join("keel")).unwrap();
        let name = dirs.root.file_name().unwrap().to_string_lossy().len();
        assert_eq!(name, MARKER_READ);
    }

    #[test]
    fn sets_only_unset_variables_and_creates_the_folders() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(dir.path().join(DEFAULT_FOLDER));
        let mut env: HashMap<String, PathBuf> = HashMap::new();
        env.insert("KEEL_DATA_DIR".into(), PathBuf::from("explicit"));
        let preset: Vec<String> = env.keys().cloned().collect();
        let set = apply_dirs(
            &dirs,
            |var| preset.iter().any(|p| p == var),
            |var, path| {
                env.insert(var.into(), path.to_owned());
            },
        );
        assert_eq!(set, ["KEEL_CONFIG_DIR", "KEEL_INDEX_DIR"]);
        assert_eq!(env["KEEL_CONFIG_DIR"], dirs.config);
        assert_eq!(env["KEEL_INDEX_DIR"], dirs.index);
        assert_eq!(env["KEEL_DATA_DIR"], PathBuf::from("explicit"));
        assert!(dirs.config.is_dir() && dirs.index.is_dir());
        assert!(
            !dirs.data.exists(),
            "the explicit data folder is used instead"
        );
    }
}
