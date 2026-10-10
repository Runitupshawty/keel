//! OS launches: default app, open-with (apps for a file, launch with one), reveal in the system file manager. Each starts a process and returns without waiting.
//! Call through `worker::spawn_local` so remote paths materialise off the UI thread.

use std::ffi::OsString;
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

    /// The Windows "Open with" chooser. Elsewhere Keel's own picker is the chooser.
    pub fn open_with_chooser(path: &Path) -> io::Result<()> {
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

    pub fn reveal(path: &Path) -> io::Result<()> {
        let mut cmd = Command::new("open");
        cmd.arg("-R").arg(path);
        run(cmd)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod sys {
    use super::*;

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

#[cfg(windows)]
pub use sys::open_with_chooser;
pub use sys::reveal;

/// `applications` folders of the XDG data dirs, the user's first.
fn app_dirs() -> Vec<std::path::PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(system.split(':').map(std::path::PathBuf::from))
        .map(|d| d.join("applications"))
        .collect()
}

/// "Open with" candidates `(name, app)` for `path`. Workers only. Linux: the applications
/// registered for its MIME type (`xdg-mime`), default first, `app` being the desktop id.
/// macOS: application bundles in /Applications and ~/Applications (Spotlight), `app` being
/// the .app path. Empty on Windows (recent apps and Browse only).
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
    let dirs = app_dirs();
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

#[cfg(target_os = "macos")]
pub fn apps_for(_path: &Path) -> io::Result<Vec<(String, String)>> {
    let mut cmd = Command::new("mdfind");
    cmd.args(["-onlyin", "/Applications"]);
    if let Some(home) = std::env::var_os("HOME") {
        cmd.arg("-onlyin")
            .arg(Path::new(&home).join("Applications"));
    }
    cmd.arg("kMDItemContentType == 'com.apple.application-bundle'");
    let out = cmd.output()?;
    let mut apps: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.ends_with(".app"))
        .map(|l| (app_label(l), l.to_owned()))
        .collect();
    apps.sort_by_key(|(name, _)| name.to_lowercase());
    Ok(apps)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn apps_for(_path: &Path) -> io::Result<Vec<(String, String)>> {
    Ok(Vec::new())
}

/// What to show for an app string: the file stem of a path (`Code`, `Notepad++`), a desktop
/// id without `.desktop`.
pub fn app_label(app: &str) -> String {
    let name = app.rsplit(['/', '\\']).next().unwrap_or(app);
    name.strip_suffix(".desktop")
        .or_else(|| name.rsplit_once('.').map(|(stem, _)| stem))
        .unwrap_or(name)
        .to_owned()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Windows,
    Mac,
    Other,
}

impl Os {
    pub const HERE: Os = if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Mac
    } else {
        Os::Other
    };
}

/// `Exec=` of a `.desktop` file's `[Desktop Entry]` group.
pub fn desktop_exec(text: &str) -> Option<String> {
    let mut entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            entry = line == "[Desktop Entry]";
        } else if entry {
            if let Some(("Exec", v)) = line.split_once('=') {
                return Some(v.trim().to_owned());
            }
        }
    }
    None
}

