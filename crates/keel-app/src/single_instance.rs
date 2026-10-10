//! One Keel per user (Task 24): the first instance listens on a local socket named
//! `keel-<hash of user and profile>`; a later `keel [FOLDER] [--search Q]` sends its request
//! there and exits.
//!
//! Only the same user gets through (`keel_api::socket`). Windows: a named pipe whose DACL grants only the
//! user's SID; the client refuses impersonation and checks that the pipe's server runs as
//! the same user (`keel_vfs::pipe`). Unix: a socket file in a 0700 folder of the user's
//! (`$XDG_RUNTIME_DIR/keel-<uid>`, else `<temp>/keel-<uid>`), and both ends check the peer's
//! uid (`SO_PEERCRED` / `LOCAL_PEERCRED`).
//!
//! Protocol: one JSON `Request` line (at most `MAX_LINE`) from the client, `ok` back once
//! it was taken. The server reads each client on its own thread, for at most `READ_WAIT`.

use crate::cli::Request;
use interprocess::local_socket::{prelude::*, Stream};
use keel_api::socket;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use keel_api::socket::Listener;

/// How long a client waits for a running instance to answer (connecting included).
const ANSWER_WAIT: Duration = Duration::from_secs(3);
/// How long the server waits for a client's request line.
const READ_WAIT: Duration = Duration::from_secs(2);
/// Longest request line read (a path and a query).
const MAX_LINE: u64 = 64 * 1024;

/// The socket name: per user, and per profile (`keel_api::socket::name`).
pub fn name(profile: &str) -> String {
    socket::name("keel", profile)
}

#[cfg(test)]
fn name_for(user: &str, profile: &str) -> String {
    socket::name_for("keel", user, profile)
}

pub enum Claim {
    /// A running instance took the request: exit.
    Handed,
    /// This process is the instance now: serve the listener once the window is up.
    Server(Listener),
    /// Neither worked (logged): run on its own.
    Alone,
}

/// Hands `req` to the instance listening on `name` (unless `hand_off` is false, as for
/// `--new-window`) or becomes that instance.
pub fn claim(name: &str, req: &Request, hand_off: bool) -> Claim {
    // Twice: losing the race to listen means another instance started just now.
    for _ in 0..2 {
        if hand_off {
            match send(name, req) {
                Ok(()) => return Claim::Handed,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    tracing::warn!("running Keel did not answer: {e}");
                    return Claim::Alone;
                }
                Err(e) => {
                    tracing::debug!("no running Keel on {name}: {e}");
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) {
                        remove_stale(name);
                    }
                }
            }
        }
        match listen(name) {
            Ok(listener) => return Claim::Server(listener),
            // Taken (Windows reports access denied for a pipe name in use).
            Err(e) if hand_off => tracing::debug!("single instance {name}: {e}"),
            Err(e) => {
                tracing::info!("single instance {name}: {e}");
                break;
            }
        }
    }
    Claim::Alone
}

#[cfg(unix)]
use keel_api::socket::check_peer;
use keel_api::socket::{bind, listen, remove_stale};

/// Connects to the instance on `name`, which must run as this user. On Windows it may take
/// the foreground (this process was just started by the user).
fn connect(name: &str) -> io::Result<impl Read + Write> {
    let (conn, _server) = socket::connect(name, ANSWER_WAIT)?;
    #[cfg(windows)]
    if let Some(server) = _server {
        keel_vfs::desktop::allow_foreground(server);
    }
    Ok(conn)
}

/// Sends `req` and waits up to `ANSWER_WAIT` for the `ok`, connecting included (a busy or
/// hung instance can't hold this process).
fn send(name: &str, req: &Request) -> io::Result<()> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    let name = name.to_owned();
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let answer = (|| {
            let mut conn = connect(&name)?;
            conn.write_all(line.as_bytes())?;
            let mut ok = String::new();
            BufReader::new(conn).take(16).read_line(&mut ok)?;
            match ok.trim() {
                "ok" => Ok(()),
                other => Err(io::Error::other(format!("unexpected answer {other:?}"))),
            }
        })();
        let _ = tx.send(answer);
    });
    rx.recv_timeout(ANSWER_WAIT)
        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

type Handler = Arc<dyn Fn(Request) + Send + Sync>;

