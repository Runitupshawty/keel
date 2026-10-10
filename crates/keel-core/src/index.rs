//! The indexer: streaming full walks in 5,000-row transactions, single-path change
//! application, and watching (notify for local sources; for remote and cloud ones the
//! provider's change feed, or the times of their folders, between full walks).
//!
//! Identity: a record is found again by `fs_id` (volume serial + file id, dev + inode) before
//! falling back to parent + name, so a move or rename updates name/parent/path in place and
//! keeps the record id (tags and hashes stay attached).

use crate::fsid::{self, Item, DANGLING, DIR, FILE};
use crate::library::{OfflineReason, Source, SourceStatus};
use crate::Cancelled;
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use keel_vfs::{ChangeCursor, ChangeKind, ChangedPath, FeedError, Provider, Router, VPath};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Rows per write transaction during a walk.
pub const BATCH: u64 = 5_000;
/// How often `Indexer::watch` asks a remote or cloud source what changed (its change feed,
/// or the times of its folders); `[library] remote_poll_secs` in the app and daemon.
pub const POLL_INTERVAL: Duration = Duration::from_secs(120);
/// How often `Indexer::watch` walks a remote source that has neither (WebDAV, an S3
/// bucket of more than 1,000 objects).
pub const WALK_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// How often `Indexer::watch` re-walks a source besides applying changes.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// `store` meta key: a remote source's change feed cursor, taken before its last walk.
const CURSOR: &str = "change_cursor";
/// Attempts at taking the store's write lock (each waits the 5 s busy timeout).
const BEGIN_ATTEMPTS: u32 = 4;

/// The search filter indexes (as in `schema.sql`, source version 5).
pub(crate) const FILTER_INDEXES: &str = "
    CREATE INDEX IF NOT EXISTS record_mtime ON record(mtime) WHERE parent IS NOT NULL;
    CREATE INDEX IF NOT EXISTS record_size ON record(size) WHERE kind = 0;
    CREATE INDEX IF NOT EXISTS record_kind ON record(kind, mtime) WHERE parent IS NOT NULL;";
const DROP_FILTER_INDEXES: &str = "
    DROP INDEX IF EXISTS record_mtime;
    DROP INDEX IF EXISTS record_size;
    DROP INDEX IF EXISTS record_kind;";

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

/// The source's root cannot be reached, or is not the indexed folder: the walk stops
/// without removing anything.
#[derive(Debug)]
struct Offline(OfflineReason, String);
impl std::fmt::Display for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "source offline: {}", self.1)
    }
}
impl std::error::Error for Offline {}

/// `full_walk` refused: another walk of the source (a watcher's) is running.
#[derive(Debug)]
pub(crate) struct Busy(String);
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is already being indexed", self.0)
    }
}
impl std::error::Error for Busy {}

