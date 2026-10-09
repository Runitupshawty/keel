//! `keel <subcommand>` (Task 37): the `keel-api` operations from the command line, through
//! keel-daemon when one runs for the profile, else on the library opened in this process.
//! `--json` prints the API's JSON; exit codes are 0 (ok), 1 (the operation failed) and 2
//! (usage). Mutating subcommands show the preview and then confirm it themselves (the
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

/// The library in this process (no daemon runs): closed on drop.
struct Local(Host);

impl Backend for Local {
    fn call(&mut self, method: &str, params: Value) -> keel_api::Result<Value> {
        keel_api::call(&self.0.ctx, method, params)
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.close() {
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
        match Host::open(cfg, Some(NetSetup::system()), false) {
            Ok(host) => return Ok(Box::new(Local(host.with_utc_offset(offset)))),
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
        Command::Mcp => return mcp(&cfg),
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
        Ok(()) => OK,
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

/// Ends with exit 1 unless the job is done.
fn job_done(info: &JobInfo, json: bool, out: &mut Out) -> Result<(), Fail> {
    if json {
        print_json(&serde_json::to_value(info).unwrap_or_default(), out);
    } else {
        let _ = writeln!(out, "job {} ({}): {}", info.id, info.kind, info.status);
    }
    match info.status.as_str() {
        "done" => Ok(()),
        _ => {
            let log = info.log.as_deref().unwrap_or("").trim();
            Err(Fail::Api(ApiError::failed(format!(
                "job {} {}{}",
                info.id,
                info.status,
                if log.is_empty() {
                    String::new()
                } else {
                    format!(": {log}")
                }
            ))))
        }
    }
}

fn command(cmd: Command, b: &mut dyn Backend, json: bool, out: &mut Out) -> Result<(), Fail> {
    match cmd {
        Command::Search { query, max } => {
            let hits = b.call("search", json!({"query": query, "max": max}))?;
            if json {
                print_json(&hits, out);
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
            if json {
                print_json(&done, out);
            }
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
                print_json(&preview, out);
            } else {
                print_preview(&parse(preview)?, out);
            }
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
                    job_done(&info, json, out)?;
                }
                _ if json => print_json(&done, out),
                Some(job) => {
                    let _ = writeln!(out, "started job {job}");
                }
                None => print_json(&done["result"], out),
            }
        }
        Command::Devices => {
            let d = b.call("devices.list", Value::Null)?;
            if json {
                print_json(&d, out);
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
        }
        Command::Shares => {
            let g = b.call("shares.list", Value::Null)?;
            if json {
                print_json(&g, out);
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
        }
        Command::Sources { action } => sources(action, b, json, out)?,
        Command::Daemon(_) | Command::Mcp => unreachable!("handled by run"),
    }
    Ok(())
}

fn sources(
    action: Option<SourcesCmd>,
    b: &mut dyn Backend,
    json: bool,
    out: &mut Out,
) -> Result<(), Fail> {
    let index = |b: &mut dyn Backend, id: &str, out: &mut Out| -> Result<(), Fail> {
        let started = confirm(b, "sources.index", json!({ "id": id }), json, out)?;
        let job = started["job"].as_i64().unwrap_or_default();
        let info = wait_job(b, job)?;
        job_done(&info, json, out)
    };
    match action {
        None => {
            let list = b.call("sources.list", Value::Null)?;
            if json {
                print_json(&list, out);
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
            if json {
                print_json(&added, out);
            } else {
                let _ = writeln!(out, "source {id}");
            }
            if !no_index {
                index(b, &id, out)?;
            }
        }
        Some(SourcesCmd::Remove { id, delete_store }) => {
            let removed = b.call(
                "sources.remove",
                json!({"id": id, "delete_store": delete_store}),
            )?;
            if json {
                print_json(&removed, out);
            } else {
                let _ = writeln!(
                    out,
                    "removed source {} ({})",
                    removed["removed"]["label"].as_str().unwrap_or(""),
                    id
                );
            }
        }
        Some(SourcesCmd::Index { id }) => index(b, &id, out)?,
    }
    Ok(())
}

fn print_preview(p: &PlanPreview, out: &mut Out) {
    let _ = writeln!(out, "{}", p.summary);
    for c in &p.changes {
        let mut line = format!("  {:<8} {}", c.action, c.path.as_deref().unwrap_or(""));
        if let Some(to) = &c.to {
            line.push_str(&format!(" -> {to}"));
        }
        if let (Some(files), Some(bytes)) = (c.files, c.bytes) {
            line.push_str(&format!("  ({files} file(s), {bytes} bytes)"));
        }
        if let Some(d) = &c.detail {
            line.push_str(&format!("  [{d}]"));
        }
        let _ = writeln!(out, "{line}");
    }
    if !p.warnings.is_empty() {
        let _ = writeln!(out, "Warnings:");
        for w in &p.warnings {
            let _ = writeln!(out, "  ! {}", w.message);
        }
    }
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

fn mcp(cfg: &HostConfig) -> i32 {
    let mut backend = match connect(cfg) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("keel mcp: {e:#}");
            return FAILED;
        }
    };
    let stdin = std::io::stdin();
    match keel_api::mcp::serve(&mut *backend, stdin.lock(), std::io::stdout().lock()) {
        Ok(()) => OK,
        Err(e) => {
            eprintln!("keel mcp: {e}");
            FAILED
        }
    }
}

fn daemon_exe() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe
        .parent()?
        .join(format!("keel-daemon{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
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
            let Some(exe) = daemon_exe() else {
                eprintln!("keel: keel-daemon was not found next to keel");
                return FAILED;
            };
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
                .args(["--profile", &cfg.profile])
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
            let mut child = match command.spawn() {
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