/// The running listener (see `serve`).
pub struct Server {
    name: String,
    stop: Arc<AtomicBool>,
    on_request: Handler,
}

/// Accepts requests on `listener` (bound to `name`) on a worker for the life of the
/// process; `on_request` runs on a per-client thread.
pub fn serve(
    listener: Listener,
    name: &str,
    on_request: impl Fn(Request) + Send + Sync + 'static,
) -> Server {
    let server = Server {
        name: name.to_owned(),
        stop: Arc::new(AtomicBool::new(false)),
        on_request: Arc::new(on_request),
    };
    server.accept(listener);
    server
}

impl Server {
    fn accept(&self, listener: Listener) {
        let (stop, on_request) = (self.stop.clone(), self.on_request.clone());
        let spawned = std::thread::Builder::new()
            .name("keel-instance".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break; // dropping the listener frees the name
                    }
                    match conn {
                        Ok(conn) => {
                            let on_request = on_request.clone();
                            let spawned = std::thread::Builder::new()
                                .name("keel-instance-client".into())
                                .spawn(move || serve_one(conn, &*on_request));
                            if let Err(e) = spawned {
                                tracing::warn!("spawn keel-instance-client: {e}");
                            }
                        }
                        Err(e) => tracing::warn!("single instance accept: {e}"),
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::error!("spawn keel-instance: {e}");
        }
    }

    /// Listens on `name` instead (a profile switch): later `keel --profile` runs reach this
    /// instance under its new profile. When another instance holds `name`, this one keeps its
    /// old name and the error says so.
    pub fn rebind(&mut self, name: &str) -> io::Result<()> {
        if name == self.name {
            return Ok(());
        }
        let listener = bind(name)?;
        self.stop.store(true, Ordering::Release);
        // Wakes the old accept loop, which then sees `stop` and lets go of the old name. The
        // connection stays open until it does (a client gone before the accept would not
        // wake it on Windows), on a thread of its own.
        let old = self.name.clone();
        std::thread::spawn(move || {
            if let Ok(mut conn) = connect(&old) {
                let _ = conn.read(&mut [0u8; 1]);
            }
        });
        self.stop = Arc::new(AtomicBool::new(false));
        self.name = name.to_owned();
        self.accept(listener);
        Ok(())
    }
}

/// Reads one request line from `conn` (at most `MAX_LINE`, within `READ_WAIT`) and hands it
/// to `on_request`. Anything else is logged and the connection closed.
fn serve_one(conn: Stream, on_request: &dyn Fn(Request)) {
    #[cfg(unix)]
    if let Err(e) = check_peer(&conn).and_then(|()| conn.set_recv_timeout(Some(READ_WAIT))) {
        tracing::warn!("single instance: client refused: {e}");
        return;
    }
    // Named pipes have no read timeout: a watchdog cancels the read instead.
    #[cfg(windows)]
    let watchdog = socket::Watchdog::arm(&conn, READ_WAIT);
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    let read = (&mut reader).take(MAX_LINE).read_line(&mut line);
    #[cfg(windows)]
    drop(watchdog);
    match read {
        Err(e) => tracing::warn!("single instance read: {e}"),
        Ok(_) if !line.ends_with('\n') => {
            tracing::warn!("single instance: request cut off or too long, closed")
        }
        Ok(_) => match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let _ = reader.get_mut().write_all(b"ok\n");
                on_request(req);
            }
            Err(e) => tracing::warn!("single instance: bad request: {e}"),
        },
    }
}

