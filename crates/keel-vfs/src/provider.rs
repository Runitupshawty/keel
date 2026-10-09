use crate::{Entry, Progress, VPath};
use anyhow::Result;
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::atomic::AtomicBool,
};

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
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>>;
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>>;
    /// Creates `p` only if it does not exist yet (no check-then-create race); the default
    /// refuses so a provider never silently truncates.
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        anyhow::bail!("create_new is not supported for {}", p.display())
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
