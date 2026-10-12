use crate::server::{Daemon, Options};
use keel_api::client::Client;
use keel_api::config::HostConfig;
use keel_api::types::PlanPreview;
use keel_api::ApiError;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

struct Env {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
    files: tempfile::TempDir,
    cfg: HostConfig,
}

/// Settings and library in temp folders (never the user's own), a unique profile.
fn env() -> Env {
    let (config, data, files) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::write(files.path().join("invoice-2026.pdf"), b"pdf").unwrap();
    std::fs::write(files.path().join("notes.txt"), b"notes").unwrap();
    let profile = format!("test-{}-{}", std::process::id(), rand());
    let cfg = HostConfig::read(&profile, config.path().into(), data.path().into());
    Env {
        config,
        data,
        files,
        cfg,
    }
}

fn rand() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    u64::from_le_bytes(b)
}

fn start(env: &Env, ws: Option<&str>) -> Daemon {
    Daemon::start(Options {
        cfg: env.cfg.clone(),
        ws: ws.map(|a| a.parse().unwrap()),
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .unwrap()
}

fn apply(c: &mut Client, method: &str, params: Value) -> Value {
    let p: PlanPreview = serde_json::from_value(c.call(method, params).unwrap()).unwrap();
    c.call(
        "execute",
        json!({"plan_id": p.plan_id, "input_hash": p.input_hash}),
    )
    .unwrap()["result"]
        .clone()
}

fn wait_job(c: &mut Client, job: i64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let info = c.call("jobs.info", json!({"id": job})).unwrap();
        if ["done", "failed", "cancelled"].contains(&info["status"].as_str().unwrap()) {
            return info;
        }
        assert!(Instant::now() < deadline, "job {job} did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn add_source(c: &mut Client, env: &Env) -> String {
    let root = env.files.path().display().to_string();
    apply(c, "sources.add", json!({ "root": root }))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn serves_the_api_on_the_local_socket() {
    let env = env();
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let v = c.call("version", Value::Null).unwrap();
    assert_eq!(v["library"], "main");
    assert_eq!(v["pid"], std::process::id());
    assert_eq!(v["net"], false);
    c.call("subscribe", Value::Null).unwrap();
    let id = add_source(&mut c, &env);
    let job = apply(&mut c, "sources.index", json!({ "id": id }))["job"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&mut c, job)["status"], "done");
    let hits = c.call("search", json!({"query": "invoice"})).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1, "{hits}");
    // The subscription saw the source change and the job.
    let mut seen = Vec::new();
    while let Ok(n) = c.next_notification(Duration::from_secs(5)) {
        let method = n["method"].as_str().unwrap().to_owned();
        seen.push(method.clone());
        if method == "job.progress" && n["params"]["status"] == "done" {
            break;
        }
    }
    assert!(seen.contains(&"library.changed".to_owned()), "{seen:?}");
    assert!(seen.contains(&"job.progress".to_owned()), "{seen:?}");
    // Unknown methods and devices (net off) are typed errors.
    let mut c = Client::connect(daemon.name()).unwrap();
    assert_eq!(
        c.call("nope", Value::Null).unwrap_err().code,
        ApiError::METHOD_NOT_FOUND
    );
    assert_eq!(
        c.call("devices.list", Value::Null).unwrap_err().code,
        ApiError::NET_DISABLED
    );
    // The daemon serves mounts; without a backend built in, adding one says so.
    assert_eq!(c.call("mounts.list", Value::Null).unwrap(), json!([]));
    if keel_mount::backend().is_none() {
        let e = c
            .call(
                "mounts.add",
                json!({"source": id, "target": "/nonexistent"}),
            )
            .unwrap_err();
        assert_eq!(e.code, ApiError::MOUNTS_UNAVAILABLE, "{e:?}");
        assert!(e.message.contains("--features"), "{e:?}");
    }
}

#[test]
fn subscribers_hear_the_daemon_stop() {
    let env = env();
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let v = c.call("subscribe", Value::Null).unwrap();
    assert!(
        v["subscribed"].to_string().contains("daemon.stopping"),
        "{v}"
    );
    daemon.shutdown();
    let n = c.next_notification(Duration::from_secs(10)).unwrap();
    assert_eq!(n["method"], "daemon.stopping", "{n}");
}

#[test]
fn second_daemon_for_the_profile_exits() {
    let env = env();
    let _daemon = start(&env, None);
    let err = Daemon::start(Options {
        cfg: env.cfg.clone(),
        ws: None,
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .err()
    .unwrap();
    assert!(
        err.to_string().contains("already running for profile"),
        "{err:#}"
    );
}

#[test]
fn reattach_keeps_the_library_open() {
    let env = env();
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let id = add_source(&mut c, &env);
    drop(c);
    std::thread::sleep(Duration::from_millis(100));
    let mut c = Client::connect(daemon.name()).unwrap();
    let sources = c.call("sources.list", Value::Null).unwrap();
    assert_eq!(sources[0]["id"], id);
    // Still held by the daemon.
    assert!(keel_core::Library::open(env.data.path(), "main").is_err());
    // Shutdown on request releases it.
    c.call("daemon.shutdown", Value::Null).unwrap();
    drop(c);
    daemon
        .shutdown_requests()
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    daemon.shutdown();
    drop(daemon);
    let deadline = Instant::now() + Duration::from_secs(10);
    while keel_core::Library::open(env.data.path(), "main").is_err() {
        assert!(Instant::now() < deadline, "library still locked");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn oversized_requests_are_refused() {
    let env = env();
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let mut big = br#"{"jsonrpc":"2.0","id":1,"method":"search","params":{"query":""#.to_vec();
    big.resize(keel_api::rpc::MAX_REQUEST + 10, b'a');
    big.extend_from_slice(br#""}}"#);
    let answer = c.raw(big, Value::Null).unwrap();
    assert_eq!(
        answer["error"]["code"],
        ApiError::INVALID_REQUEST,
        "{answer}"
    );
    // The connection survives (the rest of the line was skipped).
    assert_eq!(c.call("version", Value::Null).unwrap()["api"], 1);
    // Exactly at the limit is fine.
    let mut fits = br#"{"jsonrpc":"2.0","id":2,"method":"version","params":{},"pad":""#.to_vec();
    fits.resize(keel_api::rpc::MAX_REQUEST - 2, b'a');
    fits.extend_from_slice(br#""}"#);
    assert_eq!(fits.len(), keel_api::rpc::MAX_REQUEST);
    let answer = c.raw(fits, json!(2)).unwrap();
    assert_eq!(answer["result"]["api"], 1, "{}", answer["error"]);
}

#[test]
#[allow(clippy::result_large_err)]
fn websocket_needs_the_token() {
    use tungstenite::client::IntoClientRequest;
    let env = env();
    let daemon = start(&env, Some("127.0.0.1:0"));
    let addr = daemon.ws_addr().unwrap();
    let token = std::fs::read_to_string(env.config.path().join("daemon.token")).unwrap();
    assert_eq!(token.len(), 64);
    let connect = |bearer: Option<&str>| {
        let mut req = format!("ws://{addr}/").into_client_request().unwrap();
        if let Some(b) = bearer {
            req.headers_mut()
                .insert("authorization", format!("Bearer {b}").parse().unwrap());
        }
        tungstenite::client(req, std::net::TcpStream::connect(addr).unwrap())
    };
    for bad in [None, Some("wrong"), Some(&token[..63])] {
        match connect(bad) {
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp))) => {
                assert_eq!(resp.status(), 401)
            }
            other => panic!("{bad:?}: {:?}", other.map(|_| ())),
        }
    }
    let (mut ws, _) = connect(Some(&token)).unwrap();
    ws.send(tungstenite::Message::text(
        r#"{"jsonrpc":"2.0","id":1,"method":"version"}"#,
    ))
    .unwrap();
    let answer: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(answer["result"]["library"], "main");
}

#[test]
fn websocket_refuses_remote_binds() {
    let env = env();
    let err = Daemon::start(Options {
        cfg: env.cfg.clone(),
        ws: Some("0.0.0.0:0".parse().unwrap()),
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("--ws-allow-remote"), "{err:#}");
}

/// A raw HTTP GET; returns the status line, headers and body.
fn http_get(addr: std::net::SocketAddr, path: &str, host: &str) -> (String, String, Vec<u8>) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(out[..split].to_vec()).unwrap();
    let (status, headers) = head.split_once("\r\n").unwrap();
    (
        status.to_owned(),
        headers.to_lowercase(),
        out[split + 4..].to_vec(),
    )
}

#[test]
#[allow(clippy::result_large_err)]
fn web_serves_the_client_and_rpc_with_in_band_auth() {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::Message;
    let env = env();
    let daemon = Daemon::start(Options {
        cfg: env.cfg.clone(),
        ws: None,
        web: Some("127.0.0.1:0".parse().unwrap()),
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .unwrap();
    let addr = daemon.web_addr().unwrap();
    let host = addr.to_string();
    let token = std::fs::read_to_string(env.config.path().join("daemon.token")).unwrap();

    // The bundle (or, unbuilt, the page saying how to build it), never cached.
    let (status, headers, body) = http_get(addr, "/", &host);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains("content-type: text/html"), "{headers}");
    assert!(headers.contains("cache-control: no-store"), "{headers}");
    assert!(
        headers.contains("referrer-policy: no-referrer"),
        "{headers}"
    );
    assert!(
        headers.contains("content-security-policy: default-src 'none'"),
        "{headers}"
    );
    assert!(String::from_utf8_lossy(&body).contains("<title>Keel</title>"));
    assert_eq!(
        http_get(addr, "/nope.js", &host).0,
        "HTTP/1.1 404 Not Found"
    );
    // DNS rebinding: a loopback bind answers loopback host names only.
    assert_eq!(
        http_get(addr, "/", "evil.example").0,
        "HTTP/1.1 403 Forbidden"
    );
    // The token is never taken from the address.
    let q = format!("/rpc?token={token}");
    assert_eq!(http_get(addr, &q, &host).0, "HTTP/1.1 400 Bad Request");

    let connect = |origin: Option<&str>| {
        let mut req = format!("ws://{addr}/rpc").into_client_request().unwrap();
        if let Some(o) = origin {
            req.headers_mut().insert("origin", o.parse().unwrap());
        }
        tungstenite::client(req, std::net::TcpStream::connect(addr).unwrap())
    };
    match connect(Some("http://evil.example")) {
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r))) => {
            assert_eq!(r.status(), 403)
        }
        other => panic!("cross-origin: {:?}", other.map(|_| ())),
    }
    let send = |ws: &mut tungstenite::WebSocket<_>, v: Value| -> Value {
        ws.send(Message::text(v.to_string())).unwrap();
        serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap()
    };
    // Anything before auth is refused and the connection closes.
    let (mut ws, _) = connect(Some(&format!("http://{host}"))).unwrap();
    let answer = send(&mut ws, json!({"jsonrpc":"2.0","id":1,"method":"version"}));
    assert_eq!(answer["error"]["code"], ApiError::UNAUTHORIZED);
    assert!(!matches!(ws.read(), Ok(Message::Text(_))));
    let (mut ws, _) = connect(None).unwrap();
    let wrong = json!({"jsonrpc":"2.0","id":1,"method":"auth","params":{"token": "0".repeat(64)}});
    let refused = send(&mut ws, wrong);
    assert_eq!(refused["error"]["code"], ApiError::UNAUTHORIZED);
    // The web client shows the reason to whoever typed the token.
    assert_eq!(refused["error"]["message"], "not the token in daemon.token");

    let (mut ws, _) = connect(Some(&format!("http://{host}"))).unwrap();
    let auth = json!({"jsonrpc":"2.0","id":1,"method":"auth","params":{"token": token}});
    assert_eq!(send(&mut ws, auth)["result"]["ok"], true);
    let v = send(&mut ws, json!({"jsonrpc":"2.0","id":2,"method":"version"}));
    assert_eq!(v["result"]["library"], "main", "{v}");

    // A one-time download link.
    let notes = env.files.path().join("notes.txt").display().to_string();
    let link = send(
        &mut ws,
        json!({"jsonrpc":"2.0","id":3,"method":"file.get","params":{"path": notes}}),
    );
    let url = link["result"]["url"].as_str().unwrap().to_owned();
    let (status, headers, body) = http_get(addr, &url, &host);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains("filename*=utf-8''notes.txt"), "{headers}");
    assert_eq!(body, b"notes");
    assert_eq!(http_get(addr, &url, &host).0, "HTTP/1.1 404 Not Found");
}

#[test]
fn web_refuses_remote_binds() {
    let env = env();
    let err = Daemon::start(Options {
        cfg: env.cfg.clone(),
        ws: None,
        web: Some("0.0.0.0:0".parse().unwrap()),
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("--web 0.0.0.0:0"), "{err:#}");
}

#[test]
fn a_token_others_may_access_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.token");
    let planted = "a".repeat(64);
    std::fs::write(&path, &planted).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let token = crate::ws::token(&path).unwrap();
    assert_ne!(token, planted, "a planted token is not used");
    assert_eq!(token.len(), 64);
    assert!(
        keel_api::private::read(&path).unwrap().is_some(),
        "owner-only"
    );
    assert_eq!(
        crate::ws::token(&path).unwrap(),
        token,
        "a private token is kept"
    );
}

#[test]
fn a_dripping_request_runs_out_of_time() {
    /// One byte per read, never a newline.
    struct Drip;
    impl std::io::Read for Drip {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(10));
            buf[0] = b'x';
            Ok(1)
        }
    }
    let started = Instant::now();
    let deadline = started + Duration::from_millis(200);
    let mut reader = std::io::BufReader::with_capacity(1, Drip);
    let err =
        keel_api::rpc::read_line(&mut reader, || crate::server::time_left(deadline).map(drop))
            .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// The server closed the connection (rather than the read timing out).
fn closed(read: std::io::Result<usize>) -> bool {
    match read {
        Ok(n) => n == 0,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ),
    }
}

