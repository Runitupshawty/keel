//! Synchronous SFTP provider. Call on a worker thread. Uploads are `SftpUpload`s: they
//! commit on `finish()` (or `flush()` through `Box<dyn Write>`); dropped before that, they
//! log an error and remove their staging file. Commit errors must be handled.
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
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    /// Folders already swept for stale staging files this session.
    swept: parking_lot::Mutex<HashSet<String>>,
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

/// The SFTP library decodes names lossily: a name that is not UTF-8 on the server lists
/// with U+FFFD and cannot be addressed. Such entries are refused with this message.
const UNDECODABLE: char = '\u{FFFD}';
fn undecodable(p: &VPath) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: the name is not valid UTF-8 on the server (shown with \u{FFFD}); rename it on \
         the host to use it here",
        p.display()
    )
}

/// Requests in flight while resolving the symlinks of one listing.
const LINKS_IN_FLIGHT: usize = 32;
/// Staging files for uploads: `<name>.keel-partial-<pid>-<n>`, the names `ops::transfer`
/// stages under too (`ops::is_partial` recognises both).
const PARTIAL: &str = ".keel-partial";
const STALE_PARTIAL: Duration = Duration::from_secs(24 * 3600);
use crate::ops::is_partial;
/// A staging path unique to this attempt, its name capped at 255 bytes (NAME_MAX).
pub(crate) fn partial_path(target: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (dir, name) = target.rsplit_once('/').unwrap_or(("", target));
    let suffix = format!(
        "{PARTIAL}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut keep = name.len().min(255 - suffix.len());
    while !name.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{dir}/{}{suffix}", &name[..keep])
}

impl SftpProvider {
    pub fn new(host: RemoteHost, events: Sender<RemoteEvent>) -> Self {
        Self::with_prompt_listener(host, events, Arc::new(AtomicBool::new(true)))
    }
    /// `listener`: whether anything answers host key prompts (see `Router`).
    pub fn with_prompt_listener(
        host: RemoteHost,
        events: Sender<RemoteEvent>,
        listener: Arc<AtomicBool>,
    ) -> Self {
        Self {
            conn: Arc::new(ConnPool::with_prompt_listener(
                host.clone(),
                events,
                listener,
            )),
            host,
            swept: parking_lot::Mutex::default(),
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
        if p.path.contains(UNDECODABLE) {
            return Err(undecodable(p));
        }
        Ok(())
    }
    /// One operation under the pool's overall timeout.
    fn call<T>(
        &self,
        p: &VPath,
        f: impl AsyncFnOnce(Arc<conn::Session>) -> Result<T>,
    ) -> Result<T> {
        self.call_with(p, true, f)
    }
    /// `bounded` false: no overall deadline, only the per-request timeout of the session,
    /// so a slow but progressing operation is never mistaken for a dead connection.
    fn call_with<T>(
        &self,
        p: &VPath,
        bounded: bool,
        f: impl AsyncFnOnce(Arc<conn::Session>) -> Result<T>,
    ) -> Result<T> {
        self.validate(p)?;
        let session = self.conn.session().with_context(|| p.display())?;
        let future = f(session);
        if bounded {
            self.conn.run(future)
        } else {
            self.conn.run_per_request(future)
        }
        .inspect_err(|e| {
            if is_transport(e) {
                self.conn.failed("connection lost");
            }
        })
        .with_context(|| p.display())
    }
    /// `attrs`: lstat attributes, or a link target's when `is_link`.
    fn entry(p: VPath, attrs: &FileAttributes, is_link: bool) -> Entry {
        let name = p.name().to_owned();
        let ext = name
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, ext)| ext.to_lowercase())
            .unwrap_or_default();
        Entry {
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
        }
    }
    fn rename_with(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.validate(to)?;
        self.call(from, async |s| {
            rename(&s, &from.path, &to.path, replace).await
        })
        .with_context(|| to.display())
    }
    /// Starts an upload to `p` (replacing it on `finish()` unless `exclusive`). Streams
    /// to a uniquely named staging file created with the server's default permissions.
    pub fn upload(&self, p: &VPath, exclusive: bool) -> Result<SftpUpload> {
        self.validate(p)?;
        let dir = match p.path.rsplit_once('/') {
            Some(("", _)) | None => "/".to_owned(),
            Some((dir, _)) => dir.to_owned(),
        };
        if self.swept.lock().insert(dir.clone()) {
            let folder = VPath {
                path: dir.clone(),
                ..p.clone()
            };
            // Best effort; a transport failure still surfaces through the upload below.
            let _ = self.call_with(&folder, false, async |s| {
                sweep_partials(&s, &dir).await;
                Ok(())
            });
        }
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
            let partial = partial_path(&p.path);
            // No explicit mode: the server's umask applies, as for any new file; replacing
            // an existing file copies its mode at commit (see `rename`).
            let handle = s
                .raw
                .open(
                    &partial,
                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                    FileAttributes::empty(),
                )
                .await
                .map_err(wire_error)
                .context("cannot stage the upload")?
                .handle;
            Ok(SftpUpload {
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
            })
        })
    }
    /// Materialise with bounded memory. Cache identity includes endpoint, user, path, mtime and size.
    pub fn local_copy_with_progress(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
    ) -> Result<PathBuf> {
        self.download(p, progress, &AtomicBool::new(false))
    }
    /// Downloads into the remote cache (LRU by bytes, `DOWNLOAD_BUDGET` over all hosts).
    fn download(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let port = self.host.port.to_string();
        cached_download(
            self,
            p,
            &self.host.id,
            &[&self.host.host, &self.host.user, &port],
            progress,
            cancel,
        )
    }
}