fn unreachable(msg: String) -> anyhow::Error {
    Offline(OfflineReason::Unreachable, msg).into()
}

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
            .ok_or_else(|| unreachable(format!("no provider for {}", src.def.root.display())))
    }

    fn list(&self, dir: &VPath) -> Result<Vec<Item>> {
        match self {
            Lister::Local => {
                Ok(fsid::list(&local(dir)?).with_context(|| format!("list {}", dir.display()))?)
            }
            // Fresh and complete: a cut-off listing fails (the folder is kept as unreadable).
            // Device sources: with the content ids their host claims (`remote_cas`).
            Lister::Remote(p) => Ok(p
                .list_complete_ids(dir)?
                .into_iter()
                .map(|(e, cas)| Item { cas, ..item_of(e) })
                .collect()),
        }
    }

    /// The source root (only ever called on it): a root that is a junction or symlink is
    /// followed, so its target is indexed.
    fn stat(&self, p: &VPath) -> Result<Item> {
        match self {
            Lister::Local => {
                Ok(fsid::stat_root(&local(p)?).with_context(|| format!("stat {}", p.display()))?)
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
        mtime: e.modified.map(fsid::unix_ns),
        ctime: None,
        hidden: e.hidden,
        link: e.is_link,
        fs_id: None,
        error: None,
        cas: None,
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

/// Is the file with native id `fs_id` still at the record path `old` (a hard link of it
/// appeared at `new`, rather than the file moving there)?
type StillAt<'a> = &'a dyn Fn(&str, &str, &str) -> bool;

/// For a local source: `StillAt` by a stat of the old path.
fn still_at(src: &Source) -> impl Fn(&str, &str, &str) -> bool + '_ {
    move |old: &str, new: &str, fs_id: &str| {
        // On a case-insensitive volume a case-only rename finds the file at its old name.
        if src.nocase() && old.to_lowercase() == new.to_lowercase() {
            return false;
        }
        src.absolute(old)
            .to_local_path()
            .and_then(|p| fsid::stat(&p).ok())
            .and_then(|i| i.fs_id)
            .is_some_and(|id| id == fs_id)
    }
}

/// Writes `item` at `rel` (under `parent`), reusing the record with the same identity that
/// this walk has not seen yet (`gen < unseen_below`), else the one at the same parent+name.
/// The record already at parent+name is reused whatever its generation: a walk lists each
/// folder once, so one with the walk's own generation was written by a change applied
/// meanwhile (`apply_paths`), and a second record there would leave the first one, the one
/// paths resolve to, without children.
/// A native-id match elsewhere is taken over only when `still` says the file is no longer
/// at that record's path (a move, not a new hard link). For an item with a native id, a
/// parent+name match with a different native id is reused only when `gone(that id)`: the
/// file it described is no longer in this folder (a file replaced on save), not merely
/// renamed next to the new one.
#[allow(clippy::too_many_arguments)]
fn upsert(
    c: &Connection,
    gen: i64,
    unseen_below: i64,
    parent: Option<i64>,
    rel: &str,
    item: &Item,
    fs_id: &str,
    gone: &dyn Fn(&str) -> bool,
    still: StillAt,
) -> Result<(i64, Outcome)> {
    let native = |id: &str| !id.starts_with("h:");
    // A hash identity is parent+name itself: the second lookup finds it.
    let mut found = None;
    if !fs_id.starts_with("h:") {
        // Several records share a native id when the file has hard links: the one at this
        // path, else one whose file has left its path (moved here).
        let candidates: Vec<Found> = c
            .prepare_cached(&format!(
                "SELECT {FOUND} FROM record
                 WHERE fs_id = ?1 AND substr(fs_id, 1, 2) <> 'h:'
                   AND (gen < ?2 OR (parent IS ?3 AND name = ?4))
                 ORDER BY (parent IS ?3 AND name = ?4) DESC"
            ))?
            .query_map(params![fs_id, unseen_below, parent, item.name], found_row)?
            .collect::<rusqlite::Result<_>>()?;
        found = candidates.into_iter().find(|f| {
            (f.parent == parent && f.name == item.name)
                || f.path == rel
                || !still(&f.path, rel, fs_id)
        });
    }
    if found.is_none() {
        found = c
            .prepare_cached(&format!(
                "SELECT {FOUND} FROM record
                 WHERE parent IS ?1 AND name = ?2 AND kind = ?3 LIMIT 1"
            ))?
            .query_row(params![parent, item.name, item.kind], found_row)
            .optional()?
            .filter(|f| !(native(fs_id) && native(&f.fs_id) && f.fs_id != fs_id) || gone(&f.fs_id));
    }
    let flags = if item.hidden { HIDDEN } else { 0 }
        | if item.link { LINK } else { 0 }
        | if item.error.is_some() { UNREADABLE } else { 0 };
    let Some(f) = found else {
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
    if f.parent != parent || f.name != item.name || f.path != rel {
        c.prepare_cached("UPDATE record SET parent = ?2, name = ?3, path = ?4 WHERE id = ?1")?
            .execute(params![f.id, parent, item.name, rel])?;
        if f.kind == DIR && f.path != rel {
            rename_subtree(c, f.id, rel)?;
        }
        outcome = Outcome::Moved;
    }
    let unchanged = (f.kind, f.size, f.mtime, f.ctime, f.flags)
        == (item.kind, item.size, item.mtime, item.ctime, flags)
        && f.fs_id == fs_id
        && f.error == item.error;
    if unchanged {
        // Only seen again: no indexed column is rewritten (nor a row, when its generation is
        // current: `apply_paths` reads "no rows changed" as "nothing to recount").
        c.prepare_cached("UPDATE record SET gen = ?2 WHERE id = ?1 AND gen IS NOT ?2")?
            .execute(params![f.id, gen])?;
        return Ok((f.id, outcome));
    }
    // Content ids survive only while kind, size, mtime and change time (all at full
    // precision) are unchanged.
    c.prepare_cached(
        "UPDATE record SET kind = ?2, size = ?3, mtime = ?4, ctime = ?5, fs_id = ?6, gen = ?7,
             flags = ?8, error = ?9,
             cas_id = CASE WHEN size = ?3 AND mtime IS ?4 AND ctime IS ?5 AND kind = ?2
                 THEN cas_id END,
             sampled_hash = CASE WHEN size = ?3 AND mtime IS ?4 AND ctime IS ?5 AND kind = ?2
                 THEN sampled_hash END,
             drift = CASE WHEN size = ?3 AND mtime IS ?4 AND ctime IS ?5 AND kind = ?2
                 THEN drift END
         WHERE id = ?1",
    )?
    .execute(params![
        f.id, item.kind, item.size, item.mtime, item.ctime, fs_id, gen, flags, item.error
    ])?;
    Ok((f.id, outcome))
}

/// A record `upsert` may reuse, as stored.
struct Found {
    id: i64,
    parent: Option<i64>,
    name: String,
    path: String,
    kind: i64,
    size: i64,
    mtime: Option<i64>,
    ctime: Option<i64>,
    fs_id: String,
    flags: i64,
    error: Option<String>,
}

const FOUND: &str = "id, parent, name, path, kind, size, mtime, ctime, fs_id, flags, error";

fn found_row(r: &rusqlite::Row) -> rusqlite::Result<Found> {
    Ok(Found {
        id: r.get(0)?,
        parent: r.get(1)?,
        name: r.get(2)?,
        path: r.get(3)?,
        kind: r.get(4)?,
        size: r.get(5)?,
        mtime: r.get(6)?,
        ctime: r.get(7)?,
        fs_id: r.get(8)?,
        flags: r.get(9)?,
        error: r.get(10)?,
    })
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
    /// A listed folder's records that the listing no longer has are removed (a folder
    /// listed again outside a full walk).
    prune: bool,
    /// Walk every folder found (else only the ones that are new to the index).
    deep: bool,
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
            begin_immediate(self.conn)?;
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
            if self.cancel.load(Ordering::Relaxed) || self.src.removed.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            // Never list (slow on remotes and dead drives) while holding the write lock.
            self.commit()?;
            self.current.clone_from(&p.rel);
            let items = match self.lister.list(&p.dir) {
                Ok(items) => items,
                Err(e) => {
                    if let Err(root) = self.lister.stat(&self.src.def.root) {
                        return Err(unreachable(format!("{root:#}")));
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
            let listed: HashSet<String> = items.iter().filter_map(|i| i.fs_id.clone()).collect();
            let gone = |id: &str| !listed.contains(id);
            let still = still_at(self.src);
            let mut kept = Vec::new();
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
                let (id, outcome) = upsert(
                    self.conn,
                    self.gen,
                    self.unseen_below,
                    Some(p.id),
                    &rel,
                    &item,
                    &fs_id,
                    &gone,
                    &still,
                )?;
                kept.push(id);
                if matches!(self.lister, Lister::Remote(_)) && item.kind == FILE {
                    // What a device claims (absent: no claim) stays apart from confirmed
                    // content ids: it never counts as a copy.
                    let claim = item.cas.map(|c| c.to_vec());
                    self.conn
                        .prepare_cached(
                            "UPDATE record SET remote_cas = ?2, cas_id = NULL
                             WHERE id = ?1 AND (remote_cas IS NOT ?2 OR cas_id IS NOT NULL)",
                        )?
                        .execute(params![id, claim])?;
                }
                if item.kind == DIR && !item.link && (self.deep || outcome == Outcome::Inserted) {
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
            if self.prune {
                self.begin()?;
                prune_children(self.conn, p.id, &kept)?;
            }
        }
        Ok(())
    }
}

/// Removes the records below folder `parent` that its new listing did not keep.
fn prune_children(c: &Connection, parent: i64, kept: &[i64]) -> Result<()> {
    let kept: HashSet<i64> = kept.iter().copied().collect();
    let children: Vec<i64> = c
        .prepare_cached("SELECT id FROM record WHERE parent = ?1")?
        .query_map([parent], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in children.into_iter().filter(|id| !kept.contains(id)) {
        delete_subtree(c, id)?;
    }
    Ok(())
}

/// The folder record (id, fs_id) at `rel`; None when it is not indexed or not a folder.
fn folder_at(c: &Connection, rel: &str, nocase: bool) -> Result<Option<(i64, String)>> {
    let Some((id, fs_id)) = resolve(c, rel, nocase)? else {
        return Ok(None);
    };
    let kind: i64 = c.query_row("SELECT kind FROM record WHERE id = ?1", [id], |r| r.get(0))?;
    Ok((kind == DIR).then_some((id, fs_id)))
}

/// `rel`, or the first folder above it that the index does not have.
fn first_missing(c: &Connection, rel: &str, nocase: bool) -> Result<String> {
    let mut at = 0;
    while let Some(slash) = rel[at..].find('/') {
        let folder = &rel[..at + slash];
        if resolve(c, folder, nocase)?.is_none() {
            return Ok(folder.to_owned());
        }
        at += slash + 1;
    }
    Ok(rel.to_owned())
}

/// The error (or one it wraps) says the path is not there.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// What `apply` reconciles a path with.
enum At {
    /// A local path, stat'ed inside the transaction.
    Local(std::path::PathBuf),
    /// A remote entry stat'ed before it (a network call): None when it is not there.
    Remote(Option<Item>),
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
            if src.pending_gen.load(Ordering::SeqCst) != 0 {
                return Err(Busy(src.def.label.clone()).into());
            }
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
        // Cleared however the walk ends, a panic included (else the source would stay
        // "already being indexed").
        struct PendingGen<'a>(&'a Source);
        impl Drop for PendingGen<'_> {
            fn drop(&mut self) {
                self.0.pending_gen.store(0, Ordering::SeqCst);
            }
        }
        let pending = PendingGen(src);
        let last_walk: Option<i64> = src
            .store
            .meta("last_full_walk")
            .ok()
            .flatten()
            .and_then(|t| t.parse().ok());
        let result = (|| -> Result<IndexProgress> {
            let conn = src.store.get()?;
            // Identity lookups touch the fs_id index at random: give the walk a bigger cache,
            // and the pooled connection its normal one back afterwards.
            conn.execute_batch("PRAGMA cache_size=-65536")?;
            struct Cache<'a>(&'a Connection);
            impl Drop for Cache<'_> {
                fn drop(&mut self) {
                    let _ = self.0.execute_batch(crate::db::CACHE_SIZE);
                }
            }
            let _cache = Cache(&conn);
            let total: i64 = conn.query_row("SELECT count(*) FROM record", [], |r| r.get(0))?;
            // A first walk inserts every row: the search filter indexes are built once
            // after it (seconds for 2M rows) rather than row by row (tripling the walk).
            struct Filters<'a>(&'a Connection);
            impl Drop for Filters<'_> {
                fn drop(&mut self) {
                    if let Err(e) = self.0.execute_batch(FILTER_INDEXES) {
                        tracing::warn!("search filter indexes: {e:#}");
                    }
                }
            }
            let _filters = (total < BATCH as i64)
                .then(|| {
                    conn.execute_batch(DROP_FILTER_INDEXES)
                        .map(|()| Filters(&conn))
                })
                .transpose()?;
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
                .map_err(|e| unreachable(format!("{e:#}")))?;
            anyhow::ensure!(
                root.kind == DIR,
                "{} is not a folder",
                src.def.root.display()
            );
            // The same folder reached another way (a junction, a mapped drive, a bind mount)
            // would count every file in it twice.
            if let Some(other) = root
                .fs_id
                .as_deref()
                .and_then(|id| src.folder_elsewhere(id))
            {
                anyhow::bail!(
                    "{} is a folder source {other} already indexes, reached another way",
                    src.def.root.display()
                );
            }
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
                // A link root indexed before 0.7.0 stored the link's own id.
                let link_id = || {
                    (src.def.root.to_local_path())
                        .is_some_and(|p| crate::fsid::root_ids(&p).contains(&was))
                };
                if *now_id != was && !link_id() {
                    return Err(Offline(
                        OfflineReason::RootMismatch,
                        format!(
                            "{} is a different folder than the one indexed",
                            src.def.root.display()
                        ),
                    )
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
                prune: false,
                deep: true,
                batch: 0,
                batch_started: None,
                done: 1,
                total: total as u64,
                current: String::new(),
                progress: &report,
                cancel,
            };
            walk.begin()?;
            let (id, _) = upsert(
                &conn,
                gen as i64,
                gen as i64,
                None,
                "",
                &root,
                &root_fs,
                &|_| true,
                &|_, _, _| false,
            )?;
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
                return Err(Offline(
                    OfflineReason::Empty,
                    format!(
                        "{} is empty but had {} entries",
                        src.def.root.display(),
                        total - 1
                    ),
                )
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
            tx.execute(
                "DELETE FROM meta WHERE key IN ('adopt_root', 'offline_reason')",
                [],
            )?;
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
        drop(pending);
        match result {
            Ok(done) => {
                // Released in the store by the walk's last transaction.
                src.release_held();
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
                } else if let Some(Offline(reason, _)) = e.downcast_ref::<Offline>() {
                    // A different folder or an empty root holds the source until a walk
                    // finds the indexed folder again (or adopts the new one).
                    if *reason != OfflineReason::Unreachable {
                        if let Err(e) = src.hold(Some(*reason)) {
                            tracing::warn!("hold {}: {e:#}", src.def.label);
                        }
                    }
                    SourceStatus::Offline {
                        last_seen: last_walk,
                        reason: src.held().unwrap_or(*reason),
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
    /// Until that walk the source stays held: changes are not applied to the old snapshot.
    pub fn adopt_root(src: &Source) -> Result<()> {
        src.store.set_meta("adopt_root", "1")
    }

    /// Reconciles one local path with the store: stats it and inserts, updates, moves (by
    /// identity) or removes its record. A folder that appears from outside is indexed with
    /// its contents. Apply the events of a batch for paths that exist before those that do
    /// not, so a rename is seen as a move rather than a delete + insert.
    /// Refused while the source is held (a different folder or an empty root is at its
    /// root): that folder's changes do not belong in the snapshot.
    pub fn apply_change(src: &Source, ev: ChangeEvent) -> Result<()> {
        let path = match ev {
            ChangeEvent::Changed(p) | ChangeEvent::Removed(p) => p,
            ChangeEvent::Rescan => anyhow::bail!("a rescan needs Indexer::full_walk"),
        };
        Self::apply_paths(src, std::slice::from_ref(&path), false)
    }

    /// `apply_change` for several local paths of `src` in one transaction, in order (present
    /// paths before vanished ones, so a rename reads as a move). `walk_existing` also walks
    /// folders that were already indexed (a copy merged into them), adding what is new.
    pub(crate) fn apply_paths(src: &Source, paths: &[VPath], walk_existing: bool) -> Result<()> {
        if let Some(reason) = src.held() {
            anyhow::bail!(
                "{} is held offline ({reason:?}): changes are not applied",
                src.def.label
            );
        }
        let mut at = Vec::with_capacity(paths.len());
        for path in paths {
            let rel = src.relative(path).with_context(|| {
                format!("{} is outside source {}", path.display(), src.def.label)
            })?;
            let local = path
                .to_local_path()
                .context("changes apply to local sources (remote sources are polled)")?;
            at.push((path.clone(), rel, At::Local(local)));
        }
        let never = AtomicBool::new(false);
        apply_batch(src, &Lister::Local, &at, &[], walk_existing, &never).map(|_| ())
    }

    /// Applies one page of a remote source's change feed as local watcher events are
    /// applied: a created or modified path is stat'ed (before any lock: a network call)
    /// and inserted or updated, a removed one deleted with what is below it, and an
    /// `Unknown` folder listed again with its subtree; one protection recount follows
    /// when the index changed. Paths outside the source are skipped. Ok(false): only a
    /// full walk catches up (the source is held, its root changed in a way the feed cannot
    /// name, or an `Unknown` folder is not indexed).
    pub(crate) fn apply_feed(
        src: &Source,
        provider: &Arc<dyn Provider>,
        changes: &[ChangedPath],
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if src.held().is_some() {
            return Ok(false);
        }
        let root = &src.def.root;
        let conn = src.store.get()?;
        let mut at = Vec::new();
        let mut relist = Vec::new();
        // Paths stat'ed in this page (their state now: once is enough).
        let mut stated = HashSet::new();
        for c in changes {
            // The provider's path, at the address the source reaches it by.
            let path = VPath {
                path: c.path.path.clone(),
                ..root.clone()
            };
            let Some(rel) = src.relative(&path) else {
                // A folder above the source's root: the root may have changed.
                if c.kind == ChangeKind::Unknown && crate::library::relative(&path, root).is_some()
                {
                    return Ok(false);
                }
                continue;
            };
            match c.kind {
                // The root itself gone or unknown: a walk says whether it is offline.
                ChangeKind::Removed | ChangeKind::Unknown if rel.is_empty() => return Ok(false),
                ChangeKind::Unknown => relist.push((rel, true)),
                ChangeKind::Removed => {
                    stated.remove(&rel);
                    at.push((path, rel, At::Remote(None)));
                }
                ChangeKind::Created | ChangeKind::Modified => {
                    // In a folder the index does not have yet: that folder (the first one
                    // missing on the way down) is applied instead, and walked.
                    let rel = first_missing(&conn, &rel, src.nocase())?;
                    if !stated.insert(rel.clone()) {
                        continue;
                    }
                    let path = src.absolute(&rel);
                    let item = match provider.stat(&path) {
                        Ok(e) => Some(item_of(e)),
                        Err(e) if is_not_found(&e) && !rel.is_empty() => None,
                        Err(e) => return Err(e),
                    };
                    at.push((path, rel, At::Remote(item)));
                }
            }
        }
        drop(conn);
        if at.is_empty() && relist.is_empty() {
            return Ok(true);
        }
        let lister = Lister::Remote(provider.clone());
        apply_batch(src, &lister, &at, &relist, false, cancel)
    }

    /// One cheap poll of a remote source whose folder times move when entries come and go
    /// (SFTP): every indexed folder is stat'ed and the ones whose time moved are listed
    /// again (vanished entries removed, new ones added, new folders walked). Changes inside
    /// a file do not move its folder's time: the full walk every `reconcile` finds those.
    /// Ok(false): only a full walk catches up (the source is held).
    /// ponytail: one stat after another; tens of thousands of folders want them pipelined.
    pub(crate) fn poll_folders(
        src: &Source,
        provider: &Arc<dyn Provider>,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if src.held().is_some() {
            return Ok(false);
        }
        let folders: Vec<(String, Option<i64>)> = src
            .store
            .get()?
            .prepare("SELECT path, mtime FROM record WHERE kind = ?1 AND flags & ?2 = 0")?
            .query_map(params![DIR, LINK], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut moved = Vec::new();
        for (rel, was) in folders {
            if cancel.load(Ordering::Relaxed) || src.removed.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            match provider.stat(&src.absolute(&rel)) {
                Ok(e) => {
                    let now = e.modified.map(fsid::unix_ns);
                    if now != was {
                        moved.push((rel, now));
                    }
                }
                // Gone: the time of the folder it was in moved too.
                Err(e) if is_not_found(&e) && !rel.is_empty() => {}
                Err(e) => return Err(e),
            }
        }
        if moved.is_empty() {
            return Ok(true);
        }
        moved.sort();
        let relist: Vec<(String, bool)> =
            moved.iter().map(|(rel, _)| (rel.clone(), false)).collect();
        let lister = Lister::Remote(provider.clone());
        if !apply_batch(src, &lister, &[], &relist, false, cancel)? {
            return Ok(false);
        }
        // Each folder's time as it was before its listing: a change made meanwhile moves
        // it again. Written after the batch: a time is no reason to recount.
        let c = src.store.get()?;
        for (rel, now) in moved {
            c.execute(
                "UPDATE record SET mtime = ?2 WHERE path = ?1 AND kind = ?3",
                params![rel, now, DIR],
            )?;
        }
        Ok(true)
    }

    /// `watch_with` with the defaults and the source's own poll interval.
    pub fn watch(src: &Arc<Source>, router: &Arc<Router>) -> Result<WatchHandle> {
        Self::watch_with(src, router, WatchConfig::default().for_source(src))
    }

    /// Keeps a source current until the handle is dropped, starting with a full walk (what
    /// changed while nobody watched). Local sources: a recursive notify watcher, debounced
    /// (500 ms quiet, 5 s at most), applied with `apply_change`; lost events (a rescan flag or
    /// a watcher error) and every `cfg.reconcile` trigger a full walk. Other sources, every
    /// `cfg.poll`: the provider's change feed from the cursor taken before the last walk
    /// (applied like local events), else the times of the indexed folders where they track
    /// their entries (SFTP), with a full walk every `cfg.reconcile`, or when the feed
    /// refuses its cursor or names a change it cannot place; a source with neither is
    /// walked every `cfg.walk`. A completed walk recounts the protection counters; applied
    /// changes recount them 5 s after the last one.
    pub fn watch_with(
        src: &Arc<Source>,
        router: &Arc<Router>,
        cfg: WatchConfig,
    ) -> Result<WatchHandle> {
        Self::watch_hooked(
            src,
            router,
            cfg,
            Box::new(|src| {
                if let Some(lib) = src.owner.read().upgrade() {
                    crate::protect::after_walk(&lib, src);
                }
            }),
        )
    }

    /// `watch_with`, calling `after_walk` after each completed full walk.
    pub(crate) fn watch_hooked(
        src: &Arc<Source>,
        router: &Arc<Router>,
        cfg: WatchConfig,
        after_walk: Box<dyn Fn(&Source) + Send>,
    ) -> Result<WatchHandle> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        let (src, stop) = (src.clone(), cancel.clone());
        let walker = router.clone();
        let rescan = move |src: &Source| match Indexer::full_walk(src, &walker, &|_| {}, &stop) {
            Ok(()) => after_walk(src),
            Err(e) => tracing::warn!("re-walk of {}: {e:#}", src.def.label),
        };
        let Some(root) = src.def.root.to_local_path() else {
            let (router, quit) = (router.clone(), cancel.clone());
            let thread = std::thread::Builder::new()
                .name("keel-poll".into())
                .spawn(move || poll_loop(&src, &router, cfg, &stop_rx, &quit, &rescan))?;
            return Ok(WatchHandle {
                cancel,
                stop: stop_tx,
                watcher: None,
                thread: Some(thread),
            });
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        notify::Watcher::watch(&mut watcher, &root, notify::RecursiveMode::Recursive)
            .with_context(|| format!("watch {}", root.display()))?;
        let quit = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("keel-watch-source".into())
            .spawn(move || {
                drop(stop_rx);
                watch_loop(&src, &rx, cfg.reconcile, &quit, &rescan);
            })?;
        Ok(WatchHandle {
            cancel,
            stop: stop_tx,
            watcher: Some(watcher),
            thread: Some(thread),
        })
    }
}

/// How `Indexer::watch_with` keeps a source current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// Remote and cloud sources: asked what changed this often.
    pub poll: Duration,
    /// Remote sources with no change feed whose folder times do not track their entries:
    /// a full walk this often (or every `poll`, when that is longer).
    pub walk: Duration,
    /// A full walk this often on top of change events, which can be lost without notice
    /// (a Windows change buffer overflow is not reported; a feed can miss what it cannot
    /// place).
    pub reconcile: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            poll: POLL_INTERVAL,
            walk: WALK_INTERVAL,
            reconcile: RECONCILE_INTERVAL,
        }
    }
}

impl WatchConfig {
    /// A source's own `poll_secs` (when it has one) for both `poll` and `walk`.
    pub fn for_source(mut self, src: &Source) -> Self {
        if let Some(secs) = src.def.poll_secs {
            self.poll = Duration::from_secs(secs);
            self.walk = self.poll;
        }
        self
    }
}

/// How a remote source is kept current between its full walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Between {
    /// `Provider::changes` from the cursor saved with the last walk.
    Feed,
    /// `Indexer::poll_folders`.
    FolderTimes,
    /// Nothing: a full walk every `WatchConfig::walk` (or `poll`, when that is longer).
    Walks,
}

/// The remote poll loop: a full walk first (with a feed cursor taken just before it),
/// then every `cfg.poll` the feed or the folder times, and a full walk again every
/// `cfg.reconcile` (`cfg.walk` without either), at once when the feed asks for one, and
/// at the next poll after a walk that failed (an offline source stays offline until then).
/// Ends when `quit` is set, the source is removed or `stop` disconnects.
fn poll_loop(
    src: &Source,
    router: &Router,
    cfg: WatchConfig,
    stop: &crossbeam_channel::Receiver<()>,
    quit: &AtomicBool,
    rescan: &dyn Fn(&Source),
) {
    let mut between = Between::Walks;
    let mut walk_at = Instant::now();
    while !quit.load(Ordering::SeqCst) && !src.removed.load(Ordering::SeqCst) {
        let provider = router.provider_for(&src.def.root);
        if Instant::now() >= walk_at {
            // The cursor is taken before the walk: what changes while it runs comes again.
            let mut cursor = None;
            if let Some(p) = &provider {
                between = match p.changes(None, quit) {
                    Ok(feed) => {
                        cursor = Some(feed.cursor);
                        Between::Feed
                    }
                    Err(e) if e.downcast_ref::<FeedError>().is_some() => {
                        match p.folder_times_track_entries() {
                            true => Between::FolderTimes,
                            false => Between::Walks,
                        }
                    }
                    Err(e) => {
                        tracing::debug!("change feed of {}: {e:#}", src.def.label);
                        Between::Walks
                    }
                };
            }
            let generation = src.generation.load(Ordering::SeqCst);
            rescan(src);
            if src.generation.load(Ordering::SeqCst) != generation {
                if let Err(e) = save_cursor(src, cursor.as_ref()) {
                    tracing::warn!("change cursor of {}: {e:#}", src.def.label);
                }
                walk_at = Instant::now()
                    + match between {
                        Between::Walks => cfg.walk.max(cfg.poll),
                        Between::Feed | Between::FolderTimes => cfg.reconcile,
                    };
            } else {
                walk_at = Instant::now() + cfg.poll;
            }
        } else if let Some(p) = provider {
            let caught_up = match between {
                Between::Feed => follow_feed(src, &p, quit),
                Between::FolderTimes => Indexer::poll_folders(src, &p, quit),
                Between::Walks => Ok(true),
            };
            match caught_up {
                Ok(true) => {}
                Ok(false) => walk_at = Instant::now(),
                // The cursor stays: asked again at the next poll.
                Err(e) => tracing::debug!("changes of {}: {e:#}", src.def.label),
            }
        }
        let next = (Instant::now() + cfg.poll).min(walk_at);
        while Instant::now() < next {
            if src.removed.load(Ordering::SeqCst) {
                return;
            }
            match stop.recv_timeout(TICK) {
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
    }
}

fn save_cursor(src: &Source, cursor: Option<&ChangeCursor>) -> Result<()> {
    match cursor {
        Some(c) => src.store.set_meta(CURSOR, &c.0),
        None => {
            (src.store.get()?).execute("DELETE FROM meta WHERE key = ?1", [CURSOR])?;
            Ok(())
        }
    }
}

/// Applies a remote source's change feed from its saved cursor, page by page (the cursor
/// saved after each). Ok(false): only a walk catches up (no cursor, a refused cursor, or
/// `Indexer::apply_feed` says so). An error keeps the cursor for the next poll.
fn follow_feed(src: &Source, provider: &Arc<dyn Provider>, cancel: &AtomicBool) -> Result<bool> {
    let Some(saved) = src.store.meta(CURSOR)? else {
        return Ok(false);
    };
    let mut cursor = ChangeCursor(saved);
    loop {
        let feed = match provider.changes(Some(cursor.clone()), cancel) {
            Err(e) if e.downcast_ref::<FeedError>().is_some() => return Ok(false),
            feed => feed?,
        };
        if !Indexer::apply_feed(src, provider, &feed.changes, cancel)? {
            return Ok(false);
        }
        if feed.cursor != cursor {
            src.store.set_meta(CURSOR, &feed.cursor.0)?;
        }
        if !feed.more {
            return Ok(true);
        }
        cursor = feed.cursor;
    }
}

/// Quiet time before a burst of change events is applied.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// Longest a change waits while events keep coming.
const MAX_DELAY: Duration = Duration::from_secs(5);
/// How often the watch loop looks at its stop flag.
const TICK: Duration = Duration::from_millis(250);

/// The local watcher's loop: an initial full walk, then debounced change events, with a full
/// walk whenever events were lost and every `reconcile`. Ends when the event channel closes
/// or `quit` is set.
fn watch_loop(
    src: &Source,
    rx: &crossbeam_channel::Receiver<notify::Result<notify::Event>>,
    reconcile: Duration,
    quit: &AtomicBool,
    rescan: &dyn Fn(&Source),
) {
    rescan(src);
    // The library's own folder (inside this source when Keel's data folder is): its writes,
    // a recount saving its counters among them, are not changes to index.
    let own: Vec<std::path::PathBuf> = src
        .store_dir()
        .parent()
        .and_then(std::path::Path::parent)
        .map(|dir| {
            let canon = std::fs::canonicalize(dir)
                .map(|c| std::path::PathBuf::from(c.to_string_lossy().trim_start_matches(r"\\?\")));
            [Ok(dir.to_owned()), canon].into_iter().flatten().collect()
        })
        .unwrap_or_default();
    let mut next_walk = Instant::now() + reconcile;
    let mut pending: Vec<std::path::PathBuf> = Vec::new();
    let mut lost = false;
    // (first, last) event of the current burst.
    let mut burst: Option<(Instant, Instant)> = None;
    while !quit.load(Ordering::SeqCst) && !src.removed.load(Ordering::SeqCst) {
        match rx.recv_timeout(TICK) {
            Ok(res) => {
                let now = Instant::now();
                burst = Some(burst.map_or((now, now), |(first, _)| (first, now)));
                match res {
                    Ok(ev) => {
                        lost |= ev.need_rescan();
                        pending.extend(ev.paths);
                    }
                    Err(e) => {
                        tracing::debug!("watch {}: {e}", src.def.label);
                        lost = true;
                    }
                }
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
        let due = burst.is_some_and(|(first, last)| {
            last.elapsed() >= DEBOUNCE || first.elapsed() >= MAX_DELAY
        });
        if lost || Instant::now() >= next_walk {
            if burst.is_none() || due {
                rescan(src);
                next_walk = Instant::now() + reconcile;
                (lost, burst) = (false, None);
                pending.clear();
            }
            continue;
        }
        if !due {
            continue;
        }
        burst = None;
        let mut paths = std::mem::take(&mut pending);
        if src.held().is_some() {
            // Another folder is at the root: its events stay out of the snapshot.
            continue;
        }
        // Watchers may report the canonical spelling of the root (macOS FSEvents turns
        // /var/... into /private/var/...; a mounted or junctioned root likewise). Map those
        // back under the source's own root so they are not taken for outside paths.
        if let Some(root_local) = src.def.root.to_local_path() {
            if let Ok(canon) = std::fs::canonicalize(&root_local) {
                let canon =
                    std::path::PathBuf::from(canon.to_string_lossy().trim_start_matches(r"\?\"));
                if canon != root_local {
                    for p in paths.iter_mut() {
                        if !p.starts_with(&root_local) {
                            if let Ok(rest) = p.strip_prefix(&canon) {
                                *p = root_local.join(rest);
                            }
                        }
                    }
                }
            }
        }
        paths.retain(|p| !own.iter().any(|dir| p.starts_with(dir)));
        paths.sort();
        paths.dedup();
        // Present paths first: a rename then reads as a move.
        paths.sort_by_key(|p| p.symlink_metadata().is_err());
        for p in paths {
            let ev = ChangeEvent::Changed(VPath::local(&p));
            if let Err(e) = Indexer::apply_change(src, ev) {
                tracing::debug!("index change {}: {e:#}", p.display());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply(
    c: &Connection,
    src: &Source,
    ignore: &Gitignore,
    gen: i64,
    path: &VPath,
    rel: &str,
    at: &At,
    walk_existing: bool,
) -> Result<Option<Pending>> {
    let nocase = src.nocase();
    let existing = resolve(c, rel, nocase)?;
    let stat = match at {
        At::Local(local) => fsid::stat(local),
        At::Remote(Some(item)) => Ok(item.clone()),
        At::Remote(None) => Err(std::io::ErrorKind::NotFound.into()),
    };
    let mut item = match stat {
        Ok(item) => item,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some((id, _)) = existing {
                delete_subtree(c, id)?;
            }
            return Ok(None);
        }
        Err(e) => {
            if let Some((id, _)) = existing {
                c.execute(
                    "UPDATE record SET flags = flags | ?3, error = ?2 WHERE id = ?1",
                    params![id, e.to_string(), UNREADABLE],
                )?;
            }
            return Ok(None);
        }
    };
    if rel.is_empty() {
        // The root itself: keep its record, refresh its metadata.
        let Some((id, fs_id)) = existing else {
            return Ok(None);
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
            &|_| true,
            &|_, _, _| false,
        )?;
        return Ok(None);
    }
    let (parent_rel, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    item.name = name.to_owned();
    // An event can carry the old casing of a case-only rename: take the name on disk.
    if let (true, false, At::Local(local)) = (nocase, item.link, at) {
        let real = std::fs::canonicalize(local)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        if let Some(real) = real.filter(|r| r.to_lowercase() == name.to_lowercase()) {
            item.name = real;
        }
    }
    let real_name = item.name.clone();
    let real_rel = match parent_rel {
        "" => real_name.clone(),
        parent => format!("{parent}/{real_name}"),
    };
    let (name, rel) = (real_name.as_str(), real_rel.as_str());
    // A parent that is not indexed (hidden, ignored, never walked): nothing to do.
    let Some((parent_id, parent_fs)) = resolve(c, parent_rel, nocase)? else {
        return Ok(None);
    };
    let excluded = (item.hidden && !src.def.include_hidden)
        || ignore
            .matched_path_or_any_parents(rel, item.kind == DIR)
            .is_ignore();
    if excluded {
        if let Some((id, _)) = existing {
            delete_subtree(c, id)?;
        }
        return Ok(None);
    }
    let fs_id = item
        .fs_id
        .clone()
        .unwrap_or_else(|| name_hash(&parent_fs, name));
    // The native ids in the parent folder, listed only when a name is contested.
    let listed: std::cell::OnceCell<HashSet<String>> = std::cell::OnceCell::new();
    let dir = match at {
        At::Local(local) => local.parent(),
        At::Remote(_) => None,
    };
    let in_folder = |id: &str| {
        listed
            .get_or_init(|| {
                dir.and_then(|dir| fsid::list(dir).ok())
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i.fs_id)
                    .collect()
            })
            .contains(id)
    };
    let (id, outcome) = upsert(
        c,
        gen,
        i64::MAX,
        Some(parent_id),
        rel,
        &item,
        &fs_id,
        &|old| !in_folder(old),
        &still_at(src),
    )?;
    // Whatever else sat at this path was replaced, unless it was renamed within the folder
    // (its own event moves it).
    let others: Vec<(i64, String)> = c
        .prepare_cached(
            "SELECT id, fs_id FROM record WHERE parent = ?1 AND name = ?2 AND id != ?3",
        )?
        .query_map(params![parent_id, name, id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (other, other_fs) in others {
        if other_fs.starts_with("h:") || !in_folder(&other_fs) {
            delete_subtree(c, other)?;
        }
    }
    // A new folder's contents (or all of a folder a copy merged into): walked by the caller
    // after this transaction.
    let walk = outcome == Outcome::Inserted || walk_existing;
    Ok((item.kind == DIR && !item.link && walk).then(|| Pending {
        dir: path.clone(),
        id,
        rel: rel.to_owned(),
        fs_id,
    }))
}

/// Applies `at` in order in one transaction, then walks the folders that appeared (all
/// folders with `walk_existing`) and lists each `relist` folder again (pruning what is
/// gone; `true`: its whole subtree), in batches like a full walk. One protection recount
/// follows when the index changed. Ok(false): a `relist` folder is not indexed.
fn apply_batch(
    src: &Source,
    lister: &Lister,
    at: &[(VPath, String, At)],
    relist: &[(String, bool)],
    walk_existing: bool,
    cancel: &AtomicBool,
) -> Result<bool> {
    let ignore = matcher(&src.def.ignore)?;
    let w = src.write.lock();
    let gen = match src.pending_gen.load(Ordering::SeqCst) {
        0 => src.generation.load(Ordering::SeqCst),
        pending => pending,
    } as i64;
    let conn = src.store.get()?;
    let changes = conn.total_changes();
    let result = (|| -> Result<bool> {
        begin_immediate(&conn)?;
        let applied = at
            .iter()
            .map(|(path, rel, at)| apply(&conn, src, &ignore, gen, path, rel, at, walk_existing))
            .collect::<Result<Vec<_>>>();
        conn.execute_batch(if applied.is_ok() {
            "COMMIT"
        } else {
            "ROLLBACK"
        })?;
        // (folder, prune it, walk all of it)
        let mut walks: Vec<(Pending, bool, bool)> = (applied?.into_iter().flatten())
            .map(|p| (p, false, true))
            .collect();
        let mut found = true;
        for (rel, deep) in relist {
            match folder_at(&conn, rel, src.nocase())? {
                Some((id, fs_id)) => walks.push((
                    Pending {
                        dir: src.absolute(rel),
                        id,
                        rel: rel.clone(),
                        fs_id,
                    },
                    true,
                    *deep,
                )),
                None => found = false,
            }
        }
        // A folder that appeared is walked in batches like a full walk (no long lock).
        for (folder, prune, deep) in walks {
            let mut walk = Walk {
                src,
                lister,
                ignore: &ignore,
                conn: &conn,
                gen,
                unseen_below: i64::MAX,
                batched: true,
                prune,
                deep,
                batch: 0,
                batch_started: None,
                done: 0,
                total: 0,
                current: String::new(),
                progress: &|_| {},
                cancel,
            };
            let walked = walk.run(vec![folder]);
            walk.commit()?;
            walked?;
        }
        Ok(found)
    })();
    drop(w);
    // Only a change to the index is worth a recount (an ignored path, or a file whose
    // record is unchanged, is not: a recount must never set off the next one).
    if conn.total_changes() != changes {
        if let Some(lib) = src.owner.read().upgrade() {
            crate::protect::schedule_recount(&lib, src);
        }
    }
    result
}

/// `BEGIN IMMEDIATE`, retried while another writer keeps the store busy.
fn begin_immediate(c: &Connection) -> Result<()> {
    let mut attempt = 1;
    loop {
        match c.execute_batch("BEGIN IMMEDIATE") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy && attempt < BEGIN_ATTEMPTS =>
            {
                attempt += 1;
            }
            r => return Ok(r?),
        }
    }
}

/// Stops watching when dropped (waits for an in-flight change or walk to stop).
pub struct WatchHandle {
    cancel: Arc<AtomicBool>,
    stop: crossbeam_channel::Sender<()>,
    watcher: Option<notify::RecommendedWatcher>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        // Dropping the watcher closes the event channel.
        self.watcher.take();
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
