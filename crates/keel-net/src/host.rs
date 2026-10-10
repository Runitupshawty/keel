//! `LibraryHandler`: serves a `keel_core::Library`'s sources to paired devices.
//!
//! Listings and stats come from the live provider (never the index). Every path is
//! resolved through the router and refused unless its real location (after following
//! links, junctions and short names) is exactly the requested one under the source root,
//! so a link inside a shared folder cannot reach outside the grant. Every served request
//! is appended to the library's op log with the requesting device's id. Writes into a
//! local source are staged on this machine; writes into an SFTP or cloud source stream
//! through the router to that source's provider.
use crate::*;
use anyhow::{ensure, Context, Result};
use keel_core::{Library, SourceId, SourceKind};
use keel_vfs::{Entry, Kind, Provider, VPath};
use parking_lot::Mutex;
use serde_json::json;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};
use tokio::io::AsyncRead;

pub struct LibraryHandler {
    lib: Arc<Library>,
    /// Spacedrop: the inbox and who decides on offers (`on_drop`).
    drops: Mutex<Option<(PathBuf, Ask)>>,
    /// Op log entries, written in batches by a logger thread (each library commit syncs;
    /// one per served request or received file would cap a drop of small files at a few
    /// dozen a second). `close` (and dropping the handler) waits until they are written.
    log: crossbeam_channel::Sender<Logged>,
}

/// What the op log thread gets: an entry, or a request to say once all before it are in.
enum Logged {
    Op(keel_core::OpDone),
    Flush(crossbeam_channel::Sender<()>),
}

/// How long `close` waits for the op log to be written.
const FLUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

type Ask = Arc<dyn Fn(IncomingDrop) + Send + Sync>;

impl LibraryHandler {
    pub fn new(lib: Arc<Library>) -> Self {
        let (log, entries) = crossbeam_channel::unbounded::<Logged>();
        let logger = lib.clone();
        // Ends (after writing what is queued) once the handler is gone.
        let _ = std::thread::Builder::new()
            .name("keel-net-oplog".into())
            .spawn(move || {
                let write = |batch: &mut Vec<keel_core::OpDone>| {
                    if batch.is_empty() {
                        return;
                    }
                    // One retry (a busy library); then the batch is lost, logged.
                    if let Err(e) = logger.log_ops(batch) {
                        tracing::warn!("op log, retrying: {e:#}");
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        if let Err(e) = logger.log_ops(batch) {
                            tracing::warn!("op log: {} entries lost: {e:#}", batch.len());
                        }
                    }
                    batch.clear();
                };
                let mut batch = Vec::new();
                while let Ok(first) = entries.recv() {
                    for msg in std::iter::once(first).chain(entries.try_iter().take(999)) {
                        match msg {
                            Logged::Op(op) => batch.push(op),
                            Logged::Flush(done) => {
                                write(&mut batch);
                                let _ = done.send(());
                            }
                        }
                    }
                    write(&mut batch);
                }
            });
        Self {
            lib,
            drops: Mutex::default(),
            log,
        }
    }

    fn log_op(&self, kind: String, payload: serde_json::Value, result: String, ok: bool) {
        let _ = self.log.send(Logged::Op((kind, payload, result, ok)));
    }

    /// Waits (up to 10 s) until every op log entry queued so far is written. The handler
    /// stays usable (a reopened node may serve with it again).
    pub fn flush(&self) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.log.send(Logged::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(FLUSH_WAIT);
        }
    }

    /// Accepts Spacedrop offers into `inbox` when `ask` answers yes (it may answer later,
    /// from another thread; it must not block). Without this, every offer is declined.
    /// Staging folders in `inbox` that an earlier session left untouched for
    /// `spacedrop::STALE` are swept (in the background).
    pub fn on_drop(&self, inbox: PathBuf, ask: impl Fn(IncomingDrop) + Send + Sync + 'static) {
        let swept = inbox.clone();
        std::thread::spawn(move || crate::spacedrop::sweep(&swept, crate::spacedrop::STALE));
        *self.drops.lock() = Some((inbox, Arc::new(ask)));
    }

    /// Runs `f` (with the library) on the blocking pool and logs it as `net.<op>`.
    async fn serve<T: Send + 'static>(
        &self,
        ctx: &RequestCtx,
        op: &str,
        payload: serde_json::Value,
        f: impl FnOnce(&Library) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let lib = self.lib.clone();
        let result = tokio::task::spawn_blocking(move || f(&lib))
            .await
            .map_err(|e| anyhow::anyhow!("host task failed: {e}"))
            .and_then(|r| r);
        let mut payload = payload;
        payload["peer"] = json!(ctx.peer.0.to_string());
        payload["device"] = json!(ctx.label);
        let (text, ok) = match &result {
            Ok(_) => ("ok".to_owned(), true),
            Err(e) => (format!("{e:#}"), false),
        };
        self.log_op(format!("net.{op}"), payload, text, ok);
        result
    }
}

