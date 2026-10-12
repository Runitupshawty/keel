//! `keel <subcommand>` end to end (Task 37): the real binary, its library in temp folders
//! (`KEEL_CONFIG_DIR` / `KEEL_DATA_DIR`), a profile no daemon serves, so every command
//! takes the in-process path.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

struct Env {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
    files: tempfile::TempDir,
    profile: String,
}

fn env() -> Env {
    let (config, data, files) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::create_dir(files.path().join("docs")).unwrap();
    std::fs::write(files.path().join("docs/invoice-2026.pdf"), b"pdf bytes").unwrap();
    std::fs::write(files.path().join("notes.txt"), b"notes").unwrap();
    let mut r = [0u8; 4];
    getrandom::fill(&mut r).unwrap();
    let profile = format!("clitest-{}-{}", std::process::id(), u32::from_le_bytes(r));
    Env {
        config,
        data,
        files,
        profile,
    }
}

impl Env {
    fn keel(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_keel"));
        c.env("KEEL_CONFIG_DIR", self.config.path())
            .env("KEEL_DATA_DIR", self.data.path())
            .args(["--profile", &self.profile])
            .args(args)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.keel(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "keel {args:?}: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn file(&self, rel: &str) -> String {
        let path = rel
            .split('/')
            .fold(self.files.path().to_owned(), |p, c| p.join(c));
        path.display().to_string()
    }
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

#[test]
fn search_against_a_fixture_library() {
    let env = env();
    let added = env.ok(&["sources", "add", &s(env.files.path()), "--json"]);
    assert!(added.contains("\"done\""), "{added}");
    let hits: Value = serde_json::from_str(&env.ok(&["search", "invoice", "--json"])).unwrap();
    let hits = hits.as_array().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["path"], env.file("docs/invoice-2026.pdf"));
    let text = env.ok(&["search", "invoice"]);
    assert!(text.contains("invoice-2026.pdf"), "{text}");
    assert_eq!(
        env.ok(&["search", "nothing-like-this", "--json"]).trim(),
        "[]"
    );
    // Tagging goes through its preview, then search finds the tag.
    env.ok(&["tag", "add", "receipts", &env.file("docs/invoice-2026.pdf")]);
    let tagged: Value =
        serde_json::from_str(&env.ok(&["search", "tag:receipts", "--json"])).unwrap();
    assert_eq!(tagged[0]["name"], "invoice-2026.pdf");
    // Operation errors exit 1, usage errors 2.
    assert_eq!(env.run(&["devices"]).status.code(), Some(1));
    assert_eq!(env.run(&["plan", "copy", "x"]).status.code(), Some(2));
    assert_eq!(env.run(&["execute"]).status.code(), Some(2));
}

#[test]
fn json_output_is_one_document() {
    let env = env();
    let doc = |args: &[&str]| -> Value {
        let out = env.run(args);
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "keel {args:?}: not one JSON document ({e}):\n{}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    };
    // The added source with its index job.
    let added = doc(&["sources", "add", &s(env.files.path()), "--json"]);
    assert!(added["id"].is_string(), "{added}");
    assert_eq!(added["job"]["status"], "done", "{added}");
    // `keel sources` carries each source's counts.
    let listed = doc(&["sources", "--json"]);
    let one = &listed[0];
    assert_eq!(one["id"], added["id"], "{listed}");
    assert_eq!(
        (one["files"].as_u64(), one["folders"].as_u64()),
        (Some(2), Some(1)),
        "{listed}"
    );
    assert!(
        one["last_walk"].is_i64() && one["hashed_files"].is_u64(),
        "{listed}"
    );
    let text = env.ok(&["sources"]);
    assert!(text.contains("files 2, folders 1, size "), "{text}");
    let tagged = doc(&["tag", "add", "receipts", &env.file("notes.txt"), "--json"]);
    assert_eq!(tagged["records"], 1, "{tagged}");
    let dst = tempfile::tempdir().unwrap();
    let preview = doc(&[
        "plan",
        "copy",
        &env.file("notes.txt"),
        "--to",
        &s(dst.path()),
        "--json",
    ]);
    let id = preview["plan_id"].as_str().unwrap();
    let hash = preview["input_hash"].as_str().unwrap();
    let done = doc(&["execute", id, "--hash", hash, "--json"]);
    assert_eq!(done["status"], "done", "{done}");
    // A failure is one `{"error": ...}` document too.
    let failed = doc(&["devices", "--json"]);
    assert!(failed["error"]["code"].is_i64(), "{failed}");
    let failed = doc(&["execute", id, "--hash", hash, "--json"]);
    assert!(
        failed["error"]["code"].is_i64(),
        "already executed: {failed}"
    );
}

/// Mounts belong to keel-daemon: without one, `keel mounts` lists none and `keel mount`
/// fails (exit 1, one JSON error document) with what to do.
#[test]
fn mounts_need_the_daemon() {
    let env = env();
    env.ok(&["sources", "add", &s(env.files.path()), "--label", "Files"]);
    assert_eq!(env.ok(&["mounts", "--json"]).trim(), "[]");
    let target = if cfg!(windows) {
        "Q:"
    } else {
        "/nonexistent-keel-mount"
    };
    for args in [
        &["mount", "Files", target, "--json"][..],
        &["mount", "files", target, "--subtree", "docs", "--json"],
        &["mount", "no-such-source", target, "--json"],
        &["unmount", target, "--json"],
    ] {
        let out = env.run(args);
        assert_eq!(out.status.code(), Some(1), "keel {args:?}");
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(doc["error"]["code"].is_i64(), "keel {args:?}: {doc}");
        if args[0] == "mount" {
            assert_eq!(doc["error"]["code"], -32008, "keel {args:?}: {doc}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("keel daemon start"),
                "keel {args:?}"
            );
        }
    }
    assert_eq!(env.run(&["mount", "Files"]).status.code(), Some(2));
}

#[test]
fn plan_piped_into_execute() {
    let env = env();
    let dst = tempfile::tempdir().unwrap();
    for json in [false, true] {
        let mut args = vec!["plan", "copy"];
        let src = env.file("notes.txt");
        let to = s(dst.path());
        args.extend([src.as_str(), "--to", to.as_str(), "--on-conflict", "rename"]);
        if json {
            args.push("--json");
        }
        let mut plan = env.keel(&args).stdout(Stdio::piped()).spawn().unwrap();
        let exec = env
            .keel(&["execute"])
            .stdin(plan.stdout.take().unwrap())
            .output()
            .unwrap();
        assert!(plan.wait().unwrap().success());
        assert!(
            exec.status.success(),
            "{}",
            String::from_utf8_lossy(&exec.stderr)
        );
        assert!(String::from_utf8_lossy(&exec.stdout).contains("done"));
    }
    assert!(dst.path().join("notes.txt").exists());
    assert!(
        dst.path().join("notes (2).txt").exists(),
        "the second copy kept both"
    );
    // A preview alone changes nothing; a wrong hash is refused (exit 1).
    let preview: Value = serde_json::from_str(&env.ok(&[
        "plan",
        "delete",
        &s(&dst.path().join("notes.txt")),
        "--json",
    ]))
    .unwrap();
    let id = preview["plan_id"].as_str().unwrap();
    let bad = env.run(&["execute", id, "--hash", &"0".repeat(64)]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(dst.path().join("notes.txt").exists());
}

/// Task 60: `keel plan flatten|organize|merge|attrs|link`, each piped into `keel execute`.
#[test]
fn folder_operations_from_the_command_line() {
    let env = env();
    let root = env.files.path();
    std::fs::create_dir_all(root.join("box/inner")).unwrap();
    std::fs::write(root.join("box/inner/deep.txt"), b"deep").unwrap();
    std::fs::write(root.join("box/photo.jpg"), b"jpg").unwrap();
    let run = |args: &[&str]| {
        let mut plan = env.keel(args).stdout(Stdio::piped()).spawn().unwrap();
        let exec = env
            .keel(&["execute"])
            .stdin(plan.stdout.take().unwrap())
            .output()
            .unwrap();
        assert!(plan.wait().unwrap().success(), "{args:?}");
        assert!(
            exec.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&exec.stderr)
        );
    };
    let boxed = s(&root.join("box"));
    let preview = env.ok(&["plan", "flatten", &boxed]);
    assert!(preview.contains("Flatten"), "{preview}");
    assert!(
        root.join("box/inner/deep.txt").exists(),
        "a preview moves nothing"
    );
    run(&["plan", "flatten", &boxed]);
    assert!(root.join("box/deep.txt").exists() && !root.join("box/inner").exists());
    run(&["plan", "organize", &boxed, "--by", "type"]);
    assert!(root.join("box/Images/photo.jpg").exists());
    assert!(root.join("box/Documents/deep.txt").exists());
    run(&[
        "plan",
        "merge",
        &s(&root.join("box/Images")),
        &s(&root.join("box/Documents")),
        "--into",
        &s(&root.join("box/Documents")),
    ]);
    assert!(root.join("box/Documents/photo.jpg").exists() && !root.join("box/Images").exists());
    let notes = env.file("notes.txt");
    run(&["plan", "attrs", &notes, "--readonly", "true"]);
    assert!(std::fs::metadata(&notes).unwrap().permissions().readonly());
    run(&["plan", "attrs", &notes, "--readonly", "false"]);
    run(&[
        "plan",
        "link",
        &notes,
        &s(&root.join("notes-link.txt")),
        "--kind",
        "hardlink",
    ]);
    assert_eq!(
        std::fs::read(root.join("notes-link.txt")).unwrap(),
        b"notes"
    );
    // Nothing to organize: exit 1 with the reason.
    let empty = tempfile::tempdir().unwrap();
    let out = env.run(&["plan", "organize", &s(empty.path()), "--by", "year"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing to organize"));
}

struct Mcp {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next: i64,
}

impl Mcp {
    fn send(&mut self, msg: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id, "{v}");
        v
    }

    fn tool(&mut self, name: &str, args: Value) -> Value {
        let v = self.call("tools/call", json!({"name": name, "arguments": args}));
        assert_eq!(v["result"]["isError"], false, "{v}");
        v["result"]["structuredContent"].clone()
    }

    fn line(&mut self) -> Value {
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// `execute` through the user's confirmation: answers the server's elicitation with
    /// `action` and returns the tool result.
    fn execute_confirmed(&mut self, preview: &Value, action: &str) -> Value {
        self.next += 1;
        let id = self.next;
        let args = json!({"plan_id": preview["plan_id"], "input_hash": preview["input_hash"]});
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "execute", "arguments": args}}));
        let ask = self.line();
        assert_eq!(ask["method"], "elicitation/create", "{ask}");
        let message = ask["params"]["message"].as_str().unwrap();
        assert!(
            message.contains(preview["summary"].as_str().unwrap()),
            "{message}"
        );
        let content = match action {
            "accept" => json!({"confirm": true}),
            _ => json!({}),
        };
        self.send(json!({"jsonrpc": "2.0", "id": ask["id"], "result": {"action": action, "content": content}}));
        let v = self.line();
        assert_eq!(v["id"], id, "{v}");
        v["result"].clone()
    }
}

