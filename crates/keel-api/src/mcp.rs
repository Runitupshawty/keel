//! An MCP server (Model Context Protocol 2025-06-18) over stdio: newline-delimited
//! JSON-RPC on stdin/stdout. Every registered operation is a tool with its JSON schema;
//! `tools/call` runs the same handlers as JSON-RPC and the CLI. Mutating tools return a
//! preview; only `execute` with that preview's plan id and input hash applies it, and
//! only once a person confirmed it:
//!
//! - a client that declared `elicitation` is asked (`elicitation/create`) with the plan's
//!   summary, changes and warnings; the plan runs only when the user accepts;
//! - a client that cannot ask gets `execute` refused, unless the server was started with
//!   [`Options::allow_execute`] (`keel mcp --allow-execute`): then the call must repeat the
//!   preview's `summary` exactly, so the client's own approval prompt shows what runs.
//!
//! `execute` applies only plans previewed in this session. Nothing but `initialize` and
//! `ping` is answered before `initialize`.

use crate::rpc::{self, Line};
use crate::types::PlanPreview;
use crate::{ApiError, Backend, Operation, OPS};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Older revisions whose tools subset is the same.
const ALSO: &[&str] = &["2025-03-26", "2024-11-05"];

/// Changes listed in a confirmation prompt (the rest are counted).
const PROMPT_CHANGES: usize = 50;

const INSTRUCTIONS: &str = "Keel file manager tools. Reads (search, list, stat, \
sources_list, jobs_list, duplicates, ...) act at once. Every mutating tool (plan for \
copy/move/delete/rename, tags_*, sources_add, shares_grant, ...) only returns a preview \
with a plan_id and input_hash: show the preview to the user, then call execute with that \
plan_id and input_hash. Keel asks the user to confirm the plan itself before it applies \
anything (when the server runs with --allow-execute instead, pass the preview's summary \
as `summary`). File operations run as jobs: follow them with jobs_info. Paths are \
absolute.";

/// How `serve` treats `execute`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Clients without elicitation may execute, passing the preview's `summary`.
    pub allow_execute: bool,
}

/// Tool names use `_` for `.` (`sources.add` -> `sources_add`): many clients allow only
/// `[A-Za-z0-9_-]`.
pub fn tool_name(op: &Operation) -> String {
    op.name.replace('.', "_")
}

fn op_for_tool(name: &str) -> Option<&'static Operation> {
    OPS.iter()
        .find(|op| tool_name(op) == name || op.name == name)
}

pub fn tool(op: &Operation) -> Value {
    let mut schema = (op.params)();
    if let Some(map) = schema.as_object_mut() {
        map.remove("$schema");
        map.remove("title");
    }
    let description = if op.previewed() {
        format!(
            "{} Returns a preview; call execute with the plan id to apply \
             (pass the preview's plan_id and input_hash).",
            op.summary
        )
    } else if op.name == "execute" {
        schema["properties"]["summary"] = json!({
            "type": "string",
            "description": "The preview's summary, exactly. Needed only when keel mcp runs \
                            with --allow-execute and cannot ask the user itself.",
        });
        format!(
            "{} Keel asks the user to confirm the plan's summary, changes and warnings \
             before anything is applied.",
            op.summary
        )
    } else {
        op.summary.to_owned()
    };
    json!({
        "name": tool_name(op),
        "title": op.name,
        "description": description,
        "inputSchema": schema,
        "annotations": {
            "title": op.name,
            "readOnlyHint": !op.mutating && op.name != "plan",
            "destructiveHint": op.name == "execute" || op.name == "shares.revoke",
            "idempotentHint": !op.mutating,
            "openWorldHint": op.name.starts_with("devices.") || op.name.starts_with("shares."),
        },
    })
}