/// Materialises `p` under `<cache>/remote/<namespace>/<hash>/` (LRU by bytes over all
/// remotes, `DOWNLOAD_BUDGET`). The hash covers `identity` (where the file lives), the path,
/// mtime and size, so a changed file downloads again. Shared by the SFTP and cloud providers.
pub(crate) fn cached_download(
    provider: &dyn Provider,
    p: &VPath,
    namespace: &str,
    identity: &[&str],
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let before = provider.stat(p)?;
    anyhow::ensure!(
        before.kind == Kind::File,
        "not a regular file: {}",
        p.display()
    );
    let mut hash = Sha256::new();
    let stamp = format!("{:?}:{}", before.modified, before.size);
    for part in identity.iter().copied().chain([p.path.as_str(), &stamp]) {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let root = dirs::cache_dir()
        .context("cache directory unavailable")?
        // Spec 2.9 cache folder: `Keel` on Windows/macOS, `keel` on Linux.
        .join(if cfg!(target_os = "linux") {
            "keel"
        } else {
            "Keel"
        })
        .join("remote");
    let cache = root.join(namespace).join(format!("{:x}", hash.finalize()));
    fs::create_dir_all(&cache)?;
    let target = cache.join(local_name(p.name()));
    if before.modified.is_some() && fs::metadata(&target).is_ok_and(|m| m.len() == before.size) {
        DOWNLOADS
            .lock()
            .record(&root, &cache, before.size, DOWNLOAD_BUDGET);
        return Ok(target);
    }
    let mut temp = tempfile::NamedTempFile::new_in(&cache)?;
    let mut reader = provider.read(p)?;
    let mut buf = vec![0; 1024 * 1024];
    let mut done = 0;
    loop {
        anyhow::ensure!(
            !cancel.load(Ordering::Relaxed),
            "download cancelled: {}",
            p.display()
        );
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
    let after = provider.stat(p)?;
    anyhow::ensure!(
        done == before.size && before.size == after.size && before.modified == after.modified,
        "source changed during download: {}",
        p.display()
    );
    temp.as_file().sync_all()?;
    temp.persist(&target).map_err(|e| e.error)?;
    DOWNLOADS
        .lock()
        .record(&root, &cache, done, DOWNLOAD_BUDGET);
    progress(Progress {
        done_bytes: done,
        total_bytes: done,
        current: p.display(),
        done_items: 1,
        total_items: 1,
    });
    Ok(target)
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
    /// Names whose bytes are not UTF-8 on the server list with U+FFFD; every operation on
    /// them is refused with a clear error (see `UNDECODABLE`).
    fn list(&self, p: &VPath) -> Result<Vec<Entry>> {
        // No overall deadline: a 50k-entry folder or thousands of links take a while, and
        // each request is still bounded by the session's request timeout.
        self.call_with(p, false, async |s| {
            let files = read_dir(&s, &p.path).await?;
            let mut items: Vec<(VPath, FileAttributes, bool)> = files
                .into_iter()
                .map(|(name, attrs)| (p.join(&name), attrs, false))
                .collect();
            resolve_links(&s, &mut items).await?;
            Ok(items
                .into_iter()
                .map(|(path, attrs, is_link)| Self::entry(path, &attrs, is_link))
                .collect())
        })
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.call(p, async |s| {
            let attrs = s.raw.lstat(&p.path).await.map_err(wire_error)?.attrs;
            if !attrs.file_type().is_symlink() {
                return Ok(Self::entry(p.clone(), &attrs, false));
            }
            let target = link_target(&s, p.path.clone()).await?;
            Ok(Self::entry(
                p.clone(),
                target.as_ref().unwrap_or(&attrs),
                true,
            ))
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
    /// An `SftpUpload`: `flush()` commits it (see `Provider::write`).
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.upload(p, false)?))
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.upload(p, true)?))
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
    /// Lists the whole tree first: an entry that cannot be addressed (a non-UTF-8 name)
    /// refuses the delete before anything is removed, instead of stopping halfway.
    fn remove(&self, p: &VPath) -> Result<()> {
        let entry = self.stat(p)?;
        let mut doomed = Vec::new();
        self.collect_tree(&entry, &mut doomed, 0)?;
        for (path, dir) in doomed {
            self.call(&path, async |s| {
                if dir {
                    s.raw.rmdir(&path.path).await
                } else {
                    s.raw.remove(&path.path).await
                }
                .map_err(wire_error)?;
                Ok(())
            })?;
        }
        Ok(())
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        self.download(p, &|_| {}, &AtomicBool::new(false))
    }
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        self.download(p, progress, cancel)
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        self.call(p, async |s| {
            s.raw.rmdir(&p.path).await.map_err(wire_error)?;
            Ok(())
        })
    }
}

