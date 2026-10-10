//! Command line (Task 24): `keel [FOLDER] [--new-window] [--profile NAME] [--search QUERY]`,
//! and the request a later `keel` hands to the running instance (`single_instance`).
//! Task 37: subcommands (`keel search`, `keel plan`, `keel mcp`, ...; see `commands`) run
//! without a window. Typed at a terminal, a lone argument that names an existing folder
//! opens it even when it is also a subcommand name (`keel devices` in a folder that has
//! `devices`), except `mcp`, `execute`, `daemon` and `search`, which are always the
//! subcommand; without a terminal (an agent starting `keel mcp`, a script) no name is
//! taken as a folder. `keel ./name` always means the folder.

use crate::keys::Action;
use crate::state::AppState;
use crate::tab::TabKind;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use keel_vfs::VPath;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
    #[arg(long, global = true, value_name = "NAME", value_parser = profile_name)]
    pub profile: Option<String>,
    /// Open a search tab with this query (in FOLDER when given).
    #[arg(long, value_name = "QUERY")]
    pub search: Option<String>,
    /// Internal: the elevated NTFS index service (`keel_search::request_full_index`).
    #[arg(long, value_name = "DIR", hide = true)]
    pub index_service: Option<PathBuf>,
    /// Subcommands: print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// `keel <subcommand>`: talks to keel-daemon when it runs for the profile, else opens the
