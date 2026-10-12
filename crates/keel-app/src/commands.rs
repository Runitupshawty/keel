//! `keel <subcommand>` (Task 37): the `keel-api` operations from the command line, through
//! keel-daemon when one runs for the profile, else on the library opened in this process.
//! `--json` prints one JSON document per invocation (the API's JSON, or `{"error": …}`);
//! exit codes are 0 (ok), 1 (the operation failed) and 2 (usage). Mutating subcommands show the preview and then confirm it themselves (the
//! command line is the confirmation), except `keel plan`, whose plan `keel execute` runs.

use crate::cli::{
    Command, Conflict, DaemonCmd, DuArgs, LibraryCmd, LinkKind, OrganizeBy, PlanCmd, SourcesCmd,
    TagCmd,
};
use keel_api::client::Client;
use keel_api::config::HostConfig;
use keel_api::host::{Host, NetSetup};
use keel_api::types::{
    ChildSize, FolderSizes, Hit, JobInfo, PlanPreview, RestoredLibrary, RestoredSourceInfo,
};
use keel_api::{ApiError, Backend};
use keel_core::JobStatus;
use serde_json::{json, Value};
use std::io::{IsTerminal, Read, Write};
use std::time::{Duration, Instant};

pub const OK: i32 = 0;
pub const FAILED: i32 = 1;
pub const USAGE: i32 = 2;
/// `keel du --strict`: everything was written, but the scan is partial (folders that could
/// not be read, or cut short).
pub const PARTIAL: i32 = 3;

/// The library in this process (no daemon runs): closed on drop. keel-net comes online
/// only for the calls that need it (devices and shares, and executing their plans).
struct Local {
    host: Host,
    cfg: HostConfig,
}

/// Devices and shares need keel-net.
fn net_op(method: &str) -> bool {
    method.starts_with("devices.") || method.starts_with("shares.")
}

impl Backend for Local {
    fn call(&mut self, method: &str, params: Value) -> keel_api::Result<Value> {
        let plan = params["plan_id"].as_str().unwrap_or_default();
        let executes_net_plan =
            method == "execute" && self.host.ctx.plans.method(plan).is_some_and(|m| net_op(&m));
        if net_op(method) || executes_net_plan {
            self.host
                .open_net(&self.cfg, NetSetup::system())
                .map_err(|e| ApiError::failed(format!("{e:#}")))?;
        }
        keel_api::call(&self.host.ctx, method, params)
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        // Jobs the command started that outlive it (the text job a walk starts, hashing)
        // stop here and resume the next time the library opens: say which.
        let running: Vec<String> = (self.host.ctx.lib.jobs().list().unwrap_or_default())
            .into_iter()
            .filter(|j| matches!(j.status, JobStatus::Running | JobStatus::Queued))
            .map(|j| j.kind)
            .collect();
        let closed = self.host.close();
        if !running.is_empty() || !closed {
            let what = match running.is_empty() {
                true => "a job".to_owned(),
                false => format!("the {} job", running.join(", ")),
            };
            eprintln!(
                "keel: {what} was still running; it resumes when Keel or keel-daemon next opens \
                 the library"
            );
        }
    }
}

/// keel-daemon for the profile, else the library in-process (keel-net only then, so one
/// node per profile).
/// `wait`: how long the daemon's answers are waited for.
pub(crate) fn connect(cfg: &HostConfig, wait: Duration) -> anyhow::Result<Box<dyn Backend>> {
    if let Ok(mut client) = Client::connect(&cfg.socket_name()) {
        client.timeout = wait;
        return Ok(Box::new(client));
    }
    let offset = chrono::Local::now().offset().local_minus_utc().into();
    // Another `keel` command may be finishing with the library: wait a little for it.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match Host::open(cfg, None, false) {
            Ok(host) => {
                return Ok(Box::new(Local {
                    host: host.with_utc_offset(offset),
                    cfg: cfg.clone(),
                }))
            }
            Err(e) if format!("{e:#}").contains("already open") && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// An error the user caused (exit 2).
struct Usage(String);

enum Fail {
    Api(ApiError),
    Usage(Usage),
    /// Already printed (exit 1).
    Reported,
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Fail::Api(e)
    }
}

impl From<Usage> for Fail {
    fn from(e: Usage) -> Self {
        Fail::Usage(e)
    }
}

type Out = Box<dyn Write>;

/// Standard output for what a command prints. A reader that closed the pipe early (`keel
/// --version | head -1`) stops the printing: the rest is dropped and the command ends as it
/// would have (exit 0), never with "failed printing to stdout".
pub struct Stdout {
    closed: bool,
}

pub fn stdout() -> Stdout {
    Stdout { closed: false }
}

impl Stdout {
    fn quiet(&mut self, result: std::io::Result<()>) -> std::io::Result<()> {
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(())
            }
            r => r,
        }
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.closed {
            let r = std::io::stdout().write_all(buf);
            self.quiet(r)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let r = std::io::stdout().flush();
        self.quiet(r)
    }
}

pub fn run(cmd: Command, profile: &str, json: bool) -> i32 {
    // Task 77: file lists need no library and no settings.
    if let Command::Filelist(cmd) = cmd {
        return crate::filelist_ui::command(cmd, json);
    }
    if let Command::Journal {
        watch,
        action,
        since,
        until,
        path,
        search,
        max,
        csv,
    } = cmd
    {
        // The journal file is read directly: no library, no daemon.
        let args = crate::journal_cli::Args {
            watch,
            action,
            since,
            until,
            path,
            search,
            max,
            csv,
            json,
        };
        let mut out: Out = Box::new(std::io::stdout());
        let file = keel_search::journal::default_path();
        return crate::journal_cli::run(file, &args, &mut out, &|| true);
    }
    // `keel es` searches the system index: no library unless it asks for `-library`.
    if let Command::Es { args } = &cmd {
        return crate::es::run(args, profile, json);
    }
    // Recovery reads volumes, not the library.
    let cmd = match cmd {
        Command::Recover(r) => return crate::recover_cli::run(r, json),
        cmd => cmd,
    };
    let cfg = match HostConfig::load(profile) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("keel: {e:#}");
            return FAILED;
        }
    };
    let mut out: Out = Box::new(stdout());
    match cmd {
        Command::Search {
            query,
            max,
            etp: Some(server),
            etp_tls,
        } => {
            return match etp_search(&cfg, &query, max, &server, etp_tls, json, &mut out) {
                Ok(()) => OK,
                Err(e) => report(Fail::Api(e), json, &mut out),
            };
        }
        Command::Daemon(d) => return daemon(d, &cfg, json, &mut out),
        Command::Mcp { allow_execute } => return mcp(&cfg, allow_execute),
        // The disk usage scanner needs no library.
        Command::Du {
            path: None,
            scan: args,
        } => {
            return match du_scan(args, json, &mut out) {
                Ok(code) => code,
                Err(e) => report(e, json, &mut out),
            };
        }
        Command::Op {
            name,
            params,
            params_file,
            execute,
        } => {
            // Machine output unless a person reads it.
            let json = json || !std::io::stdout().is_terminal();
            let call = OpCall {
                name,
                params,
                params_file,
                execute,
            };
            return match op(&cfg, call, json, &mut out) {
                Ok(()) => OK,
                Err(e) => report(e, json, &mut out),
            };
        }
        _ => {}
    }
    // Read a piped plan before opening the library: `keel plan` (which holds it while it
    // runs) has exited once its output ends.
    let cmd = match cmd {
        Command::Execute {
            plan,
            hash,
            no_wait,
        } => match plan_and_hash(plan, hash) {
            Ok((plan, hash)) => Command::Execute {
                plan: Some(plan),
                hash: Some(hash),
                no_wait,
            },
            Err(e) => return report(e, json, &mut out),
        },
        cmd => cmd,
    };
    // A backup or restore runs within its request: wait as long as the daemon lets it
    // (also for the execute of one).
    let wait = match &cmd {
        Command::Library(_) | Command::Execute { .. } => keel_api::client::LONG_REQUEST_WAIT,
        _ => keel_api::client::REQUEST_WAIT,
    };
    // A restore without a daemon needs no library open (on a new machine, opening the
    // profile's library would create the very name being restored).
    let backend = match &cmd {
        Command::Library(LibraryCmd::Restore { file, name }) => {
            match Client::connect(&cfg.socket_name()) {
                Ok(mut c) => {
                    c.timeout = wait;
                    Ok(Box::new(c) as Box<dyn Backend>)
                }
                Err(_) => return restore_here(&cfg, file, name.as_deref(), json, &mut out),
            }
        }
        _ => connect(&cfg, wait),
    };
    let mut backend = match backend {
        Ok(b) => b,
        Err(e) => return report(Fail::Api(e.into()), json, &mut out),
    };
    match command(cmd, &mut *backend, json, &mut out) {
        Ok(doc) => {
            if let (true, Some(doc)) = (json, doc) {
                print_json(&doc, &mut out);
            }
            OK
        }
        Err(e) => report(e, json, &mut out),
    }
}

