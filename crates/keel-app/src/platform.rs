//! OS launches: default app, open-with picker, reveal in the system file manager. Each starts a process and returns without waiting.
//! Call through `worker::spawn_local` so remote paths materialise off the UI thread.

use std::io;
use std::path::Path;
use std::process::Command;

pub fn open(path: &Path) -> io::Result<()> {
    open::that_detached(path)
}

/// Starts `cmd` and waits for it on a helper thread, so an exited child is reaped (no
/// zombie on macOS/Linux).
fn run(mut cmd: Command) -> io::Result<()> {
    let mut child = cmd.spawn()?;
    std::thread::Builder::new()
        .name("keel-reap".into())
        .spawn(move || child.wait())
        .map(drop)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Desktop ids (`org.gnome.gedit.desktop`) listed for `mime` in a `mimeinfo.cache` or
/// `mimeapps.list` (`mime/type=a.desktop;b.desktop;`).
pub fn apps_in_list(text: &str, mime: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| key.trim() == mime)
        .flat_map(|(_, ids)| ids.split(';'))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// `Name=` of a `.desktop` file's `[Desktop Entry]` group; None when it is hidden.
pub fn desktop_name(text: &str) -> Option<String> {
    let mut entry = false;
    let mut name = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            entry = line == "[Desktop Entry]";
        } else if entry {
            match line.split_once('=') {
                Some(("Name", v)) if name.is_none() => name = Some(v.trim().to_owned()),
                Some(("NoDisplay" | "Hidden", v)) if v.trim() == "true" => return None,
                _ => {}
            }
        }
    }
    name
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::os::windows::process::CommandExt;

    pub fn open_with(path: &Path) -> io::Result<()> {
        // OpenAs_RunDLL reads the raw tail of the command line; quotes would be part of the name.
        let mut cmd = Command::new("rundll32.exe");
        cmd.raw_arg(format!("shell32.dll,OpenAs_RunDLL {}", path.display()));
        run(cmd)
    }

    pub fn reveal(path: &Path) -> io::Result<()> {
        let mut cmd = Command::new("explorer.exe");
        cmd.raw_arg(format!("/select,\"{}\"", path.display()));
        run(cmd)
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::*;

    pub fn open_with(path: &Path) -> io::Result<()> {
        let mut cmd = Command::new("osascript");
        cmd.args([
            "-e",
            "on run argv",
            "-e",
            "set appPath to POSIX path of (choose application as alias)",
            "-e",
            "do shell script \"open -a \" & quoted form of appPath & \" \" & quoted form of (item 1 of argv)",
            "-e",
            "end run",
        ])
        .arg(path);
        run(cmd)
    }

    pub fn reveal(path: &Path) -> io::Result<()> {
        let mut cmd = Command::new("open");
        cmd.arg("-R").arg(path);
        run(cmd)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod sys {
    use super::*;

    /// The picker itself is an in-app dialog (`apps_for` + `launch_with`).
    pub fn open_with(path: &Path) -> io::Result<()> {
        super::open(path)
    }

    pub fn reveal(path: &Path) -> io::Result<()> {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let mut cmd = Command::new("xdg-open");
        cmd.arg(dir);
        run(cmd)
    }
}

pub use sys::{open_with, reveal};

/// Linux "Open with": `(name, desktop id)` of the applications registered for `path`'s
/// MIME type (`xdg-mime`), the default first. Empty elsewhere. Workers only.
#[cfg(target_os = "linux")]
pub fn apps_for(path: &Path) -> io::Result<Vec<(String, String)>> {
    let out = Command::new("xdg-mime")
        .args(["query", "filetype"])
        .arg(path)
        .output()?;
    let mime = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if mime.is_empty() {
        return Ok(Vec::new());
    }
    let default = Command::new("xdg-mime")
        .args(["query", "default", &mime])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let dirs: Vec<std::path::PathBuf> = home
        .into_iter()
        .chain(system.split(':').map(std::path::PathBuf::from))
        .map(|d| d.join("applications"))
        .collect();
    let mut ids = vec![default];
    for dir in &dirs {
        for list in ["mimeapps.list", "mimeinfo.cache"] {
            if let Ok(text) = std::fs::read_to_string(dir.join(list)) {
                ids.extend(apps_in_list(&text, &mime));
            }
        }
    }
    let mut apps: Vec<(String, String)> = Vec::new();
    for id in ids.into_iter().filter(|id| !id.is_empty()) {
        if apps.iter().any(|(_, seen)| *seen == id) {
            continue;
        }
        let name = dirs
            .iter()
            .find_map(|d| std::fs::read_to_string(d.join(&id)).ok())
            .and_then(|text| desktop_name(&text));
        if let Some(name) = name {
            apps.push((name, id));
        }
    }
    Ok(apps)
}

#[cfg(not(target_os = "linux"))]
pub fn apps_for(_path: &Path) -> io::Result<Vec<(String, String)>> {
    Ok(Vec::new())
}

/// Opens `path` with the application `id` (a desktop id, Linux).
pub fn launch_with(id: &str, path: &Path) -> io::Result<()> {
    let mut cmd = Command::new("gtk-launch");
    cmd.arg(id.trim_end_matches(".desktop")).arg(path);
    run(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launched_children_are_reaped() {
        let cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "exit"]);
            c
        } else {
            Command::new("true")
        };
        run(cmd).unwrap();
    }

    #[test]
    fn mime_lists_and_desktop_names_parse() {
        let cache = "[MIME Cache]\ntext/plain=gedit.desktop;vim.desktop;\nimage/png=eog.desktop;\n";
        assert_eq!(
            apps_in_list(cache, "text/plain"),
            ["gedit.desktop", "vim.desktop"]
        );
        assert_eq!(apps_in_list(cache, "text/html"), Vec::<String>::new());
        let desktop = "[Desktop Entry]\nName=Text Editor\nName[de]=Texteditor\nExec=gedit %U\n\
                       [Desktop Action new]\nName=New Window\n";
        assert_eq!(desktop_name(desktop).as_deref(), Some("Text Editor"));
        assert_eq!(
            desktop_name("[Desktop Entry]\nName=X\nNoDisplay=true\n"),
            None
        );
    }
}
