use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Shell {
    pub label: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub kind: ShellKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShellKind {
    PowerShell,
    Cmd,
    Wsl,
    Posix,
}

/// Probes executable paths and (on Windows) WSL. Call on a worker thread.
pub fn available_shells() -> Vec<Shell> {
    platform_shells()
}

/// Translate drive paths without launching WSL (also works on non-Windows hosts).
pub fn wslpath(win: &Path) -> String {
    let path = win.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    for prefix in ["//wsl.localhost/", "//wsl$/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            return rest
                .find('/')
                .map_or_else(|| "/".into(), |i| rest[i..].into());
        }
    }
    let b = path.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && &b[1..3] == b":/" {
        format!(
            "/mnt/{}/{}",
            (b[0] as char).to_ascii_lowercase(),
            &path[3..]
        )
    } else {
        path.into()
    }
}

/// Installed WSL distributions, the default one first; empty without WSL (and on other
/// systems). Read from the registry (`HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`:
/// each `{guid}` subkey's `DistributionName`, and `DefaultDistribution`), never from
/// `wsl.exe --list`, which is slow, answers in UTF-16 and starts the WSL service. The
/// terminal's shell list and the sidebar's WSL section both use it.
pub fn wsl_distros() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows::core::w;
        use windows::Win32::System::Registry::HKEY_CURRENT_USER;
        let (default, entries) = read_subkeys(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Lxss"),
            w!("DefaultDistribution"),
            w!("DistributionName"),
        );
        order_distros(default.as_deref(), entries)
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// `entries`: (subkey name, `DistributionName`) per `Lxss` subkey; `default`: the
/// `DefaultDistribution` value (a subkey name). Names WSL would not accept (it allows
/// letters, digits, `-`, `_` and `.`) are dropped, so a name is safe as a `wsl -d`
/// argument and as a UNC path segment. Default first, then by name.
#[cfg(any(windows, test))]
fn order_distros(default: Option<&str>, entries: Vec<(String, String)>) -> Vec<String> {
    let is_default = |key: &str| default.is_some_and(|d| d.eq_ignore_ascii_case(key));
    let mut found: Vec<(bool, String)> = entries
        .into_iter()
        .filter(|(_, name)| {
            !name.is_empty()
                && !name.starts_with('.')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
        .map(|(key, name)| (!is_default(&key), name))
        .collect();
    found.sort_by_key(|(rest, name)| (*rest, name.to_lowercase()));
    found.dedup_by(|a, b| a.1.eq_ignore_ascii_case(&b.1));
    found.into_iter().map(|(_, name)| name).collect()
}

/// The string value `default` of key `root\path`, and (subkey, its string value `value`)
/// for each subkey that has one; nothing when the key is missing (`Lxss`: no WSL).
#[cfg(windows)]
fn read_subkeys(
    root: windows::Win32::System::Registry::HKEY,
    path: windows::core::PCWSTR,
    default: windows::core::PCWSTR,
    value: windows::core::PCWSTR,
) -> (Option<String>, Vec<(String, String)>) {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, KEY_READ, RRF_RT_REG_SZ,
    };
    // A REG_SZ value of at most 260 characters, or None.
    let get = |key: HKEY, sub: PCWSTR, name: PCWSTR| {
        let mut buf = [0u16; 261];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `size` bytes; `sub` (or null) and `name` are NUL-terminated.
        unsafe {
            RegGetValueW(
                key,
                sub,
                name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        }
        .is_ok()
        .then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
    };
    let mut lxss = HKEY::default();
    // SAFETY: `path` is NUL-terminated; `lxss` is written on success.
    let opened = unsafe { RegOpenKeyExW(root, path, 0, KEY_READ, &mut lxss) };
    if opened.is_err() {
        return (None, Vec::new());
    }
    let default = get(lxss, PCWSTR::null(), default);
    let mut entries = Vec::new();
    // Bounded: a key with hundreds of subkeys is not a WSL install.
    for index in 0..256 {
        // Key names are at most 255 characters; one more for the NUL.
        let mut name = [0u16; 256];
        let mut len = 255u32;
        // SAFETY: `name` holds `len` characters plus the NUL.
        let rc = unsafe {
            RegEnumKeyExW(
                lxss,
                index,
                PWSTR(name.as_mut_ptr()),
                &mut len,
                None,
                PWSTR::null(),
                None,
                None,
            )
        };
        if rc.is_err() {
            break;
        }
        let len = (len as usize).min(255);
        name[len] = 0;
        let key = String::from_utf16_lossy(&name[..len]);
        if let Some(distro) = get(lxss, PCWSTR(name.as_ptr()), value) {
            entries.push((key, distro));
        }
    }
    // SAFETY: opened above, closed once.
    let _ = unsafe { RegCloseKey(lxss) };
    (default, entries)
}

fn add(shells: &mut Vec<Shell>, program: PathBuf, kind: ShellKind, label: &str) {
    if program.is_file() && !shells.iter().any(|s| s.program == program) {
        shells.push(Shell {
            label: label.into(),
            program,
            args: Vec::new(),
            kind,
        });
    }
}

#[cfg(target_os = "windows")]
fn platform_shells() -> Vec<Shell> {
    let root =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()));
    let system = root.join("System32");
    let find = |name: &str| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(name))
            .find(|p| p.is_file())
    };
    let mut shells = Vec::new();
    for name in ["pwsh.exe", "powershell.exe"] {
        let path = find(name).or_else(|| {
            (name == "powershell.exe").then(|| system.join("WindowsPowerShell/v1.0/powershell.exe"))
        });
        if let Some(path) = path {
            add(&mut shells, path, ShellKind::PowerShell, name);
        }
    }
    add(
        &mut shells,
        find("cmd.exe").unwrap_or_else(|| system.join("cmd.exe")),
        ShellKind::Cmd,
        "cmd.exe",
    );
    if let Some(wsl) = find("wsl.exe").or_else(|| {
        let p = system.join("wsl.exe");
        p.is_file().then_some(p)
    }) {
        for distro in wsl_distros() {
            shells.push(Shell {
                label: format!("WSL: {distro}"),
                program: wsl.clone(),
                args: vec!["-d".into(), distro],
                kind: ShellKind::Wsl,
            });
        }
    }
    if shells.is_empty() {
        shells.push(Shell {
            label: "cmd.exe".into(),
            program: system.join("cmd.exe"),
            args: Vec::new(),
            kind: ShellKind::Cmd,
        });
    }
    shells
}