fn report(e: Fail, json: bool, out: &mut Out) -> i32 {
    match e {
        Fail::Usage(Usage(msg)) => {
            eprintln!("keel: {msg}");
            USAGE
        }
        Fail::Api(e) => {
            if json {
                let _ = writeln!(out, "{}", json!({ "error": e.to_value() }));
            }
            eprintln!("keel: {}", e.message);
            if let Some(data) = &e.data {
                if let Ok(p) = serde_json::from_value::<PlanPreview>(data.clone()) {
                    eprintln!("The new preview:");
                    let mut err: Out = Box::new(std::io::stderr());
                    print_preview(&p, &mut err);
                }
            }
            FAILED
        }
        Fail::Reported => FAILED,
    }
}

/// An absolute path for the API (the daemon's working folder is not ours); URIs as given.
fn abs(p: &str) -> String {
    if p.contains("://") {
        return p.to_owned();
    }
    std::path::absolute(p)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| p.to_owned())
}

fn abs_all(ps: &[String]) -> Vec<String> {
    ps.iter().map(|p| abs(p)).collect()
}

/// The ETP server `keel search --etp [SERVER]` searches: SERVER, or `[search] etp` when it
/// is empty. The FTP remote with that host and port (or `[search] etp_remote`) lends its
/// user, keychain password and TLS setting, and the results open through it;
/// `KEEL_ETP_PASSWORD` overrides the password.
fn etp_target(
    cfg: &HostConfig,
    server: &str,
    tls: bool,
    env_password: Option<String>,
) -> anyhow::Result<keel_search::etp::Target> {
    use anyhow::Context;
    let (server, tls, pin, remote) = if server.trim().is_empty() {
        let saved = cfg.search_etp.as_ref().context(
            "no ETP server: give one (--etp HOST:PORT) or set [search] etp in config.toml",
        )?;
        (
            saved.server.clone(),
            saved.tls || tls,
            saved.pin.clone(),
            saved.remote.clone(),
        )
    } else {
        (server.to_owned(), tls, None, None)
    };
    let mut t = keel_search::etp::Target::parse(&server)?;
    t.tls = tls;
    t.pin = pin;
    let ftp = |r: &&keel_vfs::RemoteHost| r.protocol != keel_vfs::RemoteProtocol::Sftp;
    let found = match &remote {
        Some(name) => Some(
            cfg.remotes
                .iter()
                .filter(ftp)
                .find(|r| &r.id == name || &r.label == name)
                .with_context(|| format!("[search] etp_remote: no FTP remote {name}"))?,
        ),
        None => cfg
            .remotes
            .iter()
            .filter(ftp)
            .find(|r| r.host.eq_ignore_ascii_case(&t.host) && r.port == t.port),
    };
    if let Some(r) = found {
        if t.user.is_empty() {
            t.user = r.user.clone();
        }
        t.tls |= r.protocol == keel_vfs::RemoteProtocol::Ftps;
        t.authority = Some(r.id.clone());
        if t.password.is_empty() && t.user == r.user && !r.user.is_empty() {
            t.password = keel_vfs::sftp::auth::load_secret(&r.id, "password").unwrap_or_default();
        }
    }
    if let Some(p) = env_password.filter(|p| !p.is_empty()) {
        t.password = p;
    }
    Ok(t)
}

/// `keel search QUERY --etp [SERVER]`: one search on an ETP server.
fn etp_search(
    cfg: &HostConfig,
    query: &str,
    max: Option<usize>,
    server: &str,
    tls: bool,
    json: bool,
    out: &mut Out,
) -> Result<(), ApiError> {
    use keel_search::Searcher;
    let fail = |e: anyhow::Error| ApiError::failed(format!("{e:#}"));
    let password = std::env::var("KEEL_ETP_PASSWORD").ok();
    let target = etp_target(cfg, server, tls, password).map_err(fail)?;
    let etp = keel_search::etp::EtpSearcher::new(target);
    let max = u32::try_from(max.unwrap_or(100).max(1)).unwrap_or(u32::MAX);
    let hits = etp
        .query(&keel_search::Query {
            text: query.to_owned(),
            max,
            ..keel_search::Query::default()
        })
        .map_err(fail)?;
    if json {
        let docs: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "path": h.path.display(),
                    "name": h.path.name(),
                    "is_dir": h.is_dir,
                    "size": h.size,
                    "modified": h.modified
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs()),
                })
            })
            .collect();
        print_json(&Value::Array(docs), out);
    } else {
        for h in &hits {
            let size = if h.is_dir {
                "<dir>".to_owned()
            } else {
                h.size.to_string()
            };
            let _ = writeln!(out, "{size:>12}  {}", h.path.display());
        }
    }
    Ok(())
}

fn print_json(v: &Value, out: &mut Out) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_default()
    );
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, Fail> {
    serde_json::from_value(v)
        .map_err(|e| Fail::Api(ApiError::failed(format!("unexpected answer: {e}"))))
}

/// Previews `method`, then executes that exact preview.
fn confirm(
    b: &mut dyn Backend,
    method: &str,
    params: Value,
    json: bool,
    out: &mut Out,
) -> Result<Value, Fail> {
    let preview: PlanPreview = parse(b.call(method, params)?)?;
    if !json {
        let _ = writeln!(out, "{}", preview.summary);
        for w in &preview.warnings {
            let _ = writeln!(out, "  ! {}", w.message);
        }
    }
    let done = b.call(
        "execute",
        json!({"plan_id": preview.plan_id, "input_hash": preview.input_hash}),
    )?;
    Ok(done["result"].clone())
}

