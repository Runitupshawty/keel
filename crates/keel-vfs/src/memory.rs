//! An in-memory provider for tests (the `test-util` feature): files by path, folders
//! implied by them or made with `mkdir`, plus switches that take it offline, make it
//! read-only, fail the reads of chosen paths, slow every read down or set the free space
//! `space` reports. A writer's file is placed whole on `flush()` (`write_at`, and every
//! writer while `write_through` is set, put each write in place). Every change (`put`,
//! `remove`, a placed writer, a rename) feeds `changes` (its cursor is a position in the
//! change log), which can be switched off or made to refuse its cursor once;
//! `folder_times` gives folders a modified time that moves when an entry is added or
//! removed, as on an SFTP server.

use crate::{
    Caps, ChangeCursor, ChangeFeed, ChangeKind, ChangedPath, Entry, FeedError, Kind, Provider,
    RemoveKind, VPath,
};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::{Cursor, Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

type FileMap = BTreeMap<String, (Vec<u8>, SystemTime)>;
type Files = Arc<Mutex<FileMap>>;

#[derive(Default)]
pub struct MemoryProvider {
    /// Contents and mtime by `VPath::path` (`/dir/a.txt`).
    files: Files,
    /// Folders made with `mkdir` (others are implied by the files in them).
    dirs: Mutex<BTreeSet<String>>,
    /// Every call fails, as for a host that is down.
    pub offline: AtomicBool,
    /// Writes, renames and removes are refused (`caps` says so).
    pub read_only: AtomicBool,
    /// Reads of these paths fail.
    pub fail_reads: Mutex<HashSet<String>>,
    /// Milliseconds each `read` sleeps first.
    pub read_delay_ms: AtomicU64,
    /// Bytes handed out by reads.
    pub served: AtomicU64,
    /// Called with the path at the start of every `read`.
    #[allow(clippy::type_complexity)]
    pub on_read: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
    /// Writers put each write in place at once, so a file being written is there (and
    /// listed) the whole time, as on a server; otherwise it is placed on `flush()`.
    pub write_through: AtomicBool,
    /// The change log and folder times, shared with writers.
    book: Arc<Book>,
    /// `changes` answers `FeedError::Unsupported`.
    pub no_feed: AtomicBool,
    /// The next `changes` with a cursor answers `FeedError::CursorRejected`.
    pub reject_cursor: AtomicBool,
    /// Calls of `changes`.
    pub feed_calls: AtomicU64,
    /// Folders report a modified time that moves when an entry is added to or removed from
    /// them, and `folder_times_track_entries` says so.
    pub folder_times: AtomicBool,
    /// What `space` answers (None: unknown).
    pub space: Mutex<Option<crate::Space>>,
}

/// What changed, for `changes` and `folder_times`.
#[derive(Default)]
struct Book {
    /// Every change made (and `push_change` added), oldest first.
    log: Mutex<Vec<ChangedPath>>,
    /// Folder path (`/dir`, `/` for the root) -> its time (see `folder_times`).
    dir_times: Mutex<BTreeMap<String, SystemTime>>,
    clock: AtomicU64,
}

impl Book {
    fn push(&self, path: &str, kind: ChangeKind) {
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
    fn touch_dirs(&self, path: &str, files: &FileMap) {
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

    /// Puts `file` at `path` in `files`, logged as created or modified.
    fn place(&self, files: &Files, path: &str, file: (Vec<u8>, SystemTime)) {
        let mut files = files.lock();
        let kind = if files.contains_key(path) {
            ChangeKind::Modified
        } else {
            self.touch_dirs(path, &files);
            ChangeKind::Created
        };
        files.insert(path.to_owned(), file);
        drop(files);
        self.push(path, kind);
    }
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
        self.book.place(&self.files, path, (data.into(), mtime));
    }

    /// Adds a change to the feed (an `Unknown` folder, say) without touching any file.
    pub fn push_change(&self, path: &str, kind: ChangeKind) {
        self.book.push(path, kind);
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
        Some(
            self.book
                .dir_times
                .lock()
                .get(dir)
                .copied()
                .unwrap_or(epoch),
        )
    }

    /// The file at `path`, if there is one.
    pub fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().get(path).map(|(data, _)| data.clone())
    }

    /// Every file's path, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.files.lock().keys().cloned().collect()
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

    fn writable(&self, p: &VPath) -> Result<()> {
        self.up()?;
        if self.read_only.load(Ordering::SeqCst) {
            bail!("memory provider is read-only: {}", p.display());
        }
        Ok(())
    }

    /// A folder at `path`: the root, one made with `mkdir`, or one with files under it.
    fn is_dir(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        let below = format!("{path}/");
        below == "/"
            || self.files.lock().keys().any(|k| k.starts_with(&below))
            || self
                .dirs
                .lock()
                .iter()
                .any(|d| d == path || d.starts_with(&below))
    }

    /// A file or folder at `path`.
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

    fn writer(&self, p: &VPath, mode: Mode) -> Result<Box<dyn Write + Send>> {
        self.writable(p)?;
        if mode == Mode::New && self.exists(&p.path) {
            bail!("{} exists", p.display());
        }
        if self.is_dir(&p.path) {
            bail!("{} is a folder", p.display());
        }
        let mode = match mode {
            Mode::New | Mode::Replace if self.write_through.load(Ordering::SeqCst) => {
                (self.book).place(&self.files, &p.path, (Vec::new(), SystemTime::now()));
                Mode::Direct
            }
            mode => mode,
        };
        Ok(Box::new(MemoryWrite {
            files: self.files.clone(),
            book: self.book.clone(),
            path: p.path.clone(),
            buf: Vec::new(),
            mode,
        }))
    }

    /// Moves `from` (a file, or a folder with everything in it) to `to`.
    fn move_to(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.writable(from)?;
        if !self.exists(&from.path) {
            bail!("no such entry {}", from.display());
        }
        let file = self.files.lock().get(&from.path).cloned();
        if let Some(file) = file {
            let taken = self.files.lock().contains_key(&to.path);
            if taken && !replace || !taken && self.exists(&to.path) {
                bail!("{} exists", to.display());
            }
            let mut files = self.files.lock();
            files.remove(&from.path);
            self.book.touch_dirs(&from.path, &files);
            drop(files);
            self.book.push(&from.path, ChangeKind::Removed);
            self.book.place(&self.files, &to.path, file);
            return Ok(());
        }
        if self.exists(&to.path) {
            bail!("{} exists", to.display());
        }
        let (old, new) = (prefix(from), prefix(to));
        let mut files = self.files.lock();
        self.book.touch_dirs(&to.path, &files);
        let moved: Vec<String> = files
            .keys()
            .filter(|k| k.starts_with(&old))
            .cloned()
            .collect();
        for k in moved {
            if let Some(file) = files.remove(&k) {
                files.insert(format!("{new}{}", &k[old.len()..]), file);
            }
        }
        let mut dirs = self.dirs.lock();
        let moved: Vec<String> = dirs
            .iter()
            .filter(|d| **d == from.path || d.starts_with(&old))
            .cloned()
            .collect();
        for d in moved {
            dirs.remove(&d);
            dirs.insert(format!("{}{}", to.path, &d[from.path.len()..]));
        }
        drop(dirs);
        self.book.touch_dirs(&from.path, &files);
        drop(files);
        self.book.push(&from.path, ChangeKind::Removed);
        self.book.push(&to.path, ChangeKind::Created);
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Placed on `flush()`, replacing what is there.
    Replace,
    /// Placed on `flush()` only while nothing is there.
    New,
    /// Each write lands at once (`write_at`).
    Direct,
}

struct MemoryWrite {
    files: Files,
    book: Arc<Book>,
    path: String,
    buf: Vec<u8>,
    mode: Mode,
}

impl Write for MemoryWrite {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.mode == Mode::Direct {
            let mut files = self.files.lock();
            let file = files
                .get_mut(&self.path)
                .ok_or_else(|| std::io::Error::other("file removed while written"))?;
            file.0.extend_from_slice(bytes);
            file.1 = SystemTime::now();
        } else {
            self.buf.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.mode == Mode::Direct {
            return Ok(());
        }
        if self.mode == Mode::New && self.files.lock().contains_key(&self.path) {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        let data = std::mem::take(&mut self.buf);
        (self.book).place(&self.files, &self.path, (data, SystemTime::now()));
        // Placed: later writes go straight in.
        self.mode = Mode::Direct;
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
    fn space(&self, _: &VPath) -> Option<crate::Space> {
        *self.space.lock()
    }
    fn caps(&self) -> Caps {
        let write = !self.read_only.load(Ordering::SeqCst);
        Caps {
            write,
            rename: write,
            delete: write,
            watch: false,
        }
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
        for d in self.dirs.lock().iter() {
            if let Some(rest) = d.strip_prefix(&prefix).filter(|r| !r.is_empty()) {
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
    /// Placed on `flush()`.
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.writer(p, Mode::Replace)
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.writer(p, Mode::New)
    }
    fn write_at(&self, p: &VPath, offset: u64) -> Result<Option<Box<dyn Write + Send>>> {
        self.writable(p)?;
        if !self.files.lock().contains_key(&p.path) {
            anyhow::ensure!(offset == 0, "only 0 of {offset} bytes are there");
            (self.book).place(&self.files, &p.path, (Vec::new(), SystemTime::now()));
        }
        {
            let mut files = self.files.lock();
            let file = (files.get_mut(&p.path)).context("file removed while written")?;
            if (file.0.len() as u64) < offset {
                bail!("only {} of {offset} bytes are there", file.0.len());
            }
            file.0.truncate(offset as usize);
        }
        self.writer(p, Mode::Direct).map(Some)
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.writable(p)?;
        if self.exists(&p.path) {
            bail!("{} exists", p.display());
        }
        self.dirs
            .lock()
            .insert(p.path.trim_end_matches('/').to_owned());
        Ok(())
    }
    /// Never replaces an existing target.
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.move_to(from, to, false)
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.move_to(from, to, false)
    }
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.move_to(from, to, true)
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        self.writable(p)?;
        let below = prefix(p);
        if self.files.lock().keys().any(|k| k.starts_with(&below))
            || self.dirs.lock().iter().any(|d| d.starts_with(&below))
        {
            bail!("{} is not empty", p.display());
        }
        self.dirs.lock().remove(&p.path);
        Ok(())
    }
    /// Gone for good, a folder with everything in it.
    fn remove(&self, p: &VPath) -> Result<()> {
        self.writable(p)?;
        let below = prefix(p);
        let mut files = self.files.lock();
        let before = files.len();
        files.retain(|k, _| *k != p.path && !k.starts_with(&below));
        let mut dirs = self.dirs.lock();
        let dirs_before = dirs.len();
        dirs.retain(|d| *d != p.path && !d.starts_with(&below));
        let removed = files.len() != before || dirs.len() != dirs_before;
        drop(dirs);
        if removed {
            self.book.touch_dirs(&p.path, &files);
            drop(files);
            self.book.push(&p.path, ChangeKind::Removed);
        }
        Ok(())
    }
    fn remove_kind(&self) -> RemoveKind {
        RemoveKind::Permanent
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        bail!("no local copy of {}", p.display())
    }
    fn changes(&self, cursor: Option<ChangeCursor>, cancel: &AtomicBool) -> Result<ChangeFeed> {
        self.up()?;
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        self.feed_calls.fetch_add(1, Ordering::SeqCst);
        if self.no_feed.load(Ordering::SeqCst) {
            return Err(FeedError::Unsupported.into());
        }
        let log = self.book.log.lock();
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
