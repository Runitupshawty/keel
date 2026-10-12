//! The `keel:` URL protocol (Task 79): `keel --url <link>` opens what a link asks for
//! (`keel_core::link`: a search, or a folder; never a file), and on Windows
//! `keel --register-url-protocol` / `--unregister-url-protocol` (or the installer's
//! `-UrlProtocol`) add or remove the per-user handler under `HKCU\Software\Classes\keel`.
//! Nothing is registered unless the user asks.

use crate::cli::Request;
use keel_core::link::Link;

/// What a `keel:` link opens: a search (in its folder), or a folder. A file's path opens its
/// folder with the file selected (`Request::checked`); nothing is run.
pub fn request(url: &str) -> Result<Request, String> {
    match keel_core::link::parse(url).map_err(|e| e.to_string())? {
        Link::Search { query, path } => Ok(Request {
            folder: path,
            search: Some(query),
            ..Request::default()
        }),
        Link::Open { path } => Ok(Request {
            folder: Some(path),
            search: None,
            ..Request::default()
        }),
    }
}

/// The registry key (under HKCU) of the handler.
#[cfg(windows)]
const KEY: &str = r"HKCU\Software\Classes\keel";

/// The handler's command line: this executable, `--url-handler --` and the link. Windows
/// splits `"%1"` at any `"` left in the link, so `Cli::parse_args` takes the handler only as
/// exactly these three arguments and refuses anything more.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn command_line(exe: &std::path::Path) -> String {
    format!(
        "\"{}\" {} -- \"%1\"",
        exe.display(),
        crate::cli::URL_HANDLER
    )
}

#[cfg(windows)]
fn reg(args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new("reg.exe").args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "reg {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Makes this Keel the handler of `keel:` links for the current user.
#[cfg(windows)]
pub fn register() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let command = format!(r"{KEY}\shell\open\command");
    let icon = format!(r"{KEY}\DefaultIcon");
    reg(&["add", KEY, "/ve", "/d", "URL:Keel link", "/f"])?;
    reg(&["add", KEY, "/v", "URL Protocol", "/d", "", "/f"])?;
    reg(&[
        "add",
        &icon,
        "/ve",
        "/d",
        &format!("{},0", exe.display()),
        "/f",
    ])?;
    reg(&["add", &command, "/ve", "/d", &command_line(&exe), "/f"])?;
    Ok(())
}

/// Removes the `keel:` handler when it is this Keel's (another install's is left alone).
/// Ok(false) when there was none of ours.
#[cfg(windows)]
pub fn unregister() -> anyhow::Result<bool> {
    let exe = std::env::current_exe()?;
    let Ok(current) = reg(&["query", &format!(r"{KEY}\shell\open\command"), "/ve"]) else {
        return Ok(false);
    };
    let ours = current
        .to_lowercase()
        .contains(&exe.display().to_string().to_lowercase());
    if ours {
        reg(&["delete", KEY, "/f"])?;
    }
    Ok(ours)
}

/// `keel --register-url-protocol` / `--unregister-url-protocol`: exit code.
pub fn cli(on: bool) -> i32 {
    #[cfg(windows)]
    {
        keel_vfs::desktop::attach_parent_console();
        let done = match on {
            true => register().map(|()| "Keel now opens keel: links.".to_owned()),
            false => unregister().map(|ours| match ours {
                true => "Keel no longer opens keel: links.".to_owned(),
                false => "keel: links were not handled by this Keel.".to_owned(),
            }),
        };
        match done {
            Ok(msg) => {
                println!("{msg}");
                0
            }
            Err(e) => {
                eprintln!("keel: {e:#}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = on;
        eprintln!(
            "keel: registering the keel: protocol is Windows only; on Linux add \
             x-scheme-handler/keel to keel.desktop, on macOS it comes with the app bundle"
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn links_become_requests() {
        let (dir, link_dir) = if cfg!(windows) {
            (PathBuf::from(r"C:\Users\Public"), "C:%5CUsers%5CPublic")
        } else {
            (PathBuf::from("/tmp/x"), "%2Ftmp%2Fx")
        };
        assert_eq!(
            request(&format!("keel:search?q=invoice+ext%3Apdf&path={link_dir}")),
            Ok(Request {
                folder: Some(dir.clone()),
                search: Some("invoice ext:pdf".into()),
                ..Request::default()
            })
        );
        assert_eq!(
            request(&format!("keel:open?path={link_dir}")),
            Ok(Request {
                folder: Some(dir),
                ..Request::default()
            })
        );
        assert!(request("keel:open?path=%5C%5Chost%5Cshare").is_err());
        assert!(request("keel:run?path=x").is_err());
        assert!(request("https://example.com").is_err());
    }

    #[test]
    fn a_file_link_selects_the_file_and_never_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("setup.exe");
        std::fs::write(&file, "MZ").unwrap();
        let url = keel_core::link::Link::Open { path: file.clone() }.to_url();
        let req = request(&url).unwrap().checked().unwrap();
        assert_eq!(req.folder.as_deref(), Some(dir.path()));
        assert_eq!(req.select.as_deref(), Some("setup.exe"));
        assert_eq!(req.search, None);
    }

    #[test]
    fn handler_command_passes_the_link_after_the_handler_switch() {
        let exe = PathBuf::from(if cfg!(windows) {
            r"C:\Program Files\Keel\keel.exe"
        } else {
            "/opt/keel/keel"
        });
        assert_eq!(
            command_line(&exe),
            format!("\"{}\" --url-handler -- \"%1\"", exe.display())
        );
    }
}
