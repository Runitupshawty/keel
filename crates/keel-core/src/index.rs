//! The indexer: streaming full walks in 5,000-row transactions, single-path change
//! application, and watching (notify for local sources, polling for remote/cloud ones).
//!
//! Identity: a record is found again by `fs_id` (volume serial + file id, dev + inode) before
//! falling back to parent + name, so a move or rename updates name/parent/path in place and
//! keeps the record id (tags and hashes stay attached).

use crate::fsid::{self, Item, DANGLING, DIR, FILE};
use crate::library::{Source, SourceStatus};
use crate::Cancelled;
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use keel_vfs::{Provider, Router, VPath};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Rows per write transaction during a walk.
pub const BATCH: u64 = 5_000;
/// How often `Indexer::watch` re-walks a remote or cloud source.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// A walk commits before listing a folder once its open batch is this old, so a slow
/// (remote) listing never holds the store's write lock.
const MAX_BATCH_AGE: Duration = Duration::from_millis(500);

pub(crate) const UNREADABLE: i64 = 1;
pub(crate) const HIDDEN: i64 = 2;
pub(crate) const LINK: i64 = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexProgress {
    pub done: u64,
    /// The previous walk's record count (an estimate; 0 on the first walk).
    pub total: u64,
    /// The folder being listed, relative to the source root.
    pub current: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeEvent {
    /// Something at this path was created or modified (or renamed to it).
    Changed(VPath),
    Removed(VPath),
    /// Events were lost (watcher overflow): only a full walk can catch up.
    Rescan,
}

/// The source's root cannot be reached: the walk stops without removing anything.
#[derive(Debug)]
struct Offline(String);
impl std::fmt::Display for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "source offline: {}", self.0)
    }
}
impl std::error::Error for Offline {}

enum Lister {
    Local,
    Remote(Arc<dyn Provider>),
}

impl Lister {
    fn new(src: &Source, router: &Router) -> Result<Lister> {
        if src.def.root.to_local_path().is_some() {
            return Ok(Lister::Local);
        }
        router
            .provider_for(&src.def.root)
            .map(Lister::Remote)
            .ok_or_else(|| Offline(format!("no provider for {}", src.def.root.display())).into())
    }

    fn list(&self, dir: &VPath) -> Result<Vec<Item>> {
        match self {
            Lister::Local => {
                Ok(fsid::list(&local(dir)?).with_context(|| format!("list {}", dir.display()))?)
            }
            Lister::Remote(p) => Ok(p.list(dir)?.into_iter().map(item_of).collect()),
        }
    }

    fn stat(&self, p: &VPath) -> Result<Item> {
        match self {
            Lister::Local => {
                Ok(fsid::stat(&local(p)?).with_context(|| format!("stat {}", p.display()))?)
            }
            Lister::Remote(provider) => Ok(item_of(provider.stat(p)?)),
        }
    }
}

fn local(p: &VPath) -> Result<std::path::PathBuf> {
    p.to_local_path()
        .with_context(|| format!("not a local path: {}", p.display()))
}

fn item_of(e: keel_vfs::Entry) -> Item {
    let kind = match e.kind {
        keel_vfs::Kind::File => FILE,
        keel_vfs::Kind::Dir => DIR,
        keel_vfs::Kind::Symlink => DANGLING,
    };
    Item {
        size: if kind == FILE { e.size as i64 } else { 0 },
        mtime: e.modified.map(fsid::unix),
        ctime: None,
        hidden: e.hidden,
        link: e.is_link,
        fs_id: None,
        error: None,
        name: e.name,
        kind,
    }
}