impl Drop for LibraryHandler {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Whether a source may be served to devices: another device's source never is.
fn served(def: &keel_core::SourceDef) -> bool {
    def.kind != SourceKind::Device && def.root.scheme != "node"
}

/// The source `id` as served to devices.
fn source(lib: &Library, id: &str) -> Result<Arc<keel_core::Source>> {
    lib.source(&SourceId(id.into()))
        .filter(|s| served(&s.def))
        .context("no such source")
}

/// Canonical paths compare exactly: a case or Unicode variant of a name is refused (the
/// real name is what a listing shows).
fn same(a: &VPath, b: &VPath) -> bool {
    a.scheme == b.scheme && a.authority == b.authority && a.path == b.path
}

/// `rel` inside source `id` and its provider, refused when the real location is not
/// exactly `<real root>/<rel>` (a link anywhere on the way). A path that does not exist
/// yet is checked by its parent. ponytail: check-then-use; a link swapped in between the
/// check and the operation is not caught (writes re-check before publishing).
fn locate(lib: &Library, id: &str, rel: &str) -> Result<(VPath, Arc<dyn Provider>)> {
    let src = source(lib, id)?;
    let abs = src.absolute(rel);
    let provider = lib
        .router()
        .provider_for(&abs)
        .context("source unreachable")?;
    let root = provider.canonicalize(&src.def.root)?;
    let expected = if rel.is_empty() { root } else { root.join(rel) };
    let actual = match provider.canonicalize(&abs) {
        Ok(p) => p,
        Err(_) if !rel.is_empty() => {
            let parent = abs.parent().context("no parent folder")?;
            provider.canonicalize(&parent)?.join(abs.name())
        }
        Err(e) => return Err(e),
    };
    ensure!(same(&expected, &actual), "path leaves the shared folder");
    Ok((abs, provider))
}

fn changes(rel: &str) -> Result<()> {
    ensure!(!rel.is_empty(), "a source's root cannot be changed");
    Ok(())
}

fn info(e: &Entry) -> EntryInfo {
    EntryInfo {
        name: e.name.clone(),
        is_dir: e.kind == Kind::Dir,
        size: e.size,
        modified: e
            .modified
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
        content_id: None,
    }
}

/// A blocking reader as an async one: a blocking task reads ahead into a small channel.
struct Bridge {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    at: usize,
}
impl Bridge {
    fn new(mut reader: Box<dyn Read + Send>) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::task::spawn_blocking(move || loop {
            let mut buf = vec![0; 256 * 1024];
            let item = match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    buf.truncate(n);
                    Ok(buf)
                }
                Err(e) => Err(e),
            };
            let failed = item.is_err();
            // The reader side was dropped (cancelled request): stop reading.
            if tx.blocking_send(item).is_err() || failed {
                return;
            }
        });
        Self {
            rx,
            buf: Vec::new(),
            at: 0,
        }
    }
}
impl AsyncRead for Bridge {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        while self.at == self.buf.len() {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Ok(buf))) => {
                    self.buf = buf;
                    self.at = 0;
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = out.remaining().min(self.buf.len() - self.at);
        let at = self.at;
        out.put_slice(&self.buf[at..at + n]);
        self.at += n;
        Poll::Ready(Ok(()))
    }
}

/// `.keel-partial-<id>`, `<id>` fixed per device and `target`, so a transfer that broke
/// off resumes into the same file.
fn staging_name(peer: &PeerId, target: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&peer.0 .0);
    h.update(target);
    let id = data_encoding::HEXLOWER.encode(&h.finalize().as_bytes()[..16]);
    format!(".keel-partial-{id}")
}

/// The staging file next to a local `dest`.
fn staging_for(peer: &PeerId, dest: &Path) -> PathBuf {
    dest.with_file_name(staging_name(peer, dest.as_os_str().as_encoded_bytes()))
}

