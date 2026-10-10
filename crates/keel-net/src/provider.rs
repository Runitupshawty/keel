//! `node://<peer id>/<source id>/<path>`: a paired device's granted sources as a
//! `keel_vfs::Provider`. Register one with the router (`router.register`), scheme `node`.
//!
//! Every call blocks on the node's runtime: call it from ordinary or blocking threads,
//! never from an async task (like every other network provider).
use crate::*;
use anyhow::{bail, ensure, Context, Result};
use keel_vfs::{Caps, Entry, Kind, Progress, Provider, RemoveKind, VPath};
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    io::{self, Read, Seek, Write},
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, runtime::Handle};

pub struct NodeProvider {
    node: Arc<Node>,
    rt: Handle,
    /// Peers that granted this device write access somewhere (learnt when listing a
    /// device's root). The host checks every request; this only drives `caps`.
    writable: Mutex<HashSet<PeerId>>,
}

/// Where a `node://` path points: the device, then (below its root) a source and a path
/// relative to that source ("" for the source's root).
struct Target {
    peer: PeerId,
    at: Option<(String, String)>,
}

fn target(p: &VPath) -> Result<Target> {
    ensure!(p.scheme == "node", "not a node path: {}", p.display());
    let peer = PeerId(p.authority.parse().context("invalid device id")?);
    let rest = p.path.trim_matches('/');
    let at = (!rest.is_empty()).then(|| match rest.split_once('/') {
        Some((source, path)) => (source.to_owned(), path.to_owned()),
        None => (rest.to_owned(), String::new()),
    });
    Ok(Target { peer, at })
}

fn file_target(p: &VPath) -> Result<(PeerId, String, String)> {
    match target(p)? {
        Target {
            peer,
            at: Some((source, path)),
        } if !path.is_empty() => Ok((peer, source, path)),
        _ => bail!("not a file or folder inside a source: {}", p.display()),
    }
}

/// A name the host sent that cannot be addressed back (or would escape a local folder
/// when copied) is left out of listings.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && scope::valid_path(name)
}

fn answer(r: Response) -> Result<Response> {
    match r {
        Response::Denied(_) => bail!("the device refused access"),
        Response::Error(e) => bail!("the device reported an error: {e}"),
        r => Ok(r),
    }
}

fn entry(path: VPath, info: &EntryInfo) -> Entry {
    let name = info.name.clone();
    Entry {
        kind: if info.is_dir { Kind::Dir } else { Kind::File },
        size: info.size,
        modified: info
            .modified
            .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s.max(0) as u64))),
        hidden: name.starts_with('.'),
        is_link: false,
        encrypted: false,
        ext: name
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, ext)| ext.to_lowercase())
            .unwrap_or_default(),
        name,
        path,
    }
}

fn folder(path: VPath, name: String) -> Entry {
    entry(
        path,
        &EntryInfo {
            name,
            is_dir: true,
            size: 0,
            modified: None,
            content_id: None,
        },
    )
}

/// Downloads (`local_copy`) older than this are swept from the temp folder.
const STALE_DOWNLOAD: Duration = Duration::from_secs(24 * 60 * 60);

