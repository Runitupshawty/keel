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
//! The offer is answered at once: accepted, declined, or pending while the receiving
//! user decides; the sender then polls (`Request::DropStatus`) until an answer, a cancel
//! or `GIVE_UP`. Every sent file is in the sender's op log (`net.drop-sent`).
//!
//! Receiver: the node hands the offer to its `Handler::drop_offer`, which names the inbox
//! and passes the reply on. An answer holds for that device, drop id and file list only.
//! Pieces are staged in `<inbox>/.keel-partial-<key>/<n>` (`meta.json` written once, and a
//! line per published file in `published`, so a restarted receiver resumes when the sender
//! re-offers); a file is moved into the inbox only when complete and verified, never
//! replacing anything (`name (1).ext`), and logged (`net.drop-received`, through
//! `Handler::log`).
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
use tokio::{runtime::Handle, sync::watch};

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

/// Offers by device, then drop id: a decision is the device's and that file list's only.
pub(crate) type Drops = HashMap<PeerId, HashMap<String, Offer>>;

pub(crate) enum Offer {
    /// Waiting for the user (`IncomingDrop::reply`); lands in `inbox` when accepted (None
    /// while the handler is being asked).
    Asking {
        files: Vec<(String, u64)>,
        inbox: Option<PathBuf>,
        answer: watch::Receiver<Option<bool>>,
        /// A `DropStatus` waits for the answer (one at a time).
        waiting: bool,
    },
    /// Accepted; its staging folder is being set up.
    Preparing {
        files: Vec<(String, u64)>,
    },
    Accepted(Incoming),
}

impl Offer {
    fn files(&self) -> &[(String, u64)] {
        match self {
            Offer::Asking { files, .. } | Offer::Preparing { files } => files,
            Offer::Accepted(d) => &d.files,
        }
    }
}

pub(crate) struct Incoming {
    files: Vec<(String, u64)>,
    /// Offered path to its index in `files`.
    index: HashMap<String, usize>,
    inbox: PathBuf,
    dir: PathBuf,
    published: Vec<bool>,
    /// Files not published yet.
    left: usize,
}

/// Written once, when the drop is accepted; published files are appended to `published`.
#[derive(Serialize, Deserialize)]
struct Meta {
    peer: PeerId,
    files: Vec<(String, u64)>,
}

/// Staging older than this, left by an earlier session, is swept (`sweep`).
pub const STALE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// `<inbox>/.keel-partial-<hash of device and drop id>`: two devices never share one.
fn staging(inbox: &Path, peer: &PeerId, id: &str) -> PathBuf {
    let mut h = blake3::Hasher::new();
    h.update(&peer.0 .0);
    h.update(id.as_bytes());
    let key = data_encoding::HEXLOWER.encode(&h.finalize().as_bytes()[..16]);
    inbox.join(format!(".keel-partial-{key}"))
}

/// Hidden in Explorer too (the dot hides it elsewhere).
#[cfg(windows)]
fn hide(p: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN},
    };
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is a NUL-terminated path that outlives the call.
    let _ = unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) };
}
#[cfg(not(windows))]
fn hide(_: &Path) {}

/// Blocking: the staging folder of an accepted drop, resumed when an earlier session left
/// one for the same device and files, else new (meta written once).
fn prepare(peer: PeerId, id: &str, files: Vec<(String, u64)>, inbox: PathBuf) -> Result<Incoming> {
    let dir = staging(&inbox, &peer, id);
    let same = std::fs::read(dir.join("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
        .is_some_and(|m| m.peer == peer && m.files == files);
    let mut published = vec![false; files.len()];
    if same {
        let log = std::fs::read_to_string(dir.join("published")).unwrap_or_default();
        for i in log.lines().filter_map(|l| l.parse::<usize>().ok()) {
            if let Some(p) = published.get_mut(i) {
                *p = true;
            }
        }
    } else {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        hide(&dir);
        let meta = Meta {
            peer,
            files: files.clone(),
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta)?)?;
    }
    let index = files
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.clone(), i))
        .collect();
    let left = published.iter().filter(|p| !**p).count();
    Ok(Incoming {
        files,
        index,
        inbox,
        dir,
        published,
        left,
    })
}