/// Identity where the filesystem has none: FNV-1a of the parent's identity and the name.
fn name_hash(parent: &str, name: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in parent.bytes().chain([0]).chain(name.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("h:{h:016x}")
}

fn matcher(patterns: &[String]) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new("");
    for p in patterns {
        builder.add_line(None, p)?;
    }
    Ok(builder.build()?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Inserted,
    Moved,
    Kept,
}

/// Writes `item` at `rel` (under `parent`), reusing the record with the same identity that
/// this walk has not seen yet (`gen < unseen_below`), else the one at the same parent+name.
fn upsert(
    c: &Connection,
    gen: i64,
    unseen_below: i64,
    parent: Option<i64>,
    rel: &str,
    item: &Item,
    fs_id: &str,
) -> Result<(i64, Outcome)> {
    type Found = (i64, Option<i64>, String, String, i64);
    let row = |r: &rusqlite::Row| -> rusqlite::Result<Found> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    };
    // A hash identity is parent+name itself: the second lookup finds it.
    let mut found = None;
    if !fs_id.starts_with("h:") {
        found = c
            .prepare_cached(
                "SELECT id, parent, name, path, kind FROM record
                 WHERE fs_id = ?1 AND substr(fs_id, 1, 2) <> 'h:' AND gen < ?2
                 ORDER BY (parent IS ?3 AND name = ?4) DESC LIMIT 1",
            )?
            .query_row(params![fs_id, unseen_below, parent, item.name], row)
            .optional()?;
    }
    if found.is_none() {
        found = c
            .prepare_cached(
                "SELECT id, parent, name, path, kind FROM record
                 WHERE parent IS ?1 AND name = ?2 AND kind = ?3 AND gen < ?4 LIMIT 1",
            )?
            .query_row(params![parent, item.name, item.kind, unseen_below], row)
            .optional()?;
    }
    let flags = if item.hidden { HIDDEN } else { 0 }
        | if item.link { LINK } else { 0 }
        | if item.error.is_some() { UNREADABLE } else { 0 };
    let Some((id, old_parent, old_name, old_path, old_kind)) = found else {
        c.prepare_cached(
            "INSERT INTO record(parent, name, path, kind, size, mtime, ctime, fs_id, gen, flags, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?
        .execute(params![
            parent, item.name, rel, item.kind, item.size, item.mtime, item.ctime, fs_id, gen,
            flags, item.error
        ])?;
        return Ok((c.last_insert_rowid(), Outcome::Inserted));
    };
    let mut outcome = Outcome::Kept;
    // Separate statement: the FTS trigger fires whenever name/path are assigned.
    if old_parent != parent || old_name != item.name || old_path != rel {
        c.prepare_cached("UPDATE record SET parent = ?2, name = ?3, path = ?4 WHERE id = ?1")?
            .execute(params![id, parent, item.name, rel])?;
        if old_kind == DIR && old_path != rel {
            rename_subtree(c, id, rel)?;
        }
        outcome = Outcome::Moved;
    }
    // Content ids survive only while size, mtime and kind are unchanged.
    c.prepare_cached(
        "UPDATE record SET kind = ?2, size = ?3, mtime = ?4, ctime = ?5, fs_id = ?6, gen = ?7,
             flags = ?8, error = ?9,
             cas_id = CASE WHEN size = ?3 AND mtime IS ?4 AND kind = ?2 THEN cas_id END,
             sampled_hash = CASE WHEN size = ?3 AND mtime IS ?4 AND kind = ?2 THEN sampled_hash END
         WHERE id = ?1",
    )?
    .execute(params![
        id, item.kind, item.size, item.mtime, item.ctime, fs_id, gen, flags, item.error
    ])?;
    Ok((id, outcome))
}

/// Rewrites the stored paths below a folder that moved to `rel` (ids are untouched).
fn rename_subtree(c: &Connection, id: i64, rel: &str) -> Result<()> {
    c.prepare_cached(
        "WITH RECURSIVE sub(id, path) AS (
             SELECT id, ?2 || name FROM record WHERE parent = ?1
             UNION ALL SELECT r.id, sub.path || '/' || r.name FROM record r JOIN sub ON r.parent = sub.id)
         UPDATE record SET path = sub.path FROM sub WHERE record.id = sub.id",
    )?
    .execute(params![id, format!("{rel}/")])?;
    Ok(())
}

pub(crate) fn delete_subtree(c: &Connection, id: i64) -> Result<()> {
    c.prepare_cached(
        "WITH RECURSIVE sub(id) AS (
             SELECT ?1 UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id)
         DELETE FROM record WHERE id IN (SELECT id FROM sub)",
    )?
    .execute([id])?;
    Ok(())
}

/// Marks everything below `id` as seen by generation `gen` (its folder could not be listed:
/// the last known contents stay).
fn keep_subtree(c: &Connection, id: i64, gen: i64) -> Result<()> {
    c.prepare_cached(
        "WITH RECURSIVE sub(id) AS (
             SELECT id FROM record WHERE parent = ?1
             UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id)
         UPDATE record SET gen = ?2 WHERE id IN (SELECT id FROM sub)",
    )?
    .execute(params![id, gen])?;
    Ok(())
}

