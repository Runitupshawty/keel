//! [`MountFs`]: the filesystem every backend serves. Paths are [`MountPath`]s; errors are
//! `io::Error`s (backends turn them into errno values or NTSTATUS codes).
//!
//! A write in progress is private to its writing handles: they read and stat the staged
//! copy, while listings and every other program see the published file; opening a file
//! with a staged write (or looking up one being created) is refused as busy (a sharing
//! violation on Windows) until it is published. The write is published when its only
//! writer closes ([`MountFs::flush`], synchronous with `close(2)` / `CloseHandle`) and at
//! the latest on the last release; its entry stays until the publish returns, so a new
//! open waits for it rather than seeing the old content.

use crate::path::{MountPath, PathMap};
use crate::staged::{io_err, Staged};
use keel_core::{Library, SourceId, SourceStatus};
use keel_vfs::{Kind, Provider, Router};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub attr: Attr,
}

/// An open file or folder.
pub type Handle = u64;

/// A sequential reader and where it is (range reads continue it or start a new one).
struct Cursor {
    reader: Box<dyn Read + Send>,
    pos: u64,
}

struct Open {
    path: MountPath,
    writer: bool,
    cursor: Option<Cursor>,
}

/// The write in progress on one file, shared by its writing handles.
// Lock order: a `Pending::state` may take the `writes` table lock, never the reverse
// (table users clone the Arc and drop the table lock before locking the state).
struct Pending {
    path: MountPath,
    state: Mutex<WriteState>,
}

#[derive(Default)]
struct WriteState {
    /// None until the first write (a handle opened for writing that never writes leaves the
    /// file alone); Published after a flush (a later write stages again).
    staged: Option<Staged>,
    writers: usize,
    /// Closed and out of the table: a writer that finds it joins a new entry.
    gone: bool,
}

impl WriteState {
    fn open(&self) -> bool {
        self.staged.as_ref().is_some_and(Staged::is_open)
    }
}

fn not_found() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

fn busy(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::ResourceBusy, what.to_owned())
}

fn being_written() -> io::Error {
    busy("the file is being written through the mount")
}

fn closing() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the mount is closing")
}