fn wait_job(b: &mut dyn Backend, job: i64) -> Result<JobInfo, Fail> {
    let started = Instant::now();
    loop {
        let info: JobInfo = parse(b.call("jobs.info", json!({ "id": job }))?)?;
        if info.finished() {
            return Ok(info);
        }
        let pause = if started.elapsed() < Duration::from_secs(2) {
            50
        } else {
            250
        };
        std::thread::sleep(Duration::from_millis(pause));
    }
}

/// The finished job as JSON; exit 1 (the job in the error's `data`) unless it is done.
fn job_done(info: &JobInfo, json: bool, out: &mut Out) -> Result<Value, Fail> {
    let v = serde_json::to_value(info).unwrap_or_default();
    if !json {
        let _ = writeln!(out, "job {} ({}): {}", info.id, info.kind, info.status);
    }
    match info.status.as_str() {
        "done" => Ok(v),
        _ => {
            let log = info.log.as_deref().unwrap_or("").trim();
            let e = ApiError::failed(format!(
                "job {} {}{}",
                info.id,
                info.status,
                if log.is_empty() {
                    String::new()
                } else {
                    format!(": {log}")
                }
            ));
            Err(Fail::Api(e.with_data(v)))
        }
    }
}

/// `library.folder_sizes`, or for a local folder in no library source a walk of it here.
fn folder_sizes(b: &mut dyn Backend, path: &str) -> Result<FolderSizes, Fail> {
    match b.call("library.folder_sizes", json!({ "path": path })) {
        Ok(v) => parse(v),
        Err(e) if e.code == ApiError::NOT_FOUND && !path.contains("://") => {
            walk_sizes(path).map_err(|w| Fail::Api(ApiError::failed(format!("{w:#}"))))
        }
        Err(e) => Err(e.into()),
    }
}

/// Task 46: `keel du` outside the library: each child of `path` walked for its total. A
/// child folder that cannot be listed is unknown (no `bytes`) rather than an error, as is
/// one below it counted in the app's job; the folder's own total counts what was read.
fn walk_sizes(path: &str) -> anyhow::Result<FolderSizes> {
    use keel_vfs::Provider;
    let dir = keel_vfs::VPath::local(path);
    let router = keel_vfs::Router::new();
    let never = std::sync::atomic::AtomicBool::new(false);
    let mut children = Vec::new();
    for e in keel_vfs::LocalProvider.list(&dir)? {
        let (bytes, files) = match e.kind {
            _ if e.is_link => (None, None),
            keel_vfs::Kind::Dir => {
                match keel_vfs::sizes::folder_total(
                    &router,
                    &e.path,
                    None,
                    &never,
                    &mut |_, _, _| {},
                ) {
                    Ok(w) => (Some(w.bytes), Some(w.files)),
                    Err(err) => {
                        tracing::debug!("du: {}: {err:#}", e.path.display());
                        (None, None)
                    }
                }
            }
            _ => (Some(e.size), Some(1)),
        };
        children.push(ChildSize {
            path: e.path.display(),
            is_dir: e.kind == keel_vfs::Kind::Dir,
            name: e.name,
            bytes,
            files,
        });
    }
    children.sort_by(|a, b| {
        (b.bytes.is_some().cmp(&a.bytes.is_some()))
            .then(b.bytes.cmp(&a.bytes))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(FolderSizes {
        path: dir.display(),
        bytes: Some(children.iter().filter_map(|c| c.bytes).sum()),
        files: Some(children.iter().filter_map(|c| c.files).sum()),
        children,
        truncated: false,
    })
}

/// The scan options `keel du`'s flags ask for.
fn du_scan_options(a: &DuArgs) -> keel_vfs::du::ScanOptions {
    use keel_vfs::du;
    du::ScanOptions {
        count_hard_links_once: !a.count_hardlinks_each,
        follow_symlinks: a.follow_links || a.follow_symlinks,
        follow_junctions: a.follow_links || a.follow_junctions,
        follow_mount_points: a.follow_links || a.follow_mount_points,
        filters: du::Filters {
            exclude_paths: a.exclude_path.clone(),
            exclude_names: a.exclude.clone(),
            exclude_path_regex: a.exclude_regex.clone(),
            min_size: a.min_size,
            exclude_older_than: a.older_than,
            exclude_newer_than: a.newer_than,
            exclude_hidden: a.no_hidden,
            exclude_system: a.no_system,
        },
        ..du::ScanOptions::default()
    }
}

/// `keel du --scan` / `--all-drives` / `--load` (Tasks 81 and 86). Checks the outputs and
/// filters before scanning (usage errors), scans or loads, writes every output asked for
/// (one that fails does not stop the others: exit 1), then prints each root's biggest
/// items, its free and unknown space (drives) and its total unless `--quiet`. With
/// `--strict` a partial scan exits 3.
fn du_scan(a: DuArgs, json: bool, out: &mut Out) -> Result<i32, Fail> {
    use keel_vfs::du::{self, export, report};
    use std::path::PathBuf;
    let fail = |e: anyhow::Error| Fail::Api(ApiError::failed(format!("{e:#}")));
    let usage = |msg: String| Fail::Usage(Usage(msg));
    let exports: Vec<(export::List, &PathBuf, &str)> = [
        (export::List::Tree, a.save_to.as_ref(), "items"),
        (export::List::Types, a.save_types_to.as_ref(), "file types"),
        (
            export::List::Largest,
            a.save_largest_to.as_ref(),
            "largest files",
        ),
    ]
    .into_iter()
    .filter_map(|(list, path, what)| path.map(|p| (list, p, what)))
    .collect();
    for (_, path, _) in &exports {
        export::Format::from_path(path).map_err(|e| usage(format!("{e:#}")))?;
    }
    if let Some(r) = &a.report {
        let html = (r.extension().and_then(|e| e.to_str()))
            .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));
        if !html {
            return Err(usage(format!(
                "{}: a report file ends in .html",
                r.display()
            )));
        }
    }
    let mut targets: Vec<String> = (exports.iter().map(|e| e.1))
        .chain(&a.report)
        .chain(&a.save)
        .map(|p| {
            let p = std::path::absolute(p).unwrap_or_else(|_| p.clone());
            let p = p.to_string_lossy().into_owned();
            if cfg!(windows) {
                p.to_lowercase()
            } else {
                p
            }
        })
        .collect();
    let outputs = targets.len();
    targets.sort();
    targets.dedup();
    if targets.len() != outputs {
        return Err(usage("two outputs name the same file".into()));
    }
    if a.load.is_some() && a.walks_differently() {
        return Err(usage(
            "filters and link and hard link options apply to a scan, not to a loaded one".into(),
        ));
    }
    let opts = du_scan_options(&a);
    opts.filters.check().map_err(|e| usage(format!("{e:#}")))?;

    let tree = match &a.load {
        Some(file) => du::load(file).map_err(fail)?,
        None => {
            let mut roots: Vec<du::Root> = (a.scan.iter())
                .map(|p| du::Root::Path(PathBuf::from(p)))
                .collect();
            if a.all_drives {
                roots.push(du::Root::AllLocalDrives);
            }
            let handle = du::ScanHandle::new();
            let ticker = (!a.quiet).then(|| Ticker::start(&handle)).flatten();
            let scanned = du::scan(&roots, &opts, &handle);
            drop(ticker);
            scanned.map_err(fail)?
        }
    };

    let mut results: Vec<(&str, &PathBuf, anyhow::Result<()>)> = Vec::new();
    if let Some(file) = &a.save {
        results.push(("scan", file, du::save(&tree, file, true)));
    }
    let options = export::Options {
        bom: a.bom,
        top: a.top.unwrap_or(100),
        size_on_disk: a.size_on_disk,
    };
    for &(list, path, what) in &exports {
        results.push((what, path, export::save(&tree, list, options, path)));
    }
    let mut mail = None;
    if let Some(file) = &a.report {
        let options = export::Options {
            top: a.top.unwrap_or(20),
            ..options
        };
        let saved = report::save(&tree, options, file);
        if saved.is_ok() && a.mail {
            mail = Some(report::mailto(&tree, file));
        }
        results.push(("report", file, saved));
    }
    let mut written: Vec<(&str, &PathBuf)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (what, path, r) in results {
        match r {
            Ok(()) => written.push((what, path)),
            Err(e) => errors.push(format!("{}: {e:#}", path.display())),
        }
    }
    if let Some(uri) = mail {
        // A new mail with no recipient, nothing attached: nothing is sent from here.
        let opened = uri.and_then(|uri| open::that_detached(&uri).map_err(anyhow::Error::from));
        if let Err(e) = opened {
            errors.push(format!("cannot open a new mail: {e:#}"));
        }
    }

    let summary = tree.summary_by(a.top.unwrap_or(20), a.size_on_disk);
    if !a.quiet && json {
        let mut v = serde_json::to_value(&summary).unwrap_or_default();
        v["written"] = (written.iter())
            .map(|(what, path)| json!({"what": what, "path": path.display().to_string()}))
            .collect();
        if !errors.is_empty() {
            v["errors"] = json!(errors);
        }
        print_json(&v, out);
    } else if !a.quiet {
        print_du_scan(&summary, out);
        for (what, path) in &written {
            let _ = writeln!(out, "Wrote the {what} to {}", path.display());
        }
    }
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("keel: {e}");
        }
        return Ok(FAILED);
    }
    if a.strict && (!tree.info.complete || tree.info.unreadable > 0) {
        eprintln!(
            "keel: the scan is partial ({} folders could not be read{})",
            tree.info.unreadable,
            if tree.info.complete {
                ""
            } else {
                ", cut short"
            }
        );
        return Ok(PARTIAL);
    }
    Ok(OK)
}

