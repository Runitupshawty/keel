//! WSL (Task 51). On Windows: the sidebar's WSL section lists the distributions
//! (`keel_term::wsl_distros`, the terminal's list too) and opens one as
//! `\\wsl.localhost\<name>` (else `\\wsl$\<name>`) through the local provider, like any
//! network share. There is no Recycle Bin there, so a delete is permanent and the prompt
//! says so. In the Linux build running inside a distribution, `keel <path>` hands the path
//! to the Windows `keel.exe` (`hand_off`).

use keel_vfs::{Provider, VPath};
#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long opening a distribution may take (a stopped one starts first) before the
/// sidebar gives up with an error toast.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// The names Windows serves distributions under: `wsl.localhost` (Windows 11, Windows 10
/// 21H2 and later), then the older `wsl$`.
const HOSTS: [&str; 2] = ["wsl.localhost", "wsl$"];

/// Opens distribution `name`: lists its root through the local provider (which starts a
/// stopped distribution) and returns that root. Blocks for as long as starting takes:
/// workers only (`worker::spawn_wsl`, with `OPEN_TIMEOUT`).
pub fn open(name: &str) -> anyhow::Result<VPath> {
    let mut failed = Vec::new();
    for host in HOSTS {
        let root = VPath::local(format!(r"\\{host}\{name}"));
        match keel_vfs::LocalProvider.list(&root) {
            Ok(_) => return Ok(root),
            Err(e) => failed.push(format!("{}: {e:#}", root.display())),
        }
    }
    anyhow::bail!("{}", failed.join("; "))
}

