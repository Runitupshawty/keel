//! An in-memory provider for tests (the `test-util` feature): files by path, folders
//! implied by them, plus switches that take it offline, fail the reads of chosen paths or
//! slow every read down. `put` and `remove` feed `changes` (its cursor is a position in
//! the change log), which can be switched off or made to refuse its cursor once;
//! `folder_times` gives folders a modified time that moves when an entry is added or
//! removed, as on an SFTP server.

use crate::{
    Caps, ChangeCursor, ChangeFeed, ChangeKind, ChangedPath, Entry, FeedError, Kind, Provider,
    RemoveKind, VPath,
};
use anyhow::{bail, Result};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Read, Write},
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

type Files = BTreeMap<String, (Vec<u8>, SystemTime)>;

#[derive(Default)]
pub struct MemoryProvider {
    /// Contents and mtime by `VPath::path` (`/dir/a.txt`).
    files: Mutex<Files>,
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
    /// Every change `put` and `remove` made (and `push_change` added), oldest first.
    log: Mutex<Vec<ChangedPath>>,
    /// `changes` answers `FeedError::Unsupported`.
    pub no_feed: AtomicBool,
    /// The next `changes` with a cursor answers `FeedError::CursorRejected`.
    pub reject_cursor: AtomicBool,
    /// Calls of `changes`.
    pub feed_calls: AtomicU64,
    /// Folders report a modified time that moves when an entry is added to or removed from
    /// them, and `folder_times_track_entries` says so.
    pub folder_times: AtomicBool,
    /// Folder path (`/dir`, `/` for the root) -> its time (see `folder_times`).
    dir_times: Mutex<BTreeMap<String, SystemTime>>,
    clock: AtomicU64,
}

impl MemoryProvider {
    pub fn new() -> MemoryProvider {
        MemoryProvider::default()
    }

    /// Adds or replaces the file at `path` (a `VPath::path`), with a fixed mtime.
    pub fn put(&self, path: &str, data: impl Into<Vec<u8>>) {
        self.put_at(path, data, 1_700_000_000);
    }

    /// `put` with the mtime `secs` after the epoch.
    pub fn put_at(&self, path: &str, data: impl Into<Vec<u8>>, secs: u64) {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let mut files = self.files.lock();
        let kind = if files.contains_key(path) {
            ChangeKind::Modified
        } else {
            self.touch_dirs(path, &files);
            ChangeKind::Created
        };
        files.insert(path.into(), (data.into(), mtime));
        drop(files);
        self.push_change(path, kind);
    }

    /// Adds a change to the feed (an `Unknown` folder, say) without touching any file.
    pub fn push_change(&self, path: &str, kind: ChangeKind) {
        self.log.lock().push(ChangedPath {
            path: VPath {
                scheme: "memory".into(),
                authority: String::new(),
                path: path.into(),
            },
            kind,
        });
    }

    /// An entry is added at `path` (`files` as before) or was removed from it (`files` as
    /// after): its folder's time moves, and so does the time of the folder above each
    /// folder that did not exist before (or no longer does).
    fn touch_dirs(&self, path: &str, files: &Files) {
        let now = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_800_000_000 + self.clock.fetch_add(1, Ordering::SeqCst));
        let mut times = self.dir_times.lock();
        let mut at = path;
        while let Some((dir, _)) = at.rsplit_once('/') {
            let dir = if dir.is_empty() { "/" } else { dir };
            times.insert(dir.to_owned(), now);
            let inside = format!("{}/", dir.trim_end_matches('/'));
            if dir == "/" || files.keys().any(|k| k.starts_with(&inside)) {
                break;
            }
            at = dir;
        }
    }

    fn dir_time(&self, dir: &str) -> Option<SystemTime> {
        if !self.folder_times.load(Ordering::SeqCst) {
            return None;
        }
        let dir = match dir.trim_end_matches('/') {
            "" => "/",
            d => d,
        };
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        Some(self.dir_times.lock().get(dir).copied().unwrap_or(epoch))
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
                Some((sub, _)) => {
                    let sub = dir.join(sub);
                    Self::entry(&sub, Kind::Dir, 0, self.dir_time(&sub.path))
                }
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
            drop(files);
            return Ok(Self::entry(p, Kind::Dir, 0, self.dir_time(&p.path)));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
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
        bail!("memory provider is read-only: {}", p.display())
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        bail!("memory provider is read-only: {}", p.display())
    }
    fn rename(&self, from: &VPath, _: &VPath) -> Result<()> {
        bail!("memory provider is read-only: {}", from.display())
    }
    /// A file, or a folder with everything in it.
    fn remove(&self, p: &VPath) -> Result<()> {
        self.up()?;
        let inside = prefix(p);
        let mut files = self.files.lock();
        let before = files.len();
        files.retain(|k, _| *k != p.path && !k.starts_with(&inside));
        if files.len() != before {
            self.touch_dirs(&p.path, &files);
            drop(files);
            self.push_change(&p.path, ChangeKind::Removed);
        }
        Ok(())
    }
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        bail!("no local copy of {}", p.display())
    }
    fn changes(&self, cursor: Option<ChangeCursor>) -> Result<ChangeFeed> {
        self.up()?;
        self.feed_calls.fetch_add(1, Ordering::SeqCst);
        if self.no_feed.load(Ordering::SeqCst) {
            return Err(FeedError::Unsupported.into());
        }
        let log = self.log.lock();
        let from = match cursor {
            None => log.len(),
            Some(_) if self.reject_cursor.swap(false, Ordering::SeqCst) => {
                return Err(FeedError::CursorRejected.into())
            }
            Some(c) => c.0.parse::<usize>()?.min(log.len()),
        };
        Ok(ChangeFeed {
            changes: log[from..].to_vec(),
            cursor: ChangeCursor(log.len().to_string()),
            more: false,
        })
    }
    fn folder_times_track_entries(&self) -> bool {
        self.folder_times.load(Ordering::SeqCst)
    }
}