/// Deletes day-old `keel-node-*` download folders from the temp folder. Best effort.
fn sweep_downloads(temp: &std::path::Path) {
    for e in std::fs::read_dir(temp).into_iter().flatten().flatten() {
        let stale = e.file_name().to_string_lossy().starts_with("keel-node-")
            && e.file_type().is_ok_and(|t| t.is_dir())
            && e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_DOWNLOAD);
        if stale {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

impl NodeProvider {
    /// `rt`: the runtime `node` runs on. Sweeps day-old downloads (in the background).
    pub fn new(node: Arc<Node>, rt: Handle) -> Self {
        std::thread::spawn(|| sweep_downloads(&std::env::temp_dir()));
        Self {
            node,
            rt,
            writable: Mutex::default(),
        }
    }

    fn request(&self, peer: &PeerId, req: Request) -> Result<Response> {
        answer(self.rt.block_on(self.node.request(peer, req))?)
    }

    /// The device's root: its granted sources as folders (refreshes `caps`).
    fn sources(&self, root: &VPath, peer: &PeerId) -> Result<Vec<Entry>> {
        let Response::Sources(sources) = self.request(peer, Request::ListSources)? else {
            bail!("unexpected answer from the device");
        };
        if let Ok(Response::Grants(grants)) = self.request(peer, Request::Grants) {
            let mut writable = self.writable.lock();
            writable.remove(peer);
            if grants.iter().any(|g| g.access == Access::ReadWrite) {
                writable.insert(*peer);
            }
        }
        Ok(sources
            .into_iter()
            .filter(|s| valid_name(&s.id))
            .map(|s| {
                let name = if valid_name(&s.label) {
                    s.label
                } else {
                    s.id.clone()
                };
                folder(root.join(&s.id), name)
            })
            .collect())
    }

    /// Every page of a folder; any failed page fails the whole listing.
    fn pages(
        &self,
        dir: &VPath,
        peer: &PeerId,
        source: &str,
        path: &str,
    ) -> Result<Vec<(Entry, Option<[u8; 32]>)>> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let req = Request::List {
                source: source.into(),
                path: path.into(),
                after: after.clone(),
                limit: PAGE_LIMIT,
            };
            let Response::Entries { entries, more } = self.request(peer, req)? else {
                bail!("unexpected answer from the device");
            };
            let last = entries.last().map(|e| e.name.clone());
            for info in entries.iter().filter(|e| valid_name(&e.name)) {
                out.push((entry(dir.join(&info.name), info), info.content_id));
            }
            match last {
                Some(last) if more => {
                    // A host must page forward; a repeated cursor would loop forever.
                    ensure!(after.as_ref() < Some(&last), "the device paged backwards");
                    after = Some(last);
                }
                _ => return Ok(out),
            }
        }
    }

    fn upload(&self, p: &VPath, exclusive: bool) -> Result<Box<dyn Write + Send>> {
        let (peer, source, path) = file_target(p)?;
        if exclusive {
            ensure!(self.stat(p).is_err(), "destination exists: {}", p.display());
        }
        Ok(Box::new(Upload {
            node: self.node.clone(),
            rt: self.rt.clone(),
            peer,
            source,
            path,
            dest: p.clone(),
            exclusive,
            file: Some(tempfile::tempfile()?),
            hash: blake3::Hasher::new(),
            size: 0,
            committed: false,
        }))
    }
}

impl NodeProvider {
    fn read_part(&self, p: &VPath, range: Option<(u64, u64)>) -> Result<Box<dyn Read + Send>> {
        let (peer, source, path) = file_target(p)?;
        let stream = self
            .rt
            .block_on(self.node.read_stream(&peer, &source, &path, range))
            .with_context(|| p.display())?;
        Ok(Box::new(NodeReader {
            rt: self.rt.clone(),
            inner: Box::pin(stream),
            timeout: self.node.options.request_timeout,
        }))
    }
}