/// The record (id, fs_id) at `rel` ("" = the root), by parent + name.
pub(crate) fn resolve(c: &Connection, rel: &str, nocase: bool) -> Result<Option<(i64, String)>> {
    let row = |r: &rusqlite::Row| Ok((r.get(0)?, r.get(1)?));
    let mut cur: Option<(i64, String)> = c
        .prepare_cached("SELECT id, fs_id FROM record WHERE parent IS NULL ORDER BY id LIMIT 1")?
        .query_row([], row)
        .optional()?;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        let Some((id, _)) = cur else { return Ok(None) };
        cur = c
            .prepare_cached(
                "SELECT id, fs_id FROM record WHERE parent = ?1 AND name = ?2 ORDER BY id LIMIT 1",
            )?
            .query_row(params![id, part], row)
            .optional()?;
        if cur.is_none() && nocase {
            cur = c
                .prepare_cached(
                    "SELECT id, fs_id FROM record WHERE parent = ?1 AND name = ?2 COLLATE NOCASE
                     ORDER BY id LIMIT 1",
                )?
                .query_row(params![id, part], row)
                .optional()?;
        }
    }
    Ok(cur)
}

struct Pending {
    dir: VPath,
    id: i64,
    rel: String,
    fs_id: String,
}

struct Walk<'a> {
    src: &'a Source,
    lister: &'a Lister,
    ignore: &'a Gitignore,
    conn: &'a Connection,
    gen: i64,
    unseen_below: i64,
    /// Own the transactions (a full walk); false inside a caller's transaction.
    batched: bool,
    batch: u64,
    batch_started: Option<Instant>,
    done: u64,
    total: u64,
    current: String,
    progress: &'a dyn Fn(IndexProgress),
    cancel: &'a AtomicBool,
}

impl Walk<'_> {
    fn begin(&mut self) -> Result<()> {
        if self.batched && self.batch_started.is_none() {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            self.batch_started = Some(Instant::now());
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        if self.batch_started.take().is_some() {
            self.conn.execute_batch("COMMIT")?;
            self.batch = 0;
            (self.progress)(IndexProgress {
                done: self.done,
                total: self.total,
                current: self.current.clone(),
            });
        }
        Ok(())
    }

    fn run(&mut self, mut stack: Vec<Pending>) -> Result<()> {
        while let Some(p) = stack.pop() {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            if self
                .batch_started
                .is_some_and(|t| t.elapsed() > MAX_BATCH_AGE)
            {
                self.commit()?;
            }
            self.current.clone_from(&p.rel);
            let items = match self.lister.list(&p.dir) {
                Ok(items) => items,
                Err(e) => {
                    if let Err(root) = self.lister.stat(&self.src.def.root) {
                        return Err(Offline(format!("{root:#}")).into());
                    }
                    // Unreadable: keep the folder with its error and its last known contents.
                    self.begin()?;
                    self.conn
                        .prepare_cached(
                            "UPDATE record SET flags = flags | ?3, error = ?2 WHERE id = ?1",
                        )?
                        .execute(params![p.id, format!("{e:#}"), UNREADABLE])?;
                    keep_subtree(self.conn, p.id, self.gen)?;
                    continue;
                }
            };
            let prefix = if p.rel.is_empty() {
                String::new()
            } else {
                format!("{}/", p.rel)
            };
            for item in items {
                if item.hidden && !self.src.def.include_hidden {
                    continue;
                }
                let rel = format!("{prefix}{}", item.name);
                if self.ignore.matched(&rel, item.kind == DIR).is_ignore() {
                    continue;
                }
                let fs_id = item
                    .fs_id
                    .clone()
                    .unwrap_or_else(|| name_hash(&p.fs_id, &item.name));
                self.begin()?;
                let (id, _) = upsert(
                    self.conn,
                    self.gen,
                    self.unseen_below,
                    Some(p.id),
                    &rel,
                    &item,
                    &fs_id,
                )?;
                if item.kind == DIR && !item.link {
                    stack.push(Pending {
                        dir: p.dir.join(&item.name),
                        id,
                        rel,
                        fs_id,
                    });
                }
                self.done += 1;
                self.batch += 1;
                if self.batch >= BATCH {
                    self.commit()?;
                }
            }
        }
        Ok(())
    }
}

pub struct Indexer;

