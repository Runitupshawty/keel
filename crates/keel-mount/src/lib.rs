//! Keel sources as drives (spec 2.10, "Mounts"): a library source, or a subtree of it, as a
//! drive letter or mount point served by keel-daemon.
//!
//! [`MountFs`] is the backend-agnostic filesystem: listings come from the VFS when the
//! source is online and from the library index when it is offline; reads are on-demand
//! range reads through the VFS provider; writes go to a `.keel-partial-<pid>-<n>` staging
//! file next to the target (a spool file for remote sources) and are published atomically
//! when the writer closes the file. Until then only the writing handles see the new
//! content: listings show the published file and other opens of it are refused as busy
//! (a sharing violation on Windows), so another program never sees a half-written file;
//! an aborted write leaves none, and a write that cannot be published is kept as
//! `<name> (unsaved <date>).<ext>`. Unmounting drops writes still open.
//! Backends: WinFsp (feature `winfsp`, Windows) and FUSE (feature `fuse`, Linux / macFUSE),
//! both off by default. [`Mounts`] keeps the active mounts; dropping one unmounts it.

mod fs;
pub mod path;
mod staged;

#[cfg(all(unix, feature = "fuse"))]
mod fuse;
#[cfg(all(windows, feature = "winfsp"))]
mod winfsp;

pub use fs::{Attr, DirEntry, Handle, MountFs};
pub use path::{MountPath, PathMap};

use anyhow::{bail, Context};
use keel_core::{Library, SourceId};
use keel_vfs::Router;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The mount backend compiled into this build, if any.
pub fn backend() -> Option<&'static str> {
    if cfg!(all(windows, feature = "winfsp")) {
        Some("winfsp")
    } else if cfg!(all(unix, feature = "fuse")) {
        Some("fuse")
    } else {
        None
    }
}

/// What to say when [`backend`] is None.
pub const NO_BACKEND: &str = "this keel-daemon has no mount backend: build it with \
    `--features winfsp` (Windows, needs WinFsp) or `--features fuse` (Linux FUSE, macFUSE)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountInfo {
    /// `K:` or the mount folder.
    pub target: String,
    pub source: String,
    pub label: String,
    /// Relative to the source root; "" for all of it.
    pub subtree: String,
    /// What the mount shows (the source root joined with the subtree).
    pub root: String,
    pub backend: &'static str,
}

struct Active {
    info: MountInfo,
    fs: Arc<MountFs>,
    /// The backend's session; dropping it unmounts.
    session: Box<dyn Send>,
}

/// The mounts of one host (keel-daemon). Unmounted on [`Mounts::unmount_all`] or drop.
pub struct Mounts {
    active: Mutex<Vec<Active>>,
    spool: PathBuf,
}

/// `K`, `k:`, `K:\` -> `K:`; anything else must be an absolute path (Windows: a folder
/// that does not exist yet, WinFsp creates it; elsewhere an existing empty folder).
pub fn normalize_target(target: &str) -> anyhow::Result<String> {
    let t = target.trim();
    let letter = t.trim_end_matches(['\\', '/']).trim_end_matches(':');
    if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) {
        if !cfg!(windows) {
            bail!("drive letters are Windows only; give an empty folder to mount on");
        }
        return Ok(format!("{}:", letter.to_ascii_uppercase()));
    }
    let path = Path::new(t);
    if !path.is_absolute() {
        bail!("mount target {t} is neither a drive letter nor an absolute path");
    }
    let trimmed = t.trim_end_matches(['\\', '/']);
    Ok(if trimmed.is_empty() { t } else { trimmed }.to_owned())
}

/// Refuses a target that is taken (a drive letter in use, an existing folder on Windows, a
/// missing or non-empty folder elsewhere).
fn check_free(target: &str) -> anyhow::Result<()> {
    if target.len() == 2 && target.ends_with(':') {
        if Path::new(&format!("{target}\\")).exists() {
            bail!("drive {target} is in use");
        }
        return Ok(());
    }
    let path = Path::new(target);
    if cfg!(windows) {
        if path.exists() {
            bail!("{target} exists: WinFsp mounts on a folder it creates, give a new path");
        }
        let parent = path.parent().context("no parent folder")?;
        if !parent.is_dir() {
            bail!("{} is not a folder", parent.display());
        }
    } else {
        let mut entries = std::fs::read_dir(path)
            .with_context(|| format!("{target} must be an existing empty folder"))?;
        if entries.next().is_some() {
            bail!("{target} is not empty");
        }
    }
    Ok(())
}

/// Refuses a mount folder inside the folder it would show (the mount would contain itself).
fn check_outside(target: &str, root: &Path) -> anyhow::Result<()> {
    let target = Path::new(target);
    if !target.is_absolute() {
        return Ok(()); // a drive letter
    }
    // The nearest existing ancestor, resolved like the root (symlinks, case, `\\?\`).
    let mut base = target;
    let mut rest = Vec::new();
    let resolved = loop {
        if let Ok(r) = std::fs::canonicalize(base) {
            break rest.iter().rev().fold(r, |p: PathBuf, n| p.join(n));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                base = parent;
            }
            _ => return Ok(()),
        }
    };
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    if resolved.starts_with(&root) {
        bail!(
            "{} is inside the folder it would show ({}): pick a target outside it",
            target.display(),
            root.display()
        );
    }
    Ok(())
}