fn print_du_scan(s: &keel_vfs::du::Summary, out: &mut Out) {
    let size = |b: u64| humansize::format_size(b, humansize::DECIMAL);
    let files = |f: u64| format!("{f} file{}", if f == 1 { "" } else { "s" });
    let _ = writeln!(
        out,
        "{:>10}  {:>10}  {:>14}  Path",
        "Size", "On disk", "Files"
    );
    let mut line = |bytes: Option<u64>, disk: Option<u64>, count: String, what: &str| {
        let (bytes, disk) = (
            bytes.map_or(String::new(), size),
            disk.map_or(String::new(), size),
        );
        let _ = writeln!(out, "{bytes:>10}  {disk:>10}  {count:>14}  {what}");
    };
    for r in &s.roots {
        for c in &r.children {
            let count = match c.kind {
                "file" | "link" => String::new(),
                _ => files(c.files),
            };
            let mut what = c.path.clone().unwrap_or_else(|| c.name.clone());
            if !c.notes.is_empty() {
                what = format!("{what}  [{}]", c.notes.join(", "));
            }
            line(Some(c.bytes), Some(c.allocated), count, &what);
        }
        if r.truncated {
            line(None, None, String::new(), "(more, smaller items not shown)");
        }
        if let Some(free) = r.free {
            line(Some(free), None, String::new(), "<free space>");
        }
        if let Some(unknown) = r.unknown {
            line(
                Some(unknown),
                None,
                String::new(),
                "<unknown: used, not found by the scan>",
            );
        }
        line(Some(r.bytes), Some(r.allocated), files(r.files), &r.path);
    }
    if s.roots.len() > 1 {
        line(
            Some(s.bytes),
            Some(s.allocated),
            files(s.files),
            "(all roots)",
        );
    }
    let mut end = format!(
        "{} files, {} folders in {:.1} s",
        s.files,
        s.folders.saturating_sub(s.roots.len() as u64),
        s.elapsed_ms as f64 / 1000.0
    );
    if !s.complete {
        end.push_str("; cancelled, not everything was scanned");
    }
    if s.unreadable > 0 {
        end.push_str(&format!("; {} folders could not be read", s.unreadable));
    }
    if s.excluded > 0 {
        end.push_str(&format!("; {} items left out", s.excluded));
    }
    let _ = writeln!(out, "{end}");
}

/// A scan's progress on stderr (when it is a terminal), every 200 ms.
struct Ticker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ticker {
    const WIDTH: usize = 79;

