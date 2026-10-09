use std::{fmt, str::FromStr};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

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
    /// Spacedrop: `ctx.peer` offers `files` (relative paths and sizes) as drop `id`.
    /// `Some(inbox folder)` accepts; the default declines.
    async fn drop_offer(
        &self,
        ctx: &RequestCtx,
        id: &str,
        files: &[(String, u64)],
    ) -> Option<std::path::PathBuf> {
        let _ = (ctx, id, files);
        None
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
    /// `Response::Ok` accepts (again for the same device and files: idempotent, so a
    /// sender re-offers to resume), `Response::Denied` declines. Once accepted, the
    /// pieces go as `Write` / `StatPartial` with source `drop:<id>`.
    DropOffer {
        id: String,
        files: Vec<(String, u64)>,
    },
    /// Spacedrop: the sender gave up; the receiver drops what it staged.
    DropCancel {
        id: String,
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
            Self::DropCancel { .. } => "drop-cancel",
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
    Grants(Vec<Grant>),
    Denied(String),
    Error(String),
}