/// The staging file next to `dest` in a source that is not local.
fn remote_staging(peer: &PeerId, dest: &VPath) -> Result<VPath> {
    let folder = dest.parent().context("no parent folder")?;
    Ok(folder.join(&staging_name(peer, dest.path.as_bytes())))
}

fn local_dest(lib: &Library, id: &str, rel: &str) -> Result<PathBuf> {
    changes(rel)?;
    let (abs, _) = locate(lib, id, rel)?;
    abs.to_local_path()
        .context("devices can only write to local folders")
}

/// Where a device write to `rel` goes.
enum Dest {
    /// A file of a local source, staged on this machine.
    Local(PathBuf),
    /// A path of a source that is not local (SFTP, cloud), and its provider.
    Remote(VPath, Arc<dyn Provider>),
}

/// `rel` of source `id` as a write target, with the same path checks as every request.
/// A source that is not local must be writable and reachable.
fn write_dest(lib: &Library, id: &str, rel: &str) -> Result<Dest> {
    changes(rel)?;
    let (abs, provider) = locate(lib, id, rel)?;
    if let Some(local) = abs.to_local_path() {
        return Ok(Dest::Local(local));
    }
    ensure!(provider.caps().write, "the source is read-only");
    let root = source(lib, id)?.def.root.clone();
    provider.stat(&root).context("source unreachable")?;
    Ok(Dest::Remote(abs, provider))
}

/// A device's request body read from a blocking task (`write` into a remote source).
struct Blocking {
    rt: tokio::runtime::Handle,
    body: Box<dyn AsyncRead + Send + Unpin>,
}
impl Read for Blocking {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncReadExt;
        self.rt.block_on(self.body.read(buf))
    }
}

/// `body` copied into `out`, hashed on the way; returns its length.
fn copy_hashed(body: &mut dyn Read, out: &mut dyn Write, hash: &mut blake3::Hasher) -> Result<u64> {
    let mut buf = vec![0; 1 << 20];
    let mut n = 0;
    loop {
        let got = body.read(&mut buf)?;
        if got == 0 {
            return Ok(n);
        }
        hash.update(&buf[..got]);
        out.write_all(&buf[..got])?;
        n += got as u64;
    }
}

/// A device write into a source that is not local, streamed (never held whole) to the
/// source's provider. A whole file in one piece goes into the provider's own upload, which
/// places it only on `flush()`: after the content check and a fresh path check. Pieces
/// go into a staging file beside the target, written in place where the provider can
/// (`Provider::write_at`; else the file must come in one piece), and the final one is
/// checked and renamed over the target.
fn remote_write(
    lib: &Library,
    (id, rel): (&str, &str),
    peer: &PeerId,
    body: &mut dyn Read,
    at: WriteAt,
) -> Result<()> {
    let Dest::Remote(dest, provider) = write_dest(lib, id, rel)? else {
        anyhow::bail!("target moved");
    };
    let moved = |lib: &Library| -> Result<bool> {
        Ok(match write_dest(lib, id, rel)? {
            Dest::Remote(again, _) => !same(&again, &dest),
            Dest::Local(_) => true,
        })
    };
    let mut body = body.take(at.size);
    let mut hash = blake3::Hasher::new();
    if at.offset == 0 && at.final_ {
        let mut upload = provider.write(&dest)?;
        let n = copy_hashed(&mut body, &mut *upload, &mut hash)?;
        ensure!(n == at.size, "body size mismatch");
        if let Some(expect) = at.expect {
            ensure!(
                *hash.finalize().as_bytes() == expect,
                "content check failed"
            );
        }
        ensure!(!moved(lib)?, "target moved");
        // Dropped unflushed above, nothing is placed.
        upload.flush()?;
        return Ok(());
    }
    let staging = remote_staging(peer, &dest)?;
    if at.offset > 0 {
        let len = provider.stat(&staging).map_or(0, |e| e.size);
        ensure!(
            len == at.offset,
            "offset {} != staged length {len}",
            at.offset
        );
    }
    let mut out = provider
        .write_at(&staging, at.offset)?
        .context("this source takes a file in one piece: send it whole")?;
    let n = copy_hashed(&mut body, &mut *out, &mut hash)?;
    out.flush()?;
    drop(out);
    ensure!(n == at.size, "body size mismatch");
    if !at.final_ {
        return Ok(());
    }
    if let Some(expect) = at.expect {
        let mut whole = blake3::Hasher::new();
        let mut reader = provider.read(&staging)?;
        copy_hashed(&mut reader, &mut std::io::sink(), &mut whole)?;
        if *whole.finalize().as_bytes() != expect {
            let _ = provider.remove(&staging);
            anyhow::bail!("content check failed");
        }
    }
    ensure!(!moved(lib)?, "target moved");
    provider.rename_replace(&staging, &dest)
}

