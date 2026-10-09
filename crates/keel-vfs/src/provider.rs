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
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>>;
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
    /// Atomically replace a destination with a completed staged file.
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.rename(from, to)
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