#[test]
fn websocket_handshakes_have_a_deadline_and_a_cap() {
    let env = env();
    let daemon = start(&env, Some("127.0.0.1:0"));
    deadline_and_cap(&env, daemon.ws_addr().unwrap(), "/");
}

#[test]
fn web_handshakes_have_a_deadline_and_a_cap() {
    let env = env();
    let daemon = Daemon::start(Options {
        cfg: env.cfg.clone(),
        ws: None,
        web: Some("127.0.0.1:0".parse().unwrap()),
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .unwrap();
    deadline_and_cap(&env, daemon.web_addr().unwrap(), "/rpc");
}

/// A dripping handshake is cut off at `HANDSHAKE_WAIT`; past `MAX_CONNECTIONS` a connection
/// is closed at once; slots come back. Ends with a bearer-token WebSocket on `path`.
fn deadline_and_cap(env: &Env, addr: std::net::SocketAddr, path: &str) {
    use crate::ws::{HANDSHAKE_WAIT, MAX_CONNECTIONS};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let mut buf = [0u8; 64];
    // A client that drips a header line every second is cut off at the deadline.
    let mut drip = TcpStream::connect(addr).unwrap();
    let mut writer = drip.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = writer.write_all(b"GET / HTTP/1.1\r\n");
        for i in 0..30 {
            std::thread::sleep(Duration::from_secs(1));
            if writer
                .write_all(format!("X-Drip-{i}: x\r\n").as_bytes())
                .is_err()
            {
                return;
            }
        }
    });
    let started = Instant::now();
    drip.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    assert!(
        closed(drip.read(&mut buf)),
        "still open after {:?}",
        started.elapsed()
    );
    assert!(started.elapsed() < HANDSHAKE_WAIT + Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(300));
    // At most MAX_CONNECTIONS at once: one more is closed at once.
    let idle: Vec<TcpStream> = (0..MAX_CONNECTIONS)
        .map(|_| TcpStream::connect(addr).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let mut extra = TcpStream::connect(addr).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let started = Instant::now();
    assert!(closed(extra.read(&mut buf)), "over the cap");
    assert!(started.elapsed() < Duration::from_secs(2));
    // Their slots come back once they go.
    drop(idle);
    let token = std::fs::read_to_string(env.config.path().join("daemon.token")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        use tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        if let Ok((mut ws, _)) = tungstenite::client(req, TcpStream::connect(addr).unwrap()) {
            ws.send(tungstenite::Message::text(
                r#"{"jsonrpc":"2.0","id":1,"method":"version"}"#,
            ))
            .unwrap();
            let answer: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(answer["result"]["api"], 1);
            break;
        }
        assert!(Instant::now() < deadline, "no slot came back");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn web_daemon(env: &Env) -> Daemon {
    Daemon::start(Options {
        cfg: env.cfg.clone(),
        ws: None,
        web: Some("127.0.0.1:0".parse().unwrap()),
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: None,
    })
    .unwrap()
}

#[allow(clippy::result_large_err)]
fn web_socket(
    addr: std::net::SocketAddr,
) -> tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>> {
    use tungstenite::client::IntoClientRequest;
    let req = format!("ws://{addr}/rpc").into_client_request().unwrap();
    let stream = std::net::TcpStream::connect(addr).unwrap();
    let (ws, _) =
        tungstenite::client(req, tungstenite::stream::MaybeTlsStream::Plain(stream)).unwrap();
    ws
}

fn answer<S: std::io::Read + std::io::Write>(ws: &mut tungstenite::WebSocket<S>) -> Value {
    loop {
        match ws.read().unwrap() {
            tungstenite::Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            tungstenite::Message::Close(_) => panic!("closed without an answer"),
            _ => {}
        }
    }
}

/// Before `auth`: messages over 4 KiB close the connection; no `auth` in time gets
/// -32007 before the close; at most 16 unauthenticated connections at once.
#[test]
fn web_limits_connections_that_have_not_signed_in() {
    use tungstenite::Message;
    let env = env();
    let daemon = web_daemon(&env);
    let addr = daemon.web_addr().unwrap();
    let token = std::fs::read_to_string(env.config.path().join("daemon.token")).unwrap();

    // A big message before auth: refused (the connection goes), never buffered.
    let mut ws = web_socket(addr);
    let big = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"auth","params":{{"token":"{}"}}}}"#,
        "x".repeat(crate::ws::AUTH_MAX)
    );
    let _ = ws.send(Message::text(big));
    let closed = (0..20).any(|_| !matches!(ws.read(), Ok(Message::Text(_)) | Ok(Message::Ping(_))));
    assert!(closed);

    // No auth in time: -32007, then the close.
    let mut ws = web_socket(addr);
    let late = answer(&mut ws);
    assert_eq!(late["error"]["code"], ApiError::UNAUTHORIZED, "{late}");

    // 16 waiting to sign in: the 17th is closed at once; signing in frees a place.
    let mut waiting: Vec<_> = (0..crate::web::MAX_UNAUTHENTICATED)
        .map(|_| web_socket(addr))
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    let mut extra = std::net::TcpStream::connect(addr).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    assert!(closed_tcp(&mut extra), "over the unauthenticated cap");
    let auth = json!({"jsonrpc":"2.0","id":1,"method":"auth","params":{"token": token.trim()}});
    waiting[0].send(Message::text(auth.to_string())).unwrap();
    assert_eq!(answer(&mut waiting[0])["result"]["ok"], true);
    // A big message is fine once signed in.
    let big =
        json!({"jsonrpc":"2.0","id":2,"method":"search","params":{"query": "x".repeat(8 << 10)}});
    waiting[0].send(Message::text(big.to_string())).unwrap();
    assert_eq!(answer(&mut waiting[0])["id"], 2);
    let mut one_more = web_socket(addr);
    let auth = json!({"jsonrpc":"2.0","id":1,"method":"auth","params":{"token": token.trim()}});
    one_more.send(Message::text(auth.to_string())).unwrap();
    assert_eq!(answer(&mut one_more)["result"]["ok"], true);
}