impl Indexer {
    /// Walks the whole source as a new generation. Records are written as they are found
    /// (5,000 per transaction, constant memory); only a completed walk removes records it
    /// did not see and advances `src.generation`. A cancelled walk, or one whose source goes
    /// offline, removes nothing: the store keeps the last generation plus what was updated.
    /// Folders that cannot be listed are kept (with their error and last known contents).
    pub fn full_walk(
        src: &Source,
        router: &Router,
        progress: &dyn Fn(IndexProgress),
        cancel: &AtomicBool,
    ) -> Result<()> {
        let ignore = matcher(&src.def.ignore)?;
        let gen = {
            let _w = src.write.lock();
            anyhow::ensure!(
                src.pending_gen.load(Ordering::SeqCst) == 0,
                "{} is already being indexed",
                src.def.label
            );
            // Above every generation an earlier (possibly abandoned) walk wrote.
            let used: u64 = src
                .store
                .meta("gen_counter")?
                .and_then(|g| g.parse().ok())
                .unwrap_or(0);
            let gen = used.max(src.generation.load(Ordering::SeqCst)) + 1;
            src.store.set_meta("gen_counter", &gen.to_string())?;
            src.pending_gen.store(gen, Ordering::SeqCst);
            gen
        };
        let last_walk: Option<i64> = src
            .store
            .meta("last_full_walk")
            .ok()
            .flatten()
            .and_then(|t| t.parse().ok());
        let result = (|| -> Result<IndexProgress> {
            let conn = src.store.get()?;
            // Identity lookups touch the fs_id index at random: give the walk a bigger cache.
            conn.execute_batch("PRAGMA cache_size=-65536")?;
            let total: i64 = conn.query_row("SELECT count(*) FROM record", [], |r| r.get(0))?;
            *src.status.write() = SourceStatus::Indexing {
                done: 0,
                total: total as u64,
            };
            let report = |p: IndexProgress| {
                *src.status.write() = SourceStatus::Indexing {
                    done: p.done,
                    total: p.total,
                };
                progress(p);
            };
            let lister = Lister::new(src, router)?;
            let mut root = lister
                .stat(&src.def.root)
                .map_err(|e| Offline(format!("{e:#}")))?;
            anyhow::ensure!(
                root.kind == DIR,
                "{} is not a folder",
                src.def.root.display()
            );
            root.name = match src.def.root.name() {
                "" => src.def.label.clone(),
                name => name.to_owned(),
            };
            // A different volume or folder at the root (an unmounted mount point, a reassigned
            // drive letter) must not replace the snapshot: it reads as offline.
            let adopt = src.store.meta("adopt_root")?.is_some();
            if let (Some(now_id), Some(was), false) =
                (&root.fs_id, src.store.meta("root_id")?, adopt)
            {
                if *now_id != was {
                    return Err(Offline(format!(
                        "{} is a different folder than the one indexed",
                        src.def.root.display()
                    ))
                    .into());
                }
            }
            let root_fs = root.fs_id.clone().unwrap_or_else(|| name_hash("", ""));
            let mut walk = Walk {
                src,
                lister: &lister,
                ignore: &ignore,
                conn: &conn,
                gen: gen as i64,
                unseen_below: gen as i64,
                batched: true,
                batch: 0,
                batch_started: None,
                done: 1,
                total: total as u64,
                current: String::new(),
                progress: &report,
                cancel,
            };
            walk.begin()?;
            let (id, _) = upsert(&conn, gen as i64, gen as i64, None, "", &root, &root_fs)?;
            let walked = walk.run(vec![Pending {
                dir: src.def.root.clone(),
                id,
                rel: String::new(),
                fs_id: root_fs,
            }]);
            // What was written stays, also after a cancel or an outage.
            walk.commit()?;
            walked?;
            // An empty root that had entries is more likely unmounted than emptied.
            if walk.done == 1 && total > 1 && !adopt {
                return Err(Offline(format!(
                    "{} is empty but had {} entries",
                    src.def.root.display(),
                    total - 1
                ))
                .into());
            }
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM record WHERE gen < ?1", [gen as i64])?;
            match &root.fs_id {
                Some(id) => crate::db::set_meta(&tx, "root_id", id)?,
                None => {
                    tx.execute("DELETE FROM meta WHERE key = 'root_id'", [])?;
                }
            }
            tx.execute("DELETE FROM meta WHERE key = 'adopt_root'", [])?;
            crate::db::set_meta(&tx, "generation", &gen.to_string())?;
            crate::db::set_meta(&tx, "last_full_walk", &crate::now().to_string())?;
            tx.commit()?;
            Ok(IndexProgress {
                done: walk.done,
                total: walk.done,
                current: String::new(),
            })
        })();
        let _w = src.write.lock();
        src.pending_gen.store(0, Ordering::SeqCst);
        match result {
            Ok(done) => {
                src.generation.store(gen, Ordering::SeqCst);
                *src.status.write() = SourceStatus::Online {
                    indexed_at: Some(crate::now()),
                };
                progress(done);
                Ok(())
            }
            Err(e) => {
                *src.status.write() = if e.is::<Cancelled>() {
                    SourceStatus::Online {
                        indexed_at: last_walk,
                    }
                } else if e.is::<Offline>() {
                    SourceStatus::Offline {
                        last_seen: last_walk,
                    }
                } else {
                    SourceStatus::Error(format!("{e:#}"))
                };
                Err(e)
            }
        }
    }