    fn start(handle: &keel_vfs::du::ScanHandle) -> Option<Ticker> {
        use std::sync::atomic::{AtomicBool, Ordering};
        if !std::io::stderr().is_terminal() {
            return None;
        }
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (h, s) = (handle.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("du-progress".into())
            .spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                    let p = h.progress();
                    let head = format!(
                        "{} files, {} ({:.0} files/s)  ",
                        p.files,
                        humansize::format_size(p.bytes, humansize::DECIMAL),
                        p.files_per_sec
                    );
                    let room = Self::WIDTH.saturating_sub(head.chars().count()).max(2);
                    let at = p.current.display().to_string();
                    let n = at.chars().count();
                    let at: String = if n > room {
                        std::iter::once('…')
                            .chain(at.chars().skip(n + 1 - room))
                            .collect()
                    } else {
                        at
                    };
                    eprint!("\r{:<w$}", format!("{head}{at}"), w = Self::WIDTH);
                }
                eprint!("\r{:w$}\r", "", w = Self::WIDTH);
            })
            .ok()?;
        Some(Ticker {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Runs `cmd`. Text goes to `out` as it comes; with `json` nothing is printed here and the
/// one document to print is returned.
fn command(
    cmd: Command,
    b: &mut dyn Backend,
    json: bool,
    out: &mut Out,
) -> Result<Option<Value>, Fail> {
    let doc = match cmd {
        Command::Search { query, max, .. } => {
            let hits = b.call("search", json!({"query": query, "max": max}))?;
            if json {
                return Ok(Some(hits));
            } else {
                for h in parse::<Vec<Hit>>(hits)? {
                    let size = if h.is_dir {
                        "<dir>".to_owned()
                    } else {
                        h.size.to_string()
                    };
                    let _ = writeln!(out, "{size:>12}  {}", h.path);
                }
            }
            None
        }
        Command::Du { path, scan } => {
            let top = scan.top;
            let path = path.unwrap_or_default();
            let mut sizes = folder_sizes(b, &abs(&path))?;
            if let Some(n) = top.filter(|&n| n < sizes.children.len()) {
                sizes.children.truncate(n);
                sizes.truncated = true;
            }
            if json {
                return Ok(Some(serde_json::to_value(&sizes).unwrap_or_default()));
            }
            let size = |b: Option<u64>| {
                b.map_or("?".to_owned(), |b| {
                    humansize::format_size(b, humansize::DECIMAL)
                })
            };
            for c in &sizes.children {
                let files = c.files.map_or(String::new(), |f| {
                    format!("{f} file{}", if f == 1 { "" } else { "s" })
                });
                let _ = writeln!(out, "{:>10}  {files:>12}  {}", size(c.bytes), c.path);
            }
            if sizes.truncated {
                let _ = writeln!(
                    out,
                    "{:>10}  {:>12}  (more, smaller items not shown)",
                    "", ""
                );
            }
            let files = sizes.files.map_or(String::new(), |f| {
                format!("{f} file{}", if f == 1 { "" } else { "s" })
            });
            let _ = writeln!(
                out,
                "{:>10}  {files:>12}  {}",
                size(sizes.bytes),
                sizes.path
            );
            None
        }
        Command::Tag(t) => {
            let (method, tag, paths) = match t {
                TagCmd::Add { tag, paths } => ("tags.add", tag, paths),
                TagCmd::Remove { tag, paths } => ("tags.remove", tag, paths),
            };
            let done = confirm(
                b,
                method,
                json!({"tag": tag, "paths": abs_all(&paths)}),
                json,
                out,
            )?;
            Some(done)
        }
        Command::Plan(p) => {
            let conflict = |c: Option<Conflict>| {
                c.map(|c| match c {
                    Conflict::Skip => "skip",
                    Conflict::Overwrite => "overwrite",
                    Conflict::Rename => "rename_new",
                })
            };
            let (method, params) = match p {
                PlanCmd::Copy {
                    src,
                    to,
                    on_conflict,
                } => (
                    "plan",
                    json!({
                        "op": "copy", "paths": abs_all(&src), "to": abs(&to), "on_conflict": conflict(on_conflict),
                    }),
                ),
                PlanCmd::Move {
                    src,
                    to,
                    on_conflict,
                } => (
                    "plan",
                    json!({
                        "op": "move", "paths": abs_all(&src), "to": abs(&to), "on_conflict": conflict(on_conflict),
                    }),
                ),
                PlanCmd::Delete { paths } => {
                    ("plan", json!({"op": "delete", "paths": abs_all(&paths)}))
                }
                PlanCmd::Merge {
                    folders,
                    into,
                    on_conflict,
                } => (
                    "plan.merge",
                    json!({
                        "paths": abs_all(&folders), "into": abs(&into), "on_conflict": conflict(on_conflict),
                    }),
                ),
                PlanCmd::Flatten {
                    folder,
                    on_conflict,
                } => (
                    "plan.flatten",
                    json!({
                        "path": abs(&folder), "on_conflict": conflict(on_conflict),
                    }),
                ),
                PlanCmd::RemoveEmpty { folder } => {
                    ("plan.remove_empty", json!({"path": abs(&folder)}))
                }
                PlanCmd::Organize {
                    folder,
                    by,
                    on_conflict,
                } => {
                    let by = match by {
                        OrganizeBy::Type => "type",
                        OrganizeBy::Year => "year",
                        OrganizeBy::Month => "month",
                    };
                    (
                        "plan.organize",
                        json!({
                            "path": abs(&folder), "by": by, "on_conflict": conflict(on_conflict),
                        }),
                    )
                }
                PlanCmd::Attrs {
                    paths,
                    hidden,
                    readonly,
                    mode,
                } => (
                    "plan.set_attributes",
                    json!({
                        "paths": abs_all(&paths), "hidden": hidden, "readonly": readonly, "mode": mode,
                    }),
                ),
                PlanCmd::Link { target, link, kind } => {
                    let kind = match kind {
                        LinkKind::Symlink => "symlink",
                        LinkKind::Junction => "junction",
                        LinkKind::Hardlink => "hardlink",
                    };
                    (
                        "plan.link",
                        json!({
                            "target": abs(&target), "link": abs(&link), "kind": kind,
                        }),
                    )
                }
                PlanCmd::FolderDates { folder } => {
                    ("plan.folder_dates", json!({"path": abs(&folder)}))
                }
            };
            let preview = b.call(method, params)?;
            if json {
                return Ok(Some(preview));
            }
            print_preview(&parse(preview)?, out);
            None
        }
        Command::Execute {
            plan,
            hash,
            no_wait,
        } => {
            let (plan_id, input_hash) = plan_and_hash(plan, hash)?;
            execute_plan(b, &plan_id, &input_hash, no_wait, json, out)?
        }
        Command::Devices => {
            let d = b.call("devices.list", Value::Null)?;
            if json {
                return Ok(Some(d));
            } else {
                let _ = writeln!(
                    out,
                    "this device: {} ({})",
                    d["label"].as_str().unwrap_or(""),
                    d["id"].as_str().unwrap_or("")
                );
                for p in d["peers"].as_array().into_iter().flatten() {
                    // Library sync on: when it last pulled from that device.
                    let sync = match p["sync"].as_bool() {
                        Some(true) => format!(
                            "  sync, last {}",
                            crate::library::when(p["last_sync"].as_i64())
                        ),
                        _ => String::new(),
                    };
                    let _ = writeln!(
                        out,
                        "{:<8} {}  {}{sync}",
                        p["link"].as_str().unwrap_or(""),
                        p["label"].as_str().unwrap_or(""),
                        p["id"].as_str().unwrap_or("")
                    );
                }
            }
            None
        }
        Command::Shares => {
            let g = b.call("shares.list", Value::Null)?;
            if json {
                return Ok(Some(g));
            } else {
                for g in g.as_array().into_iter().flatten() {
                    let _ = writeln!(
                        out,
                        "{:<10} {} {}/{}",
                        g["access"].as_str().unwrap_or(""),
                        g["peer"].as_str().unwrap_or(""),
                        g["source"].as_str().unwrap_or(""),
                        g["subtree"].as_str().unwrap_or("")
                    );
                }
            }
            None
        }
        Command::Sources { action } => sources(action, b, json, out)?,
        Command::Mount {
            source,
            target,
            subtree,
        } => {
            let source = source_id(b, &source)?;
            let info = confirm(
                b,
                "mounts.add",
                json!({"source": source, "subtree": subtree.unwrap_or_default(), "target": mount_target(&target)}),
                json,
                out,
            )?;
            if !json {
                let _ = writeln!(
                    out,
                    "mounted {} at {}",
                    info["root"].as_str().unwrap_or(""),
                    info["target"].as_str().unwrap_or("")
                );
            }
            Some(info)
        }
        Command::Unmount { target } => {
            let info = confirm(
                b,
                "mounts.remove",
                json!({ "target": mount_target(&target) }),
                json,
                out,
            )?;
            if !json {
                let _ = writeln!(out, "unmounted {}", info["target"].as_str().unwrap_or(""));
            }
            Some(info)
        }
        Command::Mounts => {
            let list = b.call("mounts.list", Value::Null)?;
            if json {
                return Ok(Some(list));
            }
            for m in list.as_array().into_iter().flatten() {
                let _ = writeln!(
                    out,
                    "{:<12} {}  ({}, {})",
                    m["target"].as_str().unwrap_or(""),
                    m["root"].as_str().unwrap_or(""),
                    m["source_label"].as_str().unwrap_or(""),
                    m["backend"].as_str().unwrap_or("")
                );
            }
            None
        }
        Command::Library(LibraryCmd::Backup { file }) => {
            let done = confirm(
                b,
                "library.backup",
                json!({ "file": abs(&file) }),
                json,
                out,
            )?;
            if !json {
                let _ = writeln!(
                    out,
                    "backed up to {} ({})",
                    done["file"].as_str().unwrap_or(""),
                    humansize::format_size(done["bytes"].as_u64().unwrap_or(0), humansize::DECIMAL)
                );
            }
            Some(done)
        }
        Command::Library(LibraryCmd::Restore { file, name }) => {
            let done = confirm(
                b,
                "library.restore",
                json!({"file": abs(&file), "name": name}),
                json,
                out,
            )?;
            if !json {
                print_restored(&parse(done.clone())?, out);
            }
            Some(done)
        }
        Command::Daemon(_)
        | Command::Mcp { .. }
        | Command::Op { .. }
        | Command::Filelist(_)
        | Command::Journal { .. }
        | Command::Es { .. }
        | Command::Recover(_) => {
            unreachable!("handled by run")
        }
    };
    Ok(doc.filter(|_| json))
}

/// Applies a previewed plan, waiting for its job unless `no_wait`: the finished job (or the
/// answer) with `json`, else printed.
fn execute_plan(
    b: &mut dyn Backend,
    plan_id: &str,
    input_hash: &str,
    no_wait: bool,
    json: bool,
    out: &mut Out,
) -> Result<Option<Value>, Fail> {
    let done = b.call(
        "execute",
        json!({"plan_id": plan_id, "input_hash": input_hash}),
    )?;
    Ok(match done["job"].as_i64() {
        Some(job) if !no_wait => {
            let info = wait_job(b, job)?;
            // In a terminal the job's line says it all.
            let done = job_done(&info, json, out)?;
            json.then_some(done)
        }
        _ if json => Some(done),
        Some(job) => {
            let _ = writeln!(out, "started job {job}");
            None
        }
        None => {
            print_json(&done["result"], out);
            None
        }
    })
}

/// `keel op`'s arguments.
struct OpCall {
    name: Option<String>,
    params: Option<String>,
    params_file: Option<std::path::PathBuf>,
    execute: bool,
}

/// Task 58: `keel op <name>`: any operation of the API. A read prints its result; an
/// operation that previews prints its preview, and with `--execute` applies exactly that
/// preview (its plan id and input hash) after printing it. With `json` one document: the
/// result, the preview, or `{"preview", "executed"}`.
fn op(cfg: &HostConfig, call: OpCall, json: bool, out: &mut Out) -> Result<(), Fail> {
    let given = call.params.is_some() || call.params_file.is_some();
    let name = match call.name.as_deref() {
        Some("list") if !given => None,
        name => name,
    };
    let Some(name) = name else {
        op_list(json, out);
        return Ok(());
    };
    let op = keel_api::find(name)
        .ok_or_else(|| Usage(format!("no operation {name} (`keel op list` lists them)")))?;
    let text = match (call.params, call.params_file) {
        (Some(text), _) => text,
        (None, Some(file)) => {
            std::fs::read_to_string(&file).map_err(|e| Usage(format!("{}: {e}", file.display())))?
        }
        (None, None) => "{}".into(),
    };
    let params: Value = serde_json::from_str(&text)
        .map_err(|e| Usage(format!("the parameters are not valid JSON: {e}")))?;
    if !params.is_object() {
        return Err(Usage("the parameters must be a JSON object".into()).into());
    }
    if call.execute && !op.previews() {
        let what = if op.mutating {
            "acts at once"
        } else {
            "only reads"
        };
        return Err(Usage(format!(
            "{name} has no preview (it {what}); --execute is for operations that preview"
        ))
        .into());
    }
    // The execute of a backup or restore runs within its request, like the preview.
    let wait = match op.name {
        "execute" => keel_api::client::LONG_REQUEST_WAIT,
        name => keel_api::client::request_wait(name),
    };
    let mut b = connect(cfg, wait).map_err(|e| Fail::Api(e.into()))?;
    // `execute` waits for the job it starts, as `keel execute` does.
    let ids = (params["plan_id"].as_str()).zip(params["input_hash"].as_str());
    if let (true, Some((id, hash))) = (op.name == "execute", ids) {
        if let Some(doc) = execute_plan(&mut *b, id, hash, false, json, out)? {
            print_json(&doc, out);
        }
        return Ok(());
    }
    let answer = b.call(op.name, params)?;
    if !op.previews() {
        print_json(&answer, out);
        return Ok(());
    }
    let preview: PlanPreview = parse(answer.clone())?;
    if !call.execute {
        match json {
            true => print_json(&answer, out),
            false => print_preview(&preview, out),
        }
        return Ok(());
    }
    if !json {
        let _ = write!(out, "{}", preview.describe(usize::MAX));
        let _ = writeln!(out, "plan: {}", preview.plan_id);
        let _ = writeln!(out, "hash: {}", preview.input_hash);
        let _ = writeln!(out, "Executing…");
        let _ = out.flush();
    }
    let (id, hash) = (&preview.plan_id, &preview.input_hash);
    match execute_plan(&mut *b, id, hash, false, json, out) {
        Ok(Some(done)) => print_json(&json!({"preview": answer, "executed": done}), out),
        Ok(None) => {}
        // The preview it failed on goes into the one document.
        Err(Fail::Api(e)) if json => {
            print_json(&json!({"error": e.to_value(), "preview": answer}), out);
            eprintln!("keel: {}", e.message);
            return Err(Fail::Reported);
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// `keel op list`: every operation, whether it previews, and what it does (with `json`
/// also whether it changes anything, its parameters' schema and an example).
fn op_list(json: bool, out: &mut Out) {
    if json {
        let list: Vec<Value> = (keel_api::OPS.iter())
            .map(|op| {
                json!({
                    "name": op.name,
                    "summary": op.summary,
                    "previews": op.previews(),
                    "mutating": op.mutating,
                    "params": (op.params)(),
                    "example": (op.example)(),
                })
            })
            .collect();
        return print_json(&Value::Array(list), out);
    }
    for op in keel_api::OPS {
        let mark = if op.previews() { "preview" } else { "" };
        let _ = writeln!(out, "{:<22} {mark:<8} {}", op.name, op.summary);
    }
}

/// A drive letter as given (`K`, `K:`), anything else made absolute.
fn mount_target(t: &str) -> String {
    let letter = t.trim_end_matches(['\\', '/']).trim_end_matches(':');
    if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) {
        return t.to_owned();
    }
    abs(t)
}

/// The id of the one source with this label (case-insensitive), else `given` as an id.
fn source_id(b: &mut dyn Backend, given: &str) -> Result<String, Fail> {
    let list = b.call("sources.list", Value::Null)?;
    let sources = list.as_array().cloned().unwrap_or_default();
    if sources.iter().any(|s| s["id"] == given) {
        return Ok(given.to_owned());
    }
    let labelled: Vec<_> = sources
        .iter()
        .filter(|s| {
            s["label"]
                .as_str()
                .is_some_and(|l| l.eq_ignore_ascii_case(given))
        })
        .collect();
    match labelled.as_slice() {
        [one] => Ok(one["id"].as_str().unwrap_or_default().to_owned()),
        // Not a label: the operation reports the unknown id.
        [] => Ok(given.to_owned()),
        _ => Err(Usage(format!("several sources are labelled {given}: give the id")).into()),
    }
}

fn sources(
    action: Option<SourcesCmd>,
    b: &mut dyn Backend,
    json: bool,
    out: &mut Out,
) -> Result<Option<Value>, Fail> {
    let index = |b: &mut dyn Backend, id: &str, out: &mut Out| -> Result<Value, Fail> {
        let started = confirm(b, "sources.index", json!({ "id": id }), json, out)?;
        let job = started["job"].as_i64().unwrap_or_default();
        let info = wait_job(b, job)?;
        job_done(&info, json, out)
    };
    Ok(match action {
        None => {
            let mut list = b.call("sources.list", Value::Null)?;
            // Each source's counts (`library.stats` `per_source`), merged in by id.
            let stats = b.call("library.stats", Value::Null)?;
            for s in list.as_array_mut().into_iter().flatten() {
                let counts = stats["per_source"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|c| c["id"] == s["id"]);
                if let (Some(s), Some(c)) = (s.as_object_mut(), counts) {
                    for k in ["files", "folders", "bytes", "hashed_files", "last_walk"] {
                        if !c[k].is_null() {
                            s.insert(k.into(), c[k].clone());
                        }
                    }
                }
            }
            if json {
                return Ok(Some(list));
            } else {
                for s in list.as_array().into_iter().flatten() {
                    let _ = writeln!(
                        out,
                        "{}  {:<9} {}  ({})",
                        s["id"].as_str().unwrap_or(""),
                        s["status"].as_str().unwrap_or(""),
                        s["label"].as_str().unwrap_or(""),
                        s["root"].as_str().unwrap_or("")
                    );
                    if let Some(files) = s["files"].as_u64() {
                        let _ = writeln!(
                            out,
                            "    files {}, folders {}, size {}, hashed {}, last walk {}",
                            crate::library::count(files),
                            crate::library::count(s["folders"].as_u64().unwrap_or(0)),
                            humansize::format_size(
                                s["bytes"].as_u64().unwrap_or(0),
                                humansize::DECIMAL
                            ),
                            crate::library::count(s["hashed_files"].as_u64().unwrap_or(0)),
                            crate::library::when(s["last_walk"].as_i64())
                        );
                    }
                }
            }
            None
        }
        Some(SourcesCmd::Add {
            path,
            label,
            no_index,
        }) => {
            let added = confirm(
                b,
                "sources.add",
                json!({"root": abs(&path), "label": label}),
                json,
                out,
            )?;
            let id = added["id"].as_str().unwrap_or_default().to_owned();
            if !json {
                let _ = writeln!(out, "source {id}");
            }
            // One document: the added source, with its index job under `job`.
            let mut doc = added;
            if !no_index {
                match index(b, &id, out) {
                    Ok(job) => doc["job"] = job,
                    Err(Fail::Api(mut e)) => {
                        doc["job"] = e.data.take().unwrap_or_default();
                        return Err(Fail::Api(e.with_data(doc)));
                    }
                    Err(e) => return Err(e),
                }
            }
            Some(doc)
        }
        Some(SourcesCmd::Remove { id, delete_store }) => {
            let removed = confirm(
                b,
                "sources.remove",
                json!({"id": id, "delete_store": delete_store}),
                json,
                out,
            )?;
            if json {
                return Ok(Some(removed));
            } else {
                let _ = writeln!(
                    out,
                    "removed source {} ({})",
                    removed["removed"]["label"].as_str().unwrap_or(""),
                    id
                );
            }
            None
        }
        Some(SourcesCmd::Index { id }) => Some(index(b, &id, out)?),
    })
}

/// `keel library restore` with no daemon running: straight into the data folder.
fn restore_here(
    cfg: &HostConfig,
    file: &str,
    name: Option<&str>,
    json: bool,
    out: &mut Out,
) -> i32 {
    let restored = keel_api::private::create_dir_all(&cfg.data_dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            keel_core::backup::restore(&cfg.data_dir, std::path::Path::new(&abs(file)), name)
        });
    let r = match restored {
        Ok(r) => RestoredLibrary {
            name: r.name,
            dir: r.dir.display().to_string(),
            sources: (r.sources.into_iter())
                .map(|s| RestoredSourceInfo {
                    id: s.id,
                    label: s.label,
                    root: s.root.display(),
                    online: s.online,
                })
                .collect(),
        },
        Err(e) => return report(Fail::Api(ApiError::failed(format!("{e:#}"))), json, out),
    };
    match json {
        true => print_json(&serde_json::to_value(&r).unwrap_or_default(), out),
        false => print_restored(&r, out),
    }
    OK
}

fn print_restored(r: &RestoredLibrary, out: &mut Out) {
    let _ = writeln!(out, "restored library {} in {}", r.name, r.dir);
    for s in &r.sources {
        let state = match s.online {
            true => "walked again when the library opens",
            false => "offline until its folder is back",
        };
        let _ = writeln!(out, "  {}  {}: {state}", s.label, s.root);
    }
    let _ = writeln!(
        out,
        "Open it in Settings → Library, or with [library] name = \"{}\" in the profile's config.toml",
        r.name
    );
}

fn print_preview(p: &PlanPreview, out: &mut Out) {
    let _ = write!(out, "{}", p.describe(usize::MAX));
    let _ = writeln!(out, "plan: {}", p.plan_id);
    let _ = writeln!(out, "hash: {}", p.input_hash);
    let _ = writeln!(
        out,
        "Apply with: keel execute {} --hash {}",
        p.plan_id, p.input_hash
    );
}

/// The plan id and hash: from the arguments, else from `keel plan` output on stdin (its
/// JSON, or the `plan:` and `hash:` lines).
fn plan_and_hash(plan: Option<String>, hash: Option<String>) -> Result<(String, String), Fail> {
    if let (Some(p), Some(h)) = (&plan, &hash) {
        return Ok((p.clone(), h.clone()));
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(Usage(
            "give the plan id and --hash, or pipe `keel plan` output into `keel execute`".into(),
        )
        .into());
    }
    let mut text = String::new();
    stdin
        .lock()
        .take(1 << 20)
        .read_to_string(&mut text)
        .map_err(|e| Usage(format!("reading stdin: {e}")))?;
    let (mut id, mut h) = (None, None);
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        id = v["plan_id"].as_str().map(str::to_owned);
        h = v["input_hash"].as_str().map(str::to_owned);
    } else {
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("plan: ") {
                id = Some(v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("hash: ") {
                h = Some(v.trim().to_owned());
            }
        }
    }
    let id = plan.or(id);
    match (id, hash.or(h)) {
        (Some(p), Some(h)) => {
            if plan_given_differs(&text, &p) {
                return Err(Usage("the plan id does not match the piped plan".into()).into());
            }
            Ok((p, h))
        }
        _ => Err(Usage("no plan id and hash on stdin (pipe `keel plan` output)".into()).into()),
    }
}

/// A plan id given as an argument must be the piped plan's when stdin has one.
fn plan_given_differs(text: &str, plan: &str) -> bool {
    let piped = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["plan_id"].as_str().map(str::to_owned))
        .or_else(|| {
            text.lines()
                .find_map(|l| l.strip_prefix("plan: ").map(|v| v.trim().to_owned()))
        });
    piped.is_some_and(|p| p != plan)
}

fn mcp(cfg: &HostConfig, allow_execute: bool) -> i32 {
    // An agent may run a backup or restore: the daemon answers everything else in 120 s.
    let mut backend = match connect(cfg, keel_api::client::LONG_REQUEST_WAIT) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("keel mcp: {e:#}");
            return FAILED;
        }
    };
    let stdin = std::io::stdin();
    let opts = keel_api::mcp::Options { allow_execute };
    match keel_api::mcp::serve(&mut *backend, stdin.lock(), std::io::stdout().lock(), opts) {
        Ok(()) => OK,
        Err(e) => {
            eprintln!("keel mcp: {e}");
            FAILED
        }
    }
}