fn closed_tcp(s: &mut std::net::TcpStream) -> bool {
    use std::io::Read;
    let started = Instant::now();
    let mut buf = [0u8; 16];
    closed(s.read(&mut buf)) && started.elapsed() < Duration::from_secs(2)
}

/// `keel daemon rotate-token`: the old token no longer signs in, and a session signed in
/// with it is closed with -32007.
#[test]
fn a_rotated_token_closes_old_sessions() {
    use tungstenite::Message;
    let env = env();
    let daemon = web_daemon(&env);
    let addr = daemon.web_addr().unwrap();
    let path = env.config.path().join("daemon.token");
    let old = std::fs::read_to_string(&path).unwrap();
    let auth =
        |t: &str| json!({"jsonrpc":"2.0","id":1,"method":"auth","params":{"token": t.trim()}});
    let mut ws = web_socket(addr);
    ws.send(Message::text(auth(&old).to_string())).unwrap();
    assert_eq!(answer(&mut ws)["result"]["ok"], true);

    let new = keel_api::config::new_token(&path).unwrap();
    assert_ne!(new, old.trim());
    let gone = answer(&mut ws);
    assert_eq!(gone["error"]["code"], ApiError::UNAUTHORIZED, "{gone}");
    let mut ws = web_socket(addr);
    ws.send(Message::text(auth(&old).to_string())).unwrap();
    assert_eq!(answer(&mut ws)["error"]["code"], ApiError::UNAUTHORIZED);
    let mut ws = web_socket(addr);
    ws.send(Message::text(auth(&new).to_string())).unwrap();
    assert_eq!(answer(&mut ws)["result"]["ok"], true);
}

