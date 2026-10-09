//! Spacedrop: send files and folders to a paired device, resumably.
//!
//! Sender: `send` starts a durable keel-core job (kind `drop`). It offers the file list
//! (`Request::DropOffer`), then pushes each file in 4 MiB pieces as `Request::Write` to
//! source `drop:<id>`. Before each file it asks the receiver how much it holds
//! (`Request::StatPartial`) and resumes there, hashing the bytes it skips so the final
//! piece still carries the BLAKE3 of the whole file. A dropped link (or a closed and
//! reopened node) is retried with backoff until nothing moved for `GIVE_UP`; the job
//! survives restarts like any durable job.
//!
//! Receiver: the node asks its `Handler::drop_offer`, which names an inbox folder to
//! accept. Pieces are staged in `<inbox>/.keel-partial-<id>/<n>` (with `meta.json`, so
//! a restarted receiver resumes when the sender re-offers); a file is moved into the
//! inbox only when complete and verified, never overwriting (`name (1).ext`).
use crate::{stage, *};
use anyhow::{anyhow, bail, ensure, Context, Result};
use keel_core::{Cancelled, Job, JobCtx, JobId, Library};
use keel_vfs::{Entry, Kind, Provider, Router, VPath};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Weak},
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

/// The job kind (`Jobs::register`).
pub const KIND: &str = "drop";
/// Piece size.
pub const CHUNK: u64 = 4 << 20;
/// A transfer that moved nothing for this long fails.
pub const GIVE_UP: Duration = Duration::from_secs(10 * 60);
/// Request source prefix for drop pieces.
pub(crate) const SOURCE: &str = "drop:";
const MAX_FILES: usize = 100_000;

/// Open nodes by id, for jobs restored from a checkpoint (one node per identity).
type Nodes = HashMap<NodeId, (Weak<Node>, Handle)>;
static NODES: LazyLock<Mutex<Nodes>> = LazyLock::new(Mutex::default);

pub(crate) fn register_node(node: &Arc<Node>) {
    if let Ok(rt) = Handle::try_current() {
        NODES.lock().insert(node.id(), (Arc::downgrade(node), rt));
    }
}

fn node_for(id: &NodeId) -> Option<(Arc<Node>, Handle)> {
    let nodes = NODES.lock();
    let (node, rt) = nodes.get(id)?;
    Some((node.upgrade()?, rt.clone()))
}

/// Makes drop jobs resumable in `lib` (call before `jobs().resume_all()`).
pub fn register(lib: &Library) {
    lib.jobs().register(KIND, DropJob::restore);
}

