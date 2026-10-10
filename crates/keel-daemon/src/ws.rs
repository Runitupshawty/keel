//! JSON-RPC over a WebSocket (`--ws`): off by default, loopback only unless
//! `--ws-allow-remote`, and every connection must send `Authorization: Bearer <token>`
//! with the token from `<config dir>/daemon.token` (owner-only: `keel_api::private`).
//! Browsers cannot set that header on a WebSocket, so web pages cannot connect. No TLS:
//! a remote bind should sit behind a TLS proxy or a private network. At most
//! `MAX_CONNECTIONS` connections are served at once (more are closed at once), and the
//! handshake must be over within `HANDSHAKE_WAIT`.

use crate::server::{Session, Shared};
use keel_api::rpc;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::Message;

/// The whole handshake (the token check included) must be over within this.
pub(crate) const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);
/// Connections served at once, handshakes included.
pub(crate) const MAX_CONNECTIONS: usize = 64;
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    keel_api::private::write(path, token.as_bytes())?;
    Ok(token)
}

/// One of the `MAX_CONNECTIONS`; given back on drop.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(count: &Arc<AtomicUsize>) -> Option<Slot> {
        let slot = Slot(count.clone());
        (count.fetch_add(1, Ordering::AcqRel) < MAX_CONNECTIONS).then_some(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn same(a: &[u8], b: &[u8]) -> bool {
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
    let count = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("keel-daemon-ws".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if s.stop.load(Ordering::Acquire) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let Some(slot) = Slot::take(&count) else {
                    tracing::debug!("websocket: over {MAX_CONNECTIONS} connections, dropped");
                    continue; // dropping `stream` closes it
                };
                let (s, token) = (s.clone(), token.clone());
                let _ = std::thread::Builder::new()
                    .name("keel-daemon-ws-client".into())
                    .spawn(move || {
                        let _slot = slot;
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
    // Non-blocking until the handshake is over, so its deadline holds however slowly the
    // client sends (a per-read timeout restarts with every byte).
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + HANDSHAKE_WAIT;
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
    let config = WebSocketConfig::default()
        .max_message_size(Some(rpc::MAX_REQUEST))
        .max_frame_size(Some(rpc::MAX_REQUEST));
    let mut shake = tungstenite::accept_hdr_with_config(stream, check, Some(config));
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
    ws.get_ref().set_nonblocking(false)?;
    ws.get_ref().set_read_timeout(Some(POLL))?;
    ws.get_ref()
        .set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut session = Session::default();
    loop {
        if shared.stop.load(Ordering::Acquire) {
            let _ = ws.close(None);
            return Ok(());
        }
        let msg = match ws.read() {
            Ok(Message::Text(t)) => t.as_bytes().to_vec(),
            Ok(Message::Binary(b)) => b.to_vec(),
            Ok(Message::Close(_)) => return Ok(()),
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
            Err(e) => return Err(e.into()),
        };
        if let Some(answer) = rpc::handle(&msg, &mut |req| shared.dispatch(req, &mut session)) {
            ws.send(Message::text(answer.to_string()))?;
        }
    }
}