impl SftpProvider {
    /// Post-order (children before their folder): `(path, is a real folder)`.
    fn collect_tree(
        &self,
        entry: &Entry,
        out: &mut Vec<(VPath, bool)>,
        depth: usize,
    ) -> Result<()> {
        anyhow::ensure!(
            depth < 256,
            "directory nesting limit: {}",
            entry.path.display()
        );
        let dir = entry.kind == Kind::Dir && !entry.is_link;
        if dir {
            for child in self.list(&entry.path)? {
                if child.name.contains(UNDECODABLE) {
                    return Err(undecodable(&child.path).context("nothing was deleted"));
                }
                self.collect_tree(&child, out, depth + 1)?;
            }
        }
        out.push((entry.path.clone(), dir));
        Ok(())
    }
}

/// Every entry of `dir` except `.` and `..`, in server order.
async fn read_dir(s: &conn::Session, dir: &str) -> Result<Vec<(String, FileAttributes)>> {
    let handle = s.raw.opendir(dir).await.map_err(wire_error)?.handle;
    let result = async {
        let mut files = Vec::new();
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
                        files.push((file.filename, file.attrs));
                    }
                }
                Err(SftpError::Status(e)) if e.status_code == StatusCode::Eof => break,
                Err(e) => return Err(wire_error(e)),
            }
        }
        Ok(files)
    }
    .await;
    let closed = s.raw.close(handle).await.map_err(wire_error);
    let files = result?;
    closed?;
    Ok(files)
}
/// A link's target attributes; `None` for a dangling or unreadable target.
async fn link_target(s: &conn::Session, path: String) -> Result<Option<FileAttributes>> {
    match s.raw.stat(path).await {
        Ok(a) => Ok(Some(a.attrs)),
        Err(SftpError::Status(e))
            if e.status_code == StatusCode::NoSuchFile
                || e.status_code == StatusCode::PermissionDenied =>
        {
            Ok(None)
        }
        Err(e) => Err(wire_error(e)),
    }
}
/// Replaces each symlink's attributes with its target's (marking it a link), with up to
/// `LINKS_IN_FLIGHT` stat requests outstanding.
async fn resolve_links(
    s: &Arc<conn::Session>,
    items: &mut [(VPath, FileAttributes, bool)],
) -> Result<()> {
    let mut links = items
        .iter()
        .enumerate()
        .filter(|(_, (_, attrs, _))| attrs.file_type().is_symlink())
        .map(|(i, (path, _, _))| (i, path.path.clone()))
        .collect::<Vec<_>>()
        .into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        while tasks.len() < LINKS_IN_FLIGHT {
            let Some((i, path)) = links.next() else {
                break;
            };
            let s = s.clone();
            tasks.spawn(async move { (i, link_target(&s, path).await) });
        }
        let Some(done) = tasks.join_next().await else {
            return Ok(());
        };
        let (i, target) = done?;
        if let Some(attrs) = target? {
            items[i].1 = attrs;
        }
        items[i].2 = true;
    }
}
/// Removes this tool's staging files older than a day from `dir` (left by a crash or a
/// lost connection). Best effort: errors are ignored.
async fn sweep_partials(s: &conn::Session, dir: &str) {
    let Ok(files) = read_dir(s, dir).await else {
        return;
    };
    let now = SystemTime::now();
    for (name, attrs) in files {
        let old = attrs
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > STALE_PARTIAL);
        if old && attrs.file_type().is_file() && is_partial(&name) {
            let path = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            let _ = s.raw.remove(path).await;
        }
    }
}

