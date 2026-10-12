//! Explorer's folder menu (Windows, per user): **Search in Keel** and **Analyze disk usage
//! with Keel** on folders, folder backgrounds and drives, added and removed by a switch in
//! Settings → General. Only keys under `HKEY_CURRENT_USER\Software\Classes` are written
//! (never `HKEY_LOCAL_MACHINE`, so no administrator prompt), and only Keel's own verbs are
//! removed. Windows 11 lists them under **Show more options**.
#![cfg_attr(not(windows), allow(dead_code))]

use anyhow::Result;
use std::path::Path;

/// The registry as far as this needs it: keys relative to `HKEY_CURRENT_USER` (there is no
/// way to name another root). A fake in tests.
pub trait Registry {
    /// Creates `key` (and its parents) and sets its string value `name` (None: the key's
    /// default value).
    fn set(&mut self, key: &str, name: Option<&str>, value: &str) -> Result<()>;
    /// A string value; None when the key or the value is missing.
    fn get(&self, key: &str, name: Option<&str>) -> Option<String>;
    /// Deletes `key` with everything under it (a missing key is not an error).
    fn delete_tree(&mut self, key: &str) -> Result<()>;
}

/// Under `HKEY_CURRENT_USER`: the per-user part of `HKEY_CLASSES_ROOT`.
pub const ROOT: &str = r"Software\Classes";

/// Where the verbs go: a folder, the empty area of an open folder, a drive.
const PLACES: [&str; 3] = [
    r"Directory\shell",
    r"Directory\Background\shell",
    r"Drive\shell",
];

/// (verb key, menu text, arguments after the executable). The folder (`%V`) comes last: a
/// drive root (`"C:\"`) reaches Keel as `C:"`, which `cli::shell_arg` reads back.
const VERBS: [(&str, &str, &str); 2] = [
    ("Keel.Search", "Search in Keel", r#"--search "" -- "%V""#),
    (
        "Keel.DiskUsage",
        "Analyze disk usage with Keel",
        r#"--disk-usage "%V""#,
    ),
];

/// One registry value: (key, value name (None: default), data).
pub type Value = (String, Option<&'static str>, String);

/// Every value that registers the verbs for the executable `exe`.
pub fn entries(exe: &Path) -> Vec<Value> {
    let exe = exe.display();
    let mut out = Vec::new();
    for place in PLACES {
        for (verb, text, args) in VERBS {
            let key = format!(r"{ROOT}\{place}\{verb}");
            out.push((key.clone(), None, text.to_owned()));
            out.push((key.clone(), Some("Icon"), format!("\"{exe}\",0")));
            out.push((format!(r"{key}\command"), None, format!("\"{exe}\" {args}")));
        }
    }
    out
}

/// The verb keys Keel owns (removed whole by `uninstall`).
fn verb_keys() -> impl Iterator<Item = String> {
    PLACES.into_iter().flat_map(|place| {
        VERBS
            .into_iter()
            .map(move |(verb, _, _)| format!(r"{ROOT}\{place}\{verb}"))
    })
}

/// Adds the verbs (again, for `exe`, when they point elsewhere).
pub fn install(reg: &mut dyn Registry, exe: &Path) -> Result<()> {
    for (key, name, data) in entries(exe) {
        reg.set(&key, name, &data)?;
    }
    Ok(())
}

/// Removes Keel's verbs, and nothing else.
pub fn uninstall(reg: &mut dyn Registry) -> Result<()> {
    for key in verb_keys() {
        reg.delete_tree(&key)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// None of Keel's verbs are there.
    Off,
    /// All of them, for this executable.
    On,
    /// Some, or for another copy of Keel (moved or updated elsewhere).
    Other,
}

pub fn state(reg: &dyn Registry, exe: &Path) -> State {
    let want = entries(exe);
    let found = want
        .iter()
        .filter(|(key, name, data)| reg.get(key, *name).as_deref() == Some(data.as_str()))
        .count();
    if found == want.len() {
        State::On
    } else if found == 0 && verb_keys().all(|k| reg.get(&k, None).is_none()) {
        State::Off
    } else {
        State::Other
    }
}

/// The real registry, under `HKEY_CURRENT_USER` only.
#[cfg(windows)]
pub struct CurrentUser;

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

#[cfg(windows)]
impl Registry for CurrentUser {
    fn set(&mut self, key: &str, name: Option<&str>, value: &str) -> Result<()> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
            REG_OPTION_NON_VOLATILE, REG_SZ,
        };
        let (k, n, v) = (wide(key), name.map(wide), wide(value));
        let mut hkey = HKEY::default();
        // SAFETY: the strings are NUL-terminated and outlive the calls; `hkey` is written on
        // success and closed below.
        let made = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(k.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
        };
        anyhow::ensure!(made == ERROR_SUCCESS, "cannot create {key}: {}", made.0);
        let bytes: Vec<u8> = v.iter().flat_map(|u| u.to_le_bytes()).collect();
        let name = n.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        // SAFETY: `hkey` is open; `name` is null or NUL-terminated; `bytes` is the value.
        let set = unsafe { RegSetValueExW(hkey, name, 0, REG_SZ, Some(&bytes)) };
        // SAFETY: `hkey` was opened above.
        let _ = unsafe { RegCloseKey(hkey) };
        anyhow::ensure!(set == ERROR_SUCCESS, "cannot write {key}: {}", set.0);
        Ok(())
    }

    fn get(&self, key: &str, name: Option<&str>) -> Option<String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
        let (k, n) = (wide(key), name.map(wide));
        let name = n.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        let mut buf = vec![0u16; 2048];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `size` bytes; the strings are NUL-terminated (or null).
        let got = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(k.as_ptr()),
                name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        (got == ERROR_SUCCESS)
            .then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
    }

    fn delete_tree(&mut self, key: &str) -> Result<()> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows::Win32::System::Registry::{RegDeleteKeyW, RegDeleteTreeW, HKEY_CURRENT_USER};
        let k = wide(key);
        // SAFETY: `k` is NUL-terminated.
        let gone = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr())) };
        anyhow::ensure!(
            gone == ERROR_SUCCESS || gone == ERROR_FILE_NOT_FOUND,
            "cannot remove {key}: {}",
            gone.0
        );
        // The key itself, should the tree call have left it (empty by now).
        // SAFETY: `k` is NUL-terminated.
        let _ = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr())) };
        Ok(())
    }
}

