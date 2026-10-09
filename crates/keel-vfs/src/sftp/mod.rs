//! Synchronous SFTP provider. Call on a worker thread. A writer commits on `flush()`;
//! dropping it before a successful flush aborts the upload. Flush errors must be handled.
pub mod auth;
pub mod conn;
pub mod hostkeys;
use crate::{Caps, Entry, Kind, Progress, Provider, VPath};
use anyhow::{Context, Result};
pub use conn::ConnPool;
use crossbeam_channel::Sender;
pub use hostkeys::{add_known_host, known_hosts_check, HostKeyVerdict};
use russh_sftp::{
    client::error::Error as SftpError,
    protocol::{FileAttributes, OpenFlags, Packet, StatusCode},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RemoteHost {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: RemoteAuth,
    pub home: Option<String>,
    pub bookmarks: Vec<(String, String)>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RemoteAuth {
    Agent,
    KeyFile {
        path: PathBuf,
        passphrase_in_keyring: bool,
    },
    PasswordInKeyring,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConnStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed,
}
#[derive(Clone, Debug)]
pub enum RemoteEvent {
    Status {
        host_id: String,
        status: ConnStatus,
        detail: String,
    },
    HostKeyPrompt {
        host_id: String,
        fingerprint: String,
        reply: Sender<bool>,
    },
}
pub struct SftpProvider {
    host: RemoteHost,
    conn: Arc<ConnPool>,
}

fn wire_error(error: SftpError) -> anyhow::Error {
    // Never pass arbitrary server-supplied messages to logs/toasts.
    match error {
        SftpError::Status(status) => {
            let kind = match status.status_code {
                StatusCode::NoSuchFile => io::ErrorKind::NotFound,
                StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
                _ => io::ErrorKind::Other,
            };
            io::Error::new(kind, format!("SFTP status {:?}", status.status_code)).into()
        }
        _ => io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "SFTP connection or protocol failure",
        )
        .into(),
    }
}
/// A remote name made safe as a local file name (keeps it readable for "open with").
fn local_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| match c {
            c if c.is_control() || r#"<>:"/\|?*"#.contains(c) => '_',
            c => c,
        })
        .collect();
    let clean = clean.trim_end_matches([' ', '.']);
    let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.ends_with(|c: char| c.is_ascii_digit()));
    match clean {
        "" => "file".into(),
        c if reserved => format!("_{c}"),
        c => c.into(),
    }
}
/// A dropped or hung connection (as opposed to a server-side refusal such as ENOENT).
fn is_transport(e: &anyhow::Error) -> bool {
    e.downcast_ref::<tokio::time::error::Elapsed>().is_some()
        || e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::ConnectionAborted)
}
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

