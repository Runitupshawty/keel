//! JSON-RPC over a WebSocket (`--ws`): off by default, loopback only unless
//! `--ws-allow-remote`, and every connection must send `Authorization: Bearer <token>`
//! with the token from `<config dir>/daemon.token` (owner-only: `keel_api::private`).
//! Browsers cannot set that header on a WebSocket, so web pages cannot connect here
//! (`--web` serves them, with the token as the first message: [`run`]). No TLS: a remote
//! bind should sit behind a TLS proxy or a private network. At most `MAX_CONNECTIONS`
//! connections are served at once (more are closed at once), and the handshake must be
//! over within `HANDSHAKE_WAIT`. The token file is read for each connection: after
//! `keel daemon rotate-token` new connections need the new token and sessions signed in
//! with the old one are closed (error -32007).

use crate::server::{Session, Shared};
use keel_api::{rpc, ApiError};
use serde_json::{json, Value};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

/// The whole handshake (the token check included) must be over within this; `--web`
/// uses it for the request head.
pub(crate) const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);
/// Connections served at once by one listener (`--ws` or `--web`), handshakes included.
pub(crate) const MAX_CONNECTIONS: usize = 64;
/// A browser connection must authenticate within this.
const AUTH_WAIT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(10)
};
/// Largest message (and frame) before `auth`: a stranger cannot make the daemon hold
/// 16 MiB per connection.
pub(crate) const AUTH_MAX: usize = 4 << 10;
/// How often a signed-in connection checks that the token was not rotated.
const TOKEN_CHECK: Duration = Duration::from_secs(2);
/// Each write must go through within this.
pub(crate) const WRITE_WAIT: Duration = Duration::from_secs(30);
/// How often an idle connection checks for notifications.
const POLL: Duration = Duration::from_millis(100);