/// The distribution a local path is in (`\\wsl.localhost\<name>\...` or
/// `\\wsl$\<name>\...`), None elsewhere. Pure: never touches the disk.
pub fn distro_of(p: &VPath) -> Option<String> {
    let path = p.to_local_path()?.to_string_lossy().replace('\\', "/");
    let rest = HOSTS.iter().find_map(|host| {
        let prefix = format!("//{host}/");
        path.get(..prefix.len())
            .filter(|head| head.eq_ignore_ascii_case(&prefix))
            .map(|_| &path[prefix.len()..])
    })?;
    let name = rest.split('/').next().unwrap_or_default();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The distribution all of `paths` are in, if they are in one. Such a delete is permanent
/// (`jobs::delete`) after `delete_text`; a mix with other places goes the trash's way,
/// which refuses the WSL items.
pub fn one_distro(paths: &[VPath]) -> Option<String> {
    let first = distro_of(paths.first()?)?;
    paths
        .iter()
        .all(|p| distro_of(p).as_ref() == Some(&first))
        .then_some(first)
}

/// The prompt before deleting `n` items in distribution `distro` (no Recycle Bin there).
pub fn delete_text(n: usize, distro: &str) -> String {
    format!(
        "Delete {} in {distro} (WSL) permanently? WSL folders have no Recycle Bin; this cannot be undone.",
        crate::jobs::items(n)
    )
}

/// The Windows path of `path` as seen inside distribution `distro`: a relative path is
/// resolved against `cwd` first, `.` and `..` by name (as the shell's `cd` does);
/// `/mnt/<x>/...` is `<X>:\...`, any other absolute path `\\wsl.localhost\<distro>\...`.
/// Pure: no file system access.
#[cfg(any(target_os = "linux", test))]
pub fn to_windows(path: &str, cwd: &str, distro: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{cwd}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    match parts.as_slice() {
        ["mnt", drive, rest @ ..]
            if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() =>
        {
            format!("{}:\\{}", drive.to_ascii_uppercase(), rest.join("\\"))
        }
        [] => format!(r"\\wsl.localhost\{distro}"),
        parts => format!(r"\\wsl.localhost\{distro}\{}", parts.join("\\")),
    }
}

/// `keel_exe` (`KEEL_EXE`) when set, else the first `keel.exe` in `path` (`PATH`; WSL
/// appends the Windows one as `/mnt/c/...` folders unless `appendWindowsPath` is off).
#[cfg(any(target_os = "linux", test))]
pub fn find_exe(
    keel_exe: Option<&str>,
    path: &str,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(exe) = keel_exe.filter(|e| !e.is_empty()) {
        return Some(exe.into());
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("keel.exe"))
        .find(|exe| is_file(exe))
}

/// The arguments for `keel.exe`: the translated folder, then the options that carry over.
#[cfg(any(target_os = "linux", test))]
pub fn windows_args(cli: &crate::cli::Cli, folder: String) -> Vec<String> {
    let mut args = vec![folder];
    if cli.new_window {
        args.push("--new-window".into());
    }
    if let Some(profile) = &cli.profile {
        args.extend(["--profile".into(), profile.clone()]);
    }
    if let Some(query) = &cli.search {
        args.extend(["--search".into(), query.clone()]);
    }
    args
}

/// The Linux build inside WSL (`WSL_DISTRO_NAME` set), asked to open a folder, without
/// `--here` or `KEEL_WSL_NATIVE=1`: runs the Windows `keel.exe` on the folder's Windows
/// path and returns its exit code (`wait_or_leave`). None: open the Linux window as usual
/// (also when no `keel.exe` is found, with a note on stderr). Subcommands never get here
/// (`main`).
#[cfg(target_os = "linux")]
pub fn hand_off(cli: &crate::cli::Cli) -> Option<i32> {
    let distro = std::env::var("WSL_DISTRO_NAME")
        .ok()
        .filter(|d| !d.is_empty())?;
    let folder = cli.folder.as_ref()?;
    if cli.here || std::env::var_os("KEEL_WSL_NATIVE").is_some_and(|v| v == "1") {
        return None;
    }
    let keel_exe = std::env::var("KEEL_EXE").ok();
    let path = std::env::var("PATH").unwrap_or_default();
    let Some(exe) = find_exe(keel_exe.as_deref(), &path, Path::is_file) else {
        eprintln!("keel: keel.exe is not on PATH (set KEEL_EXE to it); opening the Linux window");
        return None;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    // A path that is not UTF-8 has no Windows spelling: refused, never opened lossily (which
    // could open another folder).
    let (Some(folder_text), Some(cwd_text)) = (folder.to_str(), cwd.to_str()) else {
        eprintln!(
            "keel: {} is not valid UTF-8, so the Windows Keel cannot open it; keel --here opens \
             it in the Linux window",
            folder.display()
        );
        return Some(2);
    };
    let target = to_windows(folder_text, cwd_text, &distro);
    // No shared stdio: `keel.exe` may stay open as the window after this process exits.
    let started = std::process::Command::new(&exe)
        .args(windows_args(cli, target))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match started {
        Ok(child) => Some(wait_or_leave(child, HAND_OFF_WAIT)),
        Err(e) => {
            eprintln!("keel: could not run {}: {e}", exe.display());
            Some(1)
        }
    }
}

/// How long `keel` inside WSL waits for `keel.exe`: enough to start and hand the folder to
/// a running window (`single_instance` answers within 3 s).
#[cfg(target_os = "linux")]
const HAND_OFF_WAIT: Duration = Duration::from_secs(10);

/// `child`'s exit code when it exits within `wait` (`keel.exe` handed the folder to the
/// running window, or failed); 0 when it is still running then (it is the window now, and
/// stays open: the shell gets its prompt back).
#[cfg(any(target_os = "linux", test))]
fn wait_or_leave(mut child: std::process::Child, wait: Duration) -> i32 {
    let end = std::time::Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) if std::time::Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Ok(None) => return 0,
            Err(e) => {
                eprintln!("keel: {e}");
                return 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_paths_become_windows_paths() {
        let t = |p: &str| to_windows(p, "/home/me/src", "Ubuntu");
        assert_eq!(t("/mnt/c/Users/x"), r"C:\Users\x");
        assert_eq!(t("/mnt/d"), r"D:\");
        assert_eq!(t("/mnt/c/"), r"C:\");
        assert_eq!(t("/home/x"), r"\\wsl.localhost\Ubuntu\home\x");
        assert_eq!(t("/"), r"\\wsl.localhost\Ubuntu");
        assert_eq!(t("/mnt"), r"\\wsl.localhost\Ubuntu\mnt");
        assert_eq!(t("/mnt/wslg/x"), r"\\wsl.localhost\Ubuntu\mnt\wslg\x");
        // Relative paths are resolved against the current folder, `..` by name.
        assert_eq!(t("."), r"\\wsl.localhost\Ubuntu\home\me\src");
        assert_eq!(
            t("keel/./docs"),
            r"\\wsl.localhost\Ubuntu\home\me\src\keel\docs"
        );
        assert_eq!(t("../../.."), r"\\wsl.localhost\Ubuntu");
        assert_eq!(to_windows("x", "/mnt/c/Work", "Debian"), r"C:\Work\x");
        assert_eq!(
            to_windows("..", "/mnt/c", "Debian"),
            r"\\wsl.localhost\Debian\mnt"
        );
        // Names with spaces and non-ASCII letters pass through unchanged.
        assert_eq!(
            t("/mnt/c/Program Files/Ünïcode ✓"),
            r"C:\Program Files\Ünïcode ✓"
        );
        assert_eq!(
            t("photos 2026/été"),
            r"\\wsl.localhost\Ubuntu\home\me\src\photos 2026\été"
        );
    }

    /// A `keel.exe` that hands over and exits gives its exit code; one that stays (it is
    /// the window now) is left running and counts as success.
    #[test]
    fn exit_code_or_left_running() {
        use std::process::Command;
        let (quick, slow) = if cfg!(windows) {
            (
                Command::new("cmd").args(["/c", "exit 3"]).spawn(),
                Command::new("cmd")
                    .args(["/c", "ping -n 4 127.0.0.1 >nul"])
                    .spawn(),
            )
        } else {
            (
                Command::new("sh").args(["-c", "exit 3"]).spawn(),
                Command::new("sleep").arg("3").spawn(),
            )
        };
        assert_eq!(wait_or_leave(quick.unwrap(), Duration::from_secs(10)), 3);
        let start = std::time::Instant::now();
        assert_eq!(wait_or_leave(slow.unwrap(), Duration::from_millis(200)), 0);
        assert!(start.elapsed() < Duration::from_secs(2) || std::env::var_os("CI").is_some());
    }

    #[test]
    fn keel_exe_from_path_or_keel_exe() {
        let path = "/usr/local/bin:/usr/bin:/mnt/c/Windows/system32:/mnt/c/Program Files/Keel:/mnt/d/tools";
        let installed = |p: &Path| {
            p == Path::new("/mnt/c/Program Files/Keel/keel.exe")
                || p == Path::new("/mnt/d/tools/keel.exe")
        };
        assert_eq!(
            find_exe(None, path, installed),
            Some(PathBuf::from("/mnt/c/Program Files/Keel/keel.exe"))
        );
        assert_eq!(
            find_exe(Some("/opt/keel.exe"), path, installed),
            Some(PathBuf::from("/opt/keel.exe"))
        );
        assert_eq!(
            find_exe(Some(""), "/mnt/d/tools", installed),
            Some(PathBuf::from("/mnt/d/tools/keel.exe"))
        );
        assert_eq!(find_exe(None, path, |_| false), None);
        assert_eq!(find_exe(None, "", installed), None);
    }

    #[test]
    fn options_carry_over_to_keel_exe() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from([
            "keel",
            "/home/x",
            "--new-window",
            "--profile",
            "work",
            "--search",
            "two words",
        ])
        .unwrap();
        assert_eq!(
            windows_args(&cli, r"\\wsl.localhost\U\home\x".into()),
            [
                r"\\wsl.localhost\U\home\x",
                "--new-window",
                "--profile",
                "work",
                "--search",
                "two words"
            ]
        );
        let here = crate::cli::Cli::try_parse_from(["keel", ".", "--here"]).unwrap();
        assert!(here.here);
    }

    #[test]
    fn wsl_paths_are_recognised() {
        let d = |p: &str| distro_of(&VPath::local(p));
        if cfg!(windows) {
            assert_eq!(
                d(r"\\wsl.localhost\Ubuntu\home\x").as_deref(),
                Some("Ubuntu")
            );
            assert_eq!(d(r"\\WSL.LOCALHOST\Ubuntu").as_deref(), Some("Ubuntu"));
            assert_eq!(d(r"\\wsl$\Debian\etc").as_deref(), Some("Debian"));
            assert_eq!(d(r"\\?\UNC\wsl$\Debian\etc").as_deref(), Some("Debian"));
            assert_eq!(d(r"\\server\share\x"), None);
            assert_eq!(d(r"\\wsl.localhost\"), None);
            assert_eq!(d(r"C:\wsl.localhost\x"), None);
        }
        assert_eq!(
            distro_of(&VPath::parse("sftp://wsl.localhost/x").unwrap()),
            None
        );
        let v = |p: &str| VPath::local(p);
        if cfg!(windows) {
            let u = [v(r"\\wsl.localhost\U\a"), v(r"\\wsl$\U\b")];
            assert_eq!(one_distro(&u).as_deref(), Some("U"));
            let mixed = [v(r"\\wsl.localhost\U\a"), v(r"C:\b")];
            assert_eq!(one_distro(&mixed), None);
            let two = [v(r"\\wsl.localhost\U\a"), v(r"\\wsl.localhost\D\a")];
            assert_eq!(one_distro(&two), None);
        }
        assert_eq!(one_distro(&[]), None);
        assert!(delete_text(3, "Ubuntu").contains("3 items in Ubuntu (WSL) permanently"));
    }

    /// Del in a WSL folder says the delete is permanent (no Recycle Bin) and deletes for
    /// good when confirmed; nothing is listed or deleted here (the folder is never read).
    #[cfg(windows)]
    #[test]
    fn delete_in_a_wsl_folder_asks_for_a_permanent_delete() {
        use crate::keys::Action;
        use keel_vfs::Kind;
        let dir = VPath::local(std::env::temp_dir());
        let router = std::sync::Arc::new(keel_vfs::Router::new());
        let mut state = crate::state::AppState::new(egui::Context::default(), router, dir);
        let wsl = VPath::local(r"\\wsl.localhost\NoSuchDistro\home");
        state.tab_mut(0).dir = wsl.clone();
        state.tab_mut(0).set_listing(crate::tab::Listing::new(vec![
            crate::tab::test_entry(&wsl, "a.txt", Kind::File, 1),
            crate::tab::test_entry(&wsl, "b", Kind::Dir, 0),
        ]));
        state.run(0, Action::SelectAll);
        state.run(0, Action::Delete);
        match &state.dialog {
            Some(crate::dialogs::Dialog::Confirm { text, on_yes }) => {
                assert_eq!(
                    text,
                    "Delete 2 items in NoSuchDistro (WSL) permanently? WSL folders have no Recycle Bin; this cannot be undone."
                );
                assert!(matches!(on_yes, Action::DeleteRemote(p) if p.len() == 2));
            }
            _ => panic!("no confirm dialog"),
        }
    }

    /// A machine with WSL: `KEEL_WSL_TEST=<distro>` (one that is installed). Opens it
    /// through the sidebar's path and lists it, and `keel.exe .` in it asks for that
    /// folder. Without the variable it does nothing.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs WSL: KEEL_WSL_TEST=<distro>"]
    fn opens_a_real_distribution() {
        let Ok(distro) = std::env::var("KEEL_WSL_TEST") else {
            eprintln!("KEEL_WSL_TEST is not set: skipped");
            return;
        };
        assert!(
            keel_term::wsl_distros().contains(&distro),
            "{distro} is in the registry list"
        );
        let root = open(&distro).expect("opens");
        assert_eq!(distro_of(&root).as_deref(), Some(distro.as_str()));
        let listed = keel_vfs::LocalProvider.list(&root).expect("lists");
        assert!(listed.iter().any(|e| e.name == "etc"), "a Linux root");
        // `keel.exe .` started in the distribution's folder.
        let before = std::env::current_dir().unwrap();
        std::env::set_current_dir(root.to_local_path().unwrap()).unwrap();
        let req = crate::cli::Cli::parse_args(["keel".into(), ".".into()], false, |_| false)
            .unwrap()
            .request();
        std::env::set_current_dir(before).unwrap();
        let folder = req.checked().unwrap().folder.unwrap();
        assert_eq!(
            distro_of(&VPath::local(&folder)).as_deref(),
            Some(distro.as_str())
        );
    }
}
