//! Command line (Task 24): `keel [FOLDER] [--new-window] [--profile NAME] [--search QUERY]`,
//! and the request a later `keel` hands to the running instance (`single_instance`).

use crate::keys::Action;
use crate::state::AppState;
use crate::tab::TabKind;
use clap::Parser;
use keel_vfs::VPath;
use std::path::PathBuf;
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
    /// Internal: the elevated NTFS index service (`keel_search::request_full_index`).
    #[arg(long, value_name = "DIR", hide = true)]
    pub index_service: Option<PathBuf>,
}

/// What to open: from this process's command line, or handed over by a later `keel`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Request {
    /// Absolute (`checked`). A file opens its folder with it selected.
    pub folder: Option<PathBuf>,
    pub search: Option<String>,
    /// Set by `checked`: the file in `folder` to select. Never sent.
    #[serde(skip)]
    pub select: Option<String>,
}

impl Request {
    /// Ready to open: a relative FOLDER is refused (a later `keel` sends it absolute; the
    /// running instance's working folder is not the sender's), and a FOLDER that is a file
    /// becomes its parent with the file selected. UNC paths (`\\server\share`) are allowed:
    /// the request comes from this user (`single_instance`), who may open them anyway, but
    /// checking one contacts that server. Touches the disk: not on the UI thread.
    pub fn checked(mut self) -> Result<Self, String> {
        let Some(path) = &self.folder else {
            return Ok(self);
        };
        if !path.is_absolute() {
            return Err(format!("Not an absolute path: {}", path.display()));
        }
        if path.is_file() {
            if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
                self.select = Some(name.to_string_lossy().into_owned());
                self.folder = Some(dir.to_owned());
            }
        }
        Ok(self)
    }
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
            select: None,
        }
    }
}

/// Profile names become a folder name (`profiles::valid_name`).
fn profile_name(name: &str) -> Result<String, String> {
    match crate::profiles::valid_name(name) {
        true => Ok(name.to_owned()),
        false => Err(crate::profiles::NAME_RULE.into()),
    }
}

/// The profile in use: `--profile` at start, then whatever Settings → Profiles switched to.
pub fn profile() -> String {
    crate::profiles::current()
}

impl AppState {
    /// Opens what `req` (`Request::checked`) asks for in the active pane: FOLDER in a new
    /// tab (with the file selected), then a search tab (from that folder, else the current
    /// one) running QUERY.
    pub fn external(&mut self, req: Request) {
        let p = self.active;
        if let Some(dir) = req.folder {
            let dir = VPath::local(dir);
            match req.select {
                Some(name) => self.reveal_in(p, dir, name, true),
                None => self.run(p, Action::NewTabAt(dir)),
            }
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
                search: None,
                index_service: None,
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
    fn parses_hidden_index_service() {
        let cli = parse(&["--index-service", r"C:\cache\index"]).unwrap();
        assert_eq!(cli.index_service, Some(PathBuf::from(r"C:\cache\index")));
        assert_eq!(cli.folder, None);
        assert!(parse(&["--index-service"]).is_err(), "needs a folder");
        let help = <Cli as clap::CommandFactory>::command()
            .render_help()
            .to_string();
        assert!(!help.contains("index-service"), "hidden from --help");
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
            select: None,
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
        // A file: its folder, with it selected.
        let file = std::env::current_exe().unwrap();
        let req = Request {
            folder: Some(file.clone()),
            ..Request::default()
        };
        state.external(req.checked().unwrap());
        let tab = &state.panes[0].tabs[3];
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(tab.dir, VPath::local(file.parent().unwrap()));
        assert!(tab.selected.contains(&name) && tab.cursor.as_ref() == Some(&name));
    }

    #[test]
    fn checks_requests() {
        let rel = Request {
            folder: Some(PathBuf::from("sub")),
            ..Request::default()
        };
        assert!(rel.checked().is_err(), "relative paths are refused");
        #[cfg(windows)]
        for bad in [r"C:sub", r"\sub"] {
            let req = Request {
                folder: Some(PathBuf::from(bad)),
                ..Request::default()
            };
            assert!(req.checked().is_err(), "{bad}");
        }
        let dir = Request {
            folder: Some(std::env::temp_dir()),
            search: Some("q".into()),
            ..Request::default()
        };
        assert_eq!(
            dir.clone().checked().unwrap(),
            dir,
            "folders stay as they are"
        );
        let file = std::env::current_exe().unwrap();
        let req = Request {
            folder: Some(file.clone()),
            ..Request::default()
        }
        .checked()
        .unwrap();
        assert_eq!(req.folder.as_deref(), file.parent());
        assert_eq!(
            req.select.as_deref(),
            file.file_name().and_then(|n| n.to_str())
        );
        // `select` stays out of the wire format.
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("select"), "{json}");
        assert_eq!(Request::default().checked().unwrap(), Request::default());
    }
}