/// library in this process. Exit codes: 0 ok, 1 operation error, 2 usage.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum Command {
    /// Search the library index.
    Search {
        query: String,
        /// At most N hits (default 100).
        #[arg(long, value_name = "N")]
        max: Option<usize>,
    },
    /// Tag or untag indexed files.
    #[command(subcommand)]
    Tag(TagCmd),
    /// Preview a file operation: prints the preview, its plan id and input hash.
    #[command(subcommand)]
    Plan(PlanCmd),
    /// Apply a previewed plan. Without PLAN, reads `keel plan` output from stdin.
    Execute {
        /// The plan id `keel plan` printed.
        plan: Option<String>,
        /// The input hash `keel plan` printed.
        #[arg(long, value_name = "HASH")]
        hash: Option<String>,
        /// Return once the job started instead of waiting for it.
        #[arg(long)]
        no_wait: bool,
    },
    /// Paired devices.
    Devices,
    /// Grants to paired devices.
    Shares,
    /// Library sources (lists them without a subcommand).
    Sources {
        #[command(subcommand)]
        action: Option<SourcesCmd>,
    },
    /// Mount a library source (or a folder in it) as a drive letter or folder; keel-daemon
    /// serves it until `keel unmount` or until the daemon stops.
    Mount {
        /// Source id or label (see `keel sources`).
        source: String,
        /// A drive letter (`K:`) or a folder to mount on.
        target: String,
        /// Mount only this folder of the source (relative to its root).
        #[arg(long, value_name = "PATH")]
        subtree: Option<String>,
    },
    /// Unmount a Keel mount (writes still in progress there are discarded).
    Unmount { target: String },
    /// Active Keel mounts.
    Mounts,
    /// Start, stop or check keel-daemon for the profile.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// MCP server over stdio (for Claude Code, Codex and other MCP clients). `execute`
    /// asks the user to confirm each plan through the client (MCP elicitation).
    Mcp {
        /// Let clients that cannot ask the user (no elicitation) execute plans: each call
        /// must then repeat the preview's summary, which the client's approval shows.
        #[arg(long)]
        allow_execute: bool,
    },
}

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum TagCmd {
    /// Tag indexed files (the tag is created when missing).
    Add {
        tag: String,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Remove a tag from indexed files.
    Remove {
        tag: String,
        #[arg(required = true)]
        paths: Vec<String>,
    },
}

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum PlanCmd {
    /// Copy into a folder.
    Copy {
        #[arg(required = true)]
        src: Vec<String>,
        #[arg(long, value_name = "DIR")]
        to: String,
        #[arg(long, value_enum)]
        on_conflict: Option<Conflict>,
    },
    /// Move into a folder.
    Move {
        #[arg(required = true)]
        src: Vec<String>,
        #[arg(long, value_name = "DIR")]
        to: String,
        #[arg(long, value_enum)]
        on_conflict: Option<Conflict>,
    },
    /// Delete (to the trash where the provider has one).
    Delete {
        #[arg(required = true)]
        paths: Vec<String>,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
pub enum Conflict {
    Skip,
    Overwrite,
    /// Keep both (the new one gets a free name).
    Rename,
}

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum SourcesCmd {
    /// Add a folder as a source and index it.
    Add {
        path: String,
        #[arg(long)]
        label: Option<String>,
        /// Do not index it now.
        #[arg(long)]
        no_index: bool,
    },
    /// Forget a source (its files are not touched).
    Remove {
        id: String,
        /// Also delete its index store.
        #[arg(long)]
        delete_store: bool,
    },
    /// Index a source now.
    Index { id: String },
}

#[derive(Subcommand, Debug, Clone, Copy, PartialEq)]
pub enum DaemonCmd {
    /// Start keel-daemon in the background.
    Start,
    /// Ask the running keel-daemon to stop.
    Stop,
    /// Whether keel-daemon runs (exit 0) or not (exit 1).
    Status,
    /// Replace the token of keel-daemon's --ws and --web: clients must sign in again
    /// (sessions signed in with the old token are closed).
    RotateToken,
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
        use std::io::IsTerminal;
        let interactive = std::io::stdin().is_terminal();
        Self::parse_args(std::env::args_os(), interactive, |p| p.is_dir()).unwrap_or_else(|e| {
            #[cfg(windows)]
            keel_vfs::desktop::attach_parent_console();
            e.exit()
        })
    }

    /// Parses `args` (the program name first). When `interactive` (stdin is a terminal),
    /// the only positional argument is the last one, names a subcommand other than
    /// `ALWAYS_COMMANDS` and `is_dir` says it is a folder, it is the folder (`./<name>`),
    /// not the subcommand.
    pub fn parse_args(
        args: impl IntoIterator<Item = OsString>,
        interactive: bool,
        is_dir: impl Fn(&Path) -> bool,
    ) -> Result<Self, clap::Error> {
        let mut args: Vec<OsString> = args.into_iter().collect();
        if let Some(i) = lone_subcommand(&args).filter(|_| interactive) {
            let always = ALWAYS_COMMANDS.iter().any(|c| args[i] == **c);
            if !always && is_dir(Path::new(&args[i])) {
                args[i] = Path::new(".").join(&args[i]).into_os_string();
            }
        }
        Self::try_parse_from(args)
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

/// Subcommands a lone name always means (never a folder of that name): what agents and
/// scripts run, and what would surprise as a window.
const ALWAYS_COMMANDS: &[&str] = &["mcp", "execute", "daemon", "search"];

/// The index of the only positional argument in `args` when it is the last argument and a
/// subcommand's name.
fn lone_subcommand(args: &[OsString]) -> Option<usize> {
    const TAKE_VALUE: &[&str] = &["--profile", "--search", "--index-service"];
    let mut positional = None;
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].to_str()?;
        if arg == "--" {
            return None;
        }
        i += match arg {
            a if TAKE_VALUE.contains(&a) => 2,
            a if a.starts_with('-') => 1,
            _ if positional.is_some() => return None,
            _ => {
                positional = Some(i);
                1
            }
        };
    }
    let i = positional.filter(|&i| i == args.len() - 1)?;
    let name = args[i].to_str()?;
    Cli::command()
        .get_subcommands()
        .any(|c| c.get_name() == name)
        .then_some(i)
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
                json: false,
                command: None,
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

    #[test]
    fn parses_subcommands() {
        let cmd = |args: &[&str]| parse(args).unwrap().command.unwrap();
        assert_eq!(
            cmd(&["search", "two words", "--max", "5"]),
            Command::Search {
                query: "two words".into(),
                max: Some(5)
            }
        );
        let cli = parse(&["search", "x", "--json", "--profile", "work"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.profile.as_deref(), Some("work"));
        assert_eq!(cli.folder, None);
        assert_eq!(
            cmd(&["tag", "add", "receipts", "a.pdf", "b.pdf"]),
            Command::Tag(TagCmd::Add {
                tag: "receipts".into(),
                paths: vec!["a.pdf".into(), "b.pdf".into()]
            })
        );
        assert_eq!(
            cmd(&["tag", "remove", "receipts", "a.pdf"]),
            Command::Tag(TagCmd::Remove {
                tag: "receipts".into(),
                paths: vec!["a.pdf".into()]
            })
        );
        assert_eq!(
            cmd(&[
                "plan",
                "copy",
                "a",
                "b",
                "--to",
                "dst",
                "--on-conflict",
                "rename"
            ]),
            Command::Plan(PlanCmd::Copy {
                src: vec!["a".into(), "b".into()],
                to: "dst".into(),
                on_conflict: Some(Conflict::Rename)
            })
        );
        assert_eq!(
            cmd(&["plan", "move", "a", "--to", "dst"]),
            Command::Plan(PlanCmd::Move {
                src: vec!["a".into()],
                to: "dst".into(),
                on_conflict: None
            })
        );
        assert_eq!(
            cmd(&["plan", "delete", "a"]),
            Command::Plan(PlanCmd::Delete {
                paths: vec!["a".into()]
            })
        );
        assert_eq!(
            cmd(&["execute", "abc", "--hash", "def"]),
            Command::Execute {
                plan: Some("abc".into()),
                hash: Some("def".into()),
                no_wait: false
            }
        );
        assert_eq!(
            cmd(&["execute"]),
            Command::Execute {
                plan: None,
                hash: None,
                no_wait: false
            }
        );
        assert_eq!(cmd(&["devices"]), Command::Devices);
        assert_eq!(cmd(&["shares"]), Command::Shares);
        assert_eq!(cmd(&["sources"]), Command::Sources { action: None });
        assert_eq!(
            cmd(&["sources", "add", "D:/x", "--label", "X"]),
            Command::Sources {
                action: Some(SourcesCmd::Add {
                    path: "D:/x".into(),
                    label: Some("X".into()),
                    no_index: false
                })
            }
        );
        assert_eq!(
            cmd(&["sources", "remove", "id1", "--delete-store"]),
            Command::Sources {
                action: Some(SourcesCmd::Remove {
                    id: "id1".into(),
                    delete_store: true
                })
            }
        );
        assert_eq!(
            cmd(&["sources", "index", "id1"]),
            Command::Sources {
                action: Some(SourcesCmd::Index { id: "id1".into() })
            }
        );
        assert_eq!(cmd(&["daemon", "start"]), Command::Daemon(DaemonCmd::Start));
        assert_eq!(cmd(&["daemon", "stop"]), Command::Daemon(DaemonCmd::Stop));
        assert_eq!(
            cmd(&["daemon", "status"]),
            Command::Daemon(DaemonCmd::Status)
        );
        assert_eq!(
            cmd(&["mcp"]),
            Command::Mcp {
                allow_execute: false
            }
        );
        assert_eq!(
            cmd(&["mcp", "--allow-execute"]),
            Command::Mcp {
                allow_execute: true
            }
        );
        assert_eq!(
            cmd(&["mount", "Photos", "K:", "--subtree", "2026"]),
            Command::Mount {
                source: "Photos".into(),
                target: "K:".into(),
                subtree: Some("2026".into())
            }
        );
        assert_eq!(
            cmd(&["unmount", "K:"]),
            Command::Unmount {
                target: "K:".into()
            }
        );
        assert_eq!(cmd(&["mounts"]), Command::Mounts);
        // Usage errors (exit code 2).
        for bad in [
            &["plan", "copy", "a"][..],
            &["plan", "delete"],
            &["tag", "add", "t"],
            &["search"],
            &["daemon"],
            &["mount", "Photos"],
            &["unmount"],
            &["plan", "copy", "a", "--to", "d", "--on-conflict", "maybe"],
        ] {
            let err = parse(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?}");
        }
        // A folder that is not a subcommand still opens the window.
        let cli = parse(&[r"D:\work"]).unwrap();
        assert_eq!(cli.command, None);
        assert!(cli.folder.is_some());
    }

    #[test]
    fn a_folder_named_like_a_subcommand_opens() {
        let folders = |p: &Path| {
            ["search", "devices", "mcp"]
                .iter()
                .any(|f| p == Path::new(f))
        };
        let parse_in = |interactive: bool, args: &[&str]| {
            let args = std::iter::once("keel").chain(args.iter().copied());
            Cli::parse_args(args.map(OsString::from), interactive, folders).unwrap()
        };
        let parse = |args: &[&str]| parse_in(true, args);
        let cli = parse(&["devices"]);
        assert_eq!(cli.command, None);
        assert_eq!(cli.folder, Some(Path::new(".").join("devices")));
        assert!(cli.request().folder.unwrap().ends_with("devices"));
        let cli = parse(&["--profile", "work", "--new-window", "devices"]);
        assert_eq!((cli.command, cli.profile.as_deref()), (None, Some("work")));
        // `mcp` (and execute, daemon, search) is always the subcommand, folder or not.
        assert_eq!(
            parse(&["mcp"]).command,
            Some(Command::Mcp {
                allow_execute: false
            })
        );
        // Without a terminal (an agent, a script) no name opens a folder.
        let piped = parse_in(false, &["mcp"]);
        assert_eq!(
            (piped.command, piped.folder),
            (
                Some(Command::Mcp {
                    allow_execute: false
                }),
                None
            )
        );
        assert_eq!(
            parse_in(false, &["devices"]).command,
            Some(Command::Devices)
        );
        // Not a folder here, or followed by anything: the subcommand.
        assert_eq!(parse(&["shares"]).command, Some(Command::Shares));
        assert_eq!(
            parse(&["devices", "--json"]).command,
            Some(Command::Devices)
        );
        assert!(matches!(
            parse(&["search", "x"]).command,
            Some(Command::Search { .. })
        ));
        assert!(matches!(
            parse(&["mcp", "--allow-execute"]).command,
            Some(Command::Mcp { .. })
        ));
        // `./name` is always the folder.
        let cli = parse(&["./shares"]);
        assert_eq!(
            (cli.command, cli.folder),
            (None, Some(PathBuf::from("./shares")))
        );
    }
}