fn unix_time(secs: Option<i64>) -> Option<SystemTime> {
    let secs = u64::try_from(secs?).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

pub struct MountFs {
    lib: Arc<Library>,
    router: Arc<Router>,
    source: SourceId,
    map: PathMap,
    spool: PathBuf,
    next: AtomicU64,
    /// Set by [`MountFs::abort_all`]: nothing is published or staged any more.
    closing: AtomicBool,
    // ponytail: one lock for the handle table and one for pending writes; per-file I/O runs
    // under the per-handle / per-file mutexes, so only table lookups serialize.
    handles: Mutex<HashMap<Handle, Arc<Mutex<Open>>>>,
    writes: Mutex<HashMap<String, Arc<Pending>>>,
}

impl MountFs {
    /// Serves `subtree` (relative to the source root, "" for all of it) of `source`.
    /// `windows`: the mount is case-insensitive and shows only Windows names. Remote
    /// writes spool under `spool`.
    pub fn new(
        lib: Arc<Library>,
        router: Arc<Router>,
        source: &SourceId,
        subtree: &str,
        windows: bool,
        spool: PathBuf,
    ) -> anyhow::Result<MountFs> {
        let src = lib
            .source(source)
            .ok_or_else(|| anyhow::anyhow!("no source {source}"))?;
        let subtree = subtree.trim_matches(['/', '\\']).replace('\\', "/");
        let sub = MountPath::parse(&subtree)?;
        let root = sub
            .components()
            .fold(src.def.root.clone(), |v, c| v.join(c));
        let source_nocase = cfg!(windows) && src.def.root.scheme == "file";
        Ok(MountFs {
            map: PathMap::new(root, sub.as_str(), windows, source_nocase),
            lib,
            router,
            source: source.clone(),
            spool,
            next: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            handles: Mutex::default(),
            writes: Mutex::default(),
        })
    }

    pub fn map(&self) -> &PathMap {
        &self.map
    }

    pub fn label(&self) -> String {
        self.lib
            .source(&self.source)
            .map_or_else(|| self.source.0.clone(), |s| s.def.label.clone())
    }

    fn offline(&self) -> bool {
        self.lib
            .source(&self.source)
            .is_none_or(|s| matches!(*s.status.read(), SourceStatus::Offline { .. }))
    }

    fn provider(&self) -> io::Result<Arc<dyn Provider>> {
        self.router.provider_for(&self.map.root).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("{} is not connected", self.map.root.display()),
            )
        })
    }

    fn online_provider(&self) -> io::Result<Arc<dyn Provider>> {
        if self.offline() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("{} is offline", self.label()),
            ));
        }
        self.provider()
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// The indexed children of `dir` (the source is offline or did not answer).
    fn index_list(&self, dir: &MountPath) -> io::Result<Vec<DirEntry>> {
        let hits = self
            .lib
            .list_children(&self.source, &self.map.index_rel(dir))
            .map_err(|_| not_found())?;
        Ok(hits
            .into_iter()
            .map(|h| DirEntry {
                name: h.name,
                attr: Attr {
                    is_dir: h.is_dir,
                    size: h.size,
                    modified: unix_time(h.modified),
                },
            })
            .collect())
    }

    fn source_list(&self, dir: &MountPath) -> io::Result<Vec<DirEntry>> {
        if self.offline() {
            return self.index_list(dir);
        }
        let listed = self
            .provider()
            .and_then(|p| p.list(&self.map.vpath(dir)).map_err(io_err));
        match listed {
            Ok(entries) => Ok(entries
                .into_iter()
                .filter(|e| e.kind != Kind::Symlink)
                .map(|e| DirEntry {
                    name: e.name,
                    attr: Attr {
                        is_dir: e.kind == Kind::Dir,
                        size: e.size,
                        modified: e.modified,
                    },
                })
                .collect()),
            // A folder missing from a reachable source is missing; anything else means the
            // source went away since the last status check, and the index answers.
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    && self
                        .provider()
                        .is_ok_and(|p| p.stat(&self.map.root).is_ok()) =>
            {
                Err(e)
            }
            Err(e) => self.index_list(dir).map_err(|_| e),
        }
    }

    fn pending(&self, p: &MountPath) -> Option<Arc<Pending>> {
        self.writes.lock().get(&self.map.key(p)).cloned()
    }

    /// `p` has staged data that is not published yet (waits for a publish in progress).
    fn write_pending(&self, p: &MountPath) -> bool {
        self.pending(p).is_some_and(|w| {
            let st = w.state.lock();
            !st.gone && st.open()
        })
    }

    /// Lists `dir` as published: from the source when online, from the library index when
    /// offline; staging files, names the mount cannot show and case twins are left out.
    /// Writes in progress do not show until they are published.
    pub fn list(&self, dir: &MountPath) -> io::Result<Vec<DirEntry>> {
        Ok(self.map.filter(self.source_list(dir)?, |e| &e.name))
    }

    /// The published entry at `p` with the name as the source spells it. A file being
    /// created through the mount (not published yet) is busy rather than missing.
    pub fn lookup(&self, p: &MountPath) -> io::Result<(MountPath, Attr)> {
        self.lookup_source(p).map_err(|e| {
            let creating = e.kind() == io::ErrorKind::NotFound
                && self.pending(p).is_some_and(|w| {
                    // Locked: a publish is in progress.
                    w.state.try_lock().is_none_or(|st| !st.gone && st.open())
                });
            if creating {
                being_written()
            } else {
                e
            }
        })
    }

    fn lookup_source(&self, p: &MountPath) -> io::Result<(MountPath, Attr)> {
        let Some(parent) = p.parent() else {
            return Ok((
                MountPath::root(),
                Attr {
                    is_dir: true,
                    size: 0,
                    modified: None,
                },
            ));
        };
        if !self.map.shown(p.name()) {
            return Err(not_found());
        }
        // Exact name first (one stat), then the folder's listing (case folding, offline).
        if !self.map.fold_case && !self.offline() {
            if let Ok(provider) = self.provider() {
                match provider.stat(&self.map.vpath(p)).map_err(io_err) {
                    Ok(e) if e.kind != Kind::Symlink => {
                        return Ok((
                            p.clone(),
                            Attr {
                                is_dir: e.kind == Kind::Dir,
                                size: e.size,
                                modified: e.modified,
                            },
                        ))
                    }
                    Ok(_) => return Err(not_found()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(e),
                    Err(_) => {}
                }
            }
        }
        let (parent, _) = self.lookup_source(&parent)?;
        let entries = self.list(&parent)?;
        let found = entries
            .iter()
            .find(|e| e.name == p.name())
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| self.map.same_name(&e.name, p.name()))
            })
            .ok_or_else(not_found)?;
        Ok((parent.join(&found.name)?, found.attr))
    }

    pub fn stat(&self, p: &MountPath) -> io::Result<Attr> {
        Ok(self.lookup(p)?.1)
    }

    fn handle(&self, h: Handle) -> io::Result<Arc<Mutex<Open>>> {
        self.handles
            .lock()
            .get(&h)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no such open file"))
    }

    fn insert(&self, path: MountPath, writer: bool) -> Handle {
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        if writer {
            let key = self.map.key(&path);
            loop {
                let w = self
                    .writes
                    .lock()
                    .entry(key.clone())
                    .or_insert_with(|| {
                        Arc::new(Pending {
                            path: path.clone(),
                            state: Mutex::default(),
                        })
                    })
                    .clone();
                let mut st = w.state.lock();
                if !st.gone {
                    st.writers += 1;
                    break;
                }
            }
        }
        self.handles.lock().insert(
            h,
            Arc::new(Mutex::new(Open {
                path,
                writer,
                cursor: None,
            })),
        );
        h
    }

    /// Opens an existing file or folder. `write`: writes go to a staged copy published on
    /// close; `truncate`: that copy starts empty. A read-only open of a file with a staged
    /// write is busy.
    pub fn open(&self, p: &MountPath, write: bool, truncate: bool) -> io::Result<(Handle, Attr)> {
        let (path, attr) = self.lookup(p)?;
        if write && attr.is_dir {
            return Err(io::Error::from(io::ErrorKind::IsADirectory));
        }
        if write {
            if self.is_closing() {
                return Err(closing());
            }
            self.online_provider()?;
        } else if !attr.is_dir && self.write_pending(&path) {
            return Err(being_written());
        }
        let h = self.insert(path, write);
        if truncate {
            if let Err(e) = self.set_len(h, 0) {
                let _ = self.release(h);
                return Err(e);
            }
        }
        Ok((h, self.handle_attr(h)?))
    }

    /// Creates a new empty file (not visible outside the mount until it is closed).
    pub fn create(&self, p: &MountPath) -> io::Result<(Handle, Attr)> {
        let Some(parent) = p.parent() else {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        };
        if !self.map.shown(p.name()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the mount does not allow that name",
            ));
        }
        if self.is_closing() {
            return Err(closing());
        }
        let provider = self.online_provider()?;
        let (parent, parent_attr) = self.lookup(&parent)?;
        if !parent_attr.is_dir {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        match self.lookup(p) {
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            Err(_) => {}
        }
        let path = parent.join(p.name())?;
        let staged = Staged::begin(&*provider, &self.map.vpath(&path), false, &self.spool)?;
        let h = self.insert(path.clone(), true);
        let raced = match self.pending(&path) {
            Some(w) => {
                let mut st = w.state.lock();
                let raced = st.staged.is_some();
                if !raced {
                    st.staged = Some(staged);
                }
                raced
            }
            None => true,
        };
        if raced {
            let _ = self.release(h);
            return Err(being_written());
        }
        Ok((h, self.handle_attr(h)?))
    }

    /// What `h` sees: a writer its staged copy, everyone else the published file.
    pub fn handle_attr(&self, h: Handle) -> io::Result<Attr> {
        let (path, writer) = {
            let o = self.handle(h)?;
            let o = o.lock();
            (o.path.clone(), o.writer)
        };
        if writer {
            if let Some(w) = self.pending(&path) {
                let mut st = w.state.lock();
                if let Some(s) = st.staged.as_mut().filter(|s| s.is_open()) {
                    return Ok(Attr {
                        is_dir: false,
                        size: s.len()?,
                        modified: Some(SystemTime::now()),
                    });
                }
            }
        }
        self.stat(&path)
    }

    pub fn handle_path(&self, h: Handle) -> io::Result<MountPath> {
        Ok(self.handle(h)?.lock().path.clone())
    }

    /// The pending write of writer handle `h`, staged (from the current content) if it has
    /// not been yet or was published by a flush. Staging runs outside the file's lock: a
    /// remote target is downloaded first.
    fn staged(&self, h: Handle, keep: bool) -> io::Result<Arc<Pending>> {
        let (path, writer) = {
            let o = self.handle(h)?;
            let o = o.lock();
            (o.path.clone(), o.writer)
        };
        if !writer {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if self.is_closing() {
            return Err(closing());
        }
        let w = self.pending(&path).ok_or_else(not_found)?;
        let needs = |st: &WriteState| st.staged.as_ref().is_none_or(Staged::is_published);
        if !needs(&w.state.lock()) {
            return Ok(w);
        }
        let provider = self.online_provider()?;
        let fresh = Staged::begin(&*provider, &self.map.vpath(&path), keep, &self.spool)?;
        let mut st = w.state.lock();
        if needs(&st) {
            st.staged = Some(fresh);
        }
        drop(st);
        Ok(w)
    }

    pub fn write(&self, h: Handle, offset: u64, data: &[u8]) -> io::Result<usize> {
        let w = self.staged(h, true)?;
        let mut st = w.state.lock();
        st.staged
            .as_mut()
            .ok_or_else(not_found)?
            .write_at(offset, data)
    }

    pub fn set_len(&self, h: Handle, len: u64) -> io::Result<()> {
        let w = self.staged(h, len > 0)?;
        let mut st = w.state.lock();
        st.staged.as_mut().ok_or_else(not_found)?.set_len(len)
    }

    /// Reads at `offset`: a writer handle reads its staged write, anyone else a range of
    /// the source file (a sequential reader continues where the previous read ended; a
    /// jump starts a new one).
    pub fn read(&self, h: Handle, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let o = self.handle(h)?;
        let mut o = o.lock();
        if o.writer {
            if let Some(w) = self.pending(&o.path) {
                let mut st = w.state.lock();
                if let Some(s) = st.staged.as_mut().filter(|s| s.is_open()) {
                    return s.read_at(offset, buf);
                }
            }
        }
        let provider = self.online_provider()?;
        let target = self.map.vpath(&o.path);
        if let Some(local) = target.to_local_path() {
            return read_local(&local, offset, buf);
        }
        if o.cursor.as_ref().is_none_or(|c| c.pos > offset) {
            o.cursor = Some(Cursor {
                reader: provider.read(&target).map_err(io_err)?,
                pos: 0,
            });
        }
        let c = o.cursor.as_mut().expect("cursor");
        if c.pos < offset {
            let skip = offset - c.pos;
            let skipped = io::copy(&mut (&mut c.reader).take(skip), &mut io::sink())?;
            c.pos += skipped;
            if skipped < skip {
                return Ok(0);
            }
        }
        let mut n = 0;
        while n < buf.len() {
            match c.reader.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        c.pos += n as u64;
        Ok(n)
    }

    /// Publishes a staged write (never while closing). A source that cannot be reached
    /// keeps the data under an "unsaved" name and reports the error.
    fn publish(&self, st: &mut WriteState) -> io::Result<()> {
        if self.is_closing() {
            return Ok(());
        }
        let Some(s) = st.staged.as_mut().filter(|s| s.is_open()) else {
            return Ok(());
        };
        match self.provider() {
            Ok(p) => s.publish(&*p),
            Err(e) => {
                s.keep_unsaved(&e);
                Err(e)
            }
        }
    }

    /// A program closed `h` (FUSE `flush` on `close(2)`, WinFsp cleanup): when `h` is the
    /// only writer of its file the write is published now, before `close` returns, so the
    /// program reads back what it wrote. Errors reach the program's `close`.
    pub fn flush(&self, h: Handle) -> io::Result<()> {
        let Ok(o) = self.handle(h) else {
            return Ok(());
        };
        let (path, writer) = {
            let o = o.lock();
            (o.path.clone(), o.writer)
        };
        if !writer {
            return Ok(());
        }
        let Some(w) = self.pending(&path) else {
            return Ok(());
        };
        let mut st = w.state.lock();
        if st.writers != 1 || st.gone {
            return Ok(());
        }
        self.publish(&mut st)
    }

    /// Closes `h`; the last writer of a file publishes its staged write (if a flush has not)
    /// and only then takes the write out of the table.
    pub fn release(&self, h: Handle) -> io::Result<()> {
        let Some(o) = self.handles.lock().remove(&h) else {
            return Ok(());
        };
        let (path, writer) = {
            let o = o.lock();
            (o.path.clone(), o.writer)
        };
        if !writer {
            return Ok(());
        }
        let key = self.map.key(&path);
        let Some(w) = self.writes.lock().get(&key).cloned() else {
            return Ok(());
        };
        let mut st = w.state.lock();
        st.writers = st.writers.saturating_sub(1);
        if st.writers > 0 || st.gone {
            return Ok(());
        }
        let result = self.publish(&mut st);
        st.gone = true;
        st.staged = None;
        let mut writes = self.writes.lock();
        if writes.get(&key).is_some_and(|x| Arc::ptr_eq(x, &w)) {
            writes.remove(&key);
        }
        result
    }

    pub fn mkdir(&self, p: &MountPath) -> io::Result<()> {
        if !self.map.shown(p.name()) || p.is_root() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let (parent, _) = self.lookup(&p.parent().unwrap_or_default())?;
        if self.lookup(p).is_ok() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let path = parent.join(p.name())?;
        self.online_provider()?
            .mkdir(&self.map.vpath(&path))
            .map_err(io_err)
    }

    /// Removes a file (to the OS trash for local sources, like Keel's own delete). A write
    /// in progress on it is dropped.
    pub fn remove_file(&self, p: &MountPath) -> io::Result<()> {
        let provider = self.online_provider()?;
        let mut dropped = false;
        if let Some(w) = self.pending(p) {
            let mut st = w.state.lock();
            if let Some(s) = st.staged.as_mut() {
                dropped = s.is_open();
                s.abort();
            }
        }
        match self.lookup(p) {
            Ok((path, attr)) if !attr.is_dir => provider
                .remove(&self.map.vpath(&path))
                .map_err(io_err)
                .or_else(|e| if dropped { Ok(()) } else { Err(e) }),
            Ok(_) => Err(io::Error::from(io::ErrorKind::IsADirectory)),
            Err(_) if dropped => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Removes an empty folder.
    pub fn remove_dir(&self, p: &MountPath) -> io::Result<()> {
        let provider = self.online_provider()?;
        let (path, attr) = self.lookup(p)?;
        if path.is_root() {
            return Err(busy("the mount root"));
        }
        if !attr.is_dir {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        if !self.list(&path)?.is_empty() {
            return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty));
        }
        provider
            .remove_empty_dir(&self.map.vpath(&path))
            .map_err(io_err)
    }

    /// Renames or moves within the mount. `replace`: an existing file at `to` is replaced.
    pub fn rename(&self, from: &MountPath, to: &MountPath, replace: bool) -> io::Result<()> {
        let provider = self.online_provider()?;
        let busy_paths = |p: &MountPath| self.writes.lock().values().any(|w| w.path.within(p));
        // ponytail: a file being written (or a folder holding one) cannot be renamed until
        // its writers close; retargeting the pending write would lift this.
        if busy_paths(from) || busy_paths(to) {
            return Err(being_written());
        }
        let (from, attr) = self.lookup(from)?;
        let (to_parent, _) = self.lookup(&to.parent().ok_or_else(|| busy("the mount root"))?)?;
        if !self.map.shown(to.name()) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let to = to_parent.join(to.name())?;
        let (src, dst) = (self.map.vpath(&from), self.map.vpath(&to));
        let existing = self.lookup(&to).ok();
        let same = existing
            .as_ref()
            .is_some_and(|(p, _)| self.map.key(p) == self.map.key(&from));
        match existing {
            Some((_, a)) if !same => {
                if !replace || attr.is_dir || a.is_dir {
                    return Err(io::Error::from(io::ErrorKind::AlreadyExists));
                }
                provider.rename_replace(&src, &dst).map_err(io_err)?;
            }
            _ => provider.rename(&src, &dst).map_err(io_err)?,
        }
        // Re-aim open handles (reads continue from a new reader).
        for o in self.handles.lock().values() {
            let mut o = o.lock();
            if let Some(p) = o.path.rebase(&from, &to) {
                o.path = p;
                o.cursor = None;
            }
        }
        Ok(())
    }

    /// Writes in progress (they are discarded by [`MountFs::abort_all`]).
    pub fn pending_writes(&self) -> usize {
        let all: Vec<_> = self.writes.lock().values().cloned().collect();
        all.iter().filter(|w| w.state.lock().open()).count()
    }

    /// Unmounting: from now on nothing is staged or published, and every write in progress
    /// is dropped (its target stays as it was). Call it before the backend is torn down so
    /// a close processed during teardown cannot publish a half-written file.
    pub fn abort_all(&self) {
        self.closing.store(true, Ordering::Release);
        let all: Vec<_> = self.writes.lock().drain().map(|(_, w)| w).collect();
        for w in all {
            let mut st = w.state.lock();
            st.gone = true;
            if let Some(s) = st.staged.as_mut() {
                s.abort();
            }
        }
        self.handles.lock().clear();
    }
}

impl Drop for MountFs {
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// Positional read of a local file (no cursor needed).
// ponytail: opens the file per read; keep it open on the handle if local reads show up hot.
fn read_local(path: &std::path::Path, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
