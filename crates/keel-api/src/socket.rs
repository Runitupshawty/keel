//! Per-user local sockets (the single instance and the daemon).
//!
//! Only the same user gets through. Windows: a named pipe whose DACL grants only the
//! user's SID; the client refuses impersonation and checks that the pipe's server runs as
//! the same user (`keel_vfs::pipe`). Unix: a socket file in a 0700 folder of the user's
//! (`$XDG_RUNTIME_DIR/keel-<uid>`, else `<temp>/keel-<uid>`), and both ends check the
//! peer's uid (`SO_PEERCRED` / `LOCAL_PEERCRED`).

use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use std::io::{self, Read, Write};
use std::time::Duration;

pub use interprocess::local_socket::Listener;

/// `<prefix>-<hash of user and key>`: per user, and per profile (the key). Both are hashed
/// (FNV-1a, stable across builds): user names may be anything (non-ASCII, spaces), and a
/// Unix socket path must stay short.
pub fn name(prefix: &str, key: &str) -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    name_for(prefix, &user, key)
}

pub fn name_for(prefix: &str, user: &str, key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in user.bytes().chain([0]).chain(key.bytes()) {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{prefix}-{hash:016x}")
}

/// Windows: a named pipe (its namespace refuses a second listener). Unix: a socket file in
/// the user's private folder; a stale one is removed deliberately (never overwritten while
/// an instance is alive).
pub fn sock_name(name: &str) -> io::Result<Name<'static>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        name.to_owned().to_ns_name::<GenericNamespaced>()
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        sock_path(name)?.to_fs_name::<GenericFilePath>()
    }
}

/// `<dir>/<name>.sock`, `<dir>` created 0700 when missing and refused unless it is a real
/// folder of this user that nobody else may enter.
#[cfg(unix)]
pub fn sock_path(name: &str) -> io::Result<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = unsafe { libc::geteuid() };
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("keel-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private folder of this user", dir.display()),
        ));
    }
    Ok(dir.join(format!("{name}.sock")))
}

/// The other end of `conn` runs as this user.
#[cfg(unix)]
pub fn check_peer(conn: &Stream) -> io::Result<()> {
    let euid = conn.peer_creds()?.euid();
    match euid == Some(unsafe { libc::geteuid() }) {
        true => Ok(()),
        false => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("peer runs as uid {euid:?}"),
        )),
    }
}

/// Removes a socket file nobody answers on (a crashed instance). No-op on Windows.
pub fn remove_stale(name: &str) {
    #[cfg(unix)]
    if let Ok(path) = sock_path(name) {
        let _ = std::fs::remove_file(path);
    }
    #[cfg(windows)]
    let _ = name;
}

/// Listens on `name`, never taking over a socket another live instance owns.
pub fn listen(name: &str) -> io::Result<Listener> {
    let options = ListenerOptions::new()
        .name(sock_name(name)?)
        .try_overwrite(false);
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::{
            local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
        };
        let sddl = widestring::U16CString::from_str(keel_vfs::pipe::user_only_sddl()?)
            .map_err(io::Error::other)?;
        options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
    };
    options.create_sync()
}

/// `listen`, after clearing a stale socket file of a crashed instance.
pub fn bind(name: &str) -> io::Result<Listener> {
    match listen(name) {
        #[cfg(unix)]
        Err(e)
            if e.kind() == io::ErrorKind::AddrInUse
                && Stream::connect(sock_name(name)?)
                    .is_err_and(|c| c.kind() == io::ErrorKind::ConnectionRefused) =>
        {
            remove_stale(name);
            listen(name)
        }
        listened => listened,
    }
}

pub trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}

/// Connects to the server on `name`, which must run as this user, waiting up to `timeout`
/// while it is busy. Returns the connection and (Windows) the server's process id.
pub fn connect(name: &str, timeout: Duration) -> io::Result<(Box<dyn Conn>, Option<u32>)> {
    #[cfg(windows)]
    {
        let (pipe, server) = keel_vfs::pipe::connect(name, timeout)?;
        Ok((Box::new(pipe), Some(server)))
    }
    #[cfg(unix)]
    {
        let _ = timeout; // connecting to a Unix socket does not wait
        let conn = Stream::connect(sock_name(name)?)?;
        check_peer(&conn)?;
        Ok((Box::new(conn), None))
    }
}

/// Server side: refuses a client of another user (Unix; on Windows the DACL does).
pub fn check_client(conn: &Stream) -> io::Result<()> {
    #[cfg(unix)]
    return check_peer(conn);
    #[cfg(windows)]
    {
        let _ = conn;
        Ok(())
    }
}

/// Cancels the pending read on a server-side connection after a timeout unless dropped
/// first (named pipes have no read timeout; Unix sockets use `set_recv_timeout`).
#[cfg(windows)]
pub struct Watchdog {
    armed: std::sync::Arc<parking_lot::Mutex<bool>>,
    _disarm: crossbeam_channel::Sender<()>,
}

#[cfg(windows)]
impl Watchdog {
    pub fn arm(conn: &Stream, after: Duration) -> Self {
        use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
        let Stream::NamedPipe(pipe) = conn;
        // As an integer: raw handles are not Send. Valid while `armed` (the stream outlives
        // the watchdog, which is dropped before it).
        let raw = pipe.inner().as_handle().as_raw_handle() as usize;
        let armed = std::sync::Arc::new(parking_lot::Mutex::new(true));
        let (tx, rx) = crossbeam_channel::bounded::<()>(0);
        let flag = armed.clone();
        std::thread::spawn(move || {
            if rx.recv_timeout(after).is_err_and(|e| e.is_timeout()) {
                let armed = flag.lock();
                if *armed {
                    let pipe = unsafe { BorrowedHandle::borrow_raw(raw as _) };
                    keel_vfs::pipe::cancel_io(pipe);
                }
            }
        });
        Self { armed, _disarm: tx }
    }
}

#[cfg(windows)]
impl Drop for Watchdog {
    fn drop(&mut self) {
        *self.armed.lock() = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_per_user_and_key() {
        assert_eq!(name_for("p", "james", "a"), name_for("p", "james", "a"));
        assert_ne!(name_for("p", "james", "a"), name_for("p", "jim", "a"));
        assert_ne!(name_for("p", "james", "a"), name_for("q", "james", "a"));
        assert_eq!(name_for("keel", "x", "y").len(), 21);
    }
}