impl SftpProvider {
    pub fn new(host: RemoteHost, events: Sender<RemoteEvent>) -> Self {
        Self {
            conn: Arc::new(ConnPool::new(host.clone(), events)),
            host,
        }
    }
    pub fn status(&self) -> ConnStatus {
        self.conn.status()
    }
    pub fn connect(&self) -> Result<()> {
        self.conn.connect()
    }
    pub fn disconnect(&self) {
        self.conn.disconnect();
    }
    pub fn set_timeout(&self, timeout: Duration) {
        self.conn.set_timeout(timeout);
    }
    fn validate(&self, p: &VPath) -> Result<()> {
        anyhow::ensure!(
            p.scheme == "sftp"
                && p.authority == self.host.id
                && p.path.starts_with('/')
                && !p.path.contains('\0'),
            "invalid SFTP path: {}",
            p.display()
        );
        anyhow::ensure!(
            !self.host.id.is_empty()
                && self
                    .host
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "remote id must be a stable slug"
        );
        Ok(())
    }
    fn call<T>(
        &self,
        p: &VPath,
        f: impl AsyncFnOnce(Arc<conn::Session>) -> Result<T>,
    ) -> Result<T> {
        self.validate(p)?;
        let session = self.conn.session().with_context(|| p.display())?;
        self.conn
            .run(f(session))
            .inspect_err(|e| {
                if is_transport(e) {
                    self.conn.failed("connection lost");
                }
            })
            .with_context(|| p.display())
    }
    async fn entry(session: &conn::Session, p: VPath, attrs: FileAttributes) -> Result<Entry> {
        let is_link = attrs.file_type().is_symlink();
        let attrs = if is_link {
            match session.raw.stat(&p.path).await {
                Ok(a) => a.attrs,
                Err(SftpError::Status(s))
                    if s.status_code == StatusCode::NoSuchFile
                        || s.status_code == StatusCode::PermissionDenied =>
                {
                    attrs
                }
                Err(e) => return Err(wire_error(e)),
            }
        } else {
            attrs
        };
        let name = p.name().to_owned();
        let ext = name
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, ext)| ext.to_lowercase())
            .unwrap_or_default();
        Ok(Entry {
            kind: if attrs.file_type().is_dir() {
                Kind::Dir
            } else if attrs.file_type().is_symlink() {
                Kind::Symlink
            } else {
                Kind::File
            },
            size: attrs.size.unwrap_or(0),
            modified: attrs.modified().ok(),
            hidden: name.starts_with('.'),
            ext,
            is_link,
            encrypted: false,
            name,
            path: p,
        })
    }
    fn rename_with(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.validate(to)?;
        self.call(from, async |s| {
            rename(&s, &from.path, &to.path, replace).await
        })
        .with_context(|| to.display())
    }
    fn writer(&self, p: &VPath, exclusive: bool) -> Result<Box<dyn Write + Send>> {
        self.call(p, async |s| {
            if exclusive {
                match s.raw.lstat(&p.path).await {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "destination exists",
                        )
                        .into())
                    }
                    Err(SftpError::Status(e)) if e.status_code == StatusCode::NoSuchFile => {}
                    Err(e) => return Err(wire_error(e)),
                }
            }
            let partial = format!("{}.keel-partial", p.path);
            let handle = s
                .raw
                .open(
                    &partial,
                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                    FileAttributes {
                        permissions: Some(0o600),
                        ..Default::default()
                    },
                )
                .await
                .map_err(wire_error)
                .context("cannot stage the upload (a leftover .keel-partial may exist)")?
                .handle;
            Ok(Box::new(RemoteWriter {
                stream: RemoteReader {
                    pool: self.conn.clone(),
                    session: s,
                    handle: Some(handle),
                    offset: 0,
                    path: p.clone(),
                },
                partial,
                exclusive,
                committed: false,
                failed: false,
            }) as Box<dyn Write + Send>)
        })
    }
    /// Materialise with bounded memory. Cache identity includes endpoint, user, path, mtime and size.
    pub fn local_copy_with_progress(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
    ) -> Result<PathBuf> {
        let before = self.stat(p)?;
        anyhow::ensure!(
            before.kind == Kind::File,
            "not a regular file: {}",
            p.display()
        );
        let mut hash = Sha256::new();
        for part in [
            &self.host.host,
            &self.host.user,
            &self.host.port.to_string(),
            &p.path,
            &format!("{:?}:{}", before.modified, before.size),
        ] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        let cache = dirs::cache_dir()
            .context("cache directory unavailable")?
            // Spec 2.9 cache folder: `Keel` on Windows/macOS, `keel` on Linux.
            .join(if cfg!(target_os = "linux") {
                "keel"
            } else {
                "Keel"
            })
            .join("remote")
            .join(&self.host.id)
            .join(format!("{:x}", hash.finalize()));
        fs::create_dir_all(&cache)?;
        let target = cache.join(local_name(p.name()));
        if before.modified.is_some() && fs::metadata(&target).is_ok_and(|m| m.len() == before.size)
        {
            return Ok(target);
        }
        let mut temp = tempfile::NamedTempFile::new_in(&cache)?;
        let mut reader = self.read(p)?;
        let mut buf = vec![0; 1024 * 1024];
        let mut done = 0;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            temp.write_all(&buf[..n])?;
            done += n as u64;
            progress(Progress {
                done_bytes: done,
                total_bytes: before.size,
                current: p.display(),
                done_items: 0,
                total_items: 1,
            });
        }
        let after = self.stat(p)?;
        anyhow::ensure!(
            done == before.size && before.size == after.size && before.modified == after.modified,
            "source changed during download: {}",
            p.display()
        );
        temp.as_file().sync_all()?;
        temp.persist(&target).map_err(|e| e.error)?;
        progress(Progress {
            done_bytes: done,
            total_bytes: done,
            current: p.display(),
            done_items: 1,
            total_items: 1,
        });
        Ok(target)
    }
}

