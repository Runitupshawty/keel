//! Library sync between paired devices (spec 2.10): tags (name, color, nesting), tag
//! assignments, favorites and what a device knows of its files' content ids.
//!
//! Every local change appends an entry to `sync_log` (library schema 8): its sequence
//! number, its Lamport time and what changed ([`SyncOp`]). A peer pulls the entries after
//! the last sequence number it read ([`Library::sync_page`]) and applies them
//! ([`Library::sync_apply`]); keel-net carries the pages between devices that opted in.
//! Each key (a tag by its stable id, an assignment by tag and target, a content id by
//! device, source and path) keeps the change with the highest (Lamport time, device id):
//! last writer wins, the same on every device. Deletes are tombstones kept 30 days.
//!
//! Sync is pairwise: a device serves only its own changes, never what it received, so
//! three devices sync when each pair has the switch on. An assignment's target is a
//! content id (BLAKE3) when the file is hashed, else a path in one of the logging
//! device's own sources: a peer's path targets only ever land in this library's sources
//! of that peer (`node://<peer>/<source>/...`), never in a local folder.

use crate::library::Source;
use crate::tags::{TagId, FAVORITES};
use crate::{Library, RecordRef};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
};

/// Favorites' stable id, the same in every library.
pub const FAVORITES_UID: &str = "favorites";
/// Most entries one page carries.
pub const SYNC_PAGE: usize = 1_000;
/// Most bytes (as JSON) one page carries, well under keel-net's 1 MiB header limit.
const PAGE_BYTES: usize = 640 * 1024;
/// How long deletes are remembered (seconds).
pub const TOMBSTONE_SECS: i64 = 30 * 24 * 3600;
/// Lamport times above this are refused (no device gets near it; it keeps `+ 1` safe).
pub const MAX_LAMPORT: u64 = 1 << 53;
/// A received Lamport time moves this device's clock at most this far, so a device that
/// sent a huge one cannot push this device's own changes past `MAX_LAMPORT` (where every
/// other device would refuse them).
const LAMPORT_FOLLOW: u64 = MAX_LAMPORT / 2;
const MAX_NAME: usize = 256;
const MAX_COLOR: usize = 64;
const MAX_PATH: usize = 4096;

/// One change, as served to a peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncEntry {
    /// Its place in the serving device's log.
    pub seq: u64,
    /// Whose change it is. Informational on the wire: the receiver uses the device the
    /// connection proves.
    pub device: String,
    pub lamport: u64,
    pub op: SyncOp,
}

/// What changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncOp {
    /// A tag as it is now (created, renamed, recolored or nested); `parent` by stable id.
    Tag {
        uid: String,
        name: String,
        color: Option<String>,
        parent: Option<String>,
    },
    TagDeleted {
        uid: String,
    },
    /// A tag (Favorites: [`FAVORITES_UID`]) put on (`on`) or taken off a target.
    Assign {
        tag: String,
        target: SyncTarget,
        on: bool,
    },
    /// The logging device's file at `path` in its source `source` has content id `cas`
    /// (hex): its path-keyed assignments also reach copies of that content.
    Content {
        source: String,
        path: String,
        cas: String,
    },
}

/// What an assignment is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncTarget {
    /// Every file with this content id (64 hex digits).
    Content(String),
    /// A file or folder of the logging device: its source id and the path in it.
    Path { source: String, path: String },
}

/// One page of a device's log (`Library::sync_page`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPage {
    pub entries: Vec<SyncEntry>,
    /// More entries follow: ask again from `upto`.
    pub more: bool,
    /// The sequence number the page reaches (the next pull starts after it).
    pub upto: u64,
}

/// What `Library::sync_apply` did with a page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncApplied {
    /// Entries that won their key and were applied.
    pub applied: usize,
    /// Entries older than what this library has (or already applied).
    pub stale: usize,
    /// Entries refused (malformed or oversized).
    pub rejected: usize,
}