fn daemon_exe() -> Option<std::path::PathBuf> {
    // The installers link ~/.local/bin/keel to the install folder: resolve the link so the
    // daemon is looked for next to the real binary.
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let path = exe
        .parent()?
        .join(format!("keel-daemon{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// `keel-daemon`'s arguments for `profile`: one `--profile=<name>` (a name can never be
/// read as another option).
pub fn daemon_args(profile: &str) -> [String; 1] {
    [format!("--profile={profile}")]
}

/// Starts `keel-daemon --profile=<name>` in the background (no console window, not in
/// this console's Ctrl-C group: it outlives the caller), its log appended to
/// `<config dir>/daemon.log`. Returns the child and the log's path.
pub fn spawn_daemon(
    cfg: &HostConfig,
) -> std::io::Result<(std::process::Child, std::path::PathBuf)> {
    let exe = daemon_exe().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "keel-daemon was not found next to keel",
        )
    })?;
    let log = cfg.config_dir.join("daemon.log");
    let _ = std::fs::create_dir_all(&cfg.config_dir);
    let stderr = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        Ok(f) => std::process::Stdio::from(f),
        Err(_) => std::process::Stdio::null(),
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(daemon_args(&cfg.profile))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window; not in this console's Ctrl-C group.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        // Else it inherits this process's output too: `keel daemon start | …` would not
        // end until the daemon does.
        keel_vfs::desktop::keep_std_handles_private();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Ok((command.spawn()?, log))
}

