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
    assert_eq!(v["library"], "james");
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
    assert!(keel_core::Library::open(env.data.path(), "james").is_err());
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
    while keel_core::Library::open(env.data.path(), "james").is_err() {
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
    assert_eq!(answer["result"]["library"], "james");
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
    assert_eq!(
        send(&mut ws, wrong)["error"]["code"],
        ApiError::UNAUTHORIZED
    );

    let (mut ws, _) = connect(Some(&format!("http://{host}"))).unwrap();
    let auth = json!({"jsonrpc":"2.0","id":1,"method":"auth","params":{"token": token}});
    assert_eq!(send(&mut ws, auth)["result"]["ok"], true);
    let v = send(&mut ws, json!({"jsonrpc":"2.0","id":2,"method":"version"}));
    assert_eq!(v["result"]["library"], "james", "{v}");

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
fn profile_names_are_validated() {
    use clap::Parser;
    for bad in ["../x", "a/b", "a\\b", "..", "", "nul"] {
        let args = crate::Args::try_parse_from(["keel-daemon", "--profile", bad]);
        assert!(args.is_err(), "{bad:?}");
    }
    let args = crate::Args::try_parse_from(["keel-daemon", "--profile", "work"]).unwrap();
    assert_eq!(args.profile, "work");
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
    let id = got
        .lines()
        .find_map(|l| l.strip_prefix("location: /?share="))
        .unwrap()
        .to_owned();
    assert_eq!(id.len(), 24, "{got}");

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
