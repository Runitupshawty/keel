use crate::{Entry, Progress, VPath};
use anyhow::Result;
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::atomic::AtomicBool,
};

/// What `Provider::remove` does (for delete confirmations).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveKind {
    /// Local: the OS trash. Google Drive: the Drive trash.
    Trash,
    /// Dropbox: deleted, restorable from dropbox.com for about 30 days.
    RecoverableDelete,
    /// SFTP, S3: gone (unless the bucket keeps versions).
    Permanent,
}

/// An account's storage use (`Provider::quota`), in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    pub used: u64,
    /// None: no limit.
    pub total: Option<u64>,
}

/// `Provider::share_link`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareLink {
    /// Put `url` on the clipboard; `note` tells the user who can open it.
    Ready { url: String, note: String },
    /// There is no link yet and making one widens access: ask `question`, and on a yes
    /// call `share_link` again with `create`.
    Confirm { question: String },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub write: bool,
    pub rename: bool,
    pub delete: bool,
    pub watch: bool,
}

pub trait Provider: Send + Sync {
    fn scheme(&self) -> &'static str;
    fn caps(&self) -> Caps;
    /// May block on dead network volumes; call off the UI thread.
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>>;
    fn stat(&self, p: &VPath) -> Result<Entry>;
    /// `list` for indexing: read fresh (no cache), and an error rather than a listing cut
    /// off at a display cap (an index would take the missing entries as deleted). No
    /// default: a wrapper must forward it (or say why its `list` already qualifies).
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>>;
    /// `list_complete` with each entry's content id (the BLAKE3 of its bytes) where the
    /// provider is told it (a paired device's index); None elsewhere.
    fn list_complete_ids(&self, dir: &VPath) -> Result<Vec<(Entry, Option<[u8; 32]>)>> {
        Ok(self
            .list_complete(dir)?
            .into_iter()
            .map(|e| (e, None))
            .collect())
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>>;
    /// At most `len` bytes of `p` from byte `offset`, when the provider can start a read
    /// there without reading what comes before (SFTP, cloud services, devices); None when
    /// it cannot.
    fn read_range(&self, p: &VPath, offset: u64, len: u64) -> Result<Option<Box<dyn Read + Send>>> {
        let _ = (p, offset, len);
        Ok(None)
    }
    /// Call `flush()` and check its result when done: providers that stage writes (SFTP)
    /// commit there, and a writer dropped without a successful `flush()` is discarded
    /// (logged, staging file removed) rather than placed.
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>>;
    /// Creates `p` only if it does not exist yet (no check-then-create race); the default
    /// refuses so a provider never silently truncates. Commits on `flush()` like `write`.
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        anyhow::bail!("create_new is not supported for {}", p.display())
    }
    /// `create_new` for transfers: a provider whose upload happens on `flush()` ends its
    /// retries there when `cancel` is set.
    fn create_new_cancellable<'a>(
        &self,
        p: &VPath,
        cancel: &'a AtomicBool,
    ) -> Result<Box<dyn Write + Send + 'a>> {
        let _ = cancel;
        self.create_new(p)
    }
    /// `Some(service)`: writers hold the whole file and send it on `flush()`, so a transfer
    /// reports "Uploading to <service>…" instead of counting bytes that are only buffered.
    fn uploads_on_flush(&self) -> Option<&'static str> {
        None
    }
    fn mkdir(&self, p: &VPath) -> Result<()>;
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()>;
    /// Atomically place a completed upload without replacing an existing entry.
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> Result<()> {
        anyhow::bail!(
            "no-replace rename unsupported: {} -> {}",
            from.display(),
            to.display()
        )
    }
    fn canonicalize(&self, p: &VPath) -> Result<VPath> {
        Ok(p.clone())
    }
    /// Atomically replace a destination with a completed staged file. Contract: when it
    /// returns, `to` is either the old entry (and an error is returned) or `from`'s
    /// content, never missing or half-written in between, and nothing else is touched (a
    /// delete-then-rename is not an implementation). Transfers that overwrite and mount
    /// saves rely on it. No silent fallback: the default refuses, so a provider has to
    /// implement it (a no-replace rename qualifies when it fails on an existing `to`).
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        anyhow::bail!(
            "replacing rename unsupported: {} -> {}",
            from.display(),
            to.display()
        )
    }
    /// Must fail if the directory is no longer empty.
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        anyhow::bail!("empty directory removal unsupported: {}", p.display())
    }
    /// Local: OS trash only, never a permanent delete.
    fn remove(&self, p: &VPath) -> Result<()>;
    /// What `remove` does here (delete confirmations and plan warnings). No default: a
    /// wrapper must forward it, or a trash-backed provider reads as permanent.
    fn remove_kind(&self) -> RemoveKind;
    fn local_copy(&self, p: &VPath) -> Result<PathBuf>;
    /// The account's storage use, where the service reports it (cloud accounts). Blocks
    /// on the network: workers only. None: unknown (no such thing here, or the request
    /// failed).
    fn quota(&self) -> Option<Quota> {
        None
    }
    /// A link to `p` for other people (cloud accounts). `create`: the user agreed to make
    /// one after a `ShareLink::Confirm`. Blocks on the network: workers only.
    fn share_link(&self, p: &VPath, create: bool) -> Result<ShareLink> {
        let _ = create;
        anyhow::bail!("{} has no share links", p.display())
    }
    /// The provider is being replaced or removed: `quota` and `share_link` calls in flight
    /// stop at their next request or retry wait (they fail).
    fn cancel_requests(&self) {}
    /// `local_copy` for long downloads: reports progress and stops when `cancel` is set.
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let _ = (progress, cancel);
        self.local_copy(p)
    }
}