/// The PWA: the manifest (with the share target), the icons and the service worker are
/// served with their types, and the CSP admits them from this origin and nothing more.
#[test]
fn web_serves_the_pwa_manifest_icons_and_service_worker() {
    let env = env();
    let daemon = web_daemon(&env);
    let addr = daemon.web_addr().unwrap();
    let host = addr.to_string();
    let (status, headers, body) = http_get(addr, "/manifest.webmanifest", &host);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(
        headers.contains("content-type: application/manifest+json"),
        "{headers}"
    );
    let m: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(m["display"], "standalone");
    assert_eq!(m["start_url"], "/");
    let target = &m["share_target"];
    assert_eq!(
        (&target["action"], &target["method"], &target["enctype"]),
        (
            &json!("/share"),
            &json!("POST"),
            &json!("multipart/form-data")
        )
    );
    assert_eq!(target["params"]["files"][0]["name"], "files");
    let icons = m["icons"].as_array().unwrap();
    let sizes: Vec<&str> = icons.iter().map(|i| i["sizes"].as_str().unwrap()).collect();
    assert_eq!(sizes, ["192x192", "512x512"]);
    for icon in icons {
        let src = format!("/{}", icon["src"].as_str().unwrap());
        let (status, headers, body) = http_get(addr, &src, &host);
        assert_eq!(status, "HTTP/1.1 200 OK", "{src}");
        assert!(headers.contains("content-type: image/png"), "{headers}");
        assert!(body.starts_with(b"\x89PNG"), "{src}");
    }

    let (status, headers, body) = http_get(addr, "/sw.js", &host);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(
        headers.contains("content-type: text/javascript"),
        "{headers}"
    );
    let sw = String::from_utf8(body).unwrap();
    assert!(sw.contains("\"keel-shell-\" + VERSION"), "{sw}");
    // Only the shell is cached: never /rpc, /file/ downloads or /share.
    let shell = &sw[sw.find("const SHELL = [").unwrap()..];
    let shell = &shell[..shell.find("];").unwrap()];
    for data in ["rpc", "file/", "share"] {
        assert!(!shell.contains(data), "{shell}");
    }
    assert!(
        sw.contains("req.method !== \"GET\""),
        "only GETs are looked up"
    );
    // A built client's worker carries its build's version (scripts/build-web.* stamp it).
    if crate::web::FILES.iter().any(|(n, _)| *n == "index.html") {
        let version = sw
            .split("const VERSION = \"")
            .nth(1)
            .and_then(|v| v.split('"').next())
            .unwrap();
        assert!(
            version.len() == 16 && version.bytes().all(|b| b.is_ascii_hexdigit()),
            "{version}"
        );
    } else {
        assert!(sw.contains("__KEEL_BUILD__"), "the unbuilt template");
    }

    let csp = headers
        .lines()
        .find(|l| l.starts_with("content-security-policy:"))
        .unwrap();
    assert!(csp.contains("manifest-src 'self'"), "{csp}");
    assert!(csp.contains("worker-src 'self'"), "{csp}");
    assert!(
        !csp.replace("'wasm-unsafe-eval'", "")
            .contains("unsafe-eval"),
        "{csp}"
    );
    assert!(!csp.contains("http") && !csp.contains('*'), "{csp}");
}

