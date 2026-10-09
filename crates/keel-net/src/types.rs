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
    Request { peer: PeerId, what: String },
}

/// Host operations. Paths are validated relative slash-separated paths, and ranges
/// are `(offset, length)`. Implementations must reject symlink traversal unless
/// they independently enforce the authorized subtree after resolution. They
/// must not interpret percent escapes or normalize paths into another resource.
/// Writes must stage their body and publish atomically only after exact EOF;
/// dropping any future/body on cancellation must discard staged changes.
#[async_trait::async_trait]
pub trait Handler: Send + Sync {
    async fn sources(&self) -> Vec<SourceInfo>;
    async fn list(&self, source: &str, path: &str) -> Result<Vec<EntryInfo>>;
    async fn stat(&self, source: &str, path: &str) -> Result<EntryInfo>;
    async fn read(
        &self,
        source: &str,
        path: &str,
        range: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>>;
    async fn write(
        &self,
        source: &str,
        path: &str,
        body: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
    ) -> Result<()>;
    async fn mkdir(&self, source: &str, path: &str) -> Result<()>;
    async fn rename(&self, source: &str, from: &str, to: &str) -> Result<()>;
    async fn remove(&self, source: &str, path: &str) -> Result<()>;
    async fn storage(&self) -> Option<Storage>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
    pub content_id: Option<[u8; 32]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Ping,
    ListSources,
    List {
        source: String,
        path: String,
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
    Write {
        source: String,
        path: String,
        size: u64,
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
            Self::Mkdir { .. } => "mkdir",
            Self::Rename { .. } => "rename",
            Self::Remove { .. } => "remove",
            Self::Grants => "grants",
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
    Entries(Vec<EntryInfo>),
    Entry(EntryInfo),
    Read {
        size: u64,
    },
    Ok,
    Grants(Vec<Grant>),
    Denied(String),
    Error(String),
}
