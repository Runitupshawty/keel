//! `keel <subcommand>` (Task 37): the `keel-api` operations from the command line, through
//! keel-daemon when one runs for the profile, else on the library opened in this process.
//! `--json` prints one JSON document per invocation (the API's JSON, or `{"error": …}`);
//! exit codes are 0 (ok), 1 (the operation failed) and 2 (usage). Mutating subcommands show the preview and then confirm it themselves (the
//! command line is the confirmation), except `keel plan`, whose plan `keel execute` runs.

use crate::cli::{Command, Conflict, DaemonCmd, PlanCmd, SourcesCmd, TagCmd};
use keel_api::client::Client;
use keel_api::config::HostConfig;
use keel_api::host::{Host, NetSetup};
use keel_api::types::{Hit, JobInfo, PlanPreview};
use keel_api::{ApiError, Backend};
use serde_json::{json, Value};
use std::io::{IsTerminal, Read, Write};
use std::time::{Duration, Instant};

pub const OK: i32 = 0;
pub const FAILED: i32 = 1;
pub const USAGE: i32 = 2;

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
        if !self.host.close() {
            eprintln!("keel: a job was still busy; it resumes the next time the library opens");
        }
    }
}

/// keel-daemon for the profile, else the library in-process (keel-net only then, so one
/// node per profile).
fn connect(cfg: &HostConfig) -> anyhow::Result<Box<dyn Backend>> {
    if let Ok(client) = Client::connect(&cfg.socket_name()) {
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

pub fn run(cmd: Command, profile: &str, json: bool) -> i32 {
    let cfg = match HostConfig::load(profile) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("keel: {e:#}");
            return FAILED;
        }
    };
    let mut out: Out = Box::new(std::io::stdout());
    match cmd {
        Command::Daemon(d) => return daemon(d, &cfg, json, &mut out),
        Command::Mcp { allow_execute } => return mcp(&cfg, allow_execute),
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
    let mut backend = match connect(&cfg) {
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

/// Runs `cmd`. Text goes to `out` as it comes; with `json` nothing is printed here and the
/// one document to print is returned.
fn command(
    cmd: Command,
    b: &mut dyn Backend,
    json: bool,
    out: &mut Out,
) -> Result<Option<Value>, Fail> {
    let doc = match cmd {
        Command::Search { query, max } => {
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
            let params = match p {
                PlanCmd::Copy {
                    src,
                    to,
                    on_conflict,
                } => json!({
                    "op": "copy", "paths": abs_all(&src), "to": abs(&to), "on_conflict": conflict(on_conflict),
                }),
                PlanCmd::Move {
                    src,
                    to,
                    on_conflict,
                } => json!({
                    "op": "move", "paths": abs_all(&src), "to": abs(&to), "on_conflict": conflict(on_conflict),
                }),
                PlanCmd::Delete { paths } => json!({"op": "delete", "paths": abs_all(&paths)}),
            };
            let preview = b.call("plan", params)?;
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
            let done = b.call(
                "execute",
                json!({"plan_id": plan_id, "input_hash": input_hash}),
            )?;
            match done["job"].as_i64() {
                Some(job) if !no_wait => {
                    let info = wait_job(b, job)?;
                    Some(job_done(&info, json, out)?)
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
            }
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
                    let _ = writeln!(
                        out,
                        "{:<8} {}  {}",
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
        Command::Daemon(_) | Command::Mcp { .. } => unreachable!("handled by run"),
    };
    Ok(doc.filter(|_| json))
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
            let list = b.call("sources.list", Value::Null)?;
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
    let mut backend = match connect(cfg) {
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
            if let Err(e) = c.call("daemon.shutdown", Value::Null) {
                eprintln!("keel: {}", e.message);
                return FAILED;
            }
            drop(c);
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if running(cfg).is_none() {
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