/// Restores and focuses Keel's window (a request came in, or the global hotkey). Callable
/// from any thread.
pub fn bring_to_front(ctx: &egui::Context) {
    // Windows: at once (a minimized window may not run frames for the commands below).
    #[cfg(windows)]
    keel_vfs::desktop::focus_own_window();
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn test_name(what: &str) -> String {
        format!("keel-test-{what}-{}", std::process::id())
    }

    fn served(name: &str) -> (Server, crossbeam_channel::Receiver<Request>) {
        let Claim::Server(listener) = claim(name, &Request::default(), true) else {
            panic!("{name}: first instance should serve");
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        (serve(listener, name, move |r| tx.send(r).unwrap()), rx)
    }

    #[test]
    fn second_instance_hands_its_request_over() {
        let name = test_name("hand");
        let req = Request {
            folder: Some(std::env::temp_dir()),
            search: Some("needle".into()),
            select: None,
        };
        let (_server, rx) = served(&name);
        // A second instance hands over and is told to exit.
        assert!(matches!(claim(&name, &req, true), Claim::Handed));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), req);
        // --new-window does not hand over; the name is taken, so it runs alone.
        assert!(matches!(
            claim(&name, &Request::default(), false),
            Claim::Alone
        ));
        assert!(rx.try_recv().is_err());
    }

    /// A client that connects and never writes (or writes no newline) holds nobody off, and
    /// its connection is dropped after `READ_WAIT`.
    #[test]
    fn idle_client_does_not_block_others() {
        let name = test_name("idle");
        let (_server, rx) = served(&name);
        let mut idle = connect(&name).unwrap();
        let mut half = connect(&name).unwrap();
        half.write_all(b"{\"folder\":").unwrap();
        let started = Instant::now();
        let req = Request {
            search: Some("after idle".into()),
            ..Request::default()
        };
        assert!(matches!(claim(&name, &req, true), Claim::Handed));
        assert!(started.elapsed() < ANSWER_WAIT);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), req);
        // The server gives up on both: their reads end without an answer.
        let mut buf = [0u8; 4];
        assert!(!matches!(idle.read(&mut buf), Ok(n) if n > 0));
        assert!(!matches!(half.read(&mut buf), Ok(n) if n > 0));
        assert!(started.elapsed() >= READ_WAIT - Duration::from_millis(500));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn oversized_or_bad_requests_are_dropped() {
        let name = test_name("big");
        let (_server, rx) = served(&name);
        let mut big = connect(&name).unwrap();
        // More than MAX_LINE without a newline: refused, nothing taken. The server may close
        // before all of it is written.
        let _ = big.write_all(&vec![b'a'; MAX_LINE as usize + 10]);
        let _ = big.write_all(b"\n");
        let mut answer = String::new();
        let _ = BufReader::new(big).read_line(&mut answer);
        assert_eq!(answer, "");
        let mut bad = connect(&name).unwrap();
        bad.write_all(b"{not json}\n").unwrap();
        let mut answer = String::new();
        let _ = BufReader::new(bad).read_line(&mut answer);
        assert_eq!(answer, "");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn rebind_moves_to_the_new_name() {
        let (old, new) = (test_name("old"), test_name("new"));
        let (mut server, rx) = served(&old);
        server.rebind(&new).unwrap();
        let req = Request {
            search: Some("moved".into()),
            ..Request::default()
        };
        assert!(matches!(claim(&new, &req, true), Claim::Handed));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), req);
        // The old name is free once its accept loop has let go.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match claim(&old, &Request::default(), false) {
                Claim::Server(_) => break,
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => panic!("old name still held"),
            }
        }
        // A name another instance holds is refused, and this one keeps its own.
        let (_other, _) = served(&test_name("held"));
        assert!(server.rebind(&test_name("held")).is_err());
        assert!(matches!(claim(&new, &req, true), Claim::Handed));
    }

    #[test]
    fn names_are_per_user_and_profile() {
        let default = name("default");
        assert!(
            default.starts_with("keel-") && default.len() == 21,
            "{default}"
        );
        assert_ne!(name("work"), default);
        assert_eq!(name_for("me", "default"), name_for("me", "default"));
        // Non-ASCII users do not collapse to one name.
        let names = [
            name_for("jürgen", "default"),
            name_for("jörg", "default"),
            name_for("山田", "default"),
            name_for("", "default"),
            name_for("a", "bdefault"),
            name_for("ab", "default"),
        ];
        for (i, a) in names.iter().enumerate() {
            assert!(a.len() == 21 && a.is_ascii(), "{a}");
            for b in &names[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// The pipe only lets this user in, and the client sees the server is this user.
    #[cfg(windows)]
    #[test]
    fn pipe_is_this_users() {
        let name = test_name("acl");
        let (_server, _rx) = served(&name);
        let (_pipe, server) = keel_vfs::pipe::connect(&name, ANSWER_WAIT).unwrap();
        assert_eq!(server, std::process::id());
        assert!(keel_vfs::pipe::same_user(server).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn socket_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = keel_api::socket::sock_path("keel-test").unwrap();
        let dir = path.parent().unwrap();
        let mode = std::fs::metadata(dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