    /// Accepts whatever is at the source's root now: the next full walk replaces the snapshot
    /// even when the root is a different folder or volume than the one indexed, or empty.
    pub fn adopt_root(src: &Source) -> Result<()> {
        src.store.set_meta("adopt_root", "1")
    }

    /// Reconciles one local path with the store: stats it and inserts, updates, moves (by
    /// identity) or removes its record. A folder that appears from outside is indexed with
    /// its contents. Apply the events of a batch for paths that exist before those that do
    /// not, so a rename is seen as a move rather than a delete + insert.
    pub fn apply_change(src: &Source, ev: ChangeEvent) -> Result<()> {
        let path = match ev {
            ChangeEvent::Changed(p) | ChangeEvent::Removed(p) => p,
            ChangeEvent::Rescan => anyhow::bail!("a rescan needs Indexer::full_walk"),
        };
        let rel = src
            .relative(&path)
            .with_context(|| format!("{} is outside source {}", path.display(), src.def.label))?;
        let local = path
            .to_local_path()
            .context("changes apply to local sources (remote sources are polled)")?;
        let ignore = matcher(&src.def.ignore)?;
        let _w = src.write.lock();
        let gen = match src.pending_gen.load(Ordering::SeqCst) {
            0 => src.generation.load(Ordering::SeqCst),
            pending => pending,
        } as i64;
        let conn = src.store.get()?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = apply(&conn, src, &ignore, gen, &path, &rel, &local);
        conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result
    }

    /// `watch_every(src, router, POLL_INTERVAL)`.
    pub fn watch(src: &Arc<Source>, router: &Arc<Router>) -> Result<WatchHandle> {
        Self::watch_every(src, router, POLL_INTERVAL)
    }

    /// Local sources: a recursive notify watcher, debounced 500 ms, applied with
    /// `apply_change` (a lost-events error triggers a full walk). Other sources: a full walk
    /// (a new generation snapshot) every `poll`. Dropping the handle stops watching.
    pub fn watch_every(
        src: &Arc<Source>,
        router: &Arc<Router>,
        poll: Duration,
    ) -> Result<WatchHandle> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        let (src, router, stop) = (src.clone(), router.clone(), cancel.clone());
        let rescan = move |src: &Source| {
            if let Err(e) = Indexer::full_walk(src, &router, &|_| {}, &stop) {
                tracing::warn!("re-walk of {}: {e:#}", src.def.label);
            }
        };
        let Some(root) = src.def.root.to_local_path() else {
            let thread = std::thread::Builder::new()
                .name("keel-poll".into())
                .spawn(move || {
                    while let Err(crossbeam_channel::RecvTimeoutError::Timeout) =
                        stop_rx.recv_timeout(poll)
                    {
                        rescan(&src);
                    }
                })?;
            return Ok(WatchHandle {
                cancel,
                stop: stop_tx,
                debouncer: None,
                thread: Some(thread),
            });
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut debouncer = notify_debouncer_mini::new_debouncer(
            Duration::from_millis(500),
            move |res: notify_debouncer_mini::DebounceEventResult| {
                let _ = tx.send(res);
            },
        )?;
        debouncer
            .watcher()
            .watch(
                &root,
                notify_debouncer_mini::notify::RecursiveMode::Recursive,
            )
            .with_context(|| format!("watch {}", root.display()))?;
        let thread = std::thread::Builder::new()
            .name("keel-watch-source".into())
            .spawn(move || {
                drop(stop_rx);
                while let Ok(res) = rx.recv() {
                    let Ok(events) = res else {
                        rescan(&src);
                        continue;
                    };
                    let mut paths: Vec<_> = events.into_iter().map(|e| e.path).collect();
                    paths.sort();
                    paths.dedup();
                    // Present paths first: a rename then reads as a move.
                    paths.sort_by_key(|p| p.symlink_metadata().is_err());
                    for p in paths {
                        let ev = ChangeEvent::Changed(VPath::local(&p));
                        if let Err(e) = Indexer::apply_change(&src, ev) {
                            tracing::debug!("index change {}: {e:#}", p.display());
                        }
                    }
                }
            })?;
        Ok(WatchHandle {
            cancel,
            stop: stop_tx,
            debouncer: Some(debouncer),
            thread: Some(thread),
        })
    }
}

