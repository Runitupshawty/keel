//! JSON-RPC over a WebSocket (`--ws`): off by default, loopback only unless
//! `--ws-allow-remote`, and every connection must send `Authorization: Bearer <token>`
//! with the token from `<config dir>/daemon.token` (created owner-only on first use).
//! Browsers cannot set that header on a WebSocket, so web pages cannot connect. No TLS:
//! a remote bind should sit behind a TLS proxy or a private network.

use crate::server::{Session, Shared};
use keel_api::{rpc, ApiError};
use serde_json::{json, Value};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

/// The handshake must finish within this.
pub(crate) const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);
/// A browser connection must authenticate within this.
const AUTH_WAIT: Duration = Duration::from_secs(10);
/// How often an idle connection checks for notifications.
const POLL: Duration = Duration::from_millis(100);

/// The bearer token in `path`, created (32 random bytes, hex, owner-only) when missing.
pub fn token(path: &Path) -> io::Result<String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim();
        if t.len() >= 32 {
            return Ok(t.to_owned());
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    io::Write::write_all(&mut opts.open(path)?, token.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Binds `addr` and serves WebSocket clients on their own threads; returns the bound
/// address.
pub(crate) fn serve(
    shared: &Arc<Shared>,
    addr: SocketAddr,
    token_path: &Path,
) -> anyhow::Result<SocketAddr> {
    let token = token(token_path)?;
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let s = shared.clone();
    std::thread::Builder::new()
        .name("keel-daemon-ws".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if s.stop.load(Ordering::Acquire) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (s, token) = (s.clone(), token.clone());
                let _ = std::thread::Builder::new()
                    .name("keel-daemon-ws-client".into())
                    .spawn(move || {
                        if let Err(e) = client(&s, stream, &token) {
                            tracing::debug!("websocket client: {e}");
                        }
                    });
            }
        })?;
    tracing::info!("JSON-RPC over WebSocket on ws://{bound}");
    Ok(bound)
}

// tungstenite's handshake callback returns its (large) http response as the error.
#[allow(clippy::result_large_err)]
fn client(shared: &Arc<Shared>, stream: TcpStream, token: &str) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_WAIT))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let expected = format!("Bearer {token}");
    let check = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let given = req
            .headers()
            .get("authorization")
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        if same(given, expected.as_bytes()) {
            Ok(resp)
        } else {
            let mut refused = ErrorResponse::new(Some("a valid bearer token is required".into()));
            *refused.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
            Err(refused)
        }
    };
    let config = config();
    let mut ws = tungstenite::accept_hdr_with_config(stream, check, Some(config))?;
    run(shared, &mut ws, None)
}

/// The settings both WebSocket endpoints use.
pub(crate) fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(rpc::MAX_REQUEST))
        .max_frame_size(Some(rpc::MAX_REQUEST))
}

/// Answers JSON-RPC on `ws` until it closes. `need_auth`: the first message must be
/// `auth` with this token (else the answer is an error and the connection closes), and it
/// must come within `AUTH_WAIT`.
pub(crate) fn run(
    shared: &Arc<Shared>,
    ws: &mut WebSocket<TcpStream>,
    mut need_auth: Option<&str>,
) -> anyhow::Result<()> {
    ws.get_ref().set_read_timeout(Some(POLL))?;
    let auth_by = Instant::now() + AUTH_WAIT;
    let mut session = Session::default();
    let result = loop {
        if shared.stop.load(Ordering::Acquire) || (need_auth.is_some() && Instant::now() > auth_by)
        {
            let _ = ws.close(None);
            let _ = ws.flush();
            break Ok(());
        }
        let msg = match ws.read() {
            Ok(Message::Text(t)) => t.as_bytes().to_vec(),
            Ok(Message::Binary(b)) => b.to_vec(),
            Ok(Message::Close(_)) => break Ok(()),
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if let Some((_, rx)) = &session.sub {
                    for note in rx.try_iter().collect::<Vec<_>>() {
                        ws.send(Message::text(note.to_string()))?;
                    }
                }
                ws.flush().or_else(|e| match e {
                    tungstenite::Error::Io(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
                    e => Err(e),
                })?;
                continue;
            }
            Err(e) => break Err(e.into()),
        };
        if let Some(token) = need_auth {
            let (answer, ok) = auth(&msg, token);
            ws.send(Message::text(answer.to_string()))?;
            if !ok {
                let _ = ws.close(None);
                let _ = ws.flush();
                break Ok(());
            }
            need_auth = None;
            continue;
        }
        if let Some(answer) = rpc::handle(&msg, &mut |req| shared.dispatch(req, &mut session)) {
            ws.send(Message::text(answer.to_string()))?;
        }
    };
    if let Some((id, _)) = session.sub.take() {
        shared.hub.unsubscribe(id);
    }
    result
}

/// Checks the first message of a browser connection:
/// `{"jsonrpc":"2.0","id":1,"method":"auth","params":{"token":"…"}}`.
fn auth(msg: &[u8], token: &str) -> (Value, bool) {
    let v: Value = serde_json::from_slice(msg).unwrap_or_default();
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let given = match v.get("method").and_then(Value::as_str) {
        Some("auth") => v["params"]["token"].as_str().unwrap_or_default(),
        _ => "",
    };
    if same(given.as_bytes(), token.as_bytes()) {
        (rpc::response(id, Ok(json!({"ok": true}))), true)
    } else {
        let e = ApiError::new(
            ApiError::UNAUTHORIZED,
            "the first message must be auth with the daemon token",
        );
        (rpc::response(id, Err(e)), false)
    }
}