impl Provider for SftpProvider {
    fn scheme(&self) -> &'static str {
        "sftp"
    }
    fn caps(&self) -> Caps {
        Caps {
            write: true,
            rename: true,
            delete: true,
            watch: false,
        }
    }
    fn list(&self, p: &VPath) -> Result<Vec<Entry>> {
        self.call(p, async |s| {
            let handle = s.raw.opendir(&p.path).await.map_err(wire_error)?.handle;
            let result = async {
                let mut entries = Vec::new();
                loop {
                    match s.raw.readdir(&handle).await {
                        Ok(batch) => {
                            for file in batch.files {
                                if file.filename == "." || file.filename == ".." {
                                    continue;
                                }
                                anyhow::ensure!(
                                    valid_name(&file.filename),
                                    "invalid remote directory entry"
                                );
                                entries.push(
                                    Self::entry(&s, p.join(&file.filename), file.attrs).await?,
                                );
                            }
                        }
                        Err(SftpError::Status(e)) if e.status_code == StatusCode::Eof => break,
                        Err(e) => return Err(wire_error(e)),
                    }
                }
                Ok(entries)
            }
            .await;
            let closed = s.raw.close(handle).await.map_err(wire_error);
            let entries = result?;
            closed?;
            Ok(entries)
        })
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.call(p, async |s| {
            let attrs = s.raw.lstat(&p.path).await.map_err(wire_error)?.attrs;
            Self::entry(&s, p.clone(), attrs).await
        })
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        self.call(p, async |s| {
            let handle = s
                .raw
                .open(&p.path, OpenFlags::READ, FileAttributes::empty())
                .await
                .map_err(wire_error)?
                .handle;
            Ok(Box::new(RemoteReader {
                pool: self.conn.clone(),
                session: s,
                handle: Some(handle),
                offset: 0,
                path: p.clone(),
            }) as Box<dyn Read + Send>)
        })
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.writer(p, false)
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.writer(p, true)
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.call(p, async |s| {
            s.raw
                .mkdir(&p.path, FileAttributes::empty())
                .await
                .map_err(wire_error)?;
            Ok(())
        })
    }
    /// Never replaces an existing target (same contract as the local provider).
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename_with(from, to, false)
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename_with(from, to, false)
    }
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename_with(from, to, true)
    }
    fn canonicalize(&self, p: &VPath) -> Result<VPath> {
        self.call(p, async |s| {
            let result = s.raw.realpath(&p.path).await.map_err(wire_error)?;
            let path = result
                .files
                .first()
                .context("empty realpath reply")?
                .filename
                .clone();
            Ok(VPath { path, ..p.clone() })
        })
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        let entry = self.stat(p)?;
        if entry.kind == Kind::Dir && !entry.is_link {
            for child in self.list(p)? {
                self.remove(&child.path)?;
            }
        }
        self.call(p, async |s| {
            if entry.kind == Kind::Dir && !entry.is_link {
                s.raw.rmdir(&p.path).await
            } else {
                s.raw.remove(&p.path).await
            }
            .map_err(wire_error)?;
            Ok(())
        })
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        self.local_copy_with_progress(p, &|_| {})
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        self.call(p, async |s| {
            s.raw.rmdir(&p.path).await.map_err(wire_error)?;
            Ok(())
        })
    }
}

/// `replace`: POSIX rename over an existing target (needs the OpenSSH extension, else the
/// plain SFTP rename refuses an existing target). Otherwise never replaces.
async fn rename(s: &conn::Session, from: &str, to: &str, replace: bool) -> Result<()> {
    if !replace || !s.posix_rename {
        s.raw.rename(from, to).await.map_err(wire_error)?;
    } else {
        let mut bytes = Vec::new();
        for path in [from, to] {
            bytes.extend_from_slice(&(path.len() as u32).to_be_bytes());
            bytes.extend_from_slice(path.as_bytes());
        }
        match s
            .raw
            .extended("posix-rename@openssh.com", bytes)
            .await
            .map_err(wire_error)?
        {
            Packet::Status(s) if s.status_code == StatusCode::Ok => {}
            Packet::Status(s) => return Err(wire_error(SftpError::Status(s))),
            _ => anyhow::bail!("invalid rename reply"),
        }
    }
    Ok(())
}

/// One SFTP request moves at most this much; up to `PIPELINE` requests are in flight.
const CHUNK: usize = 32 * 1024;
const PIPELINE: usize = 32;