fn running(cfg: &HostConfig) -> Option<Value> {
    Client::connect(&cfg.socket_name())
        .ok()
        .and_then(|mut c| c.call("version", Value::Null).ok())
}

fn daemon(cmd: DaemonCmd, cfg: &HostConfig, json: bool, out: &mut Out) -> i32 {
    let say = |out: &mut Out, v: Value, text: String| {
        if json {
            print_json(&v, out);
        } else {
            let _ = writeln!(out, "{text}");
        }
    };
    match cmd {
        DaemonCmd::RotateToken => match keel_api::config::new_token(&cfg.token_path()) {
            Ok(_) => {
                let path = cfg.token_path().display().to_string();
                let text = format!("new token in {path}: clients must sign in again");
                say(out, json!({"rotated": true, "path": path}), text);
                OK
            }
            Err(e) => {
                eprintln!("keel: {}: {e}", cfg.token_path().display());
                FAILED
            }
        },
        DaemonCmd::Status => match running(cfg) {
            Some(v) => {
                let text = format!(
                    "keel-daemon is running for profile {} (pid {}, library {})",
                    cfg.profile,
                    v["pid"],
                    v["library"].as_str().unwrap_or("")
                );
                say(out, json!({"running": true, "version": v}), text);
                OK
            }
            None => {
                let text = format!("keel-daemon is not running for profile {}", cfg.profile);
                say(out, json!({"running": false}), text);
                FAILED
            }
        },
        DaemonCmd::Start => {
            if let Some(v) = running(cfg) {
                say(
                    out,
                    json!({"running": true, "version": v}),
                    format!("keel-daemon already runs for profile {}", cfg.profile),
                );
                return OK;
            }
            let (mut child, log) = match spawn_daemon(cfg) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("keel: starting keel-daemon: {e}");
                    return FAILED;
                }
            };
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if let Some(v) = running(cfg) {
                    say(
                        out,
                        json!({"running": true, "version": v}),
                        format!(
                            "keel-daemon started for profile {} (log: {})",
                            cfg.profile,
                            log.display()
                        ),
                    );
                    return OK;
                }
                if let Ok(Some(status)) = child.try_wait() {
                    eprintln!("keel: keel-daemon exited ({status}); see {}", log.display());
                    return FAILED;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!("keel: keel-daemon did not answer; see {}", log.display());
            FAILED
        }
        DaemonCmd::Stop => {
            let Ok(mut c) = Client::connect(&cfg.socket_name()) else {
                say(
                    out,
                    json!({"running": false}),
                    format!("keel-daemon is not running for profile {}", cfg.profile),
                );
                return OK;
            };
            // The library it serves: it lets go of the socket first and closes the library
            // after, so "stopped" waits for that too (else a `daemon start` right after
            // meets the old daemon still running, and `ps` shows two).
            let library = (c.call("version", Value::Null).ok())
                .and_then(|v| v["library"].as_str().map(str::to_owned))
                .unwrap_or_else(|| cfg.library.clone());
            if let Err(e) = c.call("daemon.shutdown", Value::Null) {
                eprintln!("keel: {}", e.message);
                return FAILED;
            }
            drop(c);
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if running(cfg).is_none() {
                    let dir = cfg.data_dir.join("library").join(&library);
                    let left = deadline.saturating_duration_since(Instant::now());
                    if let Err(e) = crate::library::released(&dir, left) {
                        eprintln!("keel: {e}");
                        return FAILED;
                    }
                    say(
                        out,
                        json!({"running": false}),
                        format!("keel-daemon stopped for profile {}", cfg.profile),
                    );
                    return OK;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!("keel: keel-daemon is still running");
            FAILED
        }
    }
}

