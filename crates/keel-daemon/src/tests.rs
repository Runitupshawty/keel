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
        net: None,
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("--web 0.0.0.0:0"), "{err:#}");
}