/// The bearer token in `path`: 32 random bytes, hex, in an owner-only file. A missing
/// token, or one in a file anybody else may read or change (it could have been planted
/// or read), is replaced by a new one.
pub fn token(path: &Path) -> io::Result<String> {
    match keel_api::private::read(path) {
        Ok(Some(t)) => {
            let t = String::from_utf8_lossy(&t);
            if t.trim().len() >= 32 {
                return Ok(t.trim().to_owned());
            }
        }
        Ok(None) => tracing::warn!(
            "{} may be read or changed by others: replacing the token",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    keel_api::config::new_token(path)
}

/// The daemon token file, read for each connection (so a rotation takes effect at once).
#[derive(Clone)]
pub(crate) struct Token(pub(crate) Arc<PathBuf>);

impl Token {
    pub(crate) fn current(&self) -> io::Result<String> {
        token(&self.0)
    }
}

/// One of `cap` places (connections of a listener, unauthenticated ones, ...); given back
/// on drop.
pub(crate) struct Slot(Arc<AtomicUsize>);

impl Slot {
    pub(crate) fn take(count: &Arc<AtomicUsize>, cap: usize) -> Option<Slot> {
        let slot = Slot(count.clone());
        (count.fetch_add(1, Ordering::AcqRel) < cap).then_some(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
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
    let tokens = Token(Arc::new(token_path.to_owned()));
    tokens.current()?;
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let s = shared.clone();
    let count = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("keel-daemon-ws".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if s.stop.load(Ordering::Acquire) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let Some(slot) = Slot::take(&count, MAX_CONNECTIONS) else {
                    tracing::debug!("websocket: over {MAX_CONNECTIONS} connections, dropped");
                    continue; // dropping `stream` closes it
                };
                let (s, tokens) = (s.clone(), tokens.clone());
                let _ = std::thread::Builder::new()
                    .name("keel-daemon-ws-client".into())
                    .spawn(move || {
                        let _slot = slot;
                        if let Err(e) = client(&s, stream, &tokens) {
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
fn client(shared: &Arc<Shared>, stream: TcpStream, tokens: &Token) -> anyhow::Result<()> {
    // Non-blocking until the handshake is over, so its deadline holds however slowly the
    // client sends (a per-read timeout restarts with every byte).
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + HANDSHAKE_WAIT;
    let token = tokens.current()?;
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
    let mut shake = tungstenite::accept_hdr_with_config(stream, check, Some(config()));
    let mut ws = loop {
        match shake {
            Ok(ws) => break ws,
            Err(HandshakeError::Interrupted(mid)) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("no handshake within {HANDSHAKE_WAIT:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
                shake = mid.handshake();
            }
            Err(HandshakeError::Failure(e)) => return Err(e.into()),
        }
    };
    run(shared, &mut ws, &token, false, tokens, None)
}

/// The settings both WebSocket endpoints use.
pub(crate) fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(rpc::MAX_REQUEST))
        .max_frame_size(Some(rpc::MAX_REQUEST))
}

/// Sends error -32007 (`message`) and closes.
fn refuse(ws: &mut WebSocket<TcpStream>, id: Value, message: &str) {
    let e = ApiError::new(ApiError::UNAUTHORIZED, message);
    let _ = ws.send(Message::text(rpc::response(id, Err(e)).to_string()));
    let _ = ws.close(None);
    let _ = ws.flush();
}

/// Answers JSON-RPC on `ws` (handshake done) until it closes: blocking again, reads poll
/// every `POLL`, writes time out after `WRITE_WAIT`. `token` is the token the connection
/// signs in with; when it is no longer the file's (`keel daemon rotate-token`), the
/// connection is closed with error -32007. `need_auth`: the first message must be `auth`
/// with `token` (else the answer is an error and the connection closes), within
/// `AUTH_WAIT` (else error -32007 and close), and until then a message may be at most
/// `AUTH_MAX` bytes; `pending` (its unauthenticated place) is given back once it signs in.
pub(crate) fn run(
    shared: &Arc<Shared>,
    ws: &mut WebSocket<TcpStream>,
    token: &str,
    mut need_auth: bool,
    tokens: &Token,
    mut pending: Option<Slot>,
) -> anyhow::Result<()> {
    ws.get_ref().set_nonblocking(false)?;
    ws.get_ref().set_read_timeout(Some(POLL))?;
    ws.get_ref().set_write_timeout(Some(WRITE_WAIT))?;
    if need_auth {
        ws.set_config(|c| {
            c.max_message_size = Some(AUTH_MAX);
            c.max_frame_size = Some(AUTH_MAX);
        });
    } else {
        pending = None;
    }
    let auth_by = Instant::now() + AUTH_WAIT;
    let mut checked = Instant::now();
    let mut session = Session::default();
    let result = loop {
        if shared.stop.load(Ordering::Acquire) {
            let _ = ws.close(None);
            let _ = ws.flush();
            break Ok(());
        }
        if need_auth && Instant::now() > auth_by {
            refuse(ws, Value::Null, &format!("no auth within {AUTH_WAIT:?}"));
            break Ok(());
        }
        if !need_auth && checked.elapsed() >= TOKEN_CHECK {
            checked = Instant::now();
            if tokens
                .current()
                .map_or(true, |t| !same(t.as_bytes(), token.as_bytes()))
            {
                refuse(ws, Value::Null, "the daemon token changed: sign in again");
                break Ok(());
            }
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
        if need_auth {
            let (answer, ok) = auth(&msg, token);
            ws.send(Message::text(answer.to_string()))?;
            if !ok {
                let _ = ws.close(None);
                let _ = ws.flush();
                break Ok(());
            }
            need_auth = false;
            pending = None;
            ws.set_config(|c| *c = config());
            continue;
        }
        if let Some(answer) = rpc::handle(&msg, &mut |req| shared.dispatch(req, &mut session)) {
            ws.send(Message::text(answer.to_string()))?;
        }
    };
    drop(pending);
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
