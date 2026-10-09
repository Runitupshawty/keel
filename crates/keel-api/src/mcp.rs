//! An MCP server (Model Context Protocol 2025-06-18) over stdio: newline-delimited
//! JSON-RPC on stdin/stdout. Every registered operation is a tool with its JSON schema;
//! `tools/call` runs the same handlers as JSON-RPC and the CLI. Mutating tools return a
//! preview; only `execute` with that preview's plan id and input hash applies it, so a
//! model cannot change anything without a preview the client has seen.

use crate::{Backend, Operation, OPS};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Older revisions whose tools subset is the same.
const ALSO: &[&str] = &["2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Keel file manager tools. Reads (search, list, stat, \
sources_list, jobs_list, duplicates, ...) act at once. Every mutating tool (plan for \
copy/move/delete/rename, tags_*, sources_add, shares_grant, ...) only returns a preview \
with a plan_id and input_hash: show the preview to the user, and only after they confirm \
call execute with that plan_id and input_hash. File operations run as jobs: follow them \
with jobs_info. Paths are absolute.";

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
            "destructiveHint": op.name == "execute" || op.name == "sources.remove" || op.name == "shares.revoke",
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

/// Answers one message; None for notifications and responses.
pub fn handle(backend: &mut dyn Backend, msg: &Value) -> Option<Value> {
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // A response to something we never sent, or garbage with an id.
        return msg
            .get("id")
            .filter(|_| msg.get("result").is_none() && msg.get("error").is_none())
            .map(|id| err(id.clone(), -32600, "invalid request"));
    };
    let id = msg.get("id").cloned()?; // notifications (initialized, cancelled) need no answer
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    Some(match method {
        "initialize" => {
            let asked = params["protocolVersion"]
                .as_str()
                .unwrap_or(PROTOCOL_VERSION);
            let version = if asked == PROTOCOL_VERSION || ALSO.contains(&asked) {
                asked
            } else {
                PROTOCOL_VERSION
            };
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
                return Some(err(id, -32602, &format!("unknown tool: {name}")));
            };
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            ok(id, call_result(backend.call(op.name, args)))
        }
        "resources/list" => ok(id, json!({"resources": []})),
        "prompts/list" => ok(id, json!({"prompts": []})),
        _ => err(id, -32601, &format!("method not found: {method}")),
    })
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

/// Serves MCP on `input` / `output` until `input` ends.
pub fn serve(
    backend: &mut dyn Backend,
    input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(_)) => Some(err(Value::Null, -32600, "batches are not supported")),
            Ok(msg) => handle(backend, &msg),
            Err(e) => Some(err(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(answer) = answer {
            serde_json::to_writer(&mut output, &answer)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiError;

    struct Echo;
    impl Backend for Echo {
        fn call(&mut self, method: &str, params: Value) -> crate::Result<Value> {
            match method {
                "version" => Ok(json!({"api": 1})),
                _ => Err(ApiError::invalid_params(format!("{method} {params}"))),
            }
        }
    }

    #[test]
    fn lists_tools_and_dispatches() {
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"version","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tags_add","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"rm_rf","arguments":{}}}),
        ]
        .iter()
        .map(|v| v.to_string() + "\n")
        .collect::<String>();
        let mut out = Vec::new();
        serve(&mut Echo, input.as_bytes(), &mut out).unwrap();
        let out: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
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
        assert_eq!(out[2]["result"]["structuredContent"]["api"], 1);
        assert_eq!(out[3]["result"]["isError"], true);
        assert_eq!(out[4]["error"]["code"], -32602);
    }
}
