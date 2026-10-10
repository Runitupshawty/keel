//! A JSON-RPC client of `keel-daemon` over its local socket.

use crate::error::{ApiError, Result};
use crate::socket::{self, Conn};
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::Duration;

/// How long `connect` waits for a busy daemon.
pub const CONNECT_WAIT: Duration = Duration::from_secs(3);
/// Default per-request timeout.
pub const REQUEST_WAIT: Duration = Duration::from_secs(120);
/// Longest answer read.
const MAX_RESPONSE: u64 = 256 * 1024 * 1024;

pub struct Client {
    conn: Option<BufReader<Box<dyn Conn>>>,
    next: u64,
    pub timeout: Duration,
    /// Notifications that arrived while waiting for answers (after `subscribe`).
    pub notifications: Vec<Value>,
}

impl Client {
    /// Connects to the daemon listening on `name` (`HostConfig::socket_name`).
    pub fn connect(name: &str) -> io::Result<Client> {
        let (conn, _) = socket::connect(name, CONNECT_WAIT)?;
        Ok(Self::over(conn))
    }

    pub fn over(conn: Box<dyn Conn>) -> Client {
        Client {
            conn: Some(BufReader::new(conn)),
            next: 1,
            timeout: REQUEST_WAIT,
            notifications: Vec::new(),
        }
    }

    /// Sends `line` (a newline is added) and reads lines until `done` accepts one, within
    /// `timeout`; other lines are kept as notifications. A timeout drops the connection.
    fn exchange(
        &mut self,
        line: Vec<u8>,
        done: impl Fn(&Value) -> bool + Send + 'static,
    ) -> io::Result<Value> {
        let mut conn = self
            .conn
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "connection lost"))?;
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let result = (|| -> io::Result<(Value, Vec<Value>)> {
                let w = conn.get_mut();
                w.write_all(&line)?;
                w.write_all(b"\n")?;
                w.flush()?;
                let mut notes = Vec::new();
                loop {
                    let mut buf = Vec::new();
                    (&mut conn).take(MAX_RESPONSE).read_until(b'\n', &mut buf)?;
                    if !buf.ends_with(b"\n") {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the daemon closed the connection",
                        ));
                    }
                    let v: Value = serde_json::from_slice(&buf)?;
                    if done(&v) {
                        return Ok((v, notes));
                    }
                    notes.push(v);
                }
            })();
            let _ = tx.send((conn, result));
        });
        match rx.recv_timeout(self.timeout) {
            Ok((conn, result)) => {
                self.conn = Some(conn);
                let (v, notes) = result?;
                self.notifications.extend(notes);
                Ok(v)
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the daemon did not answer in time",
            )),
        }
    }

    /// Sends a raw message and returns the answer with the same `id` (tests, tools).
    pub fn raw(&mut self, line: Vec<u8>, id: Value) -> io::Result<Value> {
        self.exchange(line, move |v| v.get("id") == Some(&id))
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let line = serde_json::to_vec(&req).map_err(|e| ApiError::failed(e.to_string()))?;
        let answer = self.raw(line, json!(id)).map_err(|e| {
            let code = match e.kind() {
                io::ErrorKind::TimedOut => ApiError::TIMEOUT,
                _ => ApiError::FAILED,
            };
            ApiError::new(code, format!("keel-daemon: {e}"))
        })?;
        if let Some(err) = answer.get("error") {
            return Err(serde_json::from_value(err.clone())
                .unwrap_or_else(|_| ApiError::failed(err.to_string())));
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Waits up to `wait` for the next notification (a timeout drops the connection).
    pub fn next_notification(&mut self, wait: Duration) -> io::Result<Value> {
        if !self.notifications.is_empty() {
            return Ok(self.notifications.remove(0));
        }
        let mut conn = self
            .conn
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "connection lost"))?;
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let r = (&mut conn).take(MAX_RESPONSE).read_until(b'\n', &mut buf);
            let _ = tx.send((conn, r.map(|_| buf)));
        });
        let (conn, buf) = rx
            .recv_timeout(wait)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no notification"))?;
        self.conn = Some(conn);
        Ok(serde_json::from_slice(&buf?)?)
    }
}

impl crate::Backend for Client {
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        Client::call(self, method, params)
    }
}