/// `replace`: POSIX rename over an existing target (needs the OpenSSH extension, else the
/// plain SFTP rename refuses an existing target); the replaced file's permission bits
/// (`0o777`, never setuid/setgid/sticky) are copied onto `from` first. Otherwise never
/// replaces.
async fn rename(s: &conn::Session, from: &str, to: &str, replace: bool) -> Result<()> {
    if replace {
        match s.raw.lstat(to).await {
            Ok(old) if old.attrs.file_type().is_file() => {
                if let Some(mode) = old.attrs.permissions {
                    let attrs = FileAttributes {
                        // Never setuid/setgid/sticky: those belong to the old content.
                        permissions: Some(mode & 0o777),
                        ..FileAttributes::empty()
                    };
                    s.raw.setstat(from, attrs).await.map_err(wire_error)?;
                }
            }
            Ok(_) => {}
            Err(SftpError::Status(e)) if e.status_code == StatusCode::NoSuchFile => {}
            Err(e) => return Err(wire_error(e)),
        }
    }
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
/// One upload, streamed as it is written to a staging file beside the target.
/// `finish()` (or `flush()`, for `Box<dyn Write>` users) verifies the size and renames it
/// into place. Dropped before that, it logs an error and removes the staging file: the
/// target is never left half-written and never silently left unchanged.
pub struct SftpUpload {
    stream: RemoteReader,
    partial: String,
    exclusive: bool,
    committed: bool,
    failed: bool,
}
impl SftpUpload {
    /// Commits the upload. Must be called (or `flush()`) for the file to appear.
    pub fn finish(mut self) -> Result<()> {
        Ok(self.commit()?)
    }
}
impl Write for SftpUpload {
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
    /// Commits, like `finish()`: closes the staging file, verifies its size, renames it
    /// into place.
    fn flush(&mut self) -> io::Result<()> {
        self.commit()
    }
}
impl SftpUpload {
    fn commit(&mut self) -> io::Result<()> {
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
impl Drop for SftpUpload {
    fn drop(&mut self) {
        if !self.committed {
            if !self.failed {
                tracing::error!(
                    target = %self.stream.path.display(),
                    "upload dropped without finish(); discarding it"
                );
            }
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

/// Remote downloads (`<cache>/remote/<host id>/<key hash>/`), LRU by bytes over all hosts.
const DOWNLOAD_BUDGET: u64 = 2 << 30;
static DOWNLOADS: LazyLock<parking_lot::Mutex<Downloads>> = LazyLock::new(Default::default);
#[derive(Default)]
struct Downloads {
    root: Option<PathBuf>,
    /// Folder -> (bytes, last use).
    folders: HashMap<PathBuf, (u64, u64)>,
    clock: u64,
}
impl Downloads {
    /// Marks `folder` (holding `size` bytes) most recently used, then deletes the least
    /// recently used folders until the total fits `budget`; `folder` itself always stays.
    /// The first call for a root counts folders left by earlier sessions (oldest first).
    fn record(&mut self, root: &Path, folder: &Path, size: u64, budget: u64) {
        if self.root.as_deref() != Some(root) {
            self.scan(root);
        }
        self.clock += 1;
        self.folders
            .insert(folder.to_path_buf(), (size, self.clock));
        let mut total: u64 = self.folders.values().map(|(bytes, _)| bytes).sum();
        while total > budget {
            let Some(oldest) = self
                .folders
                .iter()
                .filter(|(f, _)| f.as_path() != folder)
                .min_by_key(|(_, (_, used))| *used)
                .map(|(f, _)| f.clone())
            else {
                break;
            };
            let (bytes, _) = self.folders.remove(&oldest).unwrap_or_default();
            // A file still open elsewhere (Windows) keeps its folder until the next scan.
            let _ = fs::remove_dir_all(&oldest);
            total -= bytes;
        }
    }
    fn scan(&mut self, root: &Path) {
        self.root = Some(root.to_path_buf());
        self.folders.clear();
        let mut found = Vec::new();
        for host in fs::read_dir(root).into_iter().flatten().flatten() {
            for folder in fs::read_dir(host.path()).into_iter().flatten().flatten() {
                let (mut bytes, mut newest) = (0, UNIX_EPOCH);
                for file in fs::read_dir(folder.path()).into_iter().flatten().flatten() {
                    if let Ok(m) = file.metadata() {
                        bytes += m.len();
                        newest = newest.max(m.modified().unwrap_or(UNIX_EPOCH));
                    }
                }
                found.push((newest, folder.path(), bytes));
            }
        }
        found.sort();
        for (_, folder, bytes) in found {
            self.clock += 1;
            self.folders.insert(folder, (bytes, self.clock));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_cache_names_are_safe_and_readable() {
        assert_eq!(super::local_name("report 2026.pdf"), "report 2026.pdf");
        assert_eq!(super::local_name("a:b?.txt. "), "a_b_.txt");
        assert_eq!(super::local_name("con.txt"), "_con.txt");
        assert_eq!(super::local_name("COM1"), "_COM1");
        assert_eq!(super::local_name("..."), "file");
    }
    /// m27: unique per attempt, capped at 255 bytes on a char boundary; sweep matching.
    #[test]
    fn partial_names_are_unique_bounded_and_recognised() {
        let a = partial_path("/d/report.pdf");
        let b = partial_path("/d/report.pdf");
        assert_ne!(a, b);
        assert!(a.starts_with("/d/report.pdf.keel-partial-"));
        let long = format!("/d/{}", "ü".repeat(127)); // 254 bytes
        let name = partial_path(&long).rsplit_once('/').unwrap().1.to_owned();
        assert!(
            name.len() <= 255 && name.contains(".keel-partial-"),
            "{name}"
        );
        assert!(is_partial(name.as_str()));
        assert!(is_partial("x.bin.keel-partial"));
        assert!(!is_partial("notes.keel-partial-draft.txt"));
        assert!(!is_partial(".keel-partial"));
        assert!(!is_partial("x.keel-partial-1-"));
        assert!(!is_partial("report.keel-partial-2024-05"));
    }
    /// m25: names the server could not send as UTF-8 are refused before any request.
    #[test]
    fn undecodable_names_are_refused_with_a_clear_error() {
        let host = RemoteHost {
            id: "offline".into(),
            label: String::new(),
            host: "offline.invalid".into(),
            port: 22,
            user: String::new(),
            auth: RemoteAuth::Agent,
            home: None,
            bookmarks: vec![],
        };
        let provider = SftpProvider::new(host, crossbeam_channel::unbounded().0);
        let bad = VPath::parse("sftp://offline/dir/bad\u{FFFD}name").unwrap();
        for err in [
            provider.stat(&bad).unwrap_err(),
            provider.remove(&bad).unwrap_err(),
            provider.read(&bad).err().unwrap(),
            provider.upload(&bad, false).err().unwrap(),
        ] {
            assert!(format!("{err:#}").contains("not valid UTF-8"), "{err:#}");
        }
        assert_eq!(provider.status(), ConnStatus::Disconnected);
    }
    /// m26: remote downloads stay under the byte budget, least recently used first.
    #[test]
    fn download_cache_evicts_least_recently_used_folders() {
        let root = tempfile::tempdir().unwrap();
        let folder = |n: &str, bytes: usize| {
            let f = root.path().join("host").join(n);
            fs::create_dir_all(&f).unwrap();
            fs::write(f.join("file"), vec![0; bytes]).unwrap();
            f
        };
        let old = folder("old", 10); // left by an earlier session
        let mut cache = Downloads::default();
        let a = folder("a", 10);
        cache.record(root.path(), &a, 10, 25);
        assert!(old.exists() && a.exists());
        let b = folder("b", 10);
        cache.record(root.path(), &b, 10, 25);
        assert!(!old.exists(), "oldest folder evicted");
        cache.record(root.path(), &a, 10, 25); // a used again
        let c = folder("c", 10);
        cache.record(root.path(), &c, 10, 25);
        assert!(a.exists() && !b.exists() && c.exists());
        let huge = folder("huge", 100);
        cache.record(root.path(), &huge, 100, 25);
        assert!(huge.exists() && !a.exists() && !c.exists());
    }
}
