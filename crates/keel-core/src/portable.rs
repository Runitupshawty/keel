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
    marker_dirs(exe).map(|(dirs, _)| dirs)
}

/// [`dirs_for`], and why the marker's folder name was not taken (then it is `keel-data`).
fn marker_dirs(exe: &Path) -> Option<(Dirs, Option<String>)> {
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
    // NUL or other control characters (a file in an encoding this does not read) would make
    // `set_var` panic or name a folder nobody meant.
    let (name, note) = match name {
        Some(n) if n.chars().any(|c| c.is_control() || c == '\u{fffd}') => (
            DEFAULT_FOLDER,
            Some(format!(
                "{MARKER}: the folder name is not readable text (save the file as UTF-8); \
                 using {DEFAULT_FOLDER}"
            )),
        ),
        n => (n.unwrap_or(DEFAULT_FOLDER), None),
    };
    Some((Dirs::under(dir.join(name)), note))
}

/// The first [`MARKER_READ`] bytes of `path` as text (empty when unreadable).
fn read_start(path: &Path) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(MARKER_READ as u64).read_to_end(&mut buf);
    }
    decode(&buf)
}

/// The marker's bytes as text: UTF-8, or UTF-16 with a byte order mark (what Notepad's
/// "Unicode" and Windows PowerShell 5.1's `>` write), or UTF-16LE without one when the text
/// starts with an ASCII character (`D`, NUL, `:`, NUL, ...).
fn decode(buf: &[u8]) -> String {
    let utf16 = |bytes: &[u8], be: bool| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| match be {
                true => u16::from_be_bytes([c[0], c[1]]),
                false => u16::from_le_bytes([c[0], c[1]]),
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match buf {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, false),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, true),
        [a, 0, ..] if *a != 0 && a.is_ascii() => utf16(buf, false),
        _ => String::from_utf8_lossy(buf).into_owned(),
    }
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
        // A NUL makes `set_var` panic (`dirs_for` already keeps it out of the name).
        if is_set(var) || dir.as_os_str().as_encoded_bytes().contains(&0) {
            continue;
        }
        let _ = std::fs::create_dir_all(dir);
        set(var, dir);
        done.push(var);
    }
    done
}

/// Portable mode as [`apply`] found it.
struct State {
    dirs: Option<Dirs>,
    notice: Option<String>,
}

static ACTIVE: OnceLock<State> = OnceLock::new();

/// Portable mode for this process: when `keel.portable` sits beside the executable,
/// `prepare` makes the portable folder (for this user only) and checks that nobody else can
/// change it (`keel_api::private::prepare_portable`); then the folder variables not already
/// set are set. When `prepare` refuses, portable mode stays off and [`notice`] says why.
/// Call it first in `main`, before any thread starts (it changes the environment). Returns
/// the portable folders, or None.
pub fn apply(prepare: impl FnOnce(&Dirs) -> Result<(), String>) -> Option<&'static Dirs> {
    ACTIVE
        .get_or_init(|| {
            let off = State {
                dirs: None,
                notice: None,
            };
            let Ok(exe) = std::env::current_exe() else {
                return off;
            };
            let Some((dirs, note)) = marker_dirs(&exe) else {
                return off;
            };
            if let Err(why) = prepare(&dirs) {
                return State {
                    dirs: None,
                    notice: Some(format!("Portable mode is off: {why}")),
                };
            }
            apply_dirs(
                &dirs,
                |var| std::env::var_os(var).is_some_and(|v| !v.is_empty()),
                |var, dir| std::env::set_var(var, dir),
            );
            State {
                dirs: Some(dirs),
                notice: note,
            }
        })
        .dirs
        .as_ref()
}

/// The portable folders [`apply`] found, if any (None before it ran).
pub fn active() -> Option<&'static Dirs> {
    ACTIVE.get().and_then(|s| s.dirs.as_ref())
}

/// What to tell the user about portable mode: why it is off although `keel.portable` is
/// there, or that the marker's folder name was not taken.
pub fn notice() -> Option<&'static str> {
    ACTIVE.get().and_then(|s| s.notice.as_deref())
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

    /// Review 1: Windows PowerShell 5.1's `>` and Notepad's "Unicode" write UTF-16LE; that
    /// name was read with a NUL after each character, and `set_var` panicked at startup.
    #[test]
    fn utf16_markers_are_read_and_unreadable_names_refused() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("keel.exe");
        let le = |s: &str| {
            s.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>()
        };
        let be = |s: &str| {
            s.encode_utf16()
                .flat_map(u16::to_be_bytes)
                .collect::<Vec<u8>>()
        };
        let cases: [Vec<u8>; 3] = [
            [&[0xFF, 0xFE][..], &le("Keel Data\r\n")].concat(),
            [&[0xFE, 0xFF][..], &be("# c\nKeel Data\n")].concat(),
            le("Keel Data\r\n"),
        ];
        for bytes in cases {
            std::fs::write(dir.path().join(MARKER), &bytes).unwrap();
            let (dirs, note) = marker_dirs(&exe).unwrap();
            assert_eq!(dirs.root, dir.path().join("Keel Data"), "{bytes:?}");
            assert_eq!(note, None);
        }
        // NULs, other control characters or undecodable bytes: the default folder, and why.
        for bytes in [&b"ab\0c"[..], b"\x01x", b"\xff\xffname", b"x\xc3("] {
            std::fs::write(dir.path().join(MARKER), bytes).unwrap();
            let (dirs, note) = marker_dirs(&exe).unwrap();
            assert_eq!(dirs.root, dir.path().join(DEFAULT_FOLDER), "{bytes:?}");
            assert!(note.is_some_and(|n| n.contains("UTF-8")), "{bytes:?}");
            assert_eq!(dirs_for(&exe).unwrap(), dirs);
        }
    }

    #[test]
    fn a_folder_with_a_nul_is_never_set() {
        let dirs = Dirs::under(PathBuf::from("/x\0y"));
        let mut set = Vec::new();
        let done = apply_dirs(&dirs, |_| false, |var, _| set.push(var.to_owned()));
        assert!(done.is_empty() && set.is_empty());
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