fn apply(
    c: &Connection,
    src: &Source,
    ignore: &Gitignore,
    gen: i64,
    path: &VPath,
    rel: &str,
    local: &std::path::Path,
) -> Result<()> {
    let nocase = src.nocase();
    let existing = resolve(c, rel, nocase)?;
    let mut item = match fsid::stat(local) {
        Ok(item) => item,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some((id, _)) = existing {
                delete_subtree(c, id)?;
            }
            return Ok(());
        }
        Err(e) => {
            if let Some((id, _)) = existing {
                c.execute(
                    "UPDATE record SET flags = flags | ?3, error = ?2 WHERE id = ?1",
                    params![id, e.to_string(), UNREADABLE],
                )?;
            }
            return Ok(());
        }
    };
    if rel.is_empty() {
        // The root itself: keep its record, refresh its metadata.
        let Some((id, fs_id)) = existing else {
            return Ok(());
        };
        item.name = c.query_row("SELECT name FROM record WHERE id = ?1", [id], |r| r.get(0))?;
        upsert(
            c,
            gen,
            i64::MAX,
            None,
            "",
            &item,
            &item.fs_id.clone().unwrap_or(fs_id),
        )?;
        return Ok(());
    }
    let (parent_rel, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    item.name = name.to_owned();
    // A parent that is not indexed (hidden, ignored, never walked): nothing to do.
    let Some((parent_id, parent_fs)) = resolve(c, parent_rel, nocase)? else {
        return Ok(());
    };
    let excluded = (item.hidden && !src.def.include_hidden)
        || ignore
            .matched_path_or_any_parents(rel, item.kind == DIR)
            .is_ignore();
    if excluded {
        if let Some((id, _)) = existing {
            delete_subtree(c, id)?;
        }
        return Ok(());
    }
    let fs_id = item
        .fs_id
        .clone()
        .unwrap_or_else(|| name_hash(&parent_fs, name));
    let (id, outcome) = upsert(c, gen, i64::MAX, Some(parent_id), rel, &item, &fs_id)?;
    // Whatever else sat at this path was replaced.
    let others: Vec<i64> = c
        .prepare_cached("SELECT id FROM record WHERE parent = ?1 AND name = ?2 AND id != ?3")?
        .query_map(params![parent_id, name, id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for other in others {
        delete_subtree(c, other)?;
    }
    if item.kind == DIR && !item.link && outcome == Outcome::Inserted {
        let lister = Lister::Local;
        let never = AtomicBool::new(false);
        Walk {
            src,
            lister: &lister,
            ignore,
            conn: c,
            gen,
            unseen_below: i64::MAX,
            batched: false,
            batch: 0,
            batch_started: None,
            done: 0,
            total: 0,
            current: String::new(),
            progress: &|_| {},
            cancel: &never,
        }
        .run(vec![Pending {
            dir: path.clone(),
            id,
            rel: rel.to_owned(),
            fs_id,
        }])?;
    }
    Ok(())
}

/// Stops watching when dropped (waits for an in-flight change or walk to stop).
pub struct WatchHandle {
    cancel: Arc<AtomicBool>,
    stop: crossbeam_channel::Sender<()>,
    debouncer:
        Option<notify_debouncer_mini::Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        // Dropping the debouncer ends its thread and with it the event channel.
        self.debouncer.take();
        // Replacing the sender disconnects the polling thread's stop channel.
        self.stop = crossbeam_channel::bounded(0).0;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[path = "index_tests.rs"]
pub(crate) mod tests;