#[cfg(not(target_os = "windows"))]
fn platform_shells() -> Vec<Shell> {
    let mut shells = Vec::new();
    if let Some(path) = std::env::var_os("SHELL") {
        let path = PathBuf::from(path);
        let label = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        add(&mut shells, path, ShellKind::Posix, &label);
    }
    #[cfg(target_os = "macos")]
    add(&mut shells, "/bin/zsh".into(), ShellKind::Posix, "zsh");
    add(&mut shells, "/bin/bash".into(), ShellKind::Posix, "bash");
    add(&mut shells, "/bin/sh".into(), ShellKind::Posix, "sh");
    if shells.is_empty() {
        shells.push(Shell {
            label: "sh".into(),
            program: "/bin/sh".into(),
            args: Vec::new(),
            kind: ShellKind::Posix,
        });
    }
    shells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_exist_and_fallback_is_available() {
        let shells = available_shells();
        assert!(!shells.is_empty());
        assert!(shells.iter().all(|s| s.program.is_file()));
        assert!(shells
            .iter()
            .any(|s| matches!(s.kind, ShellKind::Cmd | ShellKind::Posix)));
    }

    #[test]
    fn wsl_drive_paths_and_unc_are_pure_transforms() {
        assert_eq!(wslpath(Path::new(r"D:\Work\x")), "/mnt/d/Work/x");
        assert_eq!(wslpath(Path::new(r"C:\a b\it's")), "/mnt/c/a b/it's");
        assert_eq!(wslpath(Path::new(r"\\?\D:\Work")), "/mnt/d/Work");
        assert_eq!(
            wslpath(Path::new(r"\\wsl.localhost\Ubuntu\home\user")),
            "/home/user"
        );
        assert_eq!(wslpath(Path::new("/home/user")), "/home/user");
    }

    /// The `Lxss` values as the registry has them: the default first whatever its place,
    /// the rest by name; empty, odd or duplicate names dropped.
    #[test]
    fn wsl_registry_values_default_first() {
        let e = |k: &str, n: &str| (k.to_owned(), n.to_owned());
        let entries = vec![
            e("{0a}", "Ubuntu"),
            e("{1b}", "debian"),
            e("{2C}", "Alpine"),
            e("{3d}", ""),
            e("{4e}", "has space"),
            e("{5f}", "../x"),
            e("{6a}", "ubuntu"),
            e("{7b}", "Ubuntu-24.04_x"),
            e("{8c}", ".."),
            e("{9d}", "Débian"),
        ];
        assert_eq!(
            order_distros(Some("{2c}"), entries.clone()),
            ["Alpine", "debian", "Ubuntu", "Ubuntu-24.04_x"]
        );
        assert_eq!(
            order_distros(Some("{7B}"), entries.clone()),
            ["Ubuntu-24.04_x", "Alpine", "debian", "Ubuntu"]
        );
        // A default that names no subkey, or none: by name only.
        assert_eq!(
            order_distros(Some("{99}"), entries.clone()),
            ["Alpine", "debian", "Ubuntu", "Ubuntu-24.04_x"]
        );
        assert_eq!(order_distros(None, entries)[0], "Alpine");
        assert!(order_distros(None, Vec::new()).is_empty());
    }

    /// The registry walk on a key every Windows has (installed programs), read only.
    #[cfg(windows)]
    #[test]
    fn registry_subkeys_are_read() {
        use windows::core::w;
        use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
        let (_, found) = read_subkeys(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
            w!("NoSuchValue"),
            w!("DisplayName"),
        );
        assert!(found
            .iter()
            .any(|(key, name)| !key.is_empty() && !name.is_empty()));
        let missing = read_subkeys(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\NoSuchKeelKey"),
            w!("x"),
            w!("y"),
        );
        assert_eq!(missing, (None, Vec::new()));
    }

    /// Reads the live registry: never fails, whether or not WSL is installed.
    #[test]
    fn wsl_distros_reads_without_wsl() {
        let found = wsl_distros();
        assert!(found
            .iter()
            .all(|d| !d.is_empty() && !d.contains(['\\', '/', ' '])));
    }
}