#[async_trait::async_trait]
impl Handler for LibraryHandler {
    async fn sources(&self, _: &RequestCtx) -> Vec<SourceInfo> {
        self.lib
            .sources()
            .into_iter()
            .filter(|s| s.kind != SourceKind::Device && s.root.scheme != "node")
            .map(|s| SourceInfo {
                id: s.id.0,
                label: s.label,
                kind: format!("{:?}", s.kind).to_lowercase(),
            })
            .collect()
    }
    /// Staging files of unfinished writes are left out.
    async fn list(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<Vec<EntryInfo>> {
        let (id, rel) = (source.to_owned(), path.to_owned());
        let payload = json!({"source": source, "path": path});
        self.serve(ctx, "list", payload, move |lib| {
            let (abs, provider) = locate(lib, &id, &rel)?;
            let entries: Vec<Entry> = provider
                .list_complete(&abs)?
                .into_iter()
                .filter(|e| !e.name.starts_with(".keel-partial-"))
                .collect();
            let ids = lib
                .content_ids(&SourceId(id), &rel, &entries)
                .unwrap_or_else(|_| vec![None; entries.len()]);
            Ok(entries
                .iter()
                .zip(ids)
                .map(|(e, content_id)| EntryInfo {
                    content_id,
                    ..info(e)
                })
                .collect())
        })
        .await
    }
    async fn stat(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<EntryInfo> {
        let (id, rel) = (source.to_owned(), path.to_owned());
        let payload = json!({"source": source, "path": path});
        self.serve(ctx, "stat", payload, move |lib| {
            let (abs, provider) = locate(lib, &id, &rel)?;
            Ok(info(&provider.stat(&abs)?))
        })
        .await
    }
    async fn read(
        &self,
        ctx: &RequestCtx,
        source: &str,
        path: &str,
        range: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let (id, rel) = (source.to_owned(), path.to_owned());
        let (offset, length) = range.unwrap_or((0, u64::MAX));
        let payload = json!({"source": source, "path": path, "offset": offset, "length": length});
        let reader = self
            .serve(ctx, "read", payload, move |lib| {
                let (abs, provider) = locate(lib, &id, &rel)?;
                let reader: Box<dyn Read + Send> = match abs.to_local_path() {
                    Some(p) => {
                        let mut f = std::fs::File::open(p)?;
                        f.seek(SeekFrom::Start(offset))?;
                        Box::new(f)
                    }
                    None => {
                        let mut r = provider.read(&abs)?;
                        std::io::copy(&mut (&mut r).take(offset), &mut std::io::sink())?;
                        r
                    }
                };
                Ok(Box::new(reader.take(length)) as Box<dyn Read + Send>)
            })
            .await?;
        Ok(Box::new(Bridge::new(reader)))
    }
    /// A local source: the piece goes into the staging file (see `staging_for`); a final
    /// piece is verified, the path re-checked and the staging file renamed over the
    /// target. Any other source: streamed to its provider (see `remote_write`).
    async fn write(
        &self,
        ctx: &RequestCtx,
        source: &str,
        path: &str,
        body: Box<dyn AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> Result<()> {
        let (id, rel, peer) = (source.to_owned(), path.to_owned(), ctx.peer);
        let payload = json!({"source": source, "path": path, "offset": at.offset,
            "size": at.size, "final": at.final_});
        let dest = {
            let (lib, id, rel) = (self.lib.clone(), id.clone(), rel.clone());
            tokio::task::spawn_blocking(move || write_dest(&lib, &id, &rel))
                .await
                .map_err(|e| anyhow::anyhow!("host task failed: {e}"))
                .and_then(|r| r)
        };
        if let Ok(Dest::Remote(..)) = dest {
            let rt = tokio::runtime::Handle::current();
            let mut body = Blocking { rt, body };
            return self
                .serve(ctx, "write", payload, move |lib| {
                    remote_write(lib, (&id, &rel), &peer, &mut body, at)
                })
                .await;
        }
        let staged = async {
            let Dest::Local(dest) = dest? else {
                anyhow::bail!("target moved");
            };
            let piece = crate::stage::write_piece(staging_for(&peer, &dest), body, at).await?;
            Ok::<_, anyhow::Error>((piece, dest))
        }
        .await;
        self.serve(ctx, "write", payload, move |lib| {
            let (mut piece, dest) = staged?;
            if piece.done {
                return Ok(());
            }
            piece.verify(at.expect)?;
            // The folder may have been swapped for a link while the body streamed.
            ensure!(local_dest(lib, &id, &rel)? == dest, "target moved");
            std::fs::rename(&piece.path, &dest)?;
            piece.done = true;
            Ok(())
        })
        .await
    }
    async fn stat_partial(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<u64> {
        let (id, rel, peer) = (source.to_owned(), path.to_owned(), ctx.peer);
        let payload = json!({"source": source, "path": path});
        self.serve(ctx, "stat-partial", payload, move |lib| {
            Ok(match write_dest(lib, &id, &rel)? {
                Dest::Local(dest) => {
                    std::fs::metadata(staging_for(&peer, &dest)).map_or(0, |m| m.len())
                }
                Dest::Remote(dest, provider) => provider
                    .stat(&remote_staging(&peer, &dest)?)
                    .map_or(0, |e| e.size),
            })
        })
        .await
    }
    async fn mkdir(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<()> {
        changes(path)?;
        let (id, rel) = (source.to_owned(), path.to_owned());
        let payload = json!({"source": source, "path": path});
        self.serve(ctx, "mkdir", payload, move |lib| {
            let (abs, provider) = locate(lib, &id, &rel)?;
            provider.mkdir(&abs)
        })
        .await
    }
    async fn rename(&self, ctx: &RequestCtx, source: &str, from: &str, to: &str) -> Result<()> {
        changes(from)?;
        changes(to)?;
        let (id, a, b) = (source.to_owned(), from.to_owned(), to.to_owned());
        let payload = json!({"source": source, "from": from, "to": to});
        self.serve(ctx, "rename", payload, move |lib| {
            let (from, provider) = locate(lib, &id, &a)?;
            let (to, _) = locate(lib, &id, &b)?;
            provider.rename(&from, &to)
        })
        .await
    }
    /// Local sources: to the host's trash; others as their provider removes (SFTP and S3
    /// for good).
    async fn remove(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<()> {
        changes(path)?;
        let (id, rel) = (source.to_owned(), path.to_owned());
        let payload = json!({"source": source, "path": path});
        self.serve(ctx, "remove", payload, move |lib| {
            let (abs, provider) = locate(lib, &id, &rel)?;
            provider.remove(&abs)
        })
        .await
    }
    /// Hands the offer to `on_drop`'s callback (the node asks once per device, drop id
    /// and file list).
    fn drop_offer(&self, offer: IncomingDrop) -> Option<PathBuf> {
        let (inbox, ask) = self.drops.lock().clone()?;
        ask(offer);
        Some(inbox)
    }
    /// `net.<op>` with the device's id and label.
    fn close(&self) {
        self.flush();
    }
    fn opened(&self, id: NodeId) {
        if let Err(e) = self.lib.set_sync_device(&id.to_string()) {
            tracing::warn!("library sync: could not record this device's id: {e:#}");
        }
    }
    fn sync_page(&self, since: u64, limit: usize) -> Result<keel_core::SyncPage> {
        self.lib.sync_page(since, limit)
    }
    fn sync_since(&self, peer: &PeerId) -> Result<u64> {
        self.lib.sync_since(&peer.0.to_string())
    }
    fn sync_apply(
        &self,
        peer: &PeerId,
        page: &keel_core::SyncPage,
    ) -> Result<keel_core::SyncApplied> {
        self.lib.sync_apply(&peer.0.to_string(), page)
    }
    fn log(&self, ctx: &RequestCtx, op: &str, mut payload: serde_json::Value, ok: bool) {
        payload["peer"] = json!(ctx.peer.0.to_string());
        payload["device"] = json!(ctx.label);
        let result = if ok { "ok" } else { "declined or failed" };
        self.log_op(format!("net.{op}"), payload, result.into(), ok);
    }
    /// Every local volume together.
    async fn storage(&self, _: &RequestCtx) -> Option<Storage> {
        tokio::task::spawn_blocking(|| {
            let (free, total) = keel_vfs::drives()
                .iter()
                .fold((0, 0), |(f, t), d| (f + d.2, t + d.3));
            (total > 0).then(|| Storage {
                used: total.saturating_sub(free),
                total,
            })
        })
        .await
        .ok()
        .flatten()
    }
}
