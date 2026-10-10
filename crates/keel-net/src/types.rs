use std::{fmt, str::FromStr};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncRead, sync::watch};

/// Ed25519 public identity. Display is lowercase, unpadded RFC 4648 base32.
/// iroh 1.3 displays hex instead; parsing accepts both formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 32]);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            &data_encoding::BASE32_NOPAD
                .encode(&self.0)
                .to_ascii_lowercase(),
        )
    }
}

impl FromStr for NodeId {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let key = if s.len() == 64 {
            s.parse::<iroh::PublicKey>()?
        } else {
            let bytes: [u8; 32] = data_encoding::BASE32_NOPAD
                .decode(s.to_ascii_uppercase().as_bytes())?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid node id length"))?;
            iroh::PublicKey::from_bytes(&bytes)?
        };
        Ok(Self(*key.as_bytes()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub NodeId);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    Lan,
    Relay,
    Offline,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub id: PeerId,
    pub label: String,
    pub last_seen: Option<i64>,
    pub link: Link,
    pub storage: Option<Storage>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Storage {
    pub used: u64,
    pub total: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Access {
    Read,
    ReadWrite,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub peer: PeerId,
    pub source: String,
    pub subtree: String,
    pub access: Access,
    pub created: i64,
}
#[derive(Clone, Debug)]
pub enum NetEvent {
    PeerOnline(PeerId, Link),
    PeerOffline(PeerId),
    Paired(Peer),
    GrantChanged,
    /// A device asked something (the first request of each second per device).
    Request {
        peer: PeerId,
        what: String,
    },
    /// Spacedrop: a received file was verified and moved into the inbox at `path`.
    DropReceived {
        peer: PeerId,
        id: String,
        path: std::path::PathBuf,
    },
    /// Library sync: a pull from `peer` changed the library (`applied` entries won).
    LibrarySynced {
        peer: PeerId,
        applied: usize,
    },
}

/// A Spacedrop offer waiting for an answer (`Handler::drop_offer`).
pub struct IncomingDrop {
    pub peer: PeerId,
    /// The sending device's label.
    pub label: String,
    pub id: String,
    /// Relative paths and sizes.
    pub files: Vec<(String, u64)>,
    pub reply: DropReply,
}

/// Answers an offer once; dropped unanswered, the offer is declined.
pub struct DropReply(pub(crate) watch::Sender<Option<bool>>);
impl DropReply {
    pub fn answer(self, accept: bool) {
        let _ = self.0.send(Some(accept));
    }
    /// The offer is gone: the sender cancelled it, or the device was forgotten. Take the
    /// prompt down.
    pub fn withdrawn(&self) -> bool {
        self.0.is_closed()
    }
}

/// Who a `Handler` call serves: the paired device and its label (as it last told us).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestCtx {
    pub peer: PeerId,
    pub label: String,
}

/// One pushed piece of a file (`Request::Write`). `offset` must equal the host's staged
/// length for this target (`Request::StatPartial`), except 0, which starts over. `final_`
/// publishes the staged file atomically once it holds `offset + size` bytes, after
/// checking `expect` (the BLAKE3 of the whole file) when given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteAt {
    pub offset: u64,
    pub size: u64,
    pub final_: bool,
    pub expect: Option<[u8; 32]>,
}

/// Host operations. Paths are validated relative slash-separated paths, and ranges
/// are `(offset, length)`. Implementations must reject symlink traversal unless
/// they independently enforce the authorized subtree after resolution. They
/// must not interpret percent escapes or normalize paths into another resource.
/// Writes append to a `.keel-partial-<id>` staging file (`<id>` fixed per device and
/// target, so a dropped transfer resumes from `stat_partial`) and publish atomically
/// only on a final write after exact EOF; a cancelled write leaves the staging file at
/// the piece's `offset` (removed when that is 0).
#[async_trait::async_trait]
pub trait Handler: Send + Sync {
    async fn sources(&self, ctx: &RequestCtx) -> Vec<SourceInfo>;
    async fn list(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<Vec<EntryInfo>>;
    async fn stat(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<EntryInfo>;
    async fn read(
        &self,
        ctx: &RequestCtx,
        source: &str,
        path: &str,
        range: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>>;
    async fn write(
        &self,
        ctx: &RequestCtx,
        source: &str,
        path: &str,
        body: Box<dyn AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> Result<()>;
    /// Bytes staged for `path` by this device (0 when nothing is).
    async fn stat_partial(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<u64>;
    async fn mkdir(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<()>;
    async fn rename(&self, ctx: &RequestCtx, source: &str, from: &str, to: &str) -> Result<()>;
    async fn remove(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<()>;
    async fn storage(&self, ctx: &RequestCtx) -> Option<Storage>;
    /// Spacedrop: a device offers files. Hand `offer.reply` to whoever decides (answer now
    /// or later, from any thread) and return the inbox the drop would land in; never wait
    /// here. `None` declines at once (the default).
    fn drop_offer(&self, offer: IncomingDrop) -> Option<std::path::PathBuf> {
        drop(offer);
        None
    }
    /// The node is closing: write out anything buffered (the op log). Blocking.
    fn close(&self) {}
    /// The node opened with this identity (library sync breaks ties with it). Blocking.
    fn opened(&self, id: NodeId) {
        let _ = id;
    }
    /// Library sync: this device's own changes after `since`, one page of at most
    /// `limit` (`keel_core::Library::sync_page`). Blocking. The default serves none.
    fn sync_page(&self, since: u64, limit: usize) -> Result<keel_core::SyncPage> {
        let _ = (since, limit);
        anyhow::bail!("this device does not sync a library")
    }
    /// Library sync: where the next pull from `peer` starts. Blocking.
    fn sync_since(&self, peer: &PeerId) -> Result<u64> {
        let _ = peer;
        Ok(0)
    }
    /// Library sync: applies a page pulled from `peer` (`keel_core::Library::sync_apply`).
    /// Blocking.
    fn sync_apply(
        &self,
        peer: &PeerId,
        page: &keel_core::SyncPage,
    ) -> Result<keel_core::SyncApplied> {
        let _ = (peer, page);
        anyhow::bail!("this device does not sync a library")
    }
    /// The op log hook: what the node handled itself for `ctx`'s device (`drop-offer`,
    /// `drop-received`, `drop-cancel`), with whether it went through.
    fn log(&self, ctx: &RequestCtx, op: &str, payload: serde_json::Value, ok: bool) {
        let _ = (ctx, op, payload, ok);
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
}
/// `modified` is in unix seconds; `content_id` is the BLAKE3 of the bytes, when the
/// host knows it (CBOR bytes on the wire).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
    #[serde(with = "serde_bytes")]
    pub content_id: Option<[u8; 32]>,
}
/// Most entries one `Response::Entries` page carries (keeps it under the header limit).
pub const PAGE_LIMIT: u32 = 500;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Ping,
    ListSources,
    /// One page of a folder, by name: the entries after `after` (from the start when
    /// None), at most `limit` (capped at `PAGE_LIMIT`). `Response::Entries::more` says
    /// whether to ask again with the last name as `after`.
    List {
        source: String,
        path: String,
        after: Option<String>,
        limit: u32,
    },
    Stat {
        source: String,
        path: String,
    },
    Read {
        source: String,
        path: String,
        range: Option<(u64, u64)>,
    },
    /// Body: exactly `size` bytes. See `WriteAt`.
    Write {
        source: String,
        path: String,
        offset: u64,
        size: u64,
        final_: bool,
        #[serde(with = "serde_bytes")]
        expect: Option<[u8; 32]>,
    },
    /// The staged length of an unfinished write: `Response::Partial`.
    StatPartial {
        source: String,
        path: String,
    },
    Mkdir {
        source: String,
        path: String,
    },
    Rename {
        source: String,
        from: String,
        to: String,
    },
    Remove {
        source: String,
        path: String,
    },
    Grants,
    /// Spacedrop: offer `files` (relative paths, sizes) as drop `id` (32 hex digits).
    /// Answered at once: `Response::Ok` accepted (again for the same device and files:
    /// idempotent, so a sender re-offers to resume), `Response::Pending` while the user
    /// decides (poll `DropStatus`), `Response::Denied` declined. The same id with another
    /// file list is asked again. Once accepted, the pieces go as `Write` / `StatPartial`
    /// with source `drop:<id>`.
    DropOffer {
        id: String,
        files: Vec<(String, u64)>,
    },
    /// Spacedrop: how an offer stands (waits briefly for the user): `Ok`, `Pending`,
    /// `Denied`, or `Error` when the receiver no longer knows it (offer again).
    DropStatus {
        id: String,
    },
    /// Spacedrop: the sender gave up; the receiver withdraws the prompt or drops what it
    /// staged.
    DropCancel {
        id: String,
    },
    /// Library sync: the host's own changes after the sequence number given for the
    /// host's id in `since` (0 when absent), as `Response::SyncEntries`. Answered only
    /// when the host turned sync with the asking device on; `Response::Denied` otherwise.
    SyncPull {
        since: Vec<(NodeId, u64)>,
    },
}
impl Request {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::ListSources => "sources",
            Self::List { .. } => "list",
            Self::Stat { .. } => "stat",
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::StatPartial { .. } => "stat-partial",
            Self::Mkdir { .. } => "mkdir",
            Self::Rename { .. } => "rename",
            Self::Remove { .. } => "remove",
            Self::Grants => "grants",
            Self::DropOffer { .. } => "drop-offer",
            Self::DropStatus { .. } => "drop-status",
            Self::DropCancel { .. } => "drop-cancel",
            Self::SyncPull { .. } => "sync-pull",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Pong {
        label: String,
        storage: Option<Storage>,
    },
    Sources(Vec<SourceInfo>),
    Entries {
        entries: Vec<EntryInfo>,
        more: bool,
    },
    Entry(EntryInfo),
    Read {
        size: u64,
    },
    /// Bytes staged so far; `complete` once that target was already published.
    Partial {
        len: u64,
        complete: bool,
    },
    Ok,
    /// A Spacedrop offer is waiting for the user.
    Pending,
    Grants(Vec<Grant>),
    /// Library sync: at most `keel_core::SYNC_PAGE` entries; `more` asks again from `upto`.
    SyncEntries {
        entries: Vec<keel_core::SyncEntry>,
        more: bool,
        upto: u64,
    },
    Denied(String),
    Error(String),
}