/// How far a device's log was read (`Library::sync_peers`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPeer {
    pub device: String,
    pub seq: u64,
    /// The last completed pull (unix seconds).
    pub last_sync: Option<i64>,
    /// Entries applied from it, in all.
    pub received: u64,
}

/// No control or direction-override characters, at most `max` bytes.
fn plain(s: &str, max: usize) -> bool {
    s.len() <= max
        && !s.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
}

fn valid_uid(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn valid_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Device and source ids: ASCII letters, digits, `-` and `_`, at most 128.
pub fn valid_device(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A relative slash-separated path ("" = the source's root) with no `.`, `..` or empty
/// component.
fn valid_path(p: &str) -> bool {
    p.is_empty()
        || (p.len() <= MAX_PATH
            && p.split('/').all(|c| {
                !c.is_empty() && c != "." && c != ".." && !c.contains('\\') && plain(c, MAX_PATH)
            }))
}

/// Whether `name` can be a tag name received from a device: trimmed, not empty, at most
/// 256 bytes, no control or direction-override characters.
pub fn valid_tag_name(name: &str) -> bool {
    !name.trim().is_empty() && name.trim() == name && plain(name, MAX_NAME)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

impl SyncOp {
    fn check(&self) -> Result<()> {
        use SyncOp::*;
        let ok = match self {
            Tag {
                uid,
                name,
                color,
                parent,
            } => {
                valid_uid(uid)
                    && uid != FAVORITES_UID
                    && valid_tag_name(name)
                    && color.as_deref().is_none_or(|c| plain(c, MAX_COLOR))
                    && parent
                        .as_deref()
                        .is_none_or(|p| valid_uid(p) && p != FAVORITES_UID && p != uid)
            }
            TagDeleted { uid } => valid_uid(uid) && uid != FAVORITES_UID,
            Assign { tag, target, .. } => {
                valid_uid(tag)
                    && match target {
                        SyncTarget::Content(h) => valid_hex(h),
                        SyncTarget::Path { source, path } => {
                            valid_device(source) && valid_path(path)
                        }
                    }
            }
            Content { source, path, cas } => {
                valid_device(source) && valid_path(path) && valid_hex(cas)
            }
        };
        ensure!(ok, "invalid sync entry");
        Ok(())
    }

    /// The key it competes for. `device` is whose namespace a path is in ("" = this
    /// device).
    fn key(&self, device: &str) -> String {
        match self {
            SyncOp::Tag { uid, .. } | SyncOp::TagDeleted { uid } => format!("tag:{uid}"),
            SyncOp::Assign { tag, target, .. } => match target {
                SyncTarget::Content(h) => format!("as:{tag}:c:{h}"),
                SyncTarget::Path { source, path } => {
                    format!("as:{tag}:p:{device}:{source}:{path}")
                }
            },
            SyncOp::Content { source, path, .. } => format!("ct:{device}:{source}:{path}"),
        }
    }

    /// False for tombstones (a deleted tag, an assignment taken off).
    fn live(&self) -> bool {
        match self {
            SyncOp::TagDeleted { .. } => false,
            SyncOp::Assign { on, .. } => *on,
            SyncOp::Tag { .. } | SyncOp::Content { .. } => true,
        }
    }
}

/// A local change to log: its key, what changed, and whether peers are served it (a
/// change to another device's file without a content id stays here).
pub(crate) struct Change {
    key: String,
    op: SyncOp,
    serve: bool,
}

impl Change {
    fn local(op: SyncOp) -> Change {
        Change {
            key: op.key(""),
            op,
            serve: true,
        }
    }
}

/// A `node://<device>/<source>/<sub>` root: (device, source, sub).
fn node_root(src: &Source) -> Option<(String, String, String)> {
    let root = &src.def.root;
    if root.scheme != "node" {
        return None;
    }
    let rest = root.path.trim_matches('/');
    let (source, sub) = rest.split_once('/').unwrap_or((rest, ""));
    (!source.is_empty()).then(|| (root.authority.clone(), source.to_owned(), sub.to_owned()))
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{a}/{b}"),
    }
}

impl Library {
    /// This device's id (the keel-net node's), which breaks ties between devices' changes.
    pub fn set_sync_device(&self, id: &str) -> Result<()> {
        if self.shared.db.meta("sync_device")?.as_deref() != Some(id) {
            self.shared.db.set_meta("sync_device", id)?;
        }
        Ok(())
    }

    fn sync_device(&self) -> Result<String> {
        Ok(self.shared.db.meta("sync_device")?.unwrap_or_default())
    }

    pub(crate) fn tag_uid(&self, id: TagId) -> Result<String> {
        self.shared
            .db
            .get()?
            .query_row("SELECT uid FROM tag WHERE id = ?1", [id], |r| r.get(0))
            .with_context(|| format!("no tag {id}"))
    }

    /// The tag as it is now, as a change to log.
    pub(crate) fn tag_change(&self, id: TagId) -> Result<Change> {
        let c = self.shared.db.get()?;
        let op = c.query_row(
            "SELECT t.uid, t.name, t.color, p.uid FROM tag t LEFT JOIN tag p ON p.id = t.parent
             WHERE t.id = ?1",
            [id],
            |r| {
                Ok(SyncOp::Tag {
                    uid: r.get(0)?,
                    name: r.get(1)?,
                    color: r.get(2)?,
                    parent: r.get(3)?,
                })
            },
        )?;
        Ok(Change::local(op))
    }

    pub(crate) fn tag_deleted(&self, uid: String) -> Change {
        Change::local(SyncOp::TagDeleted { uid })
    }

    /// The assignment changes for records of `src` whose tag `uid` was put on or taken
    /// off: by content id when the file is hashed, else by path. A file of another
    /// device's source without a content id is kept by that device's path and never
    /// served (a device only sends paths in its own sources).
    pub(crate) fn assign_changes(
        &self,
        uid: &str,
        src: &Source,
        records: &[i64],
        on: bool,
    ) -> Result<Vec<Change>> {
        let node = if src.is_device() {
            node_root(src)
        } else {
            None
        };
        let c = src.store.get()?;
        let mut stmt =
            c.prepare_cached("SELECT path, cas_id, remote_cas FROM record WHERE id = ?1")?;
        let mut out = Vec::new();
        for id in records {
            type Row = (String, Option<Vec<u8>>, Option<Vec<u8>>);
            let row: Option<Row> = stmt
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            let Some((path, cas, remote_cas)) = row else {
                continue;
            };
            // A device source's content ids are that device's word (`remote_cas`).
            let cas = if src.is_device() { remote_cas } else { cas };
            let (target, device, serve) = match (cas, &node) {
                (Some(h), _) if h.len() == 32 => {
                    (SyncTarget::Content(hex(&h)), String::new(), true)
                }
                (_, Some((device, source, sub))) => (
                    SyncTarget::Path {
                        source: source.clone(),
                        path: join(sub, &path),
                    },
                    device.clone(),
                    false,
                ),
                _ if src.is_device() => continue,
                _ => (
                    SyncTarget::Path {
                        source: src.id.0.clone(),
                        path,
                    },
                    String::new(),
                    true,
                ),
            };
            let op = SyncOp::Assign {
                tag: uid.to_owned(),
                target,
                on,
            };
            out.push(Change {
                key: op.key(&device),
                op,
                serve,
            });
        }
        Ok(out)
    }

    /// Appends local changes to the log (one Lamport tick each) and makes them their keys'
    /// winners. ponytail: the edit and its log entry are two transactions; a crash between
    /// them leaves that one edit unsynced until it is made again.
    pub(crate) fn log_sync(&self, changes: Vec<Change>) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut c = self.shared.db.get()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut lamport: u64 = tx
            .query_row("SELECT value FROM meta WHERE key = 'lamport'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let ts = crate::now();
        {
            let mut log = tx.prepare_cached(
                "INSERT INTO sync_log(lamport, key, op, live, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut win = tx.prepare_cached(
                "INSERT OR REPLACE INTO sync_key(key, lamport, device, op, live, ts)
                 VALUES (?1, ?2, '', ?3, ?4, ?5)",
            )?;
            for ch in changes {
                lamport += 1;
                let op = serde_json::to_string(&ch.op)?;
                let live = ch.op.live();
                if ch.serve {
                    log.execute(params![lamport as i64, ch.key, op, live, ts])?;
                }
                win.execute(params![ch.key, lamport as i64, op, live, ts])?;
            }
        }
        crate::db::set_meta(&tx, "lamport", &lamport.to_string())?;
        tx.commit()?;
        Ok(())
    }

    /// Forgets the assignments of a deleted tag (its tombstone stands for them).
    pub(crate) fn forget_assignments(&self, uid: &str) -> Result<()> {
        let (from, to) = (format!("as:{uid}:"), format!("as:{uid};"));
        let c = self.shared.db.get()?;
        c.execute(
            "DELETE FROM sync_key WHERE key >= ?1 AND key < ?2",
            [&from, &to],
        )?;
        c.execute(
            "DELETE FROM sync_log WHERE key >= ?1 AND key < ?2",
            [&from, &to],
        )?;
        Ok(())
    }

    /// Once per library: logs the tags and assignments that existed before sync, so a
    /// device that syncs later gets them too.
    pub(crate) fn sync_seed(&self) -> Result<()> {
        if self.shared.db.meta("sync_seeded")?.is_some() {
            return Ok(());
        }
        let mut changes = Vec::new();
        // Parents before their children.
        let mut tags = self.tags()?;
        let mut done: HashSet<TagId> = HashSet::new();
        while !tags.is_empty() {
            let i = tags
                .iter()
                .position(|t| t.parent.is_none_or(|p| done.contains(&p)))
                .unwrap_or(0);
            let t = tags.remove(i);
            done.insert(t.id);
            changes.push(self.tag_change(t.id)?);
        }
        let uids: std::collections::HashMap<TagId, String> = {
            let c = self.shared.db.get()?;
            let mut stmt = c.prepare("SELECT id, uid FROM tag")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        for src in &sources {
            let links: Vec<(i64, TagId)> = {
                let c = src.store.get()?;
                let mut stmt =
                    c.prepare("SELECT record, tag FROM record_tag ORDER BY tag, record")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for group in links.chunk_by(|a, b| a.1 == b.1) {
                let Some(uid) = uids.get(&group[0].1) else {
                    continue;
                };
                let ids: Vec<i64> = group.iter().map(|l| l.0).collect();
                changes.extend(self.assign_changes(uid, src, &ids, true)?);
            }
        }
        self.log_sync(changes)?;
        self.shared.db.set_meta("sync_seeded", "1")
    }

    /// Drops log entries a later local entry of the same key replaced, and tombstones (in
    /// the log and as winners) older than `TOMBSTONE_SECS`.
    pub(crate) fn sync_prune(&self) -> Result<()> {
        self.sync_prune_before(crate::now() - TOMBSTONE_SECS)
    }

    pub(crate) fn sync_prune_before(&self, cutoff: i64) -> Result<()> {
        let mut c = self.shared.db.get()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM sync_log WHERE seq NOT IN (SELECT max(seq) FROM sync_log GROUP BY key)",
            [],
        )?;
        tx.execute("DELETE FROM sync_log WHERE live = 0 AND ts < ?1", [cutoff])?;
        tx.execute("DELETE FROM sync_key WHERE live = 0 AND ts < ?1", [cutoff])?;
        tx.commit()?;
        Ok(())
    }

    /// This device's log after `since`: at most `limit` entries (and `SYNC_PAGE`), fewer
    /// when they would not fit a page. Their `device` is "" (the caller names this
    /// device).
    pub fn sync_page(&self, since: u64, limit: usize) -> Result<SyncPage> {
        self.note_content()?;
        let limit = limit.clamp(1, SYNC_PAGE);
        let c = self.shared.db.get()?;
        let mut stmt = c.prepare_cached(
            "SELECT seq, lamport, op FROM sync_log WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let since_row = i64::try_from(since).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![since_row, limit as i64 + 1], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut page = SyncPage {
            upto: since,
            ..SyncPage::default()
        };
        let mut bytes = 0;
        for row in rows {
            let (seq, lamport, op) = row?;
            bytes += op.len() + 64;
            if page.entries.len() == limit || (bytes > PAGE_BYTES && !page.entries.is_empty()) {
                page.more = true;
                break;
            }
            // An entry this version cannot read is skipped (a newer version's kind).
            if let Ok(op) = serde_json::from_str(&op) {
                page.entries.push(SyncEntry {
                    seq: seq as u64,
                    device: String::new(),
                    lamport: lamport as u64,
                    op,
                });
            }
            page.upto = seq as u64;
        }
        Ok(page)
    }

    /// Where the next pull from `device` starts (its last sequence number read).
    pub fn sync_since(&self, device: &str) -> Result<u64> {
        let seq: Option<i64> = self
            .shared
            .db
            .get()?
            .query_row(
                "SELECT seq FROM sync_peer WHERE device = ?1",
                [device],
                |r| r.get(0),
            )
            .optional()?;
        Ok(seq.unwrap_or(0).max(0) as u64)
    }

    /// Every device whose log this library read.
    pub fn sync_peers(&self) -> Result<Vec<SyncPeer>> {
        let c = self.shared.db.get()?;
        let mut stmt =
            c.prepare("SELECT device, seq, last_sync, received FROM sync_peer ORDER BY device")?;
        let rows = stmt.query_map([], |r| {
            Ok(SyncPeer {
                device: r.get(0)?,
                seq: r.get::<_, i64>(1)? as u64,
                last_sync: r.get(2)?,
                received: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Applies a page pulled from `device` (its id as the connection proved it): each
    /// entry that wins its key goes through the same code as a local edit (without being
    /// logged again), then `device`'s position moves to `page.upto`.
    pub fn sync_apply(&self, device: &str, page: &SyncPage) -> Result<SyncApplied> {
        ensure!(valid_device(device), "invalid device id");
        let me = self.sync_device()?;
        ensure!(device != me, "a device does not sync with itself");
        ensure!(
            page.entries.len() <= SYNC_PAGE,
            "too many entries in one page"
        );
        let mut done = SyncApplied::default();
        let mut tags_changed = false;
        for e in &page.entries {
            if e.lamport > MAX_LAMPORT || e.op.check().is_err() {
                done.rejected += 1;
                continue;
            }
            let key = e.op.key(device);
            let c = self.shared.db.get()?;
            c.execute(
                "INSERT INTO meta(key, value) VALUES ('lamport', ?1) ON CONFLICT(key) DO
                 UPDATE SET value = max(CAST(value AS INTEGER), CAST(excluded.value AS INTEGER))",
                [e.lamport.min(LAMPORT_FOLLOW).to_string()],
            )?;
            let winner: Option<(i64, String)> = c
                .query_row(
                    "SELECT lamport, device FROM sync_key WHERE key = ?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            // An assignment of a deleted tag stays deleted.
            let dead_tag = match &e.op {
                SyncOp::Assign { tag, .. } => {
                    c.query_row(
                        "SELECT live FROM sync_key WHERE key = ?1",
                        [format!("tag:{tag}")],
                        |r| r.get::<_, bool>(0),
                    )
                    .optional()?
                        == Some(false)
                }
                _ => false,
            };
            let wins = !dead_tag
                && winner.is_none_or(|(lamport, d)| {
                    let d = if d.is_empty() {
                        me.as_str()
                    } else {
                        d.as_str()
                    };
                    (e.lamport, device) > (lamport as u64, d)
                });
            if !wins {
                done.stale += 1;
                continue;
            }
            c.execute(
                "INSERT OR REPLACE INTO sync_key(key, lamport, device, op, live, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    key,
                    e.lamport as i64,
                    device,
                    serde_json::to_string(&e.op)?,
                    e.op.live(),
                    crate::now()
                ],
            )?;
            drop(c);
            tags_changed |= self.apply_op(device, &e.op)?;
            done.applied += 1;
        }
        if tags_changed {
            self.fix_parents()?;
            self.mirror_all()?;
        }
        let revision = self.protection_revision();
        let recounted = self.shared.sync_reapplied.swap(revision, Ordering::SeqCst) != revision;
        if done.applied > 0 || recounted {
            self.sync_reapply()?;
        }
        self.shared.db.get()?.execute(
            "INSERT INTO sync_peer(device, seq, last_sync, received) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device) DO UPDATE SET seq = max(seq, excluded.seq),
                 last_sync = excluded.last_sync, received = received + excluded.received",
            params![
                device,
                i64::try_from(page.upto).unwrap_or(i64::MAX),
                crate::now(),
                done.applied as i64
            ],
        )?;
        Ok(done)
    }

    /// The local tag a stable id names (its own, or the one it was merged into).
    fn resolve_uid(&self, uid: &str) -> Result<Option<TagId>> {
        Ok(self
            .shared
            .db
            .get()?
            .query_row(
                "SELECT id FROM tag WHERE uid = ?1
                 UNION ALL SELECT tag FROM tag_alias WHERE uid = ?1 AND tag IN (SELECT id FROM tag)
                 LIMIT 1",
                [uid],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// `name`, or `name (2)`, `name (3)`… when a sibling under `parent` other than
    /// `except` has it.
    fn free_name(
        &self,
        name: &str,
        parent: Option<TagId>,
        except: Option<TagId>,
    ) -> Result<String> {
        let c = self.shared.db.get()?;
        let taken = |n: &str| -> Result<bool> {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM tag WHERE coalesce(parent, 0) = coalesce(?1, 0)
                     AND name = ?2 COLLATE NOCASE AND id <> coalesce(?3, 0))",
                params![parent, n, except],
                |r| r.get(0),
            )?)
        };
        let mut candidate = name.to_owned();
        let mut n = 2;
        while taken(&candidate)? {
            candidate = format!("{name} ({n})");
            n += 1;
        }
        Ok(candidate)
    }

    /// One winning remote change. True when the tag definitions changed.
    fn apply_op(&self, device: &str, op: &SyncOp) -> Result<bool> {
        match op {
            SyncOp::Tag {
                uid,
                name,
                color,
                parent,
            } => {
                let parent = match parent {
                    Some(p) => self.resolve_uid(p)?.filter(|p| *p != FAVORITES),
                    None => None,
                };
                match self.resolve_uid(uid)? {
                    Some(FAVORITES) => Ok(false),
                    Some(id) => {
                        let parent = match parent {
                            Some(p) if self.tag_tree(id)?.contains(&p) => None,
                            p => p,
                        };
                        let name = self.free_name(name, parent, Some(id))?;
                        self.shared.db.get()?.execute(
                            "UPDATE tag SET name = ?2, color = ?3, parent = ?4 WHERE id = ?1",
                            params![id, name, color, parent],
                        )?;
                        Ok(true)
                    }
                    None => {
                        let c = self.shared.db.get()?;
                        let same: Option<TagId> = c
                            .query_row(
                                "SELECT id FROM tag WHERE coalesce(parent, 0) = coalesce(?1, 0)
                                     AND name = ?2 COLLATE NOCASE AND id <> ?3",
                                params![parent, name, FAVORITES],
                                |r| r.get(0),
                            )
                            .optional()?;
                        match same {
                            // The same tag made on both devices: one tag here.
                            Some(id) => c.execute(
                                "INSERT OR REPLACE INTO tag_alias(uid, tag) VALUES (?1, ?2)",
                                params![uid, id],
                            )?,
                            None => c.execute(
                                "INSERT INTO tag(name, color, parent, uid) VALUES (?1, ?2, ?3, ?4)",
                                params![name, color, parent, uid],
                            )?,
                        };
                        Ok(true)
                    }
                }
            }
            SyncOp::TagDeleted { uid } => {
                let id = self.resolve_uid(uid)?;
                self.shared
                    .db
                    .get()?
                    .execute("DELETE FROM tag_alias WHERE uid = ?1", [uid])?;
                // Its assignments go with it (';' sorts right after ':').
                let c = self.shared.db.get()?;
                c.execute(
                    "DELETE FROM sync_key WHERE key >= ?1 AND key < ?2",
                    [format!("as:{uid}:"), format!("as:{uid};")],
                )?;
                drop(c);
                match id {
                    Some(id) if id != FAVORITES => {
                        if let Err(e) = self.delete_tag_rows(id) {
                            tracing::warn!("sync: deleting tag {id}: {e:#}");
                        }
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            SyncOp::Assign { tag, target, on } => {
                // A tag not known yet: applied once it arrives (`sync_reapply`).
                if let Some(id) = self.resolve_uid(tag)? {
                    let records = self.records_for(device, target)?;
                    self.set_tag_rows(id, &records, *on)?;
                }
                Ok(false)
            }
            SyncOp::Content { .. } => Ok(false),
        }
    }

    /// The records here that a target of `device` names: copies of the content, or the
    /// file in this library's sources of that device (and copies of its content when the
    /// device said what it is).
    fn records_for(&self, device: &str, target: &SyncTarget) -> Result<Vec<RecordRef>> {
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        let by_content = |cas: &str, out: &mut Vec<RecordRef>| -> Result<()> {
            let blob = unhex(cas);
            for src in &sources {
                let c = src.store.get()?;
                let mut stmt = c.prepare_cached(if src.is_device() {
                    "SELECT id FROM record WHERE remote_cas = ?1"
                } else {
                    "SELECT id FROM record WHERE cas_id = ?1"
                })?;
                let ids = stmt.query_map([&blob], |r| r.get::<_, i64>(0))?;
                for id in ids {
                    out.push(RecordRef {
                        source: src.id.clone(),
                        id: id?,
                    });
                }
            }
            Ok(())
        };
        let mut out = Vec::new();
        match target {
            SyncTarget::Content(cas) => by_content(cas, &mut out)?,
            SyncTarget::Path { source, path } => {
                for src in sources.iter().filter(|s| s.is_device()) {
                    let Some((d, s, sub)) = node_root(src) else {
                        continue;
                    };
                    if d != device || &s != source {
                        continue;
                    }
                    let rel = if sub.is_empty() {
                        path.as_str()
                    } else if path == &sub {
                        ""
                    } else {
                        match path.strip_prefix(&sub).and_then(|r| r.strip_prefix('/')) {
                            Some(r) => r,
                            None => continue,
                        }
                    };
                    let c = src.store.get()?;
                    if let Some((id, _)) = crate::index::resolve(&c, rel, false)? {
                        out.push(RecordRef {
                            source: src.id.clone(),
                            id,
                        });
                    }
                }
                let known: Option<String> = self
                    .shared
                    .db
                    .get()?
                    .query_row(
                        "SELECT op FROM sync_key WHERE key = ?1 AND live = 1",
                        [format!("ct:{device}:{source}:{path}")],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(SyncOp::Content { cas, .. }) =
                    known.and_then(|op| serde_json::from_str(&op).ok())
                {
                    by_content(&cas, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    /// Received tags whose parent arrived after them are nested now.
    fn fix_parents(&self) -> Result<()> {
        let rows: Vec<String> = {
            let c = self.shared.db.get()?;
            let mut stmt = c.prepare(
                "SELECT op FROM sync_key WHERE key LIKE 'tag:%' AND live = 1 AND device <> ''",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for op in rows {
            let Ok(SyncOp::Tag {
                uid,
                parent: Some(parent),
                ..
            }) = serde_json::from_str(&op)
            else {
                continue;
            };
            let (Some(id), Some(p)) = (self.resolve_uid(&uid)?, self.resolve_uid(&parent)?) else {
                continue;
            };
            let (current, name): (Option<TagId>, String) = self.shared.db.get()?.query_row(
                "SELECT parent, name FROM tag WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if current != Some(p) && p != FAVORITES && !self.tag_tree(id)?.contains(&p) {
                let name = self.free_name(&name, Some(p), Some(id))?;
                self.shared.db.get()?.execute(
                    "UPDATE tag SET parent = ?2, name = ?3 WHERE id = ?1",
                    params![id, p, name],
                )?;
            }
        }
        Ok(())
    }

    /// Puts every received assignment that is on onto the records it names now (a tag or
    /// a copy that arrived, was indexed or was hashed after it). Returns how many tags
    /// were put on records. ponytail: goes over every received assignment; runs only after
    /// a page applied something or the library recounted (a walk or hashing ended).
    pub fn sync_reapply(&self) -> Result<usize> {
        let rows: Vec<(String, String)> = {
            let c = self.shared.db.get()?;
            let mut stmt = c.prepare(
                "SELECT device, op FROM sync_key WHERE key LIKE 'as:%' AND live = 1 AND device <> ''",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut added = 0;
        for (device, op) in rows {
            let Ok(SyncOp::Assign { tag, target, .. }) = serde_json::from_str(&op) else {
                continue;
            };
            let Some(id) = self.resolve_uid(&tag)? else {
                continue;
            };
            let records = self.records_for(&device, &target)?;
            added += self
                .set_tag_rows(id, &records, true)?
                .iter()
                .map(|(_, ids)| ids.len())
                .sum::<usize>();
        }
        Ok(added)
    }

    /// Logs the content id of local files whose tags are kept by path, once they are
    /// hashed (looked at when the library recounted since the last look).
    fn note_content(&self) -> Result<()> {
        let revision = self.protection_revision();
        if self.shared.sync_noted.swap(revision, Ordering::SeqCst) == revision {
            return Ok(());
        }
        let rows: Vec<String> = {
            let c = self.shared.db.get()?;
            let mut stmt = c.prepare(
                "SELECT op FROM sync_key WHERE key LIKE 'as:%:p::%' AND live = 1 AND device = ''",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut changes = Vec::new();
        let mut seen = HashSet::new();
        for op in rows {
            let Ok(SyncOp::Assign {
                target: SyncTarget::Path { source, path },
                ..
            }) = serde_json::from_str(&op)
            else {
                continue;
            };
            if !seen.insert((source.clone(), path.clone())) {
                continue;
            }
            let Some(src) = self
                .source(&crate::SourceId(source.clone()))
                .filter(|s| !s.is_device())
            else {
                continue;
            };
            let cas: Option<Vec<u8>> = {
                let c = src.store.get()?;
                match crate::index::resolve(&c, &path, src.nocase())? {
                    Some((id, _)) => {
                        c.query_row("SELECT cas_id FROM record WHERE id = ?1", [id], |r| {
                            r.get(0)
                        })?
                    }
                    None => None,
                }
            };
            let Some(cas) = cas.filter(|h| h.len() == 32).map(|h| hex(&h)) else {
                continue;
            };
            let op = SyncOp::Content { source, path, cas };
            let key = op.key("");
            let known: Option<String> = self
                .shared
                .db
                .get()?
                .query_row("SELECT op FROM sync_key WHERE key = ?1", [&key], |r| {
                    r.get(0)
                })
                .optional()?;
            if known != Some(serde_json::to_string(&op)?) {
                changes.push(Change {
                    key,
                    op,
                    serve: true,
                });
            }
        }
        self.log_sync(changes)
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
