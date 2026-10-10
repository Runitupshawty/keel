//! keel-net glue for hosts of the API. A host's node serves the library's sources through
//! keel-net's `LibraryHandler` (`Host::open_net`); [`NoSources`] offers none (tests).
//! Spacedrops sent to it go through [`Drops`]: offers from `[devices] auto_accept` devices
//! are accepted, the others wait for `spacedrop.answer` (declined when the sender stops
//! waiting), and files land in the inbox.

use anyhow::{bail, Result};
use keel_net::{
    EntryInfo, Handler, IncomingDrop, PeerId, RequestCtx, SourceInfo, Storage, WriteAt,
};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncRead;

/// An offer waiting for `spacedrop.answer`.
pub struct Waiting {
    pub peer: PeerId,
    /// The sending device's label.
    pub label: String,
    pub id: String,
    /// Relative paths and sizes.
    pub files: Vec<(String, u64)>,
}

/// The host's Spacedrop inbox and the offers waiting for an answer.
pub struct Drops {
    pub inbox: PathBuf,
    auto_accept: Vec<String>,
    pending: Mutex<Vec<IncomingDrop>>,
}

impl Drops {
    pub fn new(inbox: PathBuf, auto_accept: Vec<String>) -> Self {
        Self {
            inbox,
            auto_accept,
            pending: Mutex::default(),
        }
    }

    /// keel-net caps waiting offers per device and in all; withdrawn ones are dropped here.
    pub(crate) fn offer(&self, offer: IncomingDrop) -> Option<PathBuf> {
        if self.auto_accept.contains(&offer.peer.0.to_string()) {
            offer.reply.answer(true);
        } else {
            let mut pending = self.pending.lock();
            pending.retain(|d| !d.reply.withdrawn());
            pending.push(offer);
        }
        Some(self.inbox.clone())
    }

    /// Offers still waiting.
    pub fn pending(&self) -> Vec<Waiting> {
        let mut pending = self.pending.lock();
        pending.retain(|d| !d.reply.withdrawn());
        pending
            .iter()
            .map(|d| Waiting {
                peer: d.peer,
                label: d.label.clone(),
                id: d.id.clone(),
                files: d.files.clone(),
            })
            .collect()
    }

    /// Answers a waiting offer; false when there is none (answered, or withdrawn).
    pub fn answer(&self, peer: &PeerId, id: &str, accept: bool) -> bool {
        let mut pending = self.pending.lock();
        pending.retain(|d| !d.reply.withdrawn());
        match pending.iter().position(|d| d.peer == *peer && d.id == id) {
            Some(i) => {
                pending.remove(i).reply.answer(accept);
                true
            }
            None => false,
        }
    }
}

/// Serves no sources; takes Spacedrops when given [`Drops`] (else declines them).
#[derive(Default)]
pub struct NoSources {
    pub drops: Option<Arc<Drops>>,
}

const REFUSED: &str = "this device does not serve sources yet";

#[async_trait::async_trait]
impl Handler for NoSources {
    async fn sources(&self, _: &RequestCtx) -> Vec<SourceInfo> {
        Vec::new()
    }
    async fn list(&self, _: &RequestCtx, _: &str, _: &str) -> Result<Vec<EntryInfo>> {
        bail!(REFUSED)
    }
    async fn stat(&self, _: &RequestCtx, _: &str, _: &str) -> Result<EntryInfo> {
        bail!(REFUSED)
    }
    async fn read(
        &self,
        _: &RequestCtx,
        _: &str,
        _: &str,
        _: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        bail!(REFUSED)
    }
    async fn write(
        &self,
        _: &RequestCtx,
        _: &str,
        _: &str,
        _: Box<dyn AsyncRead + Send + Unpin>,
        _: WriteAt,
    ) -> Result<()> {
        bail!(REFUSED)
    }
    async fn stat_partial(&self, _: &RequestCtx, _: &str, _: &str) -> Result<u64> {
        bail!(REFUSED)
    }
    async fn mkdir(&self, _: &RequestCtx, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn rename(&self, _: &RequestCtx, _: &str, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn remove(&self, _: &RequestCtx, _: &str, _: &str) -> Result<()> {
        bail!(REFUSED)
    }
    async fn storage(&self, _: &RequestCtx) -> Option<Storage> {
        None
    }
    fn drop_offer(&self, offer: IncomingDrop) -> Option<PathBuf> {
        self.drops.as_ref()?.offer(offer)
    }
}