/// Sends `paths` (files or folders, any provider) to `peer` as a durable job; its
/// progress comes through `lib.jobs().subscribe()`, cancel with `jobs().cancel`.
pub fn send(node: &Node, lib: &Library, peer: PeerId, paths: Vec<VPath>) -> Result<JobId> {
    ensure!(!paths.is_empty(), "nothing to send");
    let id = data_encoding::HEXLOWER.encode(&crate::node::random::<16>()?);
    lib.jobs().spawn(Box::new(DropJob {
        id,
        from: node.id(),
        peer,
        paths,
        files: None,
        next: 0,
        sent: 0,
        errors: (0, 0),
    }))
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_rel(rel: &str) -> bool {
    !rel.is_empty()
        && scope::valid_path(rel)
        && !rel
            .split('/')
            .any(|part| part.starts_with(".keel-partial-"))
}

// ---- receiver ----

pub(crate) struct Incoming {
    peer: PeerId,
    files: Vec<(String, u64)>,
    inbox: PathBuf,
    published: Vec<bool>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    peer: PeerId,
    files: Vec<(String, u64)>,
    published: Vec<bool>,
}

fn staging(inbox: &Path, id: &str) -> PathBuf {
    inbox.join(format!(".keel-partial-{id}"))
}

fn save_meta(dir: &Path, d: &Incoming) -> Result<()> {
    let meta = Meta {
        peer: d.peer,
        files: d.files.clone(),
        published: d.published.clone(),
    };
    let tmp = dir.join("meta.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&meta)?)?;
    std::fs::rename(tmp, dir.join("meta.json"))?;
    Ok(())
}

/// `inbox/rel`, or `name (n).ext` beside it when taken. ponytail: exists-then-rename;
/// two drops landing the same name in the same instant could collide (one fails).
fn free_name(inbox: &Path, rel: &str) -> PathBuf {
    let target = rel.split('/').fold(inbox.to_owned(), |p, c| p.join(c));
    if !target.exists() {
        return target;
    }
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.into_owned(), String::new()),
    };
    (1..)
        .map(|n| target.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded")
}

impl Node {
    /// A drop piece is allowed only for the device whose offer was accepted.
    pub(crate) fn drop_permits(&self, peer: PeerId, req: &Request) -> bool {
        let id = match req {
            Request::Write { source, .. } | Request::StatPartial { source, .. } => {
                source.strip_prefix(SOURCE)
            }
            _ => None,
        };
        id.is_some_and(|id| self.drops.lock().get(id).is_some_and(|d| d.peer == peer))
    }

    pub(crate) async fn drop_offer(
        &self,
        ctx: &RequestCtx,
        id: &str,
        files: Vec<(String, u64)>,
    ) -> Result<Response> {
        let names: HashSet<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        ensure!(
            valid_id(id)
                && !files.is_empty()
                && files.len() <= MAX_FILES
                && names.len() == files.len()
                && files.iter().all(|(p, _)| valid_rel(p)),
            "invalid drop offer"
        );
        if let Some(d) = self.drops.lock().get(id) {
            return Ok(if d.peer == ctx.peer && d.files == files {
                Response::Ok
            } else {
                Response::Denied("drop id in use".into())
            });
        }
        let Some(inbox) = self.handler.drop_offer(ctx, id, &files).await else {
            return Ok(Response::Denied("declined".into()));
        };
        let dir = staging(&inbox, id);
        // Staging left by an earlier session resumes when it is the same offer.
        let published = match std::fs::read(dir.join("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
        {
            Some(m) if m.peer == ctx.peer && m.files == files => m.published,
            _ => {
                let _ = std::fs::remove_dir_all(&dir);
                vec![false; files.len()]
            }
        };
        std::fs::create_dir_all(&dir)?;
        let incoming = Incoming {
            peer: ctx.peer,
            files,
            inbox,
            published,
        };
        save_meta(&dir, &incoming)?;
        self.drops.lock().insert(id.to_owned(), incoming);
        Ok(Response::Ok)
    }

    /// (inbox, index, size, published) of `path` in drop `id` from `peer`.
    fn drop_file(&self, peer: PeerId, id: &str, path: &str) -> Result<(PathBuf, usize, u64, bool)> {
        let drops = self.drops.lock();
        let d = drops
            .get(id)
            .filter(|d| d.peer == peer)
            .context("no such drop")?;
        let i = d
            .files
            .iter()
            .position(|(p, _)| p == path)
            .context("not in this drop")?;
        Ok((d.inbox.clone(), i, d.files[i].1, d.published[i]))
    }

    pub(crate) fn drop_partial(&self, peer: PeerId, id: &str, path: &str) -> Result<Response> {
        let (inbox, i, size, published) = self.drop_file(peer, id, path)?;
        Ok(if published {
            Response::Partial {
                len: size,
                complete: true,
            }
        } else {
            let staged = staging(&inbox, id).join(i.to_string());
            Response::Partial {
                len: std::fs::metadata(staged).map_or(0, |m| m.len()),
                complete: false,
            }
        })
    }

    pub(crate) async fn drop_piece(
        &self,
        peer: PeerId,
        id: &str,
        path: &str,
        body: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> Result<()> {
        let (inbox, i, size, published) = self.drop_file(peer, id, path)?;
        let end = at.offset + at.size;
        ensure!(!published, "already received");
        ensure!(end <= size, "piece beyond the offered size");
        ensure!(
            !at.final_ || (end == size && at.expect.is_some()),
            "bad final piece"
        );
        let dir = staging(&inbox, id);
        let piece = stage::write_piece(dir.join(i.to_string()), body, at).await?;
        if piece.done {
            return Ok(());
        }
        let rel = path.to_owned();
        let target = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let mut piece = piece;
            piece.verify(at.expect)?;
            let target = free_name(&inbox, &rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&piece.path, &target)?;
            piece.done = true;
            Ok(target)
        })
        .await??;
        let finished = {
            let mut drops = self.drops.lock();
            let d = drops.get_mut(id).context("drop cancelled")?;
            d.published[i] = true;
            let done = d.published.iter().all(|p| *p);
            if !done {
                save_meta(&dir, d)?;
            }
            done
        };
        if finished {
            self.drops.lock().remove(id);
            let _ = std::fs::remove_dir_all(&dir);
        }
        self.emit(NetEvent::DropReceived {
            peer,
            id: id.to_owned(),
            path: target,
        });
        Ok(())
    }

    pub(crate) fn drop_cancel(&self, peer: PeerId, id: &str) {
        let mut drops = self.drops.lock();
        if drops.get(id).is_some_and(|d| d.peer == peer) {
            let d = drops.remove(id).expect("present");
            let _ = std::fs::remove_dir_all(staging(&d.inbox, id));
        }
    }
}

// ---- sender ----

/// An error retrying cannot fix (declined, a file changed, an unsendable name).
#[derive(Debug)]
struct Permanent(String);
impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Permanent {}
fn permanent(msg: impl Into<String>) -> anyhow::Error {
    Permanent(msg.into()).into()
}

#[derive(Clone, Serialize, Deserialize)]
struct DropFile {
    src: VPath,
    rel: String,
    size: u64,
}

#[derive(Serialize, Deserialize)]
struct DropJob {
    id: String,
    /// This device (the node that sends).
    from: NodeId,
    peer: PeerId,
    paths: Vec<VPath>,
    /// Expanded on the first run (folders walked).
    files: Option<Vec<DropFile>>,
    /// Files before this one are on the device.
    next: usize,
    #[serde(skip)]
    sent: u64,
    /// (file index, `Response::Error`s in a row for it).
    #[serde(skip)]
    errors: (usize, u32),
}

fn walk(p: &dyn Provider, e: &Entry, rel: String, out: &mut Vec<DropFile>) -> Result<()> {
    ensure!(
        out.len() < MAX_FILES,
        permanent("too many files for one drop")
    );
    match e.kind {
        Kind::Dir if !e.is_link => {
            for child in p.list_complete(&e.path)? {
                let rel = format!("{rel}/{}", child.name);
                walk(p, &child, rel, out)?;
            }
        }
        Kind::File => {
            ensure!(
                valid_rel(&rel),
                permanent(format!("cannot send {rel}: the name is not portable"))
            );
            out.push(DropFile {
                src: e.path.clone(),
                rel,
                size: e.size,
            });
        }
        // Links to folders and dangling links are not followed.
        _ => {}
    }
    Ok(())
}

fn expand(router: &Router, paths: &[VPath]) -> Result<Vec<DropFile>> {
    let mut out = Vec::new();
    for path in paths {
        let p = router
            .provider_for(path)
            .with_context(|| format!("no provider for {}", path.display()))?;
        let e = p.stat(path)?;
        walk(p.as_ref(), &e, e.name.clone(), &mut out)?;
    }
    let names: HashSet<&str> = out.iter().map(|f| f.rel.as_str()).collect();
    ensure!(
        names.len() == out.len(),
        permanent("two items share a name")
    );
    ensure!(!out.is_empty(), permanent("no files to send"));
    Ok(out)
}

impl DropJob {
    fn total(&self) -> u64 {
        self.files
            .iter()
            .flatten()
            .map(|f| f.size)
            .sum::<u64>()
            .max(1)
    }

    fn done_before(&self) -> u64 {
        self.files
            .iter()
            .flatten()
            .take(self.next)
            .map(|f| f.size)
            .sum()
    }

    fn cancel_remote(&self) {
        if let Some((node, rt)) = node_for(&self.from) {
            let req = Request::DropCancel {
                id: self.id.clone(),
            };
            let _ = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(3), node.request(&self.peer, req)).await
            });
        }
    }

    fn attempt(&mut self, ctx: &JobCtx, router: &Router) -> Result<()> {
        let (node, rt) =
            node_for(&self.from).ok_or_else(|| anyhow!("the device link is closed"))?;
        let files = self.files.clone().expect("expanded");
        let offer = Request::DropOffer {
            id: self.id.clone(),
            files: files.iter().map(|f| (f.rel.clone(), f.size)).collect(),
        };
        match rt.block_on(node.request(&self.peer, offer))? {
            Response::Ok => {}
            Response::Denied(_) => return Err(permanent("the device declined the drop")),
            _ => return Err(permanent("the device could not take the drop")),
        }
        while self.next < files.len() {
            self.send_file(ctx, router, &node, &rt, &files[self.next])?;
            self.next += 1;
            let progress = self.done_before() as f32 / self.total() as f32;
            ctx.cursor(json!({ "next": self.next }), progress)?;
        }
        Ok(())
    }

    fn send_file(
        &mut self,
        ctx: &JobCtx,
        router: &Router,
        node: &Node,
        rt: &Handle,
        f: &DropFile,
    ) -> Result<()> {
        let source = format!("{SOURCE}{}", self.id);
        let status = Request::StatPartial {
            source: source.clone(),
            path: f.rel.clone(),
        };
        let (len, complete) = match rt.block_on(node.request(&self.peer, status))? {
            Response::Partial { len, complete } => (len, complete),
            _ => bail!("the device lost the drop"),
        };
        if complete {
            return Ok(());
        }
        let offset = if len <= f.size { len } else { 0 };
        if offset > 0 {
            ctx.log(&format!("{}: resuming at {offset} bytes", f.rel))?;
        }
        let provider = router
            .provider_for(&f.src)
            .with_context(|| format!("no provider for {}", f.src.display()))?;
        let mut reader = provider.read(&f.src)?;
        let changed = || permanent(format!("{} changed while sending", f.rel));
        let mut hasher = blake3::Hasher::new();
        let skipped = std::io::copy(&mut (&mut reader).take(offset), &mut hasher)?;
        ensure!(skipped == offset, changed());
        let mut pos = offset;
        loop {
            if ctx.stopping() {
                return Err(Cancelled.into());
            }
            let n = CHUNK.min(f.size - pos);
            let mut buf = vec![0; n as usize];
            reader.read_exact(&mut buf).map_err(|_| changed())?;
            hasher.update(&buf);
            let final_ = pos + n == f.size;
            if final_ {
                ensure!(reader.read(&mut [0u8])? == 0, changed());
            }
            let at = WriteAt {
                offset: pos,
                size: n,
                final_,
                expect: final_.then(|| *hasher.finalize().as_bytes()),
            };
            let body = Box::new(std::io::Cursor::new(buf));
            match rt.block_on(node.write_stream(&self.peer, &source, &f.rel, body, at))? {
                Response::Ok => self.errors = (0, 0),
                Response::Error(e) => {
                    let n = if self.errors.0 == self.next {
                        self.errors.1 + 1
                    } else {
                        1
                    };
                    self.errors = (self.next, n);
                    ensure!(
                        n < 4,
                        permanent(format!("{}: the device keeps failing: {e}", f.rel))
                    );
                    bail!("the device reported: {e}");
                }
                _ => bail!("the device refused a piece"),
            }
            pos += n;
            self.sent += n;
            let done = self.done_before() + pos;
            ctx.progress(done as f32 / self.total() as f32)?;
            if final_ {
                return Ok(());
            }
        }
    }
}