struct RemoteReader {
    pool: Arc<ConnPool>,
    session: Arc<conn::Session>,
    handle: Option<String>,
    offset: u64,
    path: VPath,
}
impl RemoteReader {
    fn operation<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> io::Result<T> {
        self.pool.run(future).map_err(|e| {
            if is_transport(&e) {
                self.pool.failed("transfer interrupted");
            }
            io::Error::other(format!("{}: {e}", self.path.display()))
        })
    }
    fn handle(&self) -> io::Result<String> {
        self.handle
            .clone()
            .ok_or_else(|| io::Error::other("stream closed"))
    }
    fn close(&mut self) -> io::Result<()> {
        if let Some(handle) = self.handle.take() {
            self.operation(async {
                self.session.raw.close(handle).await.map_err(wire_error)?;
                Ok(())
            })?;
        }
        Ok(())
    }
}
impl Read for RemoteReader {
    /// Pipelined: one call issues up to 1 MiB of 32 KiB requests and returns the
    /// contiguous prefix (a short reply ends it; the next call resumes there).
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min(CHUNK * PIPELINE);
        if want == 0 {
            return Ok(0);
        }
        let handle = self.handle()?;
        let (session, offset) = (self.session.clone(), self.offset);
        let replies = self.operation(async move {
            let tasks: Vec<_> = (0..want.div_ceil(CHUNK))
                .map(|i| {
                    let (s, h) = (session.clone(), handle.clone());
                    let len = CHUNK.min(want - i * CHUNK);
                    let at = offset + (i * CHUNK) as u64;
                    tokio::spawn(async move {
                        match s.raw.read(h, at, len as u32).await {
                            Ok(data) => Ok((data.data, len)),
                            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => {
                                Ok((Vec::new(), len))
                            }
                            Err(e) => Err(wire_error(e)),
                        }
                    })
                })
                .collect();
            let mut replies = Vec::with_capacity(tasks.len());
            for task in tasks {
                replies.push(task.await??);
            }
            Ok(replies)
        })?;
        let mut n = 0;
        for (data, asked) in replies {
            if data.len() > asked {
                return Err(io::Error::other("oversized SFTP reply"));
            }
            buf[n..n + data.len()].copy_from_slice(&data);
            n += data.len();
            if data.len() < asked {
                break;
            }
        }
        self.offset += n as u64;
        Ok(n)
    }
}
impl Drop for RemoteReader {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
struct RemoteWriter {
    stream: RemoteReader,
    partial: String,
    exclusive: bool,
    committed: bool,
    failed: bool,
}
impl Write for RemoteWriter {
    /// Pipelined like `read`: up to 1 MiB per call, all chunks acknowledged before returning.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.committed || self.failed {
            return Err(io::Error::other("upload finished or failed"));
        }
        let n = bytes.len().min(CHUNK * PIPELINE);
        if n == 0 {
            return Ok(0);
        }
        let handle = self.stream.handle()?;
        let (session, offset) = (self.stream.session.clone(), self.stream.offset);
        let chunks: Vec<Vec<u8>> = bytes[..n].chunks(CHUNK).map(<[u8]>::to_vec).collect();
        let result = self.stream.operation(async move {
            let tasks: Vec<_> = chunks
                .into_iter()
                .enumerate()
                .map(|(i, data)| {
                    let (s, h) = (session.clone(), handle.clone());
                    let at = offset + (i * CHUNK) as u64;
                    tokio::spawn(async move { s.raw.write(h, at, data).await.map_err(wire_error) })
                })
                .collect();
            for task in tasks {
                task.await??;
            }
            Ok(())
        });
        if let Err(e) = result {
            self.failed = true;
            return Err(e);
        }
        self.stream.offset += n as u64;
        Ok(n)
    }
    /// Commits: closes the staged `.keel-partial`, verifies its size, renames it into place.
    fn flush(&mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        if self.failed {
            return Err(io::Error::other("upload failed"));
        }
        let result = (|| {
            self.stream.close()?;
            self.stream.operation(async {
                let size = self
                    .stream
                    .session
                    .raw
                    .lstat(&self.partial)
                    .await
                    .map_err(wire_error)?
                    .attrs
                    .size;
                anyhow::ensure!(size == Some(self.stream.offset), "upload size mismatch");
                rename(
                    &self.stream.session,
                    &self.partial,
                    &self.stream.path.path,
                    !self.exclusive,
                )
                .await
            })
        })();
        match result {
            Ok(()) => {
                self.committed = true;
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }
}
impl Drop for RemoteWriter {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.stream.close();
            // Reconnect for cleanup after a transport failure. Only our own partial is removed.
            if let Ok(s) = self.stream.pool.session() {
                let _ = self.stream.pool.run(async {
                    s.raw.remove(&self.partial).await.map_err(wire_error)?;
                    Ok(())
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_cache_names_are_safe_and_readable() {
        assert_eq!(super::local_name("report 2026.pdf"), "report 2026.pdf");
        assert_eq!(super::local_name("a:b?.txt. "), "a_b_.txt");
        assert_eq!(super::local_name("con.txt"), "_con.txt");
        assert_eq!(super::local_name("COM1"), "_COM1");
        assert_eq!(super::local_name("..."), "file");
    }
}
