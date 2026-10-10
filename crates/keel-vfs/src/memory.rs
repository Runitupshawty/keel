//! An in-memory provider for tests (the `test-util` feature): files by path, folders
//! implied by them, plus switches that take it offline, fail the reads of chosen paths or
//! slow every read down.

use crate::{Caps, Entry, Kind, Provider, RemoveKind, VPath};
use anyhow::{bail, Result};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Read, Write},
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

#[derive(Default)]
pub struct MemoryProvider {
    /// Contents and mtime by `VPath::path` (`/dir/a.txt`).
    files: Mutex<BTreeMap<String, (Vec<u8>, SystemTime)>>,
    /// Every call fails, as for a host that is down.
    pub offline: AtomicBool,
    /// Reads of these paths fail.
    pub fail_reads: Mutex<HashSet<String>>,
    /// Milliseconds each `read` sleeps first.
    pub read_delay_ms: AtomicU64,
    /// Bytes handed out by reads.
    pub served: AtomicU64,
}

impl MemoryProvider {
    pub fn new() -> MemoryProvider {
        MemoryProvider::default()
    }

    /// Adds or replaces the file at `path` (a `VPath::path`), with a fixed mtime.
    pub fn put(&self, path: &str, data: impl Into<Vec<u8>>) {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        self.files.lock().insert(path.into(), (data.into(), mtime));
    }

    fn up(&self) -> Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            bail!("memory provider is offline");
        }
        Ok(())
    }

    fn entry(p: &VPath, kind: Kind, size: u64, modified: Option<SystemTime>) -> Entry {
        let name = p.name().to_owned();
        Entry {
            path: p.clone(),
            ext: name
                .rsplit_once('.')
                .map(|(_, e)| e.to_lowercase())
                .unwrap_or_default(),
            name,
            kind,
            size,
            modified,
            hidden: false,
            is_link: false,
            encrypted: false,
        }
    }
}

/// `dir` with exactly one trailing slash.
fn prefix(dir: &VPath) -> String {
    format!("{}/", dir.path.trim_end_matches('/'))
}

impl Provider for MemoryProvider {
    fn scheme(&self) -> &'static str {
        "memory"
    }
    fn caps(&self) -> Caps {
        Caps::default()
    }
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.up()?;
        let prefix = prefix(dir);
        let mut out: BTreeMap<String, Entry> = BTreeMap::new();
        for (path, (data, mtime)) in self.files.lock().iter() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            let e = match rest.split_once('/') {
                Some((sub, _)) => Self::entry(&dir.join(sub), Kind::Dir, 0, None),
                None => Self::entry(&dir.join(rest), Kind::File, data.len() as u64, Some(*mtime)),
            };
            out.insert(e.name.clone(), e);
        }
        Ok(out.into_values().collect())
    }
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.up()?;
        let files = self.files.lock();
        if let Some((data, mtime)) = files.get(&p.path) {
            return Ok(Self::entry(p, Kind::File, data.len() as u64, Some(*mtime)));
        }
        let prefix = prefix(p);
        if prefix == "/" || files.keys().any(|k| k.starts_with(&prefix)) {
            return Ok(Self::entry(p, Kind::Dir, 0, None));
        }
        bail!("no such entry {}", p.display())
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        self.up()?;
        std::thread::sleep(Duration::from_millis(
            self.read_delay_ms.load(Ordering::SeqCst),
        ));
        if self.fail_reads.lock().contains(&p.path) {
            bail!("read of {} failed", p.display());
        }
        let Some((data, _)) = self.files.lock().get(&p.path).cloned() else {
            bail!("no such file {}", p.display());
        };
        self.served.fetch_add(data.len() as u64, Ordering::SeqCst);
        Ok(Box::new(Cursor::new(data)))
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        bail!("memory provider is read-only: {}", p.display())
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        bail!("memory provider is read-only: {}", p.display())
    }
    fn rename(&self, from: &VPath, _: &VPath) -> Result<()> {
        bail!("memory provider is read-only: {}", from.display())
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.up()?;
        self.files.lock().remove(&p.path);
        Ok(())
    }
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        bail!("no local copy of {}", p.display())
    }
}