/// Splits an `Exec` value into arguments: spaces separate, double quotes group (with `\"`,
/// `\\`, `\$` and `` \` `` escapes inside them).
fn split_exec(exec: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match chars.next() {
                Some(n @ ('"' | '\\' | '$' | '`')) => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            },
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Argument lists for an `Exec` line applied to `paths`. `%F`/`%U` as a whole argument takes
/// every path in one process; `%f`/`%u` (or a line with no file code) runs one process per
/// path. Other codes (`%i`, `%c`, `%k`...) are dropped and `%%` is a percent sign.
pub fn exec_argvs(exec: &str, paths: &[std::path::PathBuf]) -> Vec<Vec<OsString>> {
    let toks = split_exec(exec);
    let multi = toks.iter().any(|t| t == "%F" || t == "%U");
    let single = toks.iter().any(|t| t.contains("%f") || t.contains("%u"));
    let build = |ps: &[&std::path::PathBuf]| -> Vec<OsString> {
        let mut out = Vec::new();
        for t in &toks {
            if t == "%F" || t == "%U" {
                out.extend(ps.iter().map(|p| p.as_os_str().to_owned()));
            } else if t == "%f" || t == "%u" {
                out.extend(ps.first().map(|p| p.as_os_str().to_owned()));
            } else {
                let first = ps
                    .first()
                    .map_or_else(String::new, |p| p.to_string_lossy().into_owned());
                let mut s = String::new();
                let mut it = t.chars();
                while let Some(c) = it.next() {
                    if c != '%' {
                        s.push(c);
                        continue;
                    }
                    match it.next() {
                        Some('%') => s.push('%'),
                        Some('f' | 'u') => s.push_str(&first),
                        _ => {}
                    }
                }
                if !s.is_empty() || t.is_empty() {
                    out.push(s.into());
                }
            }
        }
        if !multi && !single {
            out.extend(ps.first().map(|p| p.as_os_str().to_owned()));
        }
        out
    };
    if multi {
        vec![build(&paths.iter().collect::<Vec<_>>())]
    } else {
        paths.iter().map(|p| build(&[p])).collect()
    }
}

/// The processes that open `paths` with `app`, program first (nothing is started here).
/// Windows: `app path` once per file. macOS: one `open -a app paths...`. Elsewhere: the
/// `Exec` line `exec` (the app's .desktop file) when given, else `app path` per file.
pub fn open_with_argvs(
    os: Os,
    app: &str,
    paths: &[std::path::PathBuf],
    exec: Option<&str>,
) -> Vec<Vec<OsString>> {
    match (os, exec) {
        (Os::Mac, _) => {
            let mut argv: Vec<OsString> = vec!["open".into(), "-a".into(), app.into()];
            argv.extend(paths.iter().map(|p| p.as_os_str().to_owned()));
            vec![argv]
        }
        (Os::Other, Some(exec)) => exec_argvs(exec, paths),
        _ => paths
            .iter()
            .map(|p| vec![app.into(), p.as_os_str().to_owned()])
            .collect(),
    }
}

/// Opens `paths` with `app` (an executable, an `.app`, or a desktop id on Linux).
pub fn open_with(app: &str, paths: &[std::path::PathBuf]) -> io::Result<()> {
    let exec = if Os::HERE == Os::Other && app.ends_with(".desktop") {
        let text = app_dirs()
            .iter()
            .find_map(|d| std::fs::read_to_string(d.join(app)).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{app} not found")))?;
        desktop_exec(&text)
    } else {
        None
    };
    for argv in open_with_argvs(Os::HERE, app, paths, exec.as_deref()) {
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        let mut cmd = Command::new(program);
        cmd.args(args);
        run(cmd)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    fn argv(v: &[Vec<OsString>]) -> Vec<Vec<String>> {
        v.iter()
            .map(|a| a.iter().map(|s| s.to_string_lossy().into_owned()).collect())
            .collect()
    }

    #[test]
    fn open_with_commands_per_platform() {
        let paths = [PathBuf::from("/a/x.txt"), PathBuf::from("/a/y.txt")];
        // Windows: one process per file, the path one argument.
        assert_eq!(
            argv(&open_with_argvs(
                Os::Windows,
                r"C:\Tools\ed.exe",
                &paths,
                None
            )),
            [
                [r"C:\Tools\ed.exe", "/a/x.txt"],
                [r"C:\Tools\ed.exe", "/a/y.txt"]
            ]
        );
        // macOS: a single `open -a` with every path.
        assert_eq!(
            argv(&open_with_argvs(
                Os::Mac,
                "/Applications/Code.app",
                &paths,
                None
            )),
            [[
                "open",
                "-a",
                "/Applications/Code.app",
                "/a/x.txt",
                "/a/y.txt"
            ]]
        );
        // Elsewhere without an Exec line: the program per file.
        assert_eq!(
            argv(&open_with_argvs(
                Os::Other,
                "/usr/bin/vim",
                &paths[..1],
                None
            )),
            [["/usr/bin/vim", "/a/x.txt"]]
        );
        assert!(open_with_argvs(Os::Windows, "x.exe", &[], None).is_empty());
    }

    #[test]
    fn desktop_exec_lines_substitute_field_codes() {
        let paths = [PathBuf::from("/a/x y.txt"), PathBuf::from("/a/z.txt")];
        let exec = |line: &str| argv(&exec_argvs(line, &paths));
        assert_eq!(
            exec("gedit --new-window %F"),
            [["gedit", "--new-window", "/a/x y.txt", "/a/z.txt"]]
        );
        assert_eq!(exec("vlc %U"), [["vlc", "/a/x y.txt", "/a/z.txt"]]);
        assert_eq!(
            exec("viewer --open=%f %i %c 100%%"),
            [
                ["viewer", "--open=/a/x y.txt", "100%"],
                ["viewer", "--open=/a/z.txt", "100%"]
            ]
        );
        assert_eq!(
            exec("\"/opt/My App/run\" %f"),
            [
                ["/opt/My App/run", "/a/x y.txt"],
                ["/opt/My App/run", "/a/z.txt"]
            ]
        );
        // No field code: the path is appended, one process each.
        assert_eq!(exec("tool -q")[1], ["tool", "-q", "/a/z.txt"]);
        // Through the builder, and from a real-looking entry.
        let entry = "[Desktop Entry]
Name=Ed
Exec=ed %F
[Desktop Action a]
Exec=other
";
        assert_eq!(desktop_exec(entry).as_deref(), Some("ed %F"));
        assert_eq!(
            argv(&open_with_argvs(
                Os::Other,
                "ed.desktop",
                &paths,
                Some("ed %F")
            )),
            [["ed", "/a/x y.txt", "/a/z.txt"]]
        );
    }

    #[test]
    fn app_labels() {
        assert_eq!(
            app_label(r"C:\Program Files\Notepad++\notepad++.exe"),
            "notepad++"
        );
        assert_eq!(
            app_label("/Applications/Visual Studio Code.app"),
            "Visual Studio Code"
        );
        assert_eq!(app_label("org.gnome.gedit.desktop"), "org.gnome.gedit");
    }
}
