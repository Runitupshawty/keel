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
    /// Check once whether a newer Keel release is out (one request to GitHub) and exit.
    #[arg(long)]
    pub check_update: bool,
    /// Linux inside WSL: open FOLDER in this Linux window, not in the Windows Keel.
    #[arg(long)]
    pub here: bool,
    /// Open a keel: link (keel:search?q=QUERY[&path=FOLDER], keel:open?path=FOLDER); takes
    /// no other argument.
    #[arg(long, value_name = "LINK", exclusive = true)]
    pub url: Option<String>,
    /// Windows: open keel: links with this Keel (per user).
    #[arg(long, conflicts_with = "unregister_url_protocol")]
    pub register_url_protocol: bool,
    /// Windows: stop opening keel: links with this Keel.
    #[arg(long)]
    pub unregister_url_protocol: bool,
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
    /// Search the system index (Everything, Keel's own index, Spotlight or locate) with
    /// es.exe's switches; `keel es -help` lists them.
    #[command(disable_help_flag = true)]
    Es {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Search the library index.
    Search {
        query: String,
        /// At most N hits (default 100).
        #[arg(long, value_name = "N")]
        max: Option<usize>,
    },
    /// Folder sizes: each child of PATH by size, biggest first (from the library index;
    /// a folder outside the library is walked).
    Du { path: String },
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
    /// Back up the library to a file, or restore one as a new library.
    #[command(subcommand)]
    Library(LibraryCmd),
    /// Start, stop or check keel-daemon for the profile.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Any API operation by name (`keel op list` lists them). One that changes something
    /// prints its preview; `--execute` applies that preview right after printing it.
    /// Output is JSON unless stdout is a terminal (`--json` forces it).
    Op {
        /// The operation's name (`search`, `tags.set`, ...); `list` without parameters, or
        /// nothing, lists every operation.
        name: Option<String>,
        /// The parameters, a JSON object (default `{}`). Paths are absolute.
        #[arg(long, value_name = "JSON", conflicts_with = "params_file")]
        params: Option<String>,
        /// Read the parameters from this JSON file.
        #[arg(long, value_name = "FILE")]
        params_file: Option<PathBuf>,
        /// Apply the preview at once (it is printed first).
        #[arg(long)]
        execute: bool,
    },
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
    /// Merge folders: everything in FOLDERS moves into --into (one of them or another
    /// folder); the folders left empty are removed.
    Merge {
        #[arg(required = true)]
        folders: Vec<String>,
        #[arg(long, value_name = "DIR")]
        into: String,
        #[arg(long, value_enum)]
        on_conflict: Option<Conflict>,
    },
    /// Flatten a folder: every file in its subfolders moves up into it; the subfolders
    /// left empty are removed.
    Flatten {
        folder: String,
        #[arg(long, value_enum)]
        on_conflict: Option<Conflict>,
    },
    /// Organize a folder's files into subfolders by type, year or year-month.
    Organize {
        folder: String,
        #[arg(long, value_enum)]
        by: OrganizeBy,
        #[arg(long, value_enum)]
        on_conflict: Option<Conflict>,
    },
    /// Set attributes (hidden, read-only, Unix mode bits in octal).
    Attrs {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        hidden: Option<bool>,
        #[arg(long)]
        readonly: Option<bool>,
        #[arg(long, value_name = "OCTAL")]
        mode: Option<String>,
    },
    /// Make a link at LINK pointing to TARGET.
    Link {
        target: String,
        link: String,
        #[arg(long, value_enum)]
        kind: LinkKind,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
pub enum OrganizeBy {
    /// Images, Videos, Audio, Documents, Archives, Other.
    Type,
    Year,
    /// Year and month (2024-03).
    Month,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
pub enum LinkKind {
    /// A symbolic link (Windows: needs Developer Mode or elevation).
    Symlink,
    /// Windows: a junction to a folder.
    Junction,
    /// A second name for a file on the same drive.
    Hardlink,
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

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum LibraryCmd {
    /// Write a backup (the index with tags, favorites and content ids, and the sources) to
    /// FILE (`.keelbackup`), or to `<library>-<date>.keelbackup` in FILE when it is a folder.
    Backup { file: String },
    /// Restore a backup as a new library (never over an existing one).
    Restore {
        file: String,
        /// The new library's name (default: the backed-up library's).
        #[arg(long = "as", value_name = "NAME")]
        name: Option<String>,
    },
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
    /// `--version` also says whether this binary is signed, for bug reports; it is read
    /// only then, not at every start.
    pub fn from_env() -> Self {
        use std::io::IsTerminal;
        let interactive = std::io::stdin().is_terminal();
        Self::parse_args(std::env::args_os(), interactive, |p| p.is_dir()).unwrap_or_else(|e| {
            #[cfg(windows)]
            keel_vfs::desktop::attach_parent_console();
            if e.kind() == clap::error::ErrorKind::DisplayVersion {
                let _ = e.print();
                println!("signature: {}", keel_api::signature::current());
                std::process::exit(0);
            }
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
        // A link is one argument and nothing else, checked before anything else reads `args`.
        if let Some(link) = url_only(&args) {
            let mut cli = Self::try_parse_from(&args[..1])?;
            cli.url = Some(link?);
            return Ok(cli);
        }
        // `keel es …`: es.exe's switches go to `es` as typed (clap would take `--` and
        // `-h` itself).
        if let Some(i) = es_index(&args) {
            let rest: Vec<String> = (args.split_off(i + 1).iter())
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let mut cli = Self::try_parse_from(args)?;
            if let Some(Command::Es { args }) = &mut cli.command {
                *args = rest;
            }
            return Ok(cli);
        }
        if let Some(i) = lone_subcommand(&args).filter(|_| interactive) {
            let always = ALWAYS_COMMANDS.iter().any(|c| args[i] == **c);
            if !always && is_dir(Path::new(&args[i])) {
                args[i] = Path::new(".").join(&args[i]).into_os_string();
            }
        }
        Self::try_parse_from(args)
    }

    /// The request, with FOLDER made absolute against this process's working folder (the
    /// running instance has its own). A `--url` link that is refused opens nothing (logged).
    pub fn request(&self) -> Request {
        if let Some(url) = &self.url {
            return crate::url_protocol::request(url).unwrap_or_else(|e| {
                tracing::warn!("--url: {e}");
                Request::default()
            });
        }
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

/// What the registered `keel:` handler runs: `"keel.exe" --url-handler -- "%1"`
/// (`url_protocol::command_line`).
pub const URL_HANDLER: &str = "--url-handler";

/// Whether `arg` asks to open a link: `--url`, `--url=…`, `--url-handler`, `--url-handler=…`.
fn is_url_arg(arg: &OsString) -> bool {
    let a = arg.to_string_lossy();
    a == "--url"
        || a == URL_HANDLER
        || a.starts_with("--url=")
        || a.starts_with(&format!("{URL_HANDLER}="))
}

/// The link when `args` open one, refused unless it is the only thing on the command line:
/// `keel --url-handler -- LINK`, `keel --url LINK` or `keel --url=LINK`, LINK starting with
/// `keel:`. Windows splits the handler's `"%1"` at any `"` a launcher left in the link
/// (`.url` files, Office, `start` do not encode it), so `keel:x" es -export-txt F` arrives as
/// several arguments; none of them may ever reach another option or a subcommand.
fn url_only(args: &[OsString]) -> Option<Result<String, clap::Error>> {
    let rest = args.get(1..)?;
    if !rest.iter().any(is_url_arg) {
        return None;
    }
    let refuse = |why: &str| -> Result<String, clap::Error> {
        Err(Cli::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            format!("--url takes one keel: link and nothing else ({why})"),
        ))
    };
    let strs: Option<Vec<&str>> = rest.iter().map(|a| a.to_str()).collect();
    let Some(strs) = strs else {
        return Some(refuse("not valid text"));
    };
    let link = match strs.as_slice() {
        [h, "--", link] if *h == URL_HANDLER => *link,
        ["--url", link] => *link,
        [one] => match one.strip_prefix("--url=") {
            Some(link) => link,
            None => return Some(refuse("no link")),
        },
        _ => return Some(refuse("other arguments were given")),
    };
    // Do not repair launcher quoting or concatenate split arguments. Literal spaces,
    // quotes and controls must be percent-encoded; encoded values remain search data.
    if link.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"') {
        return Some(refuse("quotes and whitespace must be percent-encoded"));
    }
    if let Err(e) = keel_core::link::parse(link) {
        return Some(refuse(&e.to_string()));
    }
    Some(Ok(link.to_owned()))
}

/// Subcommands a lone name always means (never a folder of that name): what agents and
/// scripts run, and what would surprise as a window.
const ALWAYS_COMMANDS: &[&str] = &["mcp", "execute", "daemon", "search", "es"];

/// Options that take the next argument as their value.
const TAKE_VALUE: &[&str] = &["--profile", "--search", "--index-service", "--url"];

/// `query` searched only in `dir`: `in:"<dir>" <query>` (every system search backend reads
/// `in:`), unless the query already has an `in:`.
pub fn scoped_query(dir: &Path, query: &str) -> String {
    if keel_search::take_within(query).0.is_some() {
        return query.to_owned();
    }
    format!("in:\"{}\" {query}", dir.display())
}

/// The index of `es` when it is the subcommand: the first argument that is not an option
/// or an option's value.
fn es_index(args: &[OsString]) -> Option<usize> {
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].to_str()?;
        match arg {
            a if TAKE_VALUE.contains(&a) => i += 2,
            "--" => return None,
            a if a.starts_with('-') => i += 1,
            "es" => return Some(i),
            _ => return None,
        }
    }
    None
}

/// The index of the only positional argument in `args` when it is the last argument and a
/// subcommand's name.
fn lone_subcommand(args: &[OsString]) -> Option<usize> {
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
    /// one) running QUERY, only in FOLDER when one was given (`in:"FOLDER"` leads the query,
    /// so the scope shows and can be edited).
    pub fn external(&mut self, req: Request) {
        let p = self.active;
        let scope = req.folder.clone();
        if let Some(dir) = req.folder {
            let dir = VPath::local(dir);
            match req.select {
                Some(name) => self.reveal_in(p, dir, name, true),
                None => self.run(p, Action::NewTabAt(dir)),
            }
        }
        if let Some(text) = req.search {
            self.run(p, Action::Search);
            let text = match scope {
                Some(dir) => scoped_query(&dir, &text),
                None => text,
            };
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

    /// `Cli::parse_args`, as `main` calls it.
    fn full(args: &[&str]) -> Result<Cli, clap::Error> {
        let args = std::iter::once("keel").chain(args.iter().copied());
        Cli::parse_args(args.map(OsString::from), true, |_| true)
    }

    /// Review 1 blocker: Windows splits the handler's `"%1"` at a `"` left in the link, so a
    /// link such as `keel:search?q=a" es -export-txt F -n 0 x` arrives as several arguments.
    /// Whatever follows (or precedes) the link, nothing but the link is ever read.
    #[test]
    fn a_link_never_reaches_other_options_or_subcommands() {
        let link = "keel:search?q=a";
        let tails: &[&[&str]] = &[
            &["es", "-export-txt", "thesis.docx", "-n", "0", "x\""],
            &["op", "files.delete", "--params", "{}", "--execute"],
            &["execute", "p1", "--hash", "h"],
            &["mcp"],
            &["--register-url-protocol"],
            &["--unregister-url-protocol"],
            &["--index-service", "x"],
            &["--check-update"],
            &["--profile", "p"],
            &["--new-window"],
            &["--json"],
            &["--here"],
            &["--search", "y"],
            &["--url", "keel:open?path=x"],
            &["--", "es"],
            &["D:\\"],
            &["x"],
        ];
        let heads: &[&[&str]] = &[&["--url-handler", "--", link], &["--url", link]];
        for tail in tails {
            for head in heads {
                let after: Vec<&str> = head.iter().chain(tail.iter()).copied().collect();
                let before: Vec<&str> = tail.iter().chain(head.iter()).copied().collect();
                for args in [after, before] {
                    for interactive in [true, false] {
                        let got = Cli::parse_args(
                            std::iter::once("keel")
                                .chain(args.iter().copied())
                                .map(OsString::from),
                            interactive,
                            |_| true,
                        );
                        assert!(got.is_err(), "{args:?} parsed: {got:?}");
                    }
                }
            }
        }
        // The handler's form only: no link, no `--`, a flag or a non-keel: value as the link.
        for args in [
            &["--url-handler"][..],
            &["--url-handler", "--"],
            &["--url-handler", link],
            &["--url-handler", "--", "--register-url-protocol"],
            &["--url-handler", "--", "es"],
            &["--url-handler", "--", "https://example.com"],
            &["--url-handler=keel:search?q=a"],
            &["--url"],
            &["--url", "--check-update"],
            &["--url=-x"],
            &["es", "--url-handler", "--", link],
            &["search", "--url", link],
        ] {
            assert!(full(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn handler_validates_links_before_cli_dispatch() {
        for link in [
            "keel:search?q=a\" es -export-txt target -n 0 x",
            "keel:search?q=a op files.delete --execute",
            "keel:search?q=a\t--profile=p",
            "keel:search?q=%zz",
            "keel:search?q=%ff",
            "keel:search?q=a&q=b",
            "keel:run?path=x",
            "keel:open?path=relative",
        ] {
            assert!(full(&[URL_HANDLER, "--", link]).is_err(), "{link}");
        }
        for query in ["a\" es -export-txt target", "op files.delete --execute", "--profile=p"] {
            let link = keel_core::link::Link::Search {
                query: query.into(),
                path: None,
            }.to_url();
            let cli = full(&[URL_HANDLER, "--", &link]).unwrap();
            assert!(cli.command.is_none());
            assert!(cli.profile.is_none());
            assert!(!cli.register_url_protocol);
            assert_eq!(crate::url_protocol::request(cli.url.as_deref().unwrap()).unwrap().search.as_deref(), Some(query));
        }
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
                check_update: false,
                here: false,
                url: None,
                register_url_protocol: false,
                unregister_url_protocol: false,
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
        // A second positional is refused (clap calls it a subcommand when one is similar:
        // "b" and "library").
        assert!(matches!(
            parse(&["a", "b"]).unwrap_err().kind(),
            ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand
        ));
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
            matches!(&tabs[2].kind, TabKind::Search { query, due: Some(_), .. }
                if *query == format!("in:\"{}\" needle", dir.display())),
            "the search runs only in FOLDER"
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
        assert_eq!(
            cmd(&["library", "backup", "b.keelbackup"]),
            Command::Library(LibraryCmd::Backup {
                file: "b.keelbackup".into()
            })
        );
        assert_eq!(
            cmd(&["library", "restore", "b.keelbackup", "--as", "copy"]),
            Command::Library(LibraryCmd::Restore {
                file: "b.keelbackup".into(),
                name: Some("copy".into())
            })
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
            cmd(&["plan", "merge", "a", "b", "--into", "b"]),
            Command::Plan(PlanCmd::Merge {
                folders: vec!["a".into(), "b".into()],
                into: "b".into(),
                on_conflict: None
            })
        );
        assert_eq!(
            cmd(&["plan", "organize", "d", "--by", "month"]),
            Command::Plan(PlanCmd::Organize {
                folder: "d".into(),
                by: OrganizeBy::Month,
                on_conflict: None
            })
        );
        assert_eq!(
            cmd(&["plan", "attrs", "a", "--readonly", "true", "--mode", "644"]),
            Command::Plan(PlanCmd::Attrs {
                paths: vec!["a".into()],
                hidden: None,
                readonly: Some(true),
                mode: Some("644".into())
            })
        );
        assert_eq!(
            cmd(&["plan", "link", "t", "l", "--kind", "hardlink"]),
            Command::Plan(PlanCmd::Link {
                target: "t".into(),
                link: "l".into(),
                kind: LinkKind::Hardlink
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
        assert_eq!(
            cmd(&["du", "photos"]),
            Command::Du {
                path: "photos".into()
            }
        );
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
        assert_eq!(
            cmd(&["op", "tags.set", "--params", "{}", "--execute"]),
            Command::Op {
                name: Some("tags.set".into()),
                params: Some("{}".into()),
                params_file: None,
                execute: true
            }
        );
        assert_eq!(
            cmd(&["op"]),
            Command::Op {
                name: None,
                params: None,
                params_file: None,
                execute: false
            }
        );
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
            &["op", "search", "--params", "{}", "--params-file", "p.json"],
            &["plan", "merge", "a"],
            &["plan", "organize", "d"],
            &["plan", "link", "t", "l"],
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

    #[test]
    fn es_takes_its_switches_as_typed() {
        let parse = |args: &[&str]| {
            let args = std::iter::once("keel").chain(args.iter().copied());
            Cli::parse_args(args.map(OsString::from), true, |_| true).unwrap()
        };
        let es = |cli: Cli| match cli.command {
            Some(Command::Es { args }) => args,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            es(parse(&["es", "-n", "5", "--", "-h", "/ad", "two words"])),
            ["-n", "5", "--", "-h", "/ad", "two words"]
        );
        assert_eq!(es(parse(&["es"])), Vec::<String>::new());
        assert_eq!(es(parse(&["es", "-help"])), ["-help"]);
        let cli = parse(&["--profile", "work", "--json", "es", "-csv", "--json", "x"]);
        assert_eq!((cli.profile.as_deref(), cli.json), (Some("work"), true));
        assert_eq!(es(cli), ["-csv", "--json", "x"]);
        // A folder named es is not opened by `keel es` (it is always the subcommand), and
        // `es` as an option's value is not the subcommand.
        assert_eq!(es(parse(&["es"])), Vec::<String>::new());
        let cli = parse(&["--search", "es"]);
        assert_eq!((cli.command, cli.search.as_deref()), (None, Some("es")));
        // Plain clap parsing (no `parse_args`) keeps hyphenated values too.
        let cli = Cli::try_parse_from(["keel", "es", "-n", "5", "x"]).unwrap();
        assert_eq!(es(cli), ["-n", "5", "x"]);
    }

    #[test]
    fn url_opens_a_keel_link() {
        let (dir, enc) = if cfg!(windows) {
            (PathBuf::from(r"C:\Users\Public"), r"C:\Users\Public")
        } else {
            (PathBuf::from("/tmp"), "/tmp")
        };
        let cli = parse(&["--url", &format!("keel:search?q=a+b&path={enc}")]).unwrap();
        assert_eq!(
            cli.request(),
            Request {
                folder: Some(dir.clone()),
                search: Some("a b".into()),
                select: None
            }
        );
        let cli = parse(&["--url", &format!("keel:open?path={enc}")]).unwrap();
        assert_eq!(cli.request().folder, Some(dir));
        // A refused link opens nothing.
        let cli = parse(&["--url", "keel:open?path=%5C%5Chost%5Cshare"]).unwrap();
        assert_eq!(cli.request(), Request::default());
        // Not with a folder or a search of its own.
        assert!(parse(&["--url", "keel:search?q=x", "D:\\"]).is_err());
        assert!(parse(&["--url", "keel:search?q=x", "--search", "y"]).is_err());
        // What the registered handler runs.
        let cli = full(&["--url-handler", "--", "keel:search?q=a+b"]).unwrap();
        assert_eq!(cli.url.as_deref(), Some("keel:search?q=a+b"));
        assert_eq!(cli.command, None);
        let cli = full(&["--url=KEEL:search?q=x"]).unwrap();
        assert_eq!(cli.url.as_deref(), Some("KEEL:search?q=x"));
        let cli = parse(&["--register-url-protocol"]).unwrap();
        assert!(cli.register_url_protocol && !cli.unregister_url_protocol);
        assert!(parse(&["--register-url-protocol", "--unregister-url-protocol"]).is_err());
    }

    #[test]
    fn scoped_queries_lead_with_the_folder() {
        let dir = Path::new("/x y");
        assert_eq!(scoped_query(dir, "a b"), "in:\"/x y\" a b");
        assert_eq!(scoped_query(dir, "in:/other a"), "in:/other a");
    }
}