pub fn tools() -> Vec<Value> {
    OPS.iter().map(tool).collect()
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn call_result(result: crate::Result<Value>) -> Value {
    match result {
        Ok(v) => {
            let text = serde_json::to_string_pretty(&v).unwrap_or_default();
            let mut out = json!({"content": [{"type": "text", "text": text}], "isError": false});
            if v.is_object() {
                out["structuredContent"] = v;
            }
            out
        }
        Err(e) => {
            let mut text = format!("error {}: {}", e.code, e.message);
            if let Some(data) = &e.data {
                text.push_str(&format!(
                    "\n{}",
                    serde_json::to_string_pretty(data).unwrap_or_default()
                ));
            }
            json!({"content": [{"type": "text", "text": text}], "isError": true})
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn refused(message: String) -> crate::Result<Value> {
    Err(ApiError::new(ApiError::FAILED, message))
}

/// Serves MCP on `input` / `output` until `input` ends.
pub fn serve(
    backend: &mut dyn Backend,
    input: impl BufRead,
    output: impl Write,
    opts: Options,
) -> io::Result<()> {
    let mut server = Server {
        backend,
        input,
        output,
        opts,
        initialized: false,
        elicit: false,
        asking: false,
        asked: 0,
        previews: HashMap::new(),
    };
    while let Some(msg) = server.next_message()? {
        let answer = match msg {
            Ok(msg) => server.handle(&msg)?,
            Err(answer) => Some(answer),
        };
        if let Some(answer) = answer {
            server.send(&answer)?;
        }
    }
    Ok(())
}

struct Server<'a, R, W> {
    backend: &'a mut dyn Backend,
    input: R,
    output: W,
    opts: Options,
    initialized: bool,
    /// The client declared `elicitation`.
    elicit: bool,
    /// Waiting for the user's answer to a confirmation.
    asking: bool,
    asked: u64,
    /// The previews this session returned, by plan id.
    previews: HashMap<String, PlanPreview>,
}

impl<R: BufRead, W: Write> Server<'_, R, W> {
    fn send(&mut self, v: &Value) -> io::Result<()> {
        serde_json::to_writer(&mut self.output, v)?;
        self.output.write_all(b"\n")?;
        self.output.flush()
    }

    /// The next message (Err: the answer to a line that is not one); None at the end.
    fn next_message(&mut self) -> io::Result<Option<Result<Value, Value>>> {
        loop {
            let line = match rpc::read_line(&mut self.input, || Ok(()))? {
                Line::Eof => return Ok(None),
                Line::TooLarge => {
                    let too_large = format!("message over {} MiB refused", rpc::MAX_REQUEST >> 20);
                    return Ok(Some(Err(err(Value::Null, -32600, &too_large))));
                }
                Line::Line(line) => line,
            };
            if line.trim_ascii().is_empty() {
                continue;
            }
            return Ok(Some(match serde_json::from_slice::<Value>(&line) {
                Ok(Value::Array(_)) => Err(err(Value::Null, -32600, "batches are not supported")),
                Ok(msg) => Ok(msg),
                Err(e) => Err(err(Value::Null, -32700, &format!("parse error: {e}"))),
            }));
        }
    }

    /// Answers one message; None for notifications and responses.
    fn handle(&mut self, msg: &Value) -> io::Result<Option<Value>> {
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response to something we never sent (or no longer wait for), or garbage.
            return Ok(msg
                .get("id")
                .filter(|_| msg.get("result").is_none() && msg.get("error").is_none())
                .map(|id| err(id.clone(), -32600, "invalid request")));
        };
        let Some(id) = msg.get("id").cloned() else {
            return Ok(None); // notifications (initialized, cancelled) need no answer
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        if !self.initialized && method != "initialize" && method != "ping" {
            return Ok(Some(err(
                id,
                -32600,
                "not initialized: send initialize first",
            )));
        }
        Ok(Some(match method {
            "initialize" => {
                let asked = params["protocolVersion"]
                    .as_str()
                    .unwrap_or(PROTOCOL_VERSION);
                let version = if asked == PROTOCOL_VERSION || ALSO.contains(&asked) {
                    asked
                } else {
                    PROTOCOL_VERSION
                };
                self.initialized = true;
                self.elicit = version == PROTOCOL_VERSION
                    && params["capabilities"]["elicitation"].is_object();
                ok(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "keel", "title": "Keel", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let Some(op) = op_for_tool(name) else {
                    return Ok(Some(err(id, -32602, &format!("unknown tool: {name}"))));
                };
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = match op.name {
                    "execute" => self.execute(args)?,
                    name => self.backend.call(name, args),
                };
                self.remember(&result);
                ok(id, call_result(result))
            }
            "resources/list" => ok(id, json!({"resources": []})),
            "prompts/list" => ok(id, json!({"prompts": []})),
            _ => err(id, -32601, &format!("method not found: {method}")),
        }))
    }

    /// Keeps the previews (also the fresh one a PLAN_CHANGED error carries).
    fn remember(&mut self, result: &crate::Result<Value>) {
        let v = match result {
            Ok(v) => v,
            Err(e) => match &e.data {
                Some(data) => data,
                None => return,
            },
        };
        if let Ok(p) = serde_json::from_value::<PlanPreview>(v.clone()) {
            let now = now_secs();
            self.previews.retain(|_, p| p.expires_at >= now);
            self.previews.insert(p.plan_id.clone(), p);
        }
    }

    /// `execute`, once a person confirmed the plan (see the module docs).
    fn execute(&mut self, mut args: Value) -> io::Result<crate::Result<Value>> {
        let summary = args.as_object_mut().and_then(|m| m.remove("summary"));
        let plan_id = args["plan_id"].as_str().unwrap_or_default().to_owned();
        if self.asking {
            return Ok(refused(
                "another plan is waiting for the user's confirmation: execute one at a time".into(),
            ));
        }
        let Some(preview) = self.previews.get(&plan_id).cloned() else {
            return Ok(refused(format!(
                "no preview of plan {plan_id} in this session: execute applies only plans \
                 previewed here (preview it again)"
            )));
        };
        if args["input_hash"].as_str() != Some(preview.input_hash.as_str()) {
            return Ok(Err(ApiError::new(
                ApiError::PLAN_MISMATCH,
                "the input hash does not match the previewed input: refused",
            )));
        }
        if self.elicit {
            if !self.ask(&preview)? {
                return Ok(refused(format!(
                    "the user did not confirm plan {plan_id}: nothing was changed"
                )));
            }
        } else if !self.opts.allow_execute {
            return Ok(refused(
                "this MCP client cannot ask the user to confirm a plan (no elicitation \
                 support), so Keel does not execute plans through it. Show the user the \
                 preview; they can apply it with `keel execute <plan_id> --hash \
                 <input_hash>`, or start the server with `keel mcp --allow-execute`."
                    .into(),
            ));
        } else if summary.as_ref().and_then(Value::as_str) != Some(preview.summary.as_str()) {
            return Ok(refused(format!(
                "pass the preview's summary as `summary`, exactly, so the approval shows \
                 what runs: {:?}",
                preview.summary
            )));
        }
        let result = self.backend.call("execute", args);
        if result.is_ok() {
            self.previews.remove(&plan_id);
        }
        Ok(result)
    }

    /// Asks the user (`elicitation/create`) to confirm `p`; true only on accept. Messages
    /// that arrive meanwhile are answered (but no second confirmation starts).
    fn ask(&mut self, p: &PlanPreview) -> io::Result<bool> {
        self.asked += 1;
        let id = json!(format!("keel-confirm-{}", self.asked));
        let message = format!(
            "Keel: apply this {} plan?\n\n{}\nPlan {} (expires in {} min).",
            p.operation,
            p.describe(PROMPT_CHANGES),
            p.plan_id,
            ((p.expires_at - now_secs()).max(0) + 59) / 60,
        );
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "elicitation/create",
            "params": {
                "message": message,
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "confirm": {
                            "type": "boolean",
                            "title": "Apply",
                            "description": "Apply this plan",
                            "default": true,
                        },
                    },
                    "required": ["confirm"],
                },
            },
        }))?;
        self.asking = true;
        let answer = loop {
            let msg = match self.next_message() {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(answer))) => {
                    self.send(&answer)?;
                    continue;
                }
                Ok(None) => break Ok(false),
                Err(e) => break Err(e),
            };
            if msg.get("method").is_none() && msg.get("id") == Some(&id) {
                let r = &msg["result"];
                break Ok(r["action"] == "accept" && r["content"]["confirm"] == true);
            }
            match self.handle(&msg) {
                Ok(Some(answer)) => self.send(&answer)?,
                Ok(None) => {}
                Err(e) => break Err(e),
            }
        };
        self.asking = false;
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Previews `tags.add` (plan `p<N>`), records what `execute` gets.
    #[derive(Default)]
    struct Fake {
        previews: u32,
        executed: Vec<Value>,
    }

    impl Backend for Fake {
        fn call(&mut self, method: &str, params: Value) -> crate::Result<Value> {
            match method {
                "version" => Ok(json!({"api": 1})),
                "tags.add" => {
                    self.previews += 1;
                    Ok(json!({
                        "plan_id": format!("p{}", self.previews),
                        "input_hash": "h",
                        "operation": "tags.add",
                        "summary": "Tag 1 item(s) with receipts",
                        "changes": [{"action": "tag.add", "path": "D:\\a.pdf"}],
                        "warnings": [{"kind": "creates_tag", "message": "creates the tag receipts"}],
                        "expires_at": now_secs() + 600,
                    }))
                }
                "execute" => {
                    self.executed.push(params.clone());
                    Ok(json!({"plan_id": params["plan_id"], "operation": "tags.add", "result": {}}))
                }
                _ => Err(ApiError::invalid_params(format!("{method} {params}"))),
            }
        }
    }

    fn init(elicit: bool) -> Value {
        let caps = if elicit {
            json!({"elicitation": {}})
        } else {
            json!({})
        };
        json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":caps,"clientInfo":{"name":"t","version":"1"}}})
    }

    fn tool_call(id: i64, name: &str, args: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
    }

    fn exec(id: i64, plan: &str, summary: Option<&str>) -> Value {
        let mut args = json!({"plan_id": plan, "input_hash": "h"});
        if let Some(s) = summary {
            args["summary"] = json!(s);
        }
        tool_call(id, "execute", args)
    }

    fn lines(msgs: &[Value]) -> Vec<u8> {
        msgs.iter()
            .map(|v| v.to_string() + "\n")
            .collect::<String>()
            .into_bytes()
    }

    fn run(backend: &mut Fake, input: &[u8], opts: Options) -> Vec<Value> {
        let mut out = Vec::new();
        serve(backend, input, &mut out, opts).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn by_id(out: &[Value], id: i64) -> &Value {
        out.iter()
            .find(|v| v["id"] == id)
            .unwrap_or_else(|| panic!("no answer {id}: {out:?}"))
    }

    fn text(answer: &Value) -> String {
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn lists_tools_and_dispatches() {
        let input = lines(&[
            init(false),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            tool_call(3, "version", json!({})),
            tool_call(4, "tags_remove", json!({})),
            tool_call(5, "rm_rf", json!({})),
        ]);
        let out = run(&mut Fake::default(), &input, Options::default());
        assert_eq!(out.len(), 5, "no answer to the notification");
        assert_eq!(out[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        let tools = out[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), OPS.len());
        for t in tools {
            let name = t["name"].as_str().unwrap();
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{name}"
            );
            assert_eq!(t["inputSchema"]["type"], "object", "{name}");
        }
        let add = tools.iter().find(|t| t["name"] == "tags_add").unwrap();
        assert!(add["description"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("returns a preview; call execute with the plan id to apply"));
        let execute = tools.iter().find(|t| t["name"] == "execute").unwrap();
        assert_eq!(
            execute["inputSchema"]["properties"]["summary"]["type"],
            "string"
        );
        assert_eq!(out[2]["result"]["structuredContent"]["api"], 1);
        assert_eq!(out[3]["result"]["isError"], true);
        assert_eq!(out[4]["error"]["code"], -32602);
    }

    #[test]
    fn execute_asks_the_user_through_elicitation() {
        let mut input = lines(&[
            init(true),
            tool_call(2, "tags_add", json!({})),
            exec(3, "p1", None),
        ]);
        // While the question is open: a ping is answered, a second execute refused.
        input.extend(lines(&[
            json!({"jsonrpc":"2.0","id":30,"method":"ping"}),
            exec(31, "p1", None),
            json!({"jsonrpc":"2.0","id":"keel-confirm-1","result":{"action":"accept","content":{"confirm":true}}}),
            tool_call(4, "tags_add", json!({})),
            exec(5, "p2", None),
            json!({"jsonrpc":"2.0","id":"keel-confirm-2","result":{"action":"decline"}}),
            exec(6, "p1", None),
        ]));
        let mut fake = Fake::default();
        let out = run(&mut fake, &input, Options::default());
        let asks: Vec<&Value> = out
            .iter()
            .filter(|v| v["method"] == "elicitation/create")
            .collect();
        assert_eq!(asks.len(), 2, "{out:?}");
        let message = asks[0]["params"]["message"].as_str().unwrap();
        for shown in [
            "Tag 1 item(s) with receipts",
            "D:\\a.pdf",
            "creates the tag receipts",
        ] {
            assert!(message.contains(shown), "{shown} not in {message}");
        }
        assert_eq!(asks[0]["id"], "keel-confirm-1");
        assert_eq!(
            asks[0]["params"]["requestedSchema"]["properties"]["confirm"]["type"],
            "boolean"
        );
        assert_eq!(by_id(&out, 30)["result"], json!({}));
        assert!(text(by_id(&out, 31)).contains("waiting for the user"));
        assert_eq!(by_id(&out, 3)["result"]["isError"], false, "accepted");
        assert_eq!(by_id(&out, 5)["result"]["isError"], true, "declined");
        assert!(text(by_id(&out, 5)).contains("did not confirm"));
        assert!(text(by_id(&out, 6)).contains("no preview"), "executed once");
        assert_eq!(
            fake.executed,
            vec![json!({"plan_id": "p1", "input_hash": "h"})]
        );
    }

    #[test]
    fn without_elicitation_execute_is_refused() {
        let input = lines(&[
            init(false),
            tool_call(2, "tags_add", json!({})),
            exec(3, "p1", Some("Tag 1 item(s) with receipts")),
        ]);
        let mut fake = Fake::default();
        let out = run(&mut fake, &input, Options::default());
        assert_eq!(by_id(&out, 3)["result"]["isError"], true);
        assert!(text(by_id(&out, 3)).contains("--allow-execute"));
        assert!(fake.executed.is_empty());
        assert!(!out.iter().any(|v| v["method"] == "elicitation/create"));
    }

    #[test]
    fn allow_execute_needs_the_exact_summary() {
        let input = lines(&[
            init(false),
            tool_call(2, "tags_add", json!({})),
            exec(3, "p1", None),
            exec(4, "p1", Some("Tag 9 item(s) with receipts")),
            tool_call(
                5,
                "execute",
                json!({"plan_id": "p1", "input_hash": "other", "summary": "Tag 1 item(s) with receipts"}),
            ),
            exec(6, "unknown", Some("Tag 1 item(s) with receipts")),
            exec(7, "p1", Some("Tag 1 item(s) with receipts")),
        ]);
        let mut fake = Fake::default();
        let out = run(
            &mut fake,
            &input,
            Options {
                allow_execute: true,
            },
        );
        for refused in [3, 4, 5, 6] {
            assert_eq!(by_id(&out, refused)["result"]["isError"], true, "{refused}");
        }
        assert!(text(by_id(&out, 3)).contains("summary"));
        assert_eq!(by_id(&out, 7)["result"]["isError"], false);
        // `summary` stays in the MCP layer.
        assert_eq!(
            fake.executed,
            vec![json!({"plan_id": "p1", "input_hash": "h"})]
        );
    }

    #[test]
    fn nothing_but_ping_before_initialize() {
        let input = lines(&[
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            tool_call(2, "version", json!({})),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
            init(false),
            tool_call(4, "version", json!({})),
        ]);
        let out = run(&mut Fake::default(), &input, Options::default());
        assert_eq!(by_id(&out, 1)["result"], json!({}));
        assert_eq!(by_id(&out, 2)["error"]["code"], -32600);
        assert_eq!(by_id(&out, 3)["error"]["code"], -32600);
        assert_eq!(by_id(&out, 4)["result"]["isError"], false);
    }

    #[test]
    fn bad_lines_are_answered_and_the_server_goes_on() {
        let mut input = lines(&[init(false)]);
        input.extend_from_slice(b"\xff\xfe{\"not utf-8\"\n");
        input.extend(vec![b' '; rpc::MAX_REQUEST + 10]);
        input.extend_from_slice(b"\n");
        input.extend(lines(&[json!({"jsonrpc":"2.0","id":9,"method":"ping"})]));
        let out = run(&mut Fake::default(), &input, Options::default());
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(out[1]["error"]["code"], -32700);
        assert_eq!(out[2]["error"]["code"], -32600);
        assert!(out[2]["error"]["message"].as_str().unwrap().contains("MiB"));
        assert_eq!(by_id(&out, 9)["result"], json!({}));
    }
}
