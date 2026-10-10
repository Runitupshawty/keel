//! An in-memory provider for tests (the `test-util` feature): files by path, folders
//! implied by them or made with `mkdir`, plus switches that take it offline, fail the
//! reads of chosen paths or slow every read down. Writers put their bytes in place as
//! they are written (a half-written file is visible, as on a disk), and `flush` is a no-op.

use crate::{Caps, Entry, Kind, Provider, RemoveKind, VPath};
use anyhow::{bail, Result};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::{self, Cursor, Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

type Files = Arc<Mutex<BTreeMap<String, (Vec<u8>, SystemTime)>>>;

#[derive(Default)]
pub struct MemoryProvider {
    /// Contents and mtime by `VPath::path` (`/dir/a.txt`).
    files: Files,
    /// Folders made with `mkdir` (others exist through the files under them).
    dirs: Mutex<BTreeSet<String>>,
    /// Every call fails, as for a host that is down.
    pub offline: AtomicBool,
    /// Reads of these paths fail.
    pub fail_reads: Mutex<HashSet<String>>,
    /// Milliseconds each `read` sleeps first.
    pub read_delay_ms: AtomicU64,
    /// Bytes handed out by reads.
    pub served: AtomicU64,
    /// Called with the path at the start of every `read`.
    #[allow(clippy::type_complexity)]
    pub on_read: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
}

/// A fixed mtime for every file.
fn mtime() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

impl MemoryProvider {
    pub fn new() -> MemoryProvider {
        MemoryProvider::default()
    }

    /// Adds or replaces the file at `path` (a `VPath::path`), with a fixed mtime.
    pub fn put(&self, path: &str, data: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .insert(path.into(), (data.into(), mtime()));
    }

    /// Every file's path (`VPath::path`) and contents, in path order.
    pub fn files(&self) -> Vec<(String, Vec<u8>)> {
        let files = self.files.lock();
        files
            .iter()
            .map(|(p, (d, _))| (p.clone(), d.clone()))
            .collect()
    }

    /// Folders made with `mkdir`.
    pub fn dirs(&self) -> Vec<String> {
        self.dirs.lock().iter().cloned().collect()
    }

    fn up(&self) -> Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            bail!("memory provider is offline");
        }
        Ok(())
    }

    fn is_dir(&self, path: &str) -> bool {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        prefix == "/"
            || self
                .dirs
                .lock()
                .iter()
                .any(|d| *d == path || d.starts_with(&prefix))
            || self.files.lock().keys().any(|k| k.starts_with(&prefix))
    }

    fn exists(&self, path: &str) -> bool {
        self.files.lock().contains_key(path) || self.is_dir(path)
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

    fn writer(&self, p: &VPath) -> Box<dyn Write + Send> {
        self.files
            .lock()
            .insert(p.path.clone(), (Vec::new(), mtime()));
        Box::new(Writer {
            files: self.files.clone(),
            path: p.path.clone(),
        })
    }

    fn move_entry(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.up()?;
        if !replace && self.exists(&to.path) {
            bail!("{} already exists", to.display());
        }
        let mut files = self.files.lock();
        if let Some(file) = files.remove(&from.path) {
            if files.contains_key(&to.path) && !replace {
                bail!("{} already exists", to.display());
            }
            files.insert(to.path.clone(), file);
            return Ok(());
        }
        let prefix = format!("{}/", from.path.trim_end_matches('/'));
        let moved: Vec<String> = files
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        let mut dirs = self.dirs.lock();
        let made: Vec<String> = dirs
            .iter()
            .filter(|d| **d == from.path || d.starts_with(&prefix))
            .cloned()
            .collect();
        if moved.is_empty() && made.is_empty() {
            bail!("no such entry {}", from.display());
        }
        let target = |k: &str| format!("{}{}", to.path, &k[from.path.len()..]);
        for k in moved {
            let file = files.remove(&k).expect("listed");
            files.insert(target(&k), file);
        }
        for d in made {
            dirs.remove(&d);
            dirs.insert(target(&d));
        }
        Ok(())
    }
}

/// Puts each write in place at once.
struct Writer {
    files: Files,
    path: String,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.files.lock().get_mut(&self.path) {
            Some((data, _)) => {
                data.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            None => Err(io::Error::other("file removed while written")),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
        for made in self.dirs.lock().iter() {
            if let Some(rest) = made.strip_prefix(&prefix).filter(|r| !r.is_empty()) {
                let sub = rest.split('/').next().unwrap_or(rest);
                out.entry(sub.to_owned())
                    .or_insert_with(|| Self::entry(&dir.join(sub), Kind::Dir, 0, None));
            }
        }
        Ok(out.into_values().collect())
    }
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.up()?;
        if let Some((data, mtime)) = self.files.lock().get(&p.path) {
            return Ok(Self::entry(p, Kind::File, data.len() as u64, Some(*mtime)));
        }
        if self.is_dir(&p.path) {
            return Ok(Self::entry(p, Kind::Dir, 0, None));
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such entry {}", p.display()),
        )
        .into())
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        self.up()?;
        if let Some(on_read) = &*self.on_read.lock() {
            on_read(&p.path);
        }
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
        self.up()?;
        if self.is_dir(&p.path) {
            bail!("{} is a folder", p.display());
        }
        Ok(self.writer(p))
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.up()?;
        if self.exists(&p.path) {
            bail!("{} already exists", p.display());
        }
        Ok(self.writer(p))
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.up()?;
        if self.exists(&p.path) {
            bail!("{} already exists", p.display());
        }
        self.dirs
            .lock()
            .insert(p.path.trim_end_matches('/').to_owned());
        Ok(())
    }
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.move_entry(from, to, false)
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.move_entry(from, to, false)
    }
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        if self.is_dir(&to.path) {
            bail!("{} is a folder", to.display());
        }
        self.move_entry(from, to, true)
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        self.up()?;
        if !self.list(p)?.is_empty() {
            bail!("{} is not empty", p.display());
        }
        self.dirs.lock().remove(p.path.trim_end_matches('/'));
        Ok(())
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.up()?;
        let prefix = prefix(p);
        self.files
            .lock()
            .retain(|k, _| *k != p.path && !k.starts_with(&prefix));
        self.dirs
            .lock()
            .retain(|d| *d != p.path && !d.starts_with(&prefix));
        Ok(())
    }
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        bail!("no local copy of {}", p.display())
    }
}
