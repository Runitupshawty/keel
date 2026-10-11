//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

use serde_json::{json, Value};

/// JSON-RPC lines as the daemon socket reads them (`rpc::read_line`, `rpc::handle`), each
/// request's params decoded by its operation (an unknown method by some operation).
pub fn rpc(data: &[u8]) {
    let mut input = data;
    while let Ok(line) = crate::rpc::read_line(&mut input, || Ok(())) {
        let crate::rpc::Line::Line(line) = line else {
            break;
        };
        let _ = crate::rpc::handle(&line, &mut |req| {
            let op = crate::find(&req.method)
                .unwrap_or(&crate::OPS[req.method.len() % crate::OPS.len()]);
            (op.check)(req.params.clone())
        });
    }
}

/// Answers every call; previews for mutating tools, so `execute` and its confirmation run.
struct Echo(u32);
impl crate::Backend for Echo {
    fn call(&mut self, method: &str, params: Value) -> crate::Result<Value> {
        self.0 += 1;
        match crate::find(method) {
            Some(op) if op.previewed() => Ok(json!({
                "plan_id": format!("p{}", self.0),
                "input_hash": "h",
                "operation": method,
                "summary": "s",
                "changes": [{"action": "tag.add", "path": "a"}],
                "warnings": [],
                "expires_at": i64::MAX,
            })),
            _ => Ok(params),
        }
    }
}

/// An MCP stdio session (`keel mcp`), with and without `--allow-execute`.
pub fn mcp(data: &[u8]) {
    for allow_execute in [false, true] {
        let opts = crate::mcp::Options { allow_execute };
        let _ = crate::mcp::serve(&mut Echo(0), data, std::io::sink(), opts);
    }
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_rpc() {
        run("rpc", super::rpc);
    }
    #[test]
    fn fuzz_mcp() {
        run("mcp", super::mcp);
    }
}
