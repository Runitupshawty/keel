//! Per-user local sockets (the single instance and the daemon).
//!
//! Only the same user gets through. Windows: a named pipe whose DACL grants only the
//! user's SID; the client refuses impersonation and checks that the pipe's server runs as
//! the same user (`keel_vfs::pipe`). Unix: a socket file in a 0700 folder of the user's
//! (`$XDG_RUNTIME_DIR/keel-<uid>`, else `<temp>/keel-<uid>`), and both ends check the
//! peer's uid (`SO_PEERCRED` / `LOCAL_PEERCRED`).

use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

pub use interprocess::local_socket::Listener;

/// `<prefix>-<hash of the user's SID (Windows) or uid (Unix) and key>`: per user, and per
/// profile (the key). Hashed (FNV-1a, stable across builds) so a Unix socket path stays
/// short.
pub fn name(prefix: &str, key: &str) -> String {
    name_for(prefix, &user_id(), key)
}

/// As [`name`], salted with this user's random `socket.salt` in `config_dir` (created
/// owner-only on first use): another local user cannot predict the name, so cannot take
/// it first.
pub fn salted_name(prefix: &str, key: &str, config_dir: &Path) -> String {
    salted_name_for(prefix, &user_id(), &salt(config_dir), key)
}

pub fn name_for(prefix: &str, user: &str, key: &str) -> String {
    salted_name_for(prefix, user, "", key)
}

pub fn salted_name_for(prefix: &str, user: &str, salt: &str, key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let bytes = user.bytes().chain([0]).chain(salt.bytes()).chain([0]);
    for b in bytes.chain(key.bytes()) {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{prefix}-{hash:016x}")
}

/// The user's SID / uid (never an environment variable another process could set).
fn user_id() -> String {
    #[cfg(windows)]
    let id = keel_vfs::pipe::user_sid().unwrap_or_else(|e| {
        tracing::warn!("no user SID: {e}");
        String::new()
    });
    #[cfg(unix)]
    let id = unsafe { libc::geteuid() }.to_string();
    id
}

/// The 128-bit hex salt in `<dir>/socket.salt`, created when missing; one that others may
/// access (or a damaged one) is replaced. Empty when none can be had (every process of
/// the user then agrees on the unsalted name).
fn salt(dir: &Path) -> String {
    let path = dir.join("socket.salt");
    for _ in 0..40 {
        match crate::private::read(&path) {
            Ok(Some(s)) if s.len() == 32 && s.iter().all(u8::is_ascii_hexdigit) => {
                return String::from_utf8_lossy(&s).into_owned();
            }
            // Another process created it and is about to write it.
            Ok(Some(s)) if s.is_empty() => {
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
            Ok(_) => {
                tracing::warn!("{} is damaged or not private: replacing it", path.display());
                let _ = std::fs::remove_file(&path);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("{}: {e}", path.display());
                return String::new();
            }
        }
        let _ = std::fs::create_dir_all(dir);
        match crate::private::create_new(&path) {
            Ok(mut f) => {
                let Ok(salt) = crate::plans::random_id() else {
                    return String::new();
                };
                return match f.write_all(salt.as_bytes()) {
                    Ok(()) => salt,
                    Err(_) => String::new(),
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                tracing::warn!("{}: {e}", path.display());
                return String::new();
            }
        }
    }
    String::new()
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
    fn names_are_per_user_salt_and_key() {
        let n = |prefix, user, salt, key| salted_name_for(prefix, user, salt, key);
        assert_eq!(
            n("p", "S-1-5-21-1", "s", "a"),
            n("p", "S-1-5-21-1", "s", "a")
        );
        assert_ne!(
            n("p", "S-1-5-21-1", "s", "a"),
            n("p", "S-1-5-21-2", "s", "a")
        );
        assert_ne!(
            n("p", "S-1-5-21-1", "s", "a"),
            n("p", "S-1-5-21-1", "t", "a")
        );
        assert_ne!(
            n("p", "S-1-5-21-1", "s", "a"),
            n("p", "S-1-5-21-1", "s", "b")
        );
        assert_ne!(
            n("p", "S-1-5-21-1", "s", "a"),
            n("q", "S-1-5-21-1", "s", "a")
        );
        assert_ne!(
            n("p", "u", "sa", "b"),
            n("p", "u", "s", "ab"),
            "fields are separated"
        );
        assert_eq!(n("keel", "x", "", "y"), name_for("keel", "x", "y"));
        assert_eq!(name_for("keel", "x", "y").len(), 21);
        assert!(!user_id().is_empty());
    }

    #[test]
    fn the_salt_is_random_private_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = salted_name("p", "k", dir.path());
        assert_eq!(a, salted_name("p", "k", dir.path()), "kept");
        let salt = std::fs::read_to_string(dir.path().join("socket.salt")).unwrap();
        assert_eq!(salt.len(), 32);
        assert!(crate::private::read(&dir.path().join("socket.salt"))
            .unwrap()
            .is_some());
        let other = tempfile::tempdir().unwrap();
        assert_ne!(a, salted_name("p", "k", other.path()), "per salt");
        assert_ne!(a, name("p", "k"));
        // A salt others could read or have planted is replaced.
        let planted = tempfile::tempdir().unwrap();
        std::fs::write(planted.path().join("socket.salt"), &salt).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(0o644);
            std::fs::set_permissions(planted.path().join("socket.salt"), perm).unwrap();
        }
        assert_ne!(a, salted_name("p", "k", planted.path()));
    }
}
