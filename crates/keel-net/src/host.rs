//! `LibraryHandler`: serves a `keel_core::Library`'s sources to paired devices.
//!
//! Listings and stats come from the live provider (never the index). Every path is
//! resolved through the router and refused unless its real location (after following
//! links, junctions and short names) is exactly the requested one under the source root,
//! so a link inside a shared folder cannot reach outside the grant. Every served request
//! is appended to the library's op log with the requesting device's id.
use crate::*;
use anyhow::{bail, ensure, Context, Result};
use keel_core::{Library, SourceId, SourceKind};
use keel_vfs::{Entry, Kind, Provider, VPath};
use parking_lot::Mutex;
use serde_json::json;
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::UNIX_EPOCH,
};
use tokio::io::{AsyncRead, AsyncSeekExt};

pub struct LibraryHandler {
    lib: Arc<Library>,
}

impl LibraryHandler {
    pub fn new(lib: Arc<Library>) -> Self {
        Self { lib }
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
        if let Err(e) = self.lib.log_op(&format!("net.{op}"), &payload, &text, ok) {
            tracing::warn!("op log: {e:#}");
        }
        result
    }
}

/// The source `id` as served to devices (a device source is never re-shared).
fn source(lib: &Library, id: &str) -> Result<Arc<keel_core::Source>> {
    lib.source(&SourceId(id.into()))
        .filter(|s| s.def.kind != SourceKind::Device)
        .context("no such source")
}

fn same(a: &VPath, b: &VPath) -> bool {
    let nocase = a.scheme == "file" && cfg!(any(windows, target_os = "macos"));
    a.scheme == b.scheme
        && a.authority == b.authority
        && if nocase {
            a.path.to_lowercase() == b.path.to_lowercase()
        } else {
            a.path == b.path
        }
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

/// Pieces being written now (one writer per staging file).
static WRITING: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

/// `.keel-partial-<id>` next to `dest`: `<id>` is fixed per device and target, so a
/// transfer that broke off resumes into the same file.
fn staging_for(peer: &PeerId, dest: &Path) -> PathBuf {
    let mut h = blake3::Hasher::new();
    h.update(&peer.0 .0);
    h.update(dest.as_os_str().as_encoded_bytes());
    let id = data_encoding::HEXLOWER.encode(&h.finalize().as_bytes()[..16]);
    dest.with_file_name(format!(".keel-partial-{id}"))
}

/// A piece in progress: unless it completed, the staging file goes back to the piece's
/// offset (removed when that is 0), also when the request is dropped.
struct Piece {
    path: PathBuf,
    offset: u64,
    done: bool,
}
impl Drop for Piece {
    fn drop(&mut self) {
        if !self.done {
            if self.offset == 0 {
                let _ = std::fs::remove_file(&self.path);
            } else if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&self.path) {
                let _ = f.set_len(self.offset);
            }
        }
        WRITING.lock().remove(&self.path);
    }
}

fn local_dest(lib: &Library, id: &str, rel: &str) -> Result<PathBuf> {
    changes(rel)?;
    let (abs, _) = locate(lib, id, rel)?;
    abs.to_local_path()
        .context("devices can only write to local folders")
}

#[async_trait::async_trait]
impl Handler for LibraryHandler {
    async fn sources(&self, _: &RequestCtx) -> Vec<SourceInfo> {
        self.lib
            .sources()
            .into_iter()
            .filter(|s| s.kind != SourceKind::Device)
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
            Ok(provider
                .list_complete(&abs)?
                .iter()
                .filter(|e| !e.name.starts_with(".keel-partial-"))
                .map(info)
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
    /// Local sources only (ponytail: SFTP/cloud-backed sources refuse device writes;
    /// bridge the body into `Provider::write` if that is ever wanted). The piece goes
    /// into the staging file (see `staging_for`); a final piece is verified, the path
    /// re-checked and the staging file renamed over the target.
    async fn write(
        &self,
        ctx: &RequestCtx,
        source: &str,
        path: &str,
        mut body: Box<dyn AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> Result<()> {
        let (id, rel, peer) = (source.to_owned(), path.to_owned(), ctx.peer);
        let payload = json!({"source": source, "path": path, "offset": at.offset,
            "size": at.size, "final": at.final_});
        let staged = async {
            let (lib, id, rel) = (self.lib.clone(), id.clone(), rel.clone());
            let dest = tokio::task::spawn_blocking(move || local_dest(&lib, &id, &rel)).await??;
            let staging = staging_for(&peer, &dest);
            ensure!(
                WRITING.lock().insert(staging.clone()),
                "this file is being written already"
            );
            // Armed (done = false) once the piece is known to start at the staged end.
            let mut piece = Piece {
                path: staging.clone(),
                offset: at.offset,
                done: true,
            };
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(at.offset == 0)
                .open(&staging)
                .await?;
            let len = file.metadata().await?.len();
            ensure!(
                len == at.offset,
                "offset {} != staged length {len}",
                at.offset
            );
            piece.done = false;
            file.seek(SeekFrom::Start(at.offset)).await?;
            let copied = tokio::io::copy(&mut body, &mut file).await?;
            ensure!(copied == at.size, "body size mismatch");
            file.sync_all().await?;
            drop(file);
            if !at.final_ {
                piece.done = true;
            }
            Ok((piece, dest))
        }
        .await;
        self.serve(ctx, "write", payload, move |lib| {
            let (mut piece, dest) = staged?;
            if piece.done {
                return Ok(());
            }
            if let Some(expect) = at.expect {
                let mut h = blake3::Hasher::new();
                h.update_reader(std::fs::File::open(&piece.path)?)?;
                if h.finalize().as_bytes() != &expect {
                    piece.offset = 0;
                    bail!("content check failed");
                }
            }
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
            let staging = staging_for(&peer, &local_dest(lib, &id, &rel)?);
            Ok(std::fs::metadata(staging).map_or(0, |m| m.len()))
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
    /// Local sources: to the host's trash.
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