/// A raw HTTP POST; returns the status line, headers and body.
fn http_post(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> (String, String, Vec<u8>) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let mut head = format!("POST {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(out[..split].to_vec()).unwrap();
    let (status, headers) = head.split_once("\r\n").unwrap();
    (
        status.to_owned(),
        headers.to_lowercase(),
        out[split + 4..].to_vec(),
    )
}

/// The share target: a POST parks the files and sends the browser to `/?share=<id>`;
/// only a client signed in over `/rpc` can claim them, once. Cross-site posts, missing
/// lengths and shares over the cap are refused before anything is stored.
#[test]
fn web_share_target_parks_files_until_the_signed_in_client_claims_them() {
    use tungstenite::Message;
    let env = env();
    let daemon = web_daemon(&env);
    let addr = daemon.web_addr().unwrap();
    let host = addr.to_string();
    let token = std::fs::read_to_string(env.config.path().join("daemon.token")).unwrap();
    let mut body = Vec::new();
    for (name, file, data) in [
        ("title", None, &b"holiday"[..]),
        ("files", Some("beach.jpg"), &b"jpeg bytes"[..]),
        ("files", Some("notes.txt"), &b"notes"[..]),
    ] {
        body.extend_from_slice(b"--KeelB\r\n");
        let disposition = match file {
            Some(f) => format!("form-data; name=\"{name}\"; filename=\"{f}\""),
            None => format!("form-data; name=\"{name}\""),
        };
        body.extend_from_slice(format!("Content-Disposition: {disposition}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--KeelB--\r\n");
    let headers = |origin: &str, site: &str, len: Option<u64>| {
        let mut h = vec![
            ("Host", host.clone()),
            (
                "Content-Type",
                "multipart/form-data; boundary=KeelB".to_owned(),
            ),
            ("Origin", origin.to_owned()),
            ("Sec-Fetch-Site", site.to_owned()),
        ];
        if let Some(n) = len {
            h.push(("Content-Length", n.to_string()));
        }
        h
    };
    let ours = format!("http://{host}");
    let shares = env.data.path().join("shares");
    let refused = |h: Vec<(&str, String)>| http_post(addr, "/share", &h, b"").0;
    assert_eq!(
        refused(headers("http://evil.example", "cross-site", Some(0))),
        "HTTP/1.1 403 Forbidden"
    );
    assert_eq!(
        refused(headers("null", "cross-site", Some(0))),
        "HTTP/1.1 403 Forbidden"
    );
    assert_eq!(
        refused(headers(&ours, "same-origin", None)),
        "HTTP/1.1 411 Length Required"
    );
    let over = crate::share::SHARE_MAX + 1;
    assert_eq!(
        refused(headers(&ours, "same-origin", Some(over))),
        "HTTP/1.1 413 Payload Too Large"
    );
    assert!(
        std::fs::read_dir(&shares).map_or(true, |d| d.count() == 0),
        "nothing stored"
    );

    let (status, got, _) = http_post(
        addr,
        "/share",
        &headers(&ours, "same-origin", Some(body.len() as u64)),
        &body,
    );
    assert_eq!(status, "HTTP/1.1 303 See Other");
    let location = got
        .lines()
        .find_map(|l| l.strip_prefix("location: /?share="))
        .unwrap();
    let (id, counts) = location.split_once('&').unwrap();
    let id = id.to_owned();
    assert_eq!(id.len(), 24, "{got}");
    assert_eq!(counts, "files=2&bytes=15", "{got}");

    // Claiming needs the token.
    let claim = json!({"jsonrpc":"2.0","id":1,"method":"share.claim","params":{"id": id}});
    let mut ws = web_socket(addr);
    ws.send(Message::text(claim.to_string())).unwrap();
    assert_eq!(answer(&mut ws)["error"]["code"], ApiError::UNAUTHORIZED);
    let mut ws = web_socket(addr);
    let auth = json!({"jsonrpc":"2.0","id":0,"method":"auth","params":{"token": token}});
    ws.send(Message::text(auth.to_string())).unwrap();
    assert_eq!(answer(&mut ws)["result"]["ok"], true);
    ws.send(Message::text(claim.to_string())).unwrap();
    let claimed = answer(&mut ws)["result"].clone();
    let files = claimed["files"].as_array().unwrap().clone();
    let names: Vec<(&str, u64)> = files
        .iter()
        .map(|f| (f["name"].as_str().unwrap(), f["size"].as_u64().unwrap()))
        .collect();
    assert_eq!(names, [("beach.jpg", 10), ("notes.txt", 5)]);
    let path = std::path::PathBuf::from(files[0]["path"].as_str().unwrap());
    assert!(path.starts_with(&shares), "{path:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"jpeg bytes");
    // Once only.
    ws.send(Message::text(claim.to_string())).unwrap();
    assert_eq!(answer(&mut ws)["error"]["code"], ApiError::NOT_FOUND);
    // The claimed files are ordinary paths for the API (spacedrop.send, stat, …).
    let stat = json!({"jsonrpc":"2.0","id":2,"method":"stat","params":{"path": files[1]["path"]}});
    ws.send(Message::text(stat.to_string())).unwrap();
    assert_eq!(answer(&mut ws)["result"]["entry"]["size"], 5);
}

#[test]
fn keeps_every_source_watched() {
    let env = env();
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let id = add_source(&mut c, &env);
    let job = apply(&mut c, "sources.index", json!({ "id": id }))["job"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&mut c, job)["status"], "done");
    // Once the watcher is armed (within WATCH_EVERY), a new file shows up without
    // another index job.
    std::thread::sleep(Duration::from_secs(4));
    std::fs::write(env.files.path().join("later-memo.txt"), b"memo").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let hits = c.call("search", json!({"query": "later-memo"})).unwrap();
        if !hits.as_array().unwrap().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the new file was never indexed");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// With devices on, the daemon's node serves the library's sources to granted devices
/// and `node://` paths browse what a paired device shares.
#[test]
fn the_node_serves_sources_and_browses_devices() {
    let start_net = |env: &Env| {
        let mut cfg = env.cfg.clone();
        cfg.net = true;
        Daemon::start(Options {
            cfg,
            ws: None,
            web: None,
            ws_allow_remote: false,
            web_hosts: Vec::new(),
            net: Some(keel_api::host::NetSetup {
                secrets: std::sync::Arc::new(keel_vfs::cloud::MemoryStore::default()),
                options: keel_net::NodeOptions::offline(),
            }),
        })
        .unwrap()
    };
    let (ea, eb) = (env(), env());
    let (da, db) = (start_net(&ea), start_net(&eb));
    let (mut a, mut b) = (
        Client::connect(da.name()).unwrap(),
        Client::connect(db.name()).unwrap(),
    );
    let a_id = a.call("devices.list", Value::Null).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b_id = b.call("devices.list", Value::Null).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ticket = apply(&mut a, "devices.pair_code", json!({}))["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    apply(&mut b, "devices.pair_with", json!({ "code": ticket }));
    let source = add_source(&mut a, &ea);
    apply(
        &mut a,
        "shares.grant",
        json!({"peer": b_id, "source": source, "access": "read"}),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let root = format!("node://{a_id}/");
    loop {
        match b.call("list", json!({ "path": root })) {
            Ok(l) if !l["entries"].as_array().unwrap().is_empty() => {
                let src = l["entries"][0]["path"].as_str().unwrap().to_owned();
                let files = b.call("list", json!({ "path": src })).unwrap();
                let names: Vec<&str> = files["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["name"].as_str().unwrap())
                    .collect();
                assert!(names.contains(&"notes.txt"), "{files}");
                break;
            }
            other => {
                assert!(Instant::now() < deadline, "{root}: {other:?}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// The next `library.changed` notification's kind.
fn next_change(c: &mut Client) -> String {
    loop {
        let n = c.next_notification(Duration::from_secs(10)).unwrap();
        if n["method"] == "library.changed" {
            return n["params"]["kind"].as_str().unwrap().to_owned();
        }
    }
}

/// `library.changed` names the change; a call that changed nothing (an integrity check
/// that is not due) sends none, an opened file (`recents.note`) does.
#[test]
fn library_changed_names_the_change_and_skips_no_ops() {
    let env = env();
    let daemon = start(&env, None);
    let mut sub = Client::connect(daemon.name()).unwrap();
    sub.call("subscribe", Value::Null).unwrap();
    let mut c = Client::connect(daemon.name()).unwrap();
    let due = apply(
        &mut c,
        "integrity.check",
        json!({"sample_pct": 1.0, "due_days": 7}),
    );
    assert_eq!(
        due,
        json!({}),
        "the first scheduled check only starts the clock"
    );
    // A tag made on nothing (create-only): announced, and the first announcement.
    assert_eq!(
        apply(&mut c, "tags.add", json!({"tag": "later", "paths": []}))["records"],
        0
    );
    assert_eq!(next_change(&mut sub), "tags.add");
    let tags = c.call("tags.list", Value::Null).unwrap();
    assert!(tags
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "later"));
    let id = add_source(&mut c, &env);
    let job = apply(&mut c, "sources.index", json!({ "id": id }))["job"]
        .as_i64()
        .unwrap();
    wait_job(&mut c, job);
    let file = env.files.path().join("notes.txt").display().to_string();
    c.call("recents.note", json!({ "path": file })).unwrap();
    while next_change(&mut sub) != "recents.note" {}
}

/// `activity.note` from a client (an attached window) pauses the daemon's idle jobs: a
/// sidecar job makes nothing while the user works, and finishes once they stop.
#[test]
fn activity_pauses_the_idle_jobs() {
    let env = env();
    for i in 0..3 {
        image::RgbImage::from_pixel(32, 24, image::Rgb([i * 80, 0, 0]))
            .save(env.files.path().join(format!("{i}.png")))
            .unwrap();
    }
    let d = start(&env, None);
    let mut c = Client::connect(d.name()).unwrap();
    let source = add_source(&mut c, &env);
    let index = apply(&mut c, "sources.index", json!({ "id": source }));
    wait_job(&mut c, index["job"].as_i64().unwrap());
    let sidecars = || d.ctx().lib.sidecars().unwrap().stats().keys;
    let done = c.call("activity.note", json!({})).unwrap();
    assert_eq!(done["ok"], true);
    let job = apply(&mut c, "media.index", json!({ "id": source }))["job"]
        .as_i64()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(sidecars(), 0, "paused while the user works");
    assert_eq!(wait_job(&mut c, job)["status"], "done");
    assert!(sidecars() >= 3, "{}", sidecars());
}

/// `devices.settings_set` changes the running node: its label at once, the inbox and the
/// always-accept list for the next drop; relays (`[devices] relay` when it started) need a
/// restart.
#[test]
fn device_settings_apply_live() {
    let env = env();
    let mut cfg = env.cfg.clone();
    cfg.net = true;
    cfg.relay = false;
    let d = Daemon::start(Options {
        cfg,
        ws: None,
        web: None,
        ws_allow_remote: false,
        web_hosts: Vec::new(),
        net: Some(keel_api::host::NetSetup {
            secrets: std::sync::Arc::new(keel_vfs::cloud::MemoryStore::default()),
            options: keel_net::NodeOptions::offline(),
        }),
    })
    .unwrap();
    let mut c = Client::connect(d.name()).unwrap();
    let got = c.call("devices.settings", Value::Null).unwrap();
    assert_eq!(got["relay"], false, "{got}");
    let default_inbox = env.data.path().join("inbox").display().to_string();
    assert_eq!(got["inbox"], default_inbox);
    let me = c.call("devices.list", Value::Null).unwrap()["id"].clone();
    let inbox = env.files.path().join("drops").display().to_string();
    let set = json!({"label": "Den", "inbox": inbox, "auto_accept": [me], "relay": false});
    let done = apply(&mut c, "devices.settings_set", set);
    assert_eq!(done["restart"], false);
    assert_eq!(done["settings"]["label"], "Den");
    assert_eq!(c.call("devices.list", Value::Null).unwrap()["label"], "Den");
    let drops = d.ctx().drops.clone().unwrap();
    assert_eq!(drops.inbox().display().to_string(), inbox);
    assert_eq!(drops.auto_accept(), [me.as_str().unwrap()]);
    let got = c.call("spacedrop.inbox", Value::Null).unwrap();
    assert_eq!(got["dir"], inbox);
    let done = apply(&mut c, "devices.settings_set", json!({ "relay": true }));
    assert_eq!(done["restart"], true);
    assert_eq!(done["settings"]["relay"], false, "as it started");
}

/// Review 46 minor 4: the recount after a watched change is announced as `library.changed`
/// `protection.recount`.
#[test]
fn a_watched_change_announces_the_protection_recount() {
    let env = env();
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let id = add_source(&mut c, &env);
    let job = apply(&mut c, "sources.index", json!({ "id": id }))["job"]
        .as_i64()
        .unwrap();
    wait_job(&mut c, job);
    // The walks' own recounts first: wait until the counters settle.
    let revision = || daemon.ctx().lib.protection_revision();
    let mut settled = revision();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if revision() == settled {
            break;
        }
        settled = revision();
    }
    let mut sub = Client::connect(daemon.name()).unwrap();
    sub.call("subscribe", Value::Null).unwrap();
    std::fs::write(env.files.path().join("notes.txt"), b"changed notes").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "no protection.recount");
        let n = sub.next_notification(Duration::from_secs(60)).unwrap();
        if n["method"] == "library.changed"
            && n["params"]["kind"] == "protection.recount"
            && revision() > settled
        {
            break;
        }
    }
}

/// The daemon answering `list` 1,000 times over the local socket (one client, one request at
/// a time): `cargo test -p keel-daemon --release perf_list -- --ignored --nocapture`.
#[test]
#[ignore = "release measurement: 1,000 list requests over the local socket"]
fn perf_list_1000_requests() {
    let env = env();
    for i in 0..100 {
        std::fs::write(env.files.path().join(format!("file {i:03}.txt")), b"x").unwrap();
    }
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let path = env.files.path().display().to_string();
    let list = |c: &mut Client| c.call("list", json!({ "path": path })).unwrap();
    assert_eq!(list(&mut c)["entries"].as_array().unwrap().len(), 102);
    let start = Instant::now();
    for _ in 0..1_000 {
        list(&mut c);
    }
    let took = start.elapsed();
    eprintln!(
        "PERF daemon_list_1000: {took:?} ({:.0} requests/s, 102 entries each)",
        1_000.0 / took.as_secs_f64()
    );
    assert!(took < Duration::from_secs(10), "{took:?}");
}

/// Task 53: the daemon holds the databases, so a client's backup and restore run there, and
/// the files are where the client asked.
#[test]
fn backup_and_restore_run_in_the_daemon() {
    let env = env();
    let daemon = start(&env, None);
    let mut c = Client::connect(daemon.name()).unwrap();
    let id = add_source(&mut c, &env);
    let job = apply(&mut c, "sources.index", json!({ "id": id }))["job"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&mut c, job)["status"], "done");
    let out = tempfile::tempdir().unwrap();
    let file = out.path().join("main.keelbackup");
    let done = apply(
        &mut c,
        "library.backup",
        json!({"file": file.display().to_string()}),
    );
    assert_eq!(done["sources"], 1);
    assert!(file.is_file());
    // The daemon's own library is never replaced; another name is restored beside it.
    let e = c
        .call(
            "library.restore",
            json!({"file": file.display().to_string()}),
        )
        .unwrap_err();
    assert!(e.message.contains("exists"), "{}", e.message);
    let restored = apply(
        &mut c,
        "library.restore",
        json!({"file": file.display().to_string(), "name": "copy"}),
    );
    assert_eq!(restored["sources"][0]["id"], id.as_str());
    assert!(env.data.path().join("library/copy/library.db").is_file());
    // The daemon still serves its library.
    let hits = c.call("search", json!({"query": "invoice"})).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1, "{hits}");
}

/// A raw HTTP GET with extra headers; returns the status line, headers and body.
fn http_get_with(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (String, String, Vec<u8>) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(out[..split].to_vec()).unwrap();
    let (status, headers) = head.split_once("\r\n").unwrap();
    (
        status.to_owned(),
        headers.to_lowercase(),
        out[split + 4..].to_vec(),
    )
}

/// The files of one folder whose names hold the query text: the platform search in tests.
struct FolderSearch(std::path::PathBuf);

impl keel_api::system::Searcher for FolderSearch {
    fn query(&self, q: &keel_api::system::Query) -> anyhow::Result<Vec<keel_api::system::Hit>> {
        let text = q.text.to_lowercase();
        let mut hits = Vec::new();
        for e in std::fs::read_dir(&self.0)?.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.contains(&text) && hits.len() < q.max as usize {
                hits.push(keel_api::system::Hit {
                    path: keel_vfs::VPath::local(e.path()),
                    is_dir: e.path().is_dir(),
                    size: 0,
                    modified: None,
                    snippet: None,
                });
            }
        }
        Ok(hits)
    }
    fn available(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "Folder"
    }
}

/// Task 78: `--web` answers Everything's HTTP search parameters, as Everything's JSON or an
/// HTML page, only with the token or the HTTP login; counts are bounded; downloads only
/// while `[web] allow_downloads` is on; `/` without those parameters is the client still.
#[test]
fn web_answers_everything_searches() {
    use base64::Engine;
    let env = env();
    let daemon = web_daemon(&env);
    let addr = daemon.web_addr().unwrap();
    let search = std::sync::Arc::new(FolderSearch(env.files.path().to_owned()));
    daemon.ctx().system.set(search);
    let store = std::sync::Arc::new(keel_vfs::cloud::MemoryStore::default());
    daemon.set_secret_store(store.clone());
    let token = std::fs::read_to_string(env.config.path().join("daemon.token")).unwrap();
    let bearer = format!("Bearer {}", token.trim());
    let auth = |path: &str| http_get_with(addr, path, &[("Authorization", bearer.as_str())]);

    // No credentials, or wrong ones: 401 with a Basic challenge.
    let (status, headers, _) = http_get_with(addr, "/?search=invoice&json=1", &[]);
    assert_eq!(status, "HTTP/1.1 401 Unauthorized");
    assert!(
        headers.contains("www-authenticate: basic realm=\"keel\""),
        "{headers}"
    );
    let (status, ..) = http_get_with(
        addr,
        "/everything?search=x",
        &[("Authorization", "Bearer nope")],
    );
    assert_eq!(status, "HTTP/1.1 401 Unauthorized");

    // The token: Everything's JSON.
    let (status, headers, body) =
        auth("/?search=INVOICE&json=1&path_column=1&size_column=1&date_modified_column=1");
    assert_eq!(
        status,
        "HTTP/1.1 200 OK",
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        headers.contains("content-type: application/json"),
        "{headers}"
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["totalResults"], 1, "{v}");
    let hit = &v["results"][0];
    assert_eq!(hit["type"], "file");
    assert_eq!(hit["name"], "invoice-2026.pdf");
    assert_eq!(hit["path"], env.files.path().display().to_string());
    assert_eq!(hit["size"], "3");
    let date: u64 = hit["date_modified"].as_str().unwrap().parse().unwrap();
    assert!(date > 116_444_736_000_000_000, "a FILETIME after 1970");
    // Short names and the count bound: both files, one shown, the biggest first.
    let (_, _, body) = auth("/everything?s=&j=1&c=1&sort=size&ascending=0&size_column=1");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["totalResults"], 2, "{v}");
    assert_eq!(v["results"].as_array().unwrap().len(), 1);
    assert_eq!(v["results"][0]["name"], "notes.txt");
    let (status, ..) = auth("/everything?search=x&count=-1&json=1");
    assert_eq!(status, "HTTP/1.1 400 Bad Request");
    let (status, _, body) = auth("/everything?search=(x&regex=1&json=1");
    assert_eq!(status, "HTTP/1.1 400 Bad Request");
    assert!(String::from_utf8_lossy(&body).contains("error"));

    // `/` without Everything's parameters is the web client.
    let (status, headers, body) = http_get_with(addr, "/?share=abc&files=1&bytes=2", &[]);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains("text/html"), "{headers}");
    assert!(String::from_utf8_lossy(&body).contains("<title>Keel</title>"));

    // The HTTP login, as Basic auth: the HTML page, script-free, no download links.
    let basic = |user: &str, pass: &str| {
        let b = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Basic {b}")
    };
    let page = |auth: &str| {
        http_get_with(
            addr,
            "/everything?search=invoice",
            &[("Authorization", auth)],
        )
    };
    let login = basic("tool", "a long password");
    assert_eq!(page(&login).0, "HTTP/1.1 401 Unauthorized");
    crate::everything::set_login(&*store, &env.cfg.profile, "tool", "a long password").unwrap();
    let (status, headers, body) = page(&login);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(
        headers.contains("content-security-policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'"),
        "{headers}"
    );
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("invoice-2026.pdf") && html.contains("1 result"),
        "{html}"
    );
    assert!(!html.contains("<script") && !html.contains("/everything/file"));
    assert!(!html.contains("Not encrypted"), "a loopback bind");
    assert_eq!(
        page(&basic("tool", "wrong password")).0,
        "HTTP/1.1 401 Unauthorized"
    );
    // The token as a Basic password works too.
    assert_eq!(page(&basic("any", token.trim())).0, "HTTP/1.1 200 OK");
    // Another site's page cannot use the browser's stored login.
    let (status, ..) = http_get_with(
        addr,
        "/everything?search=invoice",
        &[
            ("Authorization", bearer.as_str()),
            ("Sec-Fetch-Site", "cross-site"),
        ],
    );
    assert_eq!(status, "HTTP/1.1 403 Forbidden");

    // Downloads: off by default, on with `[web] allow_downloads = true` (read at once).
    let pdf = env.files.path().join("invoice-2026.pdf");
    let pdf = crate::everything::encode(&pdf.display().to_string());
    let file = format!("/everything/file?path={pdf}");
    assert_eq!(
        http_get_with(addr, &file, &[]).0,
        "HTTP/1.1 401 Unauthorized"
    );
    assert_eq!(auth(&file).0, "HTTP/1.1 403 Forbidden");
    let profile = env.config.path().join("profiles").join(&env.cfg.profile);
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("config.toml"),
        "[web]\nallow_downloads = true\n",
    )
    .unwrap();
    let (status, headers, body) = auth(&file);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(
        headers.contains("content-disposition: attachment"),
        "{headers}"
    );
    assert_eq!(body, b"pdf");
    let html = String::from_utf8_lossy(&auth("/everything?search=invoice").2).into_owned();
    assert!(html.contains("/everything/file?path="), "{html}");
    // Keel's configuration folder stays out of reach.
    let secret = env.config.path().join("daemon.token");
    let secret = crate::everything::encode(&secret.display().to_string());
    let file = format!("/everything/file?path={secret}");
    assert_eq!(auth(&file).0, "HTTP/1.1 404 Not Found");
}

/// Task 78: the ETP server through the daemon: `[etp]` from the profile's config.toml, the
/// ETP login from the keychain (none: no server), the system search limited to the roots.
#[test]
fn daemon_serves_etp() {
    let env = env();
    let daemon = web_daemon(&env);
    let search = std::sync::Arc::new(FolderSearch(env.files.path().to_owned()));
    daemon.ctx().system.set(search);
    let store = std::sync::Arc::new(keel_vfs::cloud::MemoryStore::default());
    daemon.set_secret_store(store.clone());
    let bind = Some("127.0.0.1:0".parse().unwrap());
    let e = daemon.start_etp(bind).unwrap_err();
    assert!(format!("{e:#}").contains("--set-etp-login"), "{e:#}");
    crate::etp::set_login(&*store, &env.cfg.profile, "me", "a long password").unwrap();
    let e = daemon.start_etp(bind).unwrap_err();
    assert!(format!("{e:#}").contains("[[etp.roots]]"), "{e:#}");
    let profile = env.config.path().join("profiles").join(&env.cfg.profile);
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("config.toml"),
        format!(
            "[[etp.roots]]\npath = '{}'\nname = \"Files\"\n",
            env.files.path().display()
        ),
    )
    .unwrap();
    let addr = daemon.start_etp(bind).unwrap();
    assert_eq!(daemon.etp_addr(), Some(addr));
    let mut t = keel_search::etp::Target::parse(&format!("me:a long password@{addr}")).unwrap();
    t.timeout = Duration::from_secs(10);
    let etp = keel_search::etp::EtpSearcher::new(t);
    use keel_api::system::Searcher;
    let hits = etp
        .query(&keel_api::system::Query {
            text: "invoice".into(),
            ..keel_api::system::Query::default()
        })
        .unwrap();
    let paths: Vec<&str> = hits.iter().map(|h| h.path.path.as_str()).collect();
    assert_eq!(paths, ["/Files/invoice-2026.pdf"]);
    daemon.shutdown();
}