/// The switch in Settings → General (Windows only): reads the registry when first shown
/// and again after each change.
pub fn setting(ui: &mut egui::Ui) {
    #[cfg(windows)]
    {
        let id = egui::Id::new("keel-explorer-menu");
        let exe = std::env::current_exe().ok();
        let known = ui.data(|d| d.get_temp::<(State, Option<String>)>(id));
        let (now, error) = known.unwrap_or_else(|| {
            let s = exe
                .as_deref()
                .map_or(State::Off, |e| state(&CurrentUser, e));
            (s, None)
        });
        ui.vertical(|ui| {
            let mut on = now != State::Off;
            let r = ui
                .checkbox(&mut on, "Search in Keel and Analyze disk usage")
                .on_hover_text(
                    "In Explorer's menu for folders and drives, for your user only (on \
                     Windows 11 under Show more options). Untick to remove them.",
                );
            if now == State::Other {
                ui.weak("Registered for another copy of Keel: untick, then tick to use this one");
            }
            let mut error = error;
            if r.changed() {
                let done = match (on, &exe) {
                    (true, Some(exe)) => install(&mut CurrentUser, exe),
                    (true, None) => Err(anyhow::anyhow!("where Keel runs from is not known")),
                    (false, _) => uninstall(&mut CurrentUser),
                };
                error = done.err().map(|e| format!("{e:#}"));
            }
            if let Some(e) = &error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            let now = match r.changed() {
                true => exe
                    .as_deref()
                    .map_or(State::Off, |e| state(&CurrentUser, e)),
                false => now,
            };
            ui.data_mut(|d| d.insert_temp(id, (now, error)));
        });
    }
    #[cfg(not(windows))]
    let _ = ui;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Keys and value names fold case, as the registry's do.
    #[derive(Default)]
    struct Fake(BTreeMap<(String, String), String>);

    fn fold(key: &str, name: Option<&str>) -> (String, String) {
        (key.to_lowercase(), name.unwrap_or("").to_lowercase())
    }

    impl Registry for Fake {
        fn set(&mut self, key: &str, name: Option<&str>, value: &str) -> Result<()> {
            self.0.insert(fold(key, name), value.to_owned());
            Ok(())
        }
        fn get(&self, key: &str, name: Option<&str>) -> Option<String> {
            self.0.get(&fold(key, name)).cloned()
        }
        fn delete_tree(&mut self, key: &str) -> Result<()> {
            let key = key.to_lowercase();
            self.0
                .retain(|(k, _), _| *k != key && !k.starts_with(&format!(r"{key}\")));
            Ok(())
        }
    }

    fn exe() -> std::path::PathBuf {
        std::path::PathBuf::from(r"C:\Program Files\Keel\keel.exe")
    }

    #[test]
    fn install_writes_both_verbs_per_place_under_current_user_classes() {
        let mut reg = Fake::default();
        install(&mut reg, &exe()).unwrap();
        assert_eq!(reg.0.len(), 3 * 2 * 3);
        assert!(reg
            .0
            .keys()
            .all(|(k, _)| k.starts_with(r"software\classes\")));
        let cmd = |place: &str, verb: &str| {
            reg.get(&format!(r"{ROOT}\{place}\{verb}\command"), None)
                .unwrap()
        };
        assert_eq!(
            cmd(r"Directory\shell", "Keel.Search"),
            r#""C:\Program Files\Keel\keel.exe" --search "" -- "%V""#
        );
        assert_eq!(
            cmd(r"Drive\shell", "Keel.DiskUsage"),
            r#""C:\Program Files\Keel\keel.exe" --disk-usage "%V""#
        );
        assert_eq!(
            reg.get(
                r"Software\Classes\Directory\Background\shell\Keel.Search",
                None
            )
            .as_deref(),
            Some("Search in Keel")
        );
        assert_eq!(
            reg.get(r"Software\Classes\Drive\shell\Keel.DiskUsage", Some("Icon"))
                .as_deref(),
            Some(r#""C:\Program Files\Keel\keel.exe",0"#)
        );
    }

    #[test]
    fn uninstall_removes_only_keels_verbs() {
        let mut reg = Fake::default();
        let other = r"Software\Classes\Directory\shell\SomeEditor";
        reg.set(other, None, "Open in editor").unwrap();
        reg.set(&format!(r"{other}\command"), None, "editor.exe %V")
            .unwrap();
        install(&mut reg, &exe()).unwrap();
        uninstall(&mut reg).unwrap();
        assert_eq!(reg.0.len(), 2, "{:?}", reg.0);
        assert_eq!(reg.get(other, None).as_deref(), Some("Open in editor"));
        // Nothing there: nothing to do.
        uninstall(&mut reg).unwrap();
    }

    #[test]
    fn state_follows_the_registry_and_the_executable() {
        let mut reg = Fake::default();
        assert_eq!(state(&reg, &exe()), State::Off);
        install(&mut reg, &exe()).unwrap();
        assert_eq!(state(&reg, &exe()), State::On);
        let moved = Path::new(r"D:\Tools\keel.exe");
        assert_eq!(state(&reg, moved), State::Other);
        install(&mut reg, moved).unwrap();
        assert_eq!(state(&reg, moved), State::On);
        assert_eq!(reg.0.len(), 3 * 2 * 3, "replaced, not added");
        // One verb removed by hand: partly there.
        reg.delete_tree(r"Software\Classes\Drive\shell\Keel.Search")
            .unwrap();
        assert_eq!(state(&reg, moved), State::Other);
        uninstall(&mut reg).unwrap();
        assert_eq!(state(&reg, moved), State::Off);
    }
}
