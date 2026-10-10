//! JSON-RPC 2.0 framing: one JSON message per line (local socket) or per WebSocket text
//! message. Batches are answered as batches; notifications (no `id`) get no answer.

use crate::error::{ApiError, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead};

/// Longest request accepted (bytes, without the newline).
pub const MAX_REQUEST: usize = 16 * 1024 * 1024;

/// A request, checked: `id` is None for a notification.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

fn request(v: Value) -> std::result::Result<Request, (Value, ApiError)> {
    let invalid = |id: Value, why: &str| {
        Err((
            id,
            ApiError::new(ApiError::INVALID_REQUEST, format!("invalid request: {why}")),
        ))
    };
    let Value::Object(mut map) = v else {
        return invalid(Value::Null, "not an object");
    };
    let id = map.remove("id");
    let shown = id.clone().unwrap_or(Value::Null);
    if !matches!(
        id,
        None | Some(Value::Null | Value::String(_) | Value::Number(_))
    ) {
        return invalid(Value::Null, "id must be a string or a number");
    }
    if map.get("jsonrpc") != Some(&json!("2.0")) {
        return invalid(shown, "jsonrpc must be \"2.0\"");
    }
    let Some(Value::String(method)) = map.remove("method") else {
        return invalid(shown, "method must be a string");
    };
    let params = map.remove("params").unwrap_or(Value::Null);
    if !matches!(params, Value::Null | Value::Object(_)) {
        return Err((
            shown,
            ApiError::invalid_params("params must be an object (by-name)"),
        ));
    }
    Ok(Request { id, method, params })
}

pub fn response(id: Value, result: Result<Value>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_value()}),
    }
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Parses `msg` and answers it through `dispatch`. None when nothing is to be sent back
/// (only notifications).
pub fn handle(msg: &[u8], dispatch: &mut dyn FnMut(&Request) -> Result<Value>) -> Option<Value> {
    let v: Value = match serde_json::from_slice(msg) {
        Ok(v) => v,
        Err(e) => {
            return Some(response(
                Value::Null,
                Err(ApiError::new(ApiError::PARSE, format!("parse error: {e}"))),
            ))
        }
    };
    let mut one = |v: Value| match request(v) {
        Ok(req) => {
            let result = dispatch(&req);
            req.id.map(|id| response(id, result))
        }
        Err((id, e)) => Some(response(id, Err(e))),
    };
    match v {
        Value::Array(items) if items.is_empty() => Some(response(
            Value::Null,
            Err(ApiError::new(ApiError::INVALID_REQUEST, "empty batch")),
        )),
        Value::Array(items) => {
            let out: Vec<Value> = items.into_iter().filter_map(&mut one).collect();
            (!out.is_empty()).then_some(Value::Array(out))
        }
        v => one(v),
    }
}

/// What [`read_line`] read.
#[derive(Debug, PartialEq)]
pub enum Line {
    /// One message, without its newline.
    Line(Vec<u8>),
    /// A line over [`MAX_REQUEST`], read on to its end and dropped.
    TooLarge,
    /// The input ended (mid-line too).
    Eof,
}

/// Reads one `\n`-terminated message of at most [`MAX_REQUEST`] bytes; never holds more
/// than that in memory. `before_read` runs before every read from `reader` (a deadline
/// check can fail it).
pub fn read_line(
    reader: &mut impl BufRead,
    mut before_read: impl FnMut() -> io::Result<()>,
) -> io::Result<Line> {
    let mut line = Vec::new();
    let mut over = false;
    loop {
        before_read()?;
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(Line::Eof);
        }
        let (end, used) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i, i + 1),
            None => (buf.len(), buf.len()),
        };
        if !over && line.len() + end > MAX_REQUEST {
            over = true;
            line = Vec::new();
        }
        if !over {
            line.extend_from_slice(&buf[..end]);
        }
        reader.consume(used);
        if used > end {
            return Ok(if over {
                Line::TooLarge
            } else {
                Line::Line(line)
            });
        }
    }
}

/// The error answer to a request over [`MAX_REQUEST`].
pub fn too_large() -> Value {
    response(
        Value::Null,
        Err(ApiError::new(
            ApiError::INVALID_REQUEST,
            format!("request over {} MiB refused", MAX_REQUEST >> 20),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(msg: &str) -> Option<Value> {
        handle(msg.as_bytes(), &mut |r| match r.method.as_str() {
            "echo" => Ok(r.params.clone()),
            _ => Err(ApiError::new(ApiError::METHOD_NOT_FOUND, "no")),
        })
    }

    #[test]
    fn reads_lines_within_the_limit() {
        let big = vec![b'x'; MAX_REQUEST + 1];
        let mut input: Vec<u8> = b"one\n\xff\xfe\n".to_vec();
        input.extend_from_slice(&big);
        input.extend_from_slice(b"\ntwo\npartial");
        let mut r = io::BufReader::with_capacity(1024, &input[..]);
        let mut next = || read_line(&mut r, || Ok(())).unwrap();
        assert_eq!(next(), Line::Line(b"one".to_vec()));
        assert_eq!(next(), Line::Line(b"\xff\xfe".to_vec()));
        assert_eq!(next(), Line::TooLarge);
        assert_eq!(next(), Line::Line(b"two".to_vec()));
        assert_eq!(next(), Line::Eof);
        let exact = [vec![b'y'; MAX_REQUEST], b"\n".to_vec()].concat();
        let got = read_line(&mut &exact[..], || Ok(())).unwrap();
        assert_eq!(got, Line::Line(vec![b'y'; MAX_REQUEST]));
    }

    #[test]
    fn requests_notifications_batches_and_errors() {
        let out = run(r#"{"jsonrpc":"2.0","id":7,"method":"echo","params":{"a":1}}"#).unwrap();
        assert_eq!(out, json!({"jsonrpc":"2.0","id":7,"result":{"a":1}}));
        assert_eq!(run(r#"{"jsonrpc":"2.0","method":"echo"}"#), None);
        let out = run(r#"[{"jsonrpc":"2.0","id":"a","method":"echo"},{"jsonrpc":"2.0","method":"x"},{"jsonrpc":"2.0","id":2,"method":"x"}]"#).unwrap();
        assert_eq!(out.as_array().unwrap().len(), 2);
        assert_eq!(out[1]["error"]["code"], ApiError::METHOD_NOT_FOUND);
        assert_eq!(run("{nope").unwrap()["error"]["code"], ApiError::PARSE);
        assert_eq!(
            run(r#"{"id":1,"method":"echo"}"#).unwrap()["error"]["code"],
            ApiError::INVALID_REQUEST
        );
        assert_eq!(
            run(r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":[1]}"#).unwrap()["error"]
                ["code"],
            ApiError::INVALID_PARAMS
        );
        assert_eq!(
            run("[]").unwrap()["error"]["code"],
            ApiError::INVALID_REQUEST
        );
    }
}