impl Job for DropJob {
    fn kind(&self) -> &'static str {
        KIND
    }
    fn run(&mut self, ctx: &JobCtx) -> Result<()> {
        let router = ctx.router();
        if self.files.is_none() {
            self.files = Some(expand(&router, &self.paths)?);
            ctx.checkpoint(self.checkpoint(), 0.0)?;
        }
        let mut backoff = Duration::from_millis(500);
        let mut moved = Instant::now();
        loop {
            let sent = self.sent;
            let err = match self.attempt(ctx, &router) {
                Ok(()) => {
                    let files = self.files.as_ref().map_or(0, Vec::len);
                    ctx.set_result(json!({ "files": files, "bytes": self.total() }))?;
                    return Ok(());
                }
                Err(e) => e,
            };
            if err.is::<Cancelled>() {
                // Closing (not a cancel): the job resumes on the next open.
                if !ctx.closing() {
                    self.cancel_remote();
                }
                return Err(err);
            }
            if err.is::<Permanent>() {
                self.cancel_remote();
                return Err(err);
            }
            if self.sent > sent {
                moved = Instant::now();
                backoff = Duration::from_millis(500);
            }
            if moved.elapsed() > GIVE_UP {
                self.cancel_remote();
                return Err(err.context("no progress, giving up"));
            }
            ctx.log(&format!("link problem, retrying: {err:#}"))?;
            let until = Instant::now() + backoff;
            while Instant::now() < until {
                if ctx.stopping() {
                    if !ctx.closing() {
                        self.cancel_remote();
                    }
                    return Err(Cancelled.into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }
    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<DropJob>(v)?))
    }
}
