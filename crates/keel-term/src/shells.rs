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

#[cfg(any(target_os = "windows", test))]
fn parse_distros(bytes: &[u8], success: bool) -> Vec<String> {
    if !success {
        return Vec::new();
    }
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.contains(&0) {
        String::from_utf16_lossy(
            &bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        // WSL returns an installation hint on some builds even with exit status zero.
        // Quiet output is one distro identifier per line, never prose or a URL.
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c)))
        .map(str::to_owned)
        .collect()
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
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use wait_timeout::ChildExt;
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
        if let Ok(mut child) = Command::new(&wsl)
            .args(["-l", "-q"])
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            match child.wait_timeout(Duration::from_secs(2)) {
                Ok(Some(status)) => {
                    if let Ok(out) = child.wait_with_output() {
                        for distro in parse_distros(&out.stdout, status.success()) {
                            shells.push(Shell {
                                label: format!("WSL: {distro}"),
                                program: wsl.clone(),
                                args: vec!["-d".into(), distro],
                                kind: ShellKind::Wsl,
                            });
                        }
                    }
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
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

    #[test]
    fn captured_wsl_quiet_output_utf16_and_install_hint() {
        let bytes: Vec<u8> = "\u{feff}Ubuntu\r\nDebian\r\n\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(parse_distros(&bytes, true), ["Ubuntu", "Debian"]);
        assert_eq!(
            parse_distros(b"Ubuntu\r\nDebian\r\n", true),
            ["Ubuntu", "Debian"]
        );
        let hint = b"Windows Subsystem for Linux is not installed. You can install by running 'wsl.exe --install'.\r\n";
        assert!(parse_distros(hint, false).is_empty());
        assert!(parse_distros(hint, true).is_empty());
        assert!(parse_distros(b"", true).is_empty());
    }
}
