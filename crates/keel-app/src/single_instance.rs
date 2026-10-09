//! One Keel per user (Task 24): the first instance listens on a local socket named
//! `keel-<user>` (a named pipe on Windows, an abstract socket on Linux, `/tmp/keel-<user>`
//! on macOS); a later `keel [FOLDER] [--search Q]` sends its request there and exits.
//!
//! Protocol: one JSON `Request` line from the client, `ok` back once it was taken.

use crate::cli::Request;
use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::Duration;

pub use interprocess::local_socket::Listener;

/// How long a client waits for a running instance to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(3);
/// Longest request line read (a path and a query).
const MAX_LINE: u64 = 64 * 1024;

/// The socket name: per user, and per profile when not the default one.
pub fn name(profile: &str) -> String {
    let user: String = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    match profile {
        "default" => format!("keel-{user}"),
        p => format!("keel-{user}-{p}"),
    }
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

/// Windows (named pipe) and Linux (abstract socket) have a namespace that refuses a second
/// listener; other Unixes get an explicit socket file under the temp dir so a stale file can be
/// removed deliberately (never overwritten while an instance is alive).
fn sock_name(name: &str) -> io::Result<Name<'_>> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        name.to_ns_name::<interprocess::local_socket::GenericNamespaced>()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        use interprocess::local_socket::GenericFilePath;
        sock_path(name).to_fs_name::<GenericFilePath>()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sock_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{name}.sock"))
}

/// Removes a socket file nobody answers on (a crashed instance). No-op elsewhere.
fn remove_stale(name: &str) {
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = std::fs::remove_file(sock_path(name));
    }
    #[cfg(any(windows, target_os = "linux"))]
    {
        let _ = name;
    }
}

fn listen(name: &str) -> io::Result<Listener> {
    ListenerOptions::new()
        .name(sock_name(name)?)
        // Never take over a socket another live instance owns.
        .try_overwrite(false)
        .create_sync()
}

/// Sends `req` and waits up to `ANSWER_WAIT` for the `ok`.
fn send(name: &str, req: &Request) -> io::Result<()> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    let mut conn = Stream::connect(sock_name(name)?)?;
    // The running instance may take the foreground (the user just started this one).
    #[cfg(windows)]
    keel_vfs::desktop::allow_foreground_any();
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let answer = (|| {
            conn.write_all(line.as_bytes())?;
            let mut ok = String::new();
            BufReader::new(conn).read_line(&mut ok)?;
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

/// Accepts requests on a worker for the life of the process; `on_request` runs there.
pub fn serve(listener: Listener, on_request: impl Fn(Request) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("keel-instance".into())
        .spawn(move || {
            // ponytail: one client at a time; a client that connects and never writes
            // holds the next ones off (same user only), threads per client if that matters.
            for conn in listener.incoming() {
                let conn = match conn {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("single instance accept: {e}");
                        continue;
                    }
                };
                let mut reader = BufReader::new(conn);
                let mut line = String::new();
                if let Err(e) = (&mut reader).take(MAX_LINE).read_line(&mut line) {
                    tracing::warn!("single instance read: {e}");
                    continue;
                }
                match serde_json::from_str::<Request>(&line) {
                    Ok(req) => {
                        let _ = reader.get_mut().write_all(b"ok\n");
                        on_request(req);
                    }
                    Err(e) => tracing::warn!("single instance: bad request: {e}"),
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!("spawn keel-instance: {e}");
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

    #[test]
    fn second_instance_hands_its_request_over() {
        let name = format!("keel-test-{}", std::process::id());
        let req = Request {
            folder: Some(std::env::temp_dir()),
            search: Some("needle".into()),
        };
        // Nothing listens yet: the first claim becomes the server.
        let Claim::Server(listener) = claim(&name, &req, true) else {
            panic!("first instance should serve");
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        serve(listener, move |r| tx.send(r).unwrap());
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

    #[test]
    fn names_are_per_user_and_profile() {
        let default = name("default");
        assert!(default.starts_with("keel-"));
        assert_eq!(name("work"), format!("{default}-work"));
    }
}