impl Provider for NodeProvider {
    fn scheme(&self) -> &'static str {
        "node"
    }
    /// What the grants of any device allow (each request is checked by its host).
    fn caps(&self) -> Caps {
        let write = !self.writable.lock().is_empty();
        Caps {
            write,
            rename: write,
            delete: write,
            watch: false,
        }
    }
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        Ok(self
            .list_complete_ids(dir)?
            .into_iter()
            .map(|(e, _)| e)
            .collect())
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        let t = target(p)?;
        let Some((source, path)) = t.at else {
            let label = self
                .node
                .peers()
                .into_iter()
                .find(|peer| peer.id == t.peer)
                .map(|peer| peer.label)
                .filter(|l| valid_name(l))
                .unwrap_or_else(|| p.authority.clone());
            return Ok(folder(p.clone(), label));
        };
        let Response::Entry(info) = self.request(&t.peer, Request::Stat { source, path })? else {
            bail!("unexpected answer from the device");
        };
        let mut e = entry(p.clone(), &info);
        e.name = p.name().to_owned();
        Ok(e)
    }
    /// Pages through the folder; never cut off.
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    /// With the content ids the device's index holds.
    fn list_complete_ids(&self, dir: &VPath) -> Result<Vec<(Entry, Option<[u8; 32]>)>> {
        let t = target(dir)?;
        match &t.at {
            None => Ok(self
                .sources(dir, &t.peer)?
                .into_iter()
                .map(|e| (e, None))
                .collect()),
            Some((source, path)) => self.pages(dir, &t.peer, source, path),
        }
        .with_context(|| dir.display())
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        self.read_part(p, None)
    }
    /// The protocol's ranged `Read`.
    fn read_range(&self, p: &VPath, offset: u64, len: u64) -> Result<Option<Box<dyn Read + Send>>> {
        self.read_part(p, Some((offset, len))).map(Some)
    }
    /// Buffered in an anonymous temp file, sent on `flush()` (the size goes first on the
    /// wire); the host stages it and publishes it atomically. ponytail: the whole file sits
    /// in the temp folder until then (the OS deletes it with the handle); stream it as
    /// resumable `WriteAt` pieces if big uploads to devices matter.
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.upload(p, false)
    }
    /// ponytail: checks for an existing entry, then writes (the protocol has no
    /// exclusive create); add one to `Request::Write` if two writers ever race here.
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.upload(p, true)
    }
    fn uploads_on_flush(&self) -> Option<&'static str> {
        Some("the device")
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        let (peer, source, path) = file_target(p)?;
        self.request(&peer, Request::Mkdir { source, path })
            .with_context(|| p.display())
            .map(drop)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        let (peer, source, from_path) = file_target(from)?;
        let (to_peer, to_source, to_path) = file_target(to)?;
        ensure!(
            peer == to_peer && source == to_source,
            "a rename stays inside one source of one device"
        );
        self.request(
            &peer,
            Request::Rename {
                source,
                from: from_path,
                to: to_path,
            },
        )
        .with_context(|| to.display())
        .map(drop)
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        let (peer, source, path) = file_target(p)?;
        self.request(&peer, Request::Remove { source, path })
            .with_context(|| p.display())
            .map(drop)
    }
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        self.local_copy_cancellable(p, &|_| {}, &AtomicBool::new(false))
    }
    /// Streams into a fresh `keel-node-*` folder under the temp dir, swept a day later
    /// (ponytail: no cache, so opening a file again downloads it again; a keyed cache like
    /// SFTP's if that gets slow).
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let info = self.stat(p)?;
        ensure!(info.kind == Kind::File, "not a file: {}", p.display());
        let dir = tempfile::Builder::new().prefix("keel-node-").tempdir()?;
        let target = dir.path().join(p.name());
        let mut out = std::fs::File::create(&target)?;
        let mut reader = self.read(p)?;
        let mut buf = vec![0; 1 << 20];
        let mut done = 0u64;
        loop {
            ensure!(
                !cancel.load(Ordering::Relaxed),
                "download cancelled: {}",
                p.display()
            );
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress(Progress {
                done_bytes: done,
                total_bytes: info.size,
                current: p.display(),
                done_items: 0,
                total_items: 1,
                skipped: 0,
            });
        }
        ensure!(
            done == info.size,
            "file changed during download: {}",
            p.display()
        );
        out.sync_all()?;
        let _ = dir.keep();
        Ok(target)
    }
}

/// A file body from the device; each read waits at most the request timeout.
struct NodeReader {
    rt: Handle,
    inner: Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    timeout: Duration,
}
impl Read for NodeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (rt, timeout) = (self.rt.clone(), self.timeout);
        rt.block_on(async { tokio::time::timeout(timeout, self.inner.read(buf)).await })
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the device stopped sending"))?
    }
}

/// `Provider::write`: commits on `flush()`; dropped before, nothing reaches the device.
struct Upload {
    node: Arc<Node>,
    rt: Handle,
    peer: PeerId,
    source: String,
    path: String,
    dest: VPath,
    exclusive: bool,
    file: Option<std::fs::File>,
    hash: blake3::Hasher,
    size: u64,
    committed: bool,
}
impl Write for Upload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("upload finished or failed"))?;
        let n = file.write(bytes)?;
        self.hash.update(&bytes[..n]);
        self.size += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        let mut file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("upload failed"))?;
        let result = (|| -> Result<()> {
            file.rewind()?;
            let body = tokio::fs::File::from_std(file);
            if self.exclusive {
                let stat = Request::Stat {
                    source: self.source.clone(),
                    path: self.path.clone(),
                };
                ensure!(
                    !matches!(
                        self.rt.block_on(self.node.request(&self.peer, stat)),
                        Ok(Response::Entry(_))
                    ),
                    "destination exists: {}",
                    self.dest.display()
                );
            }
            let at = WriteAt {
                offset: 0,
                size: self.size,
                final_: true,
                expect: Some(*self.hash.finalize().as_bytes()),
            };
            answer(self.rt.block_on(self.node.write_stream(
                &self.peer,
                &self.source,
                &self.path,
                Box::new(body),
                at,
            ))?)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.committed = true;
                Ok(())
            }
            Err(e) => Err(io::Error::other(format!("{e:#}"))),
        }
    }
}
