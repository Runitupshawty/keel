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
