//! Command line (Task 24): `keel [FOLDER] [--new-window] [--profile NAME] [--search QUERY]`,
//! and the request a later `keel` hands to the running instance (`single_instance`).

use crate::keys::Action;
use crate::state::AppState;
use crate::tab::TabKind;
use clap::Parser;
use keel_vfs::VPath;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Parser, Debug, PartialEq)]
#[command(name = "keel", version, about = "Keel file manager")]
pub struct Cli {
    /// Folder to open in a new tab.
    pub folder: Option<PathBuf>,
    /// Open a separate window even when Keel is already running.
    #[arg(long)]
    pub new_window: bool,
    /// Settings profile: <config dir>/profiles/<NAME>.
    #[arg(long, value_name = "NAME", value_parser = profile_name)]
    pub profile: Option<String>,
    /// Open a search tab with this query (in FOLDER when given).
    #[arg(long, value_name = "QUERY")]
    pub search: Option<String>,
}

/// What to open: from this process's command line, or handed over by a later `keel`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Request {
    /// Absolute.
    pub folder: Option<PathBuf>,
    pub search: Option<String>,
}

impl Cli {
    /// Parses `std::env::args`; `--help`, `--version` and bad arguments print and exit (a
    /// release build on Windows has no console of its own, so it borrows the shell's).
    pub fn from_env() -> Self {
        Self::try_parse().unwrap_or_else(|e| {
            #[cfg(windows)]
            keel_vfs::desktop::attach_parent_console();
            e.exit()
        })
    }

    /// The request, with FOLDER made absolute against this process's working folder (the
    /// running instance has its own).
    pub fn request(&self) -> Request {
        Request {
            folder: self
                .folder
                .as_ref()
                .and_then(|p| std::path::absolute(p).ok()),
            search: self.search.clone(),
        }
    }
}

/// Profile names become a folder name: letters, digits, `-`, `_`, `.`, and not `.`/`..`.
fn profile_name(name: &str) -> Result<String, String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(name.to_owned())
    } else {
        Err("use letters, digits, '-', '_' or '.' (at most 64)".into())
    }
}

static PROFILE: OnceLock<String> = OnceLock::new();

/// Sets the profile for this run (once, before settings load).
pub fn set_profile(name: Option<String>) {
    if let Some(name) = name {
        let _ = PROFILE.set(name);
    }
}

/// `--profile`, else "default".
pub fn profile() -> &'static str {
    PROFILE.get().map_or("default", String::as_str)
}

impl AppState {
    /// Opens what `req` asks for in the active pane: FOLDER in a new tab, then a search tab
    /// (from that folder, else the current one) running QUERY.
    pub fn external(&mut self, req: Request) {
        let p = self.active;
        if let Some(dir) = req.folder {
            self.run(p, Action::NewTabAt(VPath::local(dir)));
        }
        if let Some(text) = req.search {
            self.run(p, Action::Search);
            if let TabKind::Search { query, due, .. } = &mut self.tab_mut(p).kind {
                *query = text;
                *due = Some(Instant::now());
            }
            self.ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("keel").chain(args.iter().copied()))
    }

    #[test]
    fn parses_flags() {
        assert_eq!(
            parse(&[]).unwrap(),
            Cli {
                folder: None,
                new_window: false,
                profile: None,
                search: None
            }
        );
        let cli = parse(&[
            "D:\\",
            "--new-window",
            "--profile",
            "work",
            "--search",
            "two words",
        ])
        .unwrap();
        assert_eq!(cli.folder, Some(PathBuf::from("D:\\")));
        assert!(cli.new_window);
        assert_eq!(cli.profile.as_deref(), Some("work"));
        assert_eq!(cli.search.as_deref(), Some("two words"));
        let req = cli.request();
        assert!(req.folder.unwrap().is_absolute());
        assert_eq!(req.search.as_deref(), Some("two words"));
        // A relative folder is made absolute here, not in the running instance.
        let rel = parse(&["sub"]).unwrap().request().folder.unwrap();
        assert_eq!(rel, std::env::current_dir().unwrap().join("sub"));
    }

    #[test]
    fn rejects_bad_input() {
        use clap::error::ErrorKind;
        for bad in ["../x", "a/b", "a\\b", "..", ""] {
            assert!(parse(&["--profile", bad]).is_err(), "{bad:?}");
        }
        assert!(parse(&["--profile", "my.profile-2_x"]).is_ok());
        assert_eq!(
            parse(&["--bogus"]).unwrap_err().kind(),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            parse(&["a", "b"]).unwrap_err().kind(),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            parse(&["--help"]).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            parse(&["--version"]).unwrap_err().kind(),
            ErrorKind::DisplayVersion
        );
    }

    #[test]
    fn external_request_opens_folder_and_search() {
        let start = VPath::local(std::env::temp_dir());
        let ctx = egui::Context::default();
        let mut state = AppState::new(ctx, std::sync::Arc::new(keel_vfs::Router::new()), start);
        let dir = std::env::temp_dir().join("keel-external");
        state.external(Request {
            folder: Some(dir.clone()),
            search: Some("needle".into()),
        });
        let tabs = &state.panes[0].tabs;
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[1].dir, VPath::local(&dir));
        assert_eq!(tabs[2].dir, VPath::local(&dir), "search starts in FOLDER");
        assert!(
            matches!(&tabs[2].kind, TabKind::Search { query, due: Some(_), .. } if query == "needle")
        );
        assert_eq!(state.panes[0].active, 2);
        // Nothing asked: nothing opens.
        state.external(Request::default());
        assert_eq!(state.panes[0].tabs.len(), 3);
    }
}