fn mcp_session(env: &Env, args: &[&str], elicitation: bool) -> Mcp {
    let mut child = env
        .keel(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut mcp = Mcp {
        child,
        out,
        next: 0,
    };
    let caps = if elicitation {
        json!({"elicitation": {}})
    } else {
        json!({})
    };
    let init = mcp.call(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": caps, "clientInfo": {"name": "test", "version": "1"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "keel");
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    mcp
}

#[test]
fn mcp_over_stdio() {
    let env = env();
    let dst = tempfile::tempdir().unwrap();
    let mut mcp = mcp_session(&env, &["mcp"], true);
    let tools = mcp.call("tools/list", json!({}))["result"]["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "search",
        "plan",
        "execute",
        "sources_add",
        "tags_add",
        "shares_revoke",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    let plan_tool = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "plan")
        .unwrap();
    assert_eq!(plan_tool["inputSchema"]["type"], "object");
    assert_eq!(plan_tool["annotations"]["readOnlyHint"], false);

    // A plan call returns a preview and touches nothing.
    let src = env.file("notes.txt");
    let preview = mcp.tool(
        "plan",
        json!({"op": "copy", "paths": [src], "to": s(dst.path())}),
    );
    assert_eq!(preview["changes"][0]["action"], "copy", "{preview}");
    assert!(preview["plan_id"].is_string());
    assert!(!dst.path().join("notes.txt").exists());
    // So does a mutating tool called directly.
    let add = mcp.tool("sources_add", json!({"root": s(env.files.path())}));
    assert_eq!(add["operation"], "sources.add");
    assert!(
        mcp.tool("sources_list", json!({})).is_null(),
        "arrays are text only"
    );
    // Execute asks the user; declined, nothing happens.
    let declined = mcp.execute_confirmed(&preview, "decline");
    assert_eq!(declined["isError"], true, "{declined}");
    assert!(!dst.path().join("notes.txt").exists());
    // Accepted, it applies the exact preview; its job finishes.
    let done = mcp.execute_confirmed(&preview, "accept");
    assert_eq!(done["isError"], false, "{done}");
    let job = done["structuredContent"]["job"].as_i64().unwrap();
    for _ in 0..200 {
        let info = mcp.tool("jobs_info", json!({"id": job}));
        if info["status"] == "done" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(dst.path().join("notes.txt").exists());
    // Errors are tool results with isError.
    let bad = mcp.call(
        "tools/call",
        json!({"name": "execute", "arguments": {"plan_id": "x", "input_hash": "y"}}),
    );
    assert_eq!(bad["result"]["isError"], true);
    drop(mcp.child.stdin.take());
    assert!(mcp.child.wait().unwrap().success());
}

#[test]
fn mcp_without_elicitation() {
    let env = env();
    let dst = tempfile::tempdir().unwrap();
    let src = env.file("notes.txt");
    let plan = json!({"op": "copy", "paths": [src], "to": s(dst.path())});
    // Refused outright.
    let mut mcp = mcp_session(&env, &["mcp"], false);
    let preview = mcp.tool("plan", plan.clone());
    let args = json!({"plan_id": preview["plan_id"], "input_hash": preview["input_hash"], "summary": preview["summary"]});
    let refused = mcp.call("tools/call", json!({"name": "execute", "arguments": args}));
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    drop(mcp.child.stdin.take());
    assert!(mcp.child.wait().unwrap().success());
    assert!(!dst.path().join("notes.txt").exists());
    // --allow-execute: with the preview's summary.
    let mut mcp = mcp_session(&env, &["mcp", "--allow-execute"], false);
    let preview = mcp.tool("plan", plan);
    let mut args = json!({"plan_id": preview["plan_id"], "input_hash": preview["input_hash"]});
    let no_summary = mcp.call("tools/call", json!({"name": "execute", "arguments": args}));
    assert_eq!(no_summary["result"]["isError"], true, "{no_summary}");
    args["summary"] = preview["summary"].clone();
    let done = mcp.tool("execute", args);
    assert!(done["job"].is_i64(), "{done}");
    drop(mcp.child.stdin.take());
    assert!(mcp.child.wait().unwrap().success());
}

/// `keel --version` names the version and whether the binary is signed (a test build is
/// not), for bug reports.
#[test]
fn version_says_whether_signed() {
    let out = Command::new(env!("CARGO_BIN_EXE_keel"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(concat!("keel ", env!("CARGO_PKG_VERSION")))
    );
    let sig = lines.next().unwrap_or_default();
    assert!(
        ["signature: unsigned", "signature: not signed (Linux)"]
            .iter()
            .any(|s| sig.starts_with(s)),
        "{text}"
    );
}

/// Process start to exit of `keel --version` (median of 20): `cargo test -p keel-app
/// --release --test cli -- --ignored --nocapture perf_version`.
#[test]
#[ignore = "release measurement: process startup"]
fn perf_version_startup() {
    let mut runs: Vec<std::time::Duration> = (0..20)
        .map(|_| {
            let t = std::time::Instant::now();
            let out = Command::new(env!("CARGO_BIN_EXE_keel"))
                .arg("--version")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success());
            t.elapsed()
        })
        .collect();
    runs.sort();
    let median = runs[runs.len() / 2];
    eprintln!(
        "PERF version_startup: median {median:?}, max {:?}",
        runs[runs.len() - 1]
    );
    assert!(median < std::time::Duration::from_millis(500), "{median:?}");
}

/// Task 46: `keel du` reads folder sizes from the index, and walks a folder outside the
/// library; biggest first either way.
#[test]
fn du_from_the_index_and_by_walking() {
    let env = env();
    let outside: Value =
        serde_json::from_str(&env.ok(&["du", &s(env.files.path()), "--json"])).unwrap();
    // "pdf bytes" (9) in docs, "notes" (5)
    assert_eq!(outside["bytes"], 14, "{outside}");
    assert_eq!(outside["children"][0]["name"], "docs", "{outside}");
    assert_eq!(outside["children"][0]["files"], 1, "{outside}");
    env.ok(&["sources", "add", &s(env.files.path()), "--json"]);
    let indexed: Value =
        serde_json::from_str(&env.ok(&["du", &s(env.files.path()), "--json"])).unwrap();
    assert_eq!(indexed, outside, "the index agrees with the walk");
    let text = env.ok(&["du", &env.file("docs")]);
    assert!(
        text.lines().next().unwrap().ends_with("invoice-2026.pdf"),
        "{text}"
    );
    assert!(text.contains(" 1 file "), "{text}");
}

/// Task 46: one child folder that cannot be listed leaves `keel du` working: that child is
/// unknown (no `bytes`), the rest is counted. Windows: a deny-list ACE on a temp folder,
/// removed again; elsewhere mode 000 (skipped as root, who reads it anyway).
#[test]
fn du_reports_an_unreadable_folder_as_unknown() {
    let env = env();
    let locked = env.files.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("secret.bin"), b"0123456789").unwrap();
    #[cfg(windows)]
    let lock = |on: bool| {
        let arg: &[&str] = if on {
            &["/deny", "*S-1-1-0:(RD)"]
        } else {
            &["/remove:d", "*S-1-1-0"]
        };
        Command::new("icacls")
            .arg(&locked)
            .args(arg)
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    #[cfg(unix)]
    let lock = |on: bool| {
        use std::os::unix::fs::PermissionsExt;
        let mode = if on { 0o000 } else { 0o755 };
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(mode)).is_ok()
    };
    if !lock(true) {
        return eprintln!("skipped: cannot lock a folder here");
    }
    if std::fs::read_dir(&locked).is_ok() {
        lock(false);
        return eprintln!("skipped: the locked folder is still readable");
    }
    let out = env.run(&["du", &s(env.files.path()), "--json"]);
    lock(false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let child = |name: &str| {
        (v["children"].as_array().unwrap().iter())
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap()
    };
    assert!(child("locked").get("bytes").is_none(), "{v}");
    assert_eq!(child("docs")["bytes"], 9, "{v}");
    assert_eq!(v["bytes"], 14, "the readable part: {v}");
}

/// Task 81: `keel du --scan` walks several roots into one result without the library,
/// saves the scan, and `--load` shows the same again; `--exclude` leaves names out.
#[test]
fn du_scan_several_roots_save_and_load() {
    let env = env();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("big.bin"), vec![0u8; 3000]).unwrap();
    let saved = tempfile::tempdir().unwrap();
    let file = saved.path().join("scan.keeldu");
    let (a, b, f) = (s(env.files.path()), s(other.path()), s(&file));
    let args = ["du", "--scan", &a, &b, "--top", "1", "--save", &f, "--json"];
    let v: Value = serde_json::from_str(&env.ok(&args)).unwrap();
    assert_eq!(v["bytes"], 3014, "{v}");
    assert_eq!(v["files"], 3, "{v}");
    assert_eq!(v["complete"], true, "{v}");
    let roots = v["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 2, "{v}");
    assert_eq!(roots[0]["children"][0]["name"], "docs", "{v}");
    assert_eq!(roots[0]["children"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(roots[0]["truncated"], true, "{v}");
    assert_eq!(roots[1]["bytes"], 3000, "{v}");
    assert!(roots[1]["allocated"].as_u64().unwrap() > 0, "{v}");
    let loaded: Value =
        serde_json::from_str(&env.ok(&["du", "--load", &f, "--top", "1", "--json"])).unwrap();
    assert_eq!(loaded["roots"], v["roots"]);
    let text = env.ok(&[
        "du",
        "--scan",
        &b,
        &a,
        "--exclude",
        "*.BIN",
        "--exclude",
        "*.bin",
    ]);
    assert!(!text.contains("big.bin"), "{text}");
    assert!(
        text.contains("invoice-2026.pdf") || text.contains("docs"),
        "{text}"
    );
    assert!(text.contains("(all roots)"), "{text}");
    assert_eq!(
        env.run(&["du"]).status.code(),
        Some(2),
        "a folder or a scan"
    );
}

/// Task 53: `keel library backup` and `keel library restore`, in-process: a restore never
/// replaces a library (`--as` names another), and on a fresh data folder (another machine)
/// it needs no `--as`; the tags come back.
#[test]
fn library_backup_and_restore_round_trip() {
    let env = env();
    env.ok(&["sources", "add", &s(env.files.path())]);
    env.ok(&["tag", "add", "receipts", &env.file("docs/invoice-2026.pdf")]);
    let out = tempfile::tempdir().unwrap();
    let text = env.ok(&["library", "backup", &s(out.path())]);
    assert!(text.contains("backed up to "), "{text}");
    let file = std::fs::read_dir(out.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        file.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("main-"),
        "{file:?}"
    );
    assert_eq!(file.extension().unwrap(), "keelbackup");
    // The same library: refused (exit 1), with the way out in the message.
    let refused = env.run(&["library", "restore", &s(&file)]);
    assert_eq!(refused.status.code(), Some(1));
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(err.contains("a library named main exists"), "{err}");
    let copy: Value =
        serde_json::from_str(&env.ok(&["library", "restore", &s(&file), "--as", "copy", "--json"]))
            .unwrap();
    assert_eq!(copy["name"], "copy", "{copy}");
    assert_eq!(copy["sources"][0]["online"], true, "{copy}");
    // Another machine: a fresh data folder, no library yet.
    let other = crate::env();
    let text = other.ok(&["library", "restore", &s(&file)]);
    assert!(text.contains("restored library main"), "{text}");
    let tagged: Value =
        serde_json::from_str(&other.ok(&["search", "tag:receipts", "--json"])).unwrap();
    assert_eq!(tagged[0]["name"], "invoice-2026.pdf", "{tagged}");
    // Not a backup: exit 1, a message and no crash report.
    let junk = out.path().join("junk.keelbackup");
    std::fs::write(&junk, b"junk").unwrap();
    let bad = other.run(&["library", "restore", &s(&junk), "--as", "x"]);
    assert_eq!(bad.status.code(), Some(1));
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(err.contains("not a Keel backup"), "{err}");
}

/// Task 58: `keel op` runs any operation by name; one that previews only prints its preview
/// unless `--execute`; usage mistakes exit 2, a refused operation 1 (naming the field).
#[test]
fn op_runs_any_operation_with_preview_then_execute() {
    let env = env();
    env.ok(&["sources", "add", &s(env.files.path())]);
    let doc = |args: &[&str]| -> Value { serde_json::from_str(&env.ok(args)).unwrap() };
    // Without a terminal the output is JSON, `--json` or not.
    let list = doc(&["op", "list"]);
    let list = list.as_array().unwrap();
    let find = |name: &str| list.iter().find(|o| o["name"] == name).unwrap().clone();
    assert_eq!(find("library.stats")["previews"], false);
    assert_eq!(find("tags.set")["previews"], true);
    assert_eq!(find("plan")["previews"], true);
    assert!(find("tags.set")["params"]["properties"]["tags"].is_object());
    assert!(find("search")["example"]["query"].is_string());
    assert_eq!(
        doc(&["op"]),
        Value::Array(list.clone()),
        "`keel op` lists too"
    );
    // A read prints its result.
    let stats = doc(&["op", "library.stats"]);
    assert_eq!(stats["files"], 2, "{stats}");
    let hits = doc(&["op", "search", "--params", r#"{"query": "invoice"}"#]);
    assert_eq!(hits[0]["name"], "invoice-2026.pdf", "{hits}");
    // A previewed operation: the preview only; nothing is tagged.
    let notes = env.file("notes.txt");
    let params = json!({"paths": [notes], "tags": ["receipts"]}).to_string();
    let preview = doc(&["op", "tags.set", "--params", &params]);
    assert_eq!(preview["operation"], "tags.set", "{preview}");
    assert!(preview["plan_id"].is_string() && preview["input_hash"].is_string());
    let tagged = |env: &Env| doc_of(&env.ok(&["search", "tag:receipts", "--json"]));
    assert_eq!(tagged(&env), json!([]), "a preview changes nothing");
    // The agent's second step: `execute` with that preview's plan id and input hash.
    let ids = json!({"plan_id": preview["plan_id"], "input_hash": preview["input_hash"]});
    let done = doc(&["op", "execute", "--params", &ids.to_string()]);
    assert_eq!(done["result"]["records"], 1, "{done}");
    assert_eq!(tagged(&env)[0]["name"], "notes.txt");
    // `--execute` previews and applies at once (parameters from a file here).
    let file = env.files.path().join("params.json");
    let params = json!({"paths": [env.file("docs/invoice-2026.pdf")], "tags": ["receipts"]});
    std::fs::write(&file, params.to_string()).unwrap();
    let done = doc(&["op", "tags.set", "--params-file", &s(&file), "--execute"]);
    assert_eq!(done["preview"]["operation"], "tags.set", "{done}");
    assert_eq!(done["executed"]["result"]["records"], 1, "{done}");
    assert_eq!(tagged(&env).as_array().unwrap().len(), 2);
    // `plan` previews too, and `execute` waits for the copy's job.
    let dst = tempfile::tempdir().unwrap();
    let params = json!({"op": "copy", "paths": [notes], "to": s(dst.path())}).to_string();
    let preview = doc(&["op", "plan", "--params", &params]);
    assert!(
        !dst.path().join("notes.txt").exists(),
        "a preview copies nothing"
    );
    let ids = json!({"plan_id": preview["plan_id"], "input_hash": preview["input_hash"]});
    let job = doc(&["op", "execute", "--params", &ids.to_string()]);
    assert_eq!(job["status"], "done", "{job}");
    assert!(dst.path().join("notes.txt").exists());
    // The folder operations preview too: `--execute` applies the flatten.
    assert_eq!(find("plan.flatten")["previews"], true);
    let flat = tempfile::tempdir().unwrap();
    std::fs::create_dir(flat.path().join("sub")).unwrap();
    std::fs::write(flat.path().join("sub").join("a.txt"), b"a").unwrap();
    let params = json!({"path": s(flat.path()), "on_conflict": "skip"}).to_string();
    let done = doc(&["op", "plan.flatten", "--params", &params, "--execute"]);
    assert_eq!(done["preview"]["operation"], "plan", "{done}");
    assert_eq!(done["executed"]["status"], "done", "{done}");
    assert!(flat.path().join("a.txt").exists(), "{done}");
    // Usage: an unknown name, JSON that does not parse or is no object, --execute on a
    // read, both --params and --params-file.
    for args in [
        &["op", "no.such.op"][..],
        &["op", "search", "--params", "{query"],
        &["op", "search", "--params", "[1]"],
        &["op", "library.stats", "--execute"],
        &["op", "search", "--params", "{}", "--params-file", "p.json"],
    ] {
        let out = env.run(args);
        assert_eq!(out.status.code(), Some(2), "keel {args:?}");
        assert!(out.stdout.is_empty(), "keel {args:?}");
    }
    let wrong = env.run(&["op", "no.such.op"]);
    let err = String::from_utf8_lossy(&wrong.stderr);
    assert!(err.contains("no operation no.such.op"), "{err}");
    // Parameters the operation refuses: exit 1, the error names the field.
    let params = json!({ "paths": [notes] }).to_string();
    let out = env.run(&["op", "tags.set", "--params", &params]);
    assert_eq!(out.status.code(), Some(1));
    let failed: Value = serde_json::from_slice(&out.stdout).unwrap();
    let message = failed["error"]["message"].as_str().unwrap();
    assert!(message.contains("`tags`"), "{failed}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("`tags`"));
}

fn doc_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// Review of 0.22.0, minor 9: without a daemon the command ends when its walk does; the
/// text job the walk started stops with it, and the command says it resumes later.
#[test]
fn a_job_cut_short_by_the_command_ending_is_named() {
    let env = env();
    for i in 0..1_500 {
        std::fs::write(
            env.files.path().join(format!("n{i:04}.txt")),
            "harbour pilot",
        )
        .unwrap();
    }
    let out = env.run(&["sources", "add", &s(env.files.path())]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("text") && err.contains("was still running; it resumes"),
        "{err}"
    );
}