/// Deletes staging folders in `inbox` that nothing touched for `older_than` (left by a
/// session that ended before its drops did). Best effort.
pub fn sweep(inbox: &Path, older_than: Duration) {
    let touched = |dir: &Path| {
        std::iter::once(std::fs::metadata(dir))
            .chain(
                std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.metadata()),
            )
            .filter_map(|m| m.ok()?.modified().ok())
            .max()
    };
    for e in std::fs::read_dir(inbox).into_iter().flatten().flatten() {
        let stale = e
            .file_name()
            .to_string_lossy()
            .starts_with(".keel-partial-")
            && e.file_type().is_ok_and(|t| t.is_dir())
            && touched(&e.path())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= older_than);
        if stale {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Moves the verified staging file to `inbox/rel`, or `name (n).ext` beside it when that
/// is taken; never replaces anything (no-replace rename, so two drops landing one name at
/// once both arrive).
pub(crate) fn publish(staged: &Path, inbox: &Path, rel: &str) -> Result<PathBuf> {
    let target = rel.split('/').fold(inbox.to_owned(), |p, c| p.join(c));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.into_owned(), String::new()),
    };
    let from = VPath::local(staged);
    for n in 0..10_000 {
        let to = match n {
            0 => target.clone(),
            n => target.with_file_name(format!("{stem} ({n}){ext}")),
        };
        match keel_vfs::LocalProvider.rename_noreplace(&from, &VPath::local(&to)) {
            Ok(()) => return Ok(to),
            Err(_) if to.symlink_metadata().is_ok() => continue,
            Err(e) => return Err(e),
        }
    }
    bail!("no free name for {rel}")
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
        id.is_some_and(|id| {
            let drops = self.drops.lock();
            matches!(
                drops.get(&peer).and_then(|m| m.get(id)),
                Some(Offer::Accepted(_))
            )
        })
    }

    fn log_drop(&self, ctx: &RequestCtx, op: &str, payload: serde_json::Value, ok: bool) {
        self.handler.log(ctx, op, payload, ok);
    }

    /// Answers at once: `Ok` (accepted), `Pending` (the user is deciding: poll with
    /// `DropStatus`) or `Denied`. The same device offering the same id with another file
    /// list is asked again.
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
        // Registered before the handler is asked, so concurrent re-offers ask once.
        let (ask, replaced) = {
            let mut drops = self.drops.lock();
            let mine = drops.entry(ctx.peer).or_default();
            if mine.get(id).is_some_and(|o| o.files() == files.as_slice()) {
                (None, None)
            } else {
                let replaced = mine.remove(id);
                let (tx, rx) = watch::channel(None);
                let asking = Offer::Asking {
                    files: files.clone(),
                    inbox: None,
                    answer: rx,
                    waiting: false,
                };
                mine.insert(id.to_owned(), asking);
                (Some(tx), replaced)
            }
        };
        if let Some(Offer::Accepted(old)) = replaced {
            let _ = tokio::fs::remove_dir_all(&old.dir).await;
        }
        if let Some(tx) = ask {
            let offer = IncomingDrop {
                peer: ctx.peer,
                label: ctx.label.clone(),
                id: id.to_owned(),
                files: files.clone(),
                reply: DropReply(tx),
            };
            let inbox = self.handler.drop_offer(offer);
            let mut drops = self.drops.lock();
            let mine = drops.entry(ctx.peer).or_default();
            match (inbox, mine.get_mut(id)) {
                (Some(dir), Some(Offer::Asking { inbox, .. })) => *inbox = Some(dir),
                (Some(_), _) => {}
                (None, _) => {
                    mine.remove(id);
                    drop(drops);
                    let payload = json!({"drop": id, "files": files.len()});
                    self.log_drop(ctx, "drop-offer", payload, false);
                    return Ok(Response::Denied("declined".into()));
                }
            }
        }
        self.drop_status(ctx, id, Duration::ZERO).await
    }

    /// `DropStatus`: waits up to `wait` for the user (one waiter per offer; others get
    /// `Pending` at once), then answers like `drop_offer`. Unknown: `Error`.
    pub(crate) async fn drop_status(
        &self,
        ctx: &RequestCtx,
        id: &str,
        mut wait: Duration,
    ) -> Result<Response> {
        let peer = ctx.peer;
        loop {
            let decided = {
                let mut drops = self.drops.lock();
                let Some(offer) = drops.get_mut(&peer).and_then(|m| m.get_mut(id)) else {
                    return Ok(Response::Error("no such drop".into()));
                };
                match offer {
                    Offer::Accepted(_) => return Ok(Response::Ok),
                    Offer::Preparing { .. } => return Ok(Response::Pending),
                    // The handler is still being asked.
                    Offer::Asking { inbox: None, .. } => return Ok(Response::Pending),
                    Offer::Asking {
                        answer, waiting, ..
                    } => {
                        let seen = *answer.borrow();
                        match seen {
                            Some(accept) => Ok(accept),
                            // Dropped unanswered: declined.
                            None if answer.has_changed().is_err() => Ok(false),
                            None if wait.is_zero() || *waiting => return Ok(Response::Pending),
                            None => {
                                *waiting = true;
                                Err(answer.clone())
                            }
                        }
                    }
                }
            };
            match decided {
                Err(mut answer) => {
                    let _ = tokio::time::timeout(wait, answer.wait_for(Option::is_some)).await;
                    wait = Duration::ZERO;
                    let mut drops = self.drops.lock();
                    if let Some(Offer::Asking { waiting, .. }) =
                        drops.get_mut(&peer).and_then(|m| m.get_mut(id))
                    {
                        *waiting = false;
                    }
                }
                Ok(false) => {
                    let gone = self.drops.lock().get_mut(&peer).and_then(|m| m.remove(id));
                    let n = gone.map_or(0, |o| o.files().len());
                    self.log_drop(ctx, "drop-offer", json!({"drop": id, "files": n}), false);
                    return Ok(Response::Denied("declined".into()));
                }
                Ok(true) => return self.accept(ctx, id).await,
            }
        }
    }

    /// The user said yes: sets up the staging folder (no lock held), then takes pieces.
    async fn accept(&self, ctx: &RequestCtx, id: &str) -> Result<Response> {
        let peer = ctx.peer;
        let (files, inbox) = {
            let mut drops = self.drops.lock();
            let Some(offer) = drops.get_mut(&peer).and_then(|m| m.get_mut(id)) else {
                return Ok(Response::Error("no such drop".into()));
            };
            let Offer::Asking {
                files,
                inbox: Some(inbox),
                ..
            } = offer
            else {
                return Ok(Response::Pending);
            };
            let (files, inbox) = (files.clone(), inbox.clone());
            *offer = Offer::Preparing {
                files: files.clone(),
            };
            (files, inbox)
        };
        let n = files.len();
        let owned = id.to_owned();
        let prepared = tokio::task::spawn_blocking(move || prepare(peer, &owned, files, inbox))
            .await
            .map_err(|e| anyhow!("staging failed: {e}"))
            .and_then(|r| r);
        let mut drops = self.drops.lock();
        let slot = drops.get_mut(&peer).and_then(|m| m.get_mut(id));
        let result = match (prepared, slot) {
            // Still wanted (not cancelled or forgotten meanwhile).
            (Ok(incoming), Some(slot @ Offer::Preparing { .. })) => {
                *slot = Offer::Accepted(incoming);
                Ok(Response::Ok)
            }
            (Ok(incoming), _) => {
                drop(drops);
                let _ = std::fs::remove_dir_all(&incoming.dir);
                return Ok(Response::Error("no such drop".into()));
            }
            (Err(e), _) => {
                if let Some(m) = drops.get_mut(&peer) {
                    m.remove(id);
                }
                Err(e)
            }
        };
        drop(drops);
        let ok = result.is_ok();
        self.log_drop(ctx, "drop-offer", json!({"drop": id, "files": n}), ok);
        result
    }

    /// (inbox, staging folder, index, size, published) of `path` in drop `id` from `peer`.
    fn drop_file(
        &self,
        peer: PeerId,
        id: &str,
        path: &str,
    ) -> Result<(PathBuf, PathBuf, usize, u64, bool)> {
        let drops = self.drops.lock();
        let Some(Offer::Accepted(d)) = drops.get(&peer).and_then(|m| m.get(id)) else {
            bail!("no such drop");
        };
        let i = *d.index.get(path).context("not in this drop")?;
        Ok((
            d.inbox.clone(),
            d.dir.clone(),
            i,
            d.files[i].1,
            d.published[i],
        ))
    }

    pub(crate) fn drop_partial(&self, peer: PeerId, id: &str, path: &str) -> Result<Response> {
        let (_, dir, i, size, published) = self.drop_file(peer, id, path)?;
        Ok(if published {
            Response::Partial {
                len: size,
                complete: true,
            }
        } else {
            Response::Partial {
                len: std::fs::metadata(dir.join(i.to_string())).map_or(0, |m| m.len()),
                complete: false,
            }
        })
    }

    pub(crate) async fn drop_piece(
        &self,
        ctx: &RequestCtx,
        id: &str,
        path: &str,
        body: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> Result<()> {
        let peer = ctx.peer;
        let (inbox, dir, i, size, published) = self.drop_file(peer, id, path)?;
        let end = at.offset + at.size;
        ensure!(!published, "already received");
        ensure!(end <= size, "piece beyond the offered size");
        ensure!(
            !at.final_ || (end == size && at.expect.is_some()),
            "bad final piece"
        );
        let piece = stage::write_piece(dir.join(i.to_string()), body, at).await?;
        if piece.done {
            return Ok(());
        }
        let (rel, marks, into) = (path.to_owned(), dir.join("published"), inbox.clone());
        let target = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let mut piece = piece;
            piece.verify(at.expect)?;
            let target = publish(&piece.path, &into, &rel)?;
            piece.done = true;
            // A restarted receiver knows which files it already published.
            use std::io::Write;
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(marks)?;
            writeln!(log, "{i}")?;
            Ok(target)
        })
        .await??;
        let finished = {
            let mut drops = self.drops.lock();
            match drops.get_mut(&peer).and_then(|m| m.get_mut(id)) {
                Some(Offer::Accepted(d)) if !d.published[i] => {
                    d.published[i] = true;
                    d.left -= 1;
                    d.left == 0
                }
                _ => false,
            }
        };
        if finished {
            if let Some(m) = self.drops.lock().get_mut(&peer) {
                m.remove(id);
            }
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
        let saved = target
            .strip_prefix(&inbox)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let hash = at.expect.map(|h| data_encoding::HEXLOWER.encode(&h));
        let payload = json!({"drop": id, "path": path, "saved_as": saved, "size": size,
            "blake3": hash});
        self.log_drop(ctx, "drop-received", payload, true);
        self.emit(NetEvent::DropReceived {
            peer,
            id: id.to_owned(),
            path: target,
        });
        Ok(())
    }

    /// The sender gave up: a prompt still waiting is withdrawn, staging is dropped.
    pub(crate) async fn drop_cancel(&self, ctx: &RequestCtx, id: &str) {
        let gone = self
            .drops
            .lock()
            .get_mut(&ctx.peer)
            .and_then(|m| m.remove(id));
        if let Some(offer) = gone {
            if let Offer::Accepted(d) = &offer {
                let _ = tokio::fs::remove_dir_all(&d.dir).await;
            }
            self.log_drop(ctx, "drop-cancel", json!({"drop": id}), true);
        }
    }

    /// `forget_peer`: every offer of `peer` goes (prompts withdrawn, staging deleted).
    pub(crate) fn forget_drops(&self, peer: &PeerId) {
        let gone = self.drops.lock().remove(peer).unwrap_or_default();
        for offer in gone.into_values() {
            if let Offer::Accepted(d) = offer {
                let _ = std::fs::remove_dir_all(&d.dir);
            }
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

    /// The offer; one too big for a request header fails for good (it never fits).
    fn offer(&self, files: &[DropFile]) -> Result<Request> {
        let offer = Request::DropOffer {
            id: self.id.clone(),
            files: files.iter().map(|f| (f.rel.clone(), f.size)).collect(),
        };
        ensure!(
            crate::wire::encode(&offer).is_ok(),
            permanent("too many files (or too long names) for one drop: send fewer at a time")
        );
        Ok(offer)
    }

    /// Offers the drop, then polls (`DropStatus`) while the device's user decides.
    fn offered(&self, ctx: &JobCtx, node: &Node, rt: &Handle, offer: Request) -> Result<()> {
        let asked = Instant::now();
        let mut answer = rt.block_on(node.request(&self.peer, offer))?;
        let mut told = false;
        loop {
            match answer {
                Response::Ok => return Ok(()),
                Response::Pending => {}
                Response::Denied(_) => return Err(permanent("the device declined the drop")),
                // The device forgot the offer (it restarted): offer again.
                Response::Error(e) => bail!("the device lost the offer: {e}"),
                _ => return Err(permanent("the device could not take the drop")),
            }
            if !told {
                ctx.log("waiting for the device to accept")?;
                told = true;
            }
            ensure!(
                asked.elapsed() < GIVE_UP,
                permanent("the device did not answer the drop")
            );
            let until = Instant::now() + Duration::from_millis(250);
            while Instant::now() < until {
                if ctx.stopping() {
                    return Err(Cancelled.into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let status = Request::DropStatus {
                id: self.id.clone(),
            };
            answer = rt.block_on(node.request(&self.peer, status))?;
        }
    }

    fn attempt(&mut self, ctx: &JobCtx, router: &Router) -> Result<()> {
        let (node, rt) =
            node_for(&self.from).ok_or_else(|| anyhow!("the device link is closed"))?;
        let files = self.files.clone().expect("expanded");
        let offer = self.offer(&files)?;
        self.offered(ctx, &node, &rt, offer)?;
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
                let payload = json!({"peer": self.peer.0.to_string(), "drop": self.id,
                    "path": f.rel, "from": f.src.display(), "size": f.size,
                    "blake3": at.expect.map(|h| data_encoding::HEXLOWER.encode(&h))});
                ctx.log_op("net.drop-sent", &payload, "ok", true)?;
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