#[cfg(test)]
mod etp_tests {
    use super::*;

    fn cfg(toml: &str) -> (tempfile::TempDir, HostConfig) {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/work");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("config.toml"), toml).unwrap();
        let cfg = HostConfig::read("work", dir.path().into(), dir.path().join("data"));
        (dir, cfg)
    }

    const NAS: &str = "[[remotes]]\nid = \"nas\"\nlabel = \"NAS\"\nhost = \"FILES.example\"\n\
        port = 2121\nuser = \"\"\nauth = \"Agent\"\nbookmarks = []\nprotocol = \"ftps\"\n";

    #[test]
    fn etp_targets_from_the_command_line_and_the_settings() {
        let (_d, c) = cfg(NAS);
        assert_eq!(c.remotes.len(), 1, "{c:?}");
        // The FTP remote with that host and port lends its id and TLS.
        let t = etp_target(&c, "files.example:2121", false, None).unwrap();
        assert_eq!((t.authority.as_deref(), t.tls), (Some("nas"), true));
        let t = etp_target(&c, "other.example", false, Some("pw".into())).unwrap();
        assert_eq!((t.authority, t.tls, t.port), (None, false, 21));
        assert_eq!(t.password, "pw");
        assert!(etp_target(&c, "", false, None).is_err(), "no [search] etp");
        // `[search] etp` when no server is given; `etp_remote` by label.
        let (_d, c) = cfg(&format!(
            "[search]\netp = \"me@other.example:2121\"\netp_pin = \"AB\"\netp_remote = \"NAS\"\n{NAS}"
        ));
        let t = etp_target(&c, "", false, None).unwrap();
        assert_eq!(
            (t.host.as_str(), t.user.as_str(), t.pin.as_deref()),
            ("other.example", "me", Some("AB"))
        );
        assert_eq!((t.authority.as_deref(), t.tls), (Some("nas"), true));
        let (_d, c) = cfg("[search]\netp = \"h\"\netp_remote = \"missing\"\n");
        assert!(etp_target(&c, "", false, None).is_err());
    }
}