impl Mounts {
    /// Remote writes spool under `spool` until they are uploaded.
    pub fn new(spool: PathBuf) -> Self {
        Self {
            active: Mutex::default(),
            spool,
        }
    }

    pub fn list(&self) -> Vec<MountInfo> {
        self.active.lock().iter().map(|a| a.info.clone()).collect()
    }

    /// The active mount at `target` (any spelling [`normalize_target`] accepts).
    pub fn get(&self, target: &str) -> Option<MountInfo> {
        let t = normalize_target(target).ok()?;
        self.active
            .lock()
            .iter()
            .find(|a| a.info.target.eq_ignore_ascii_case(&t))
            .map(|a| a.info.clone())
    }

    /// Writes in progress on the mount at `target` (lost when it is unmounted).
    pub fn pending_writes(&self, target: &str) -> usize {
        let Ok(t) = normalize_target(target) else {
            return 0;
        };
        self.active
            .lock()
            .iter()
            .find(|a| a.info.target.eq_ignore_ascii_case(&t))
            .map_or(0, |a| a.fs.pending_writes())
    }

    /// Checks a mount request without mounting: the backend, the target, the source and
    /// the subtree (which must exist on the source or in its index).
    pub fn check(
        &self,
        lib: &Arc<Library>,
        router: &Arc<Router>,
        source: &SourceId,
        subtree: &str,
        target: &str,
    ) -> anyhow::Result<(String, Arc<MountFs>)> {
        let backend = backend().context(NO_BACKEND)?;
        let target = normalize_target(target)?;
        if self.get(&target).is_some() {
            bail!("{target} is already a Keel mount");
        }
        check_free(&target)?;
        let fs = Arc::new(MountFs::new(
            lib.clone(),
            router.clone(),
            source,
            subtree,
            backend == "winfsp",
            self.spool.clone(),
        )?);
        let attr = fs
            .stat(&MountPath::root())
            .and_then(|a| fs.list(&MountPath::root()).map(|_| a))
            .with_context(|| format!("{} cannot be listed", fs.map().root.display()))?;
        anyhow::ensure!(attr.is_dir, "{} is not a folder", fs.map().root.display());
        if let Some(root) = fs.map().root.to_local_path() {
            check_outside(&target, &root)?;
        }
        Ok((target, fs))
    }

    /// Mounts `subtree` of `source` at `target`; it stays mounted until removed.
    pub fn add(
        &self,
        lib: &Arc<Library>,
        router: &Arc<Router>,
        source: &SourceId,
        subtree: &str,
        target: &str,
    ) -> anyhow::Result<MountInfo> {
        let (target, fs) = self.check(lib, router, source, subtree, target)?;
        let session = mount_backend(fs.clone(), &target)?;
        let info = MountInfo {
            target,
            source: source.0.clone(),
            label: fs.label(),
            subtree: fs.map().subtree.clone(),
            root: fs.map().root.display(),
            backend: backend().unwrap_or_default(),
        };
        tracing::info!("mounted {} at {}", info.root, info.target);
        self.active.lock().push(Active {
            info: info.clone(),
            fs,
            session,
        });
        Ok(info)
    }

    /// Unmounts `target`; writes still in progress there are dropped (their files stay as
    /// they were).
    pub fn remove(&self, target: &str) -> anyhow::Result<MountInfo> {
        let t = normalize_target(target)?;
        let active = {
            let mut list = self.active.lock();
            let i = list
                .iter()
                .position(|a| a.info.target.eq_ignore_ascii_case(&t))
                .with_context(|| format!("{t} is not a Keel mount"))?;
            list.remove(i)
        };
        Ok(unmount(active))
    }

    pub fn unmount_all(&self) {
        let all: Vec<_> = self.active.lock().drain(..).collect();
        for a in all {
            unmount(a);
        }
    }
}

fn unmount(a: Active) -> MountInfo {
    // First: a close the backend processes while it shuts down must not publish.
    a.fs.abort_all();
    drop(a.session);
    tracing::info!("unmounted {}", a.info.target);
    a.info
}

impl Drop for Mounts {
    fn drop(&mut self) {
        self.unmount_all();
    }
}

#[allow(unused_variables)]
fn mount_backend(fs: Arc<MountFs>, target: &str) -> anyhow::Result<Box<dyn Send>> {
    #[cfg(all(windows, feature = "winfsp"))]
    return winfsp::mount(fs, target);
    #[cfg(all(unix, feature = "fuse"))]
    return fuse::mount(fs, target);
    #[allow(unreachable_code)]
    Err(anyhow::anyhow!(NO_BACKEND))
}

#[cfg(test)]
mod tests;
