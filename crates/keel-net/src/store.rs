use crate::{Grant, Link, Peer, PeerId};
use anyhow::{ensure, Result};
use iroh::{endpoint::Connection, EndpointAddr};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs::File, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub peer: Peer,
    pub addr: EndpointAddr,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Data {
    pub label: String,
    pub peers: Vec<Record>,
    pub grants: Vec<Grant>,
}
impl Default for Data {
    fn default() -> Self {
        Self {
            label: "Keel".into(),
            peers: Vec::new(),
            grants: Vec::new(),
        }
    }
}
pub(crate) struct Session {
    pub conn: Connection,
    pub cancel: CancellationToken,
}
/// In-memory view: the last committed `Data` plus runtime link status. Disk I/O
/// never happens while this is locked (see `Store`).
#[derive(Default)]
pub(crate) struct State {
    pub data: Data,
    pub sessions: HashMap<PeerId, HashMap<usize, Session>>,
    /// Serializes dialing per peer so concurrent requests share one connection.
    pub dialing: HashMap<PeerId, Arc<tokio::sync::Mutex<()>>>,
    pub closed: bool,
}
impl State {
    /// Installs freshly committed data, keeping runtime fields (link, storage,
    /// newest last_seen) of peers that are still paired.
    pub fn replace(&mut self, mut data: Data) {
        for record in &mut data.peers {
            let old = self.data.peers.iter().find(|r| r.peer.id == record.peer.id);
            record.peer.link = old.map_or(Link::Offline, |r| r.peer.link);
            if let Some(old) = old {
                record.peer.storage = old.peer.storage.clone();
                record.peer.last_seen = record.peer.last_seen.max(old.peer.last_seen);
            }
        }
        self.data = data;
    }
    pub fn last_seen(&self) -> Vec<(PeerId, Option<i64>)> {
        self.data
            .peers
            .iter()
            .map(|r| (r.peer.id, r.peer.last_seen))
            .collect()
    }
    /// Closes every session of `peer`. Returns true when this took the peer
    /// from online to offline (the caller emits `PeerOffline`).
    pub fn disconnect(&mut self, peer: &PeerId) -> bool {
        if let Some(sessions) = self.sessions.remove(peer) {
            for session in sessions.into_values() {
                session.cancel.cancel();
                session.conn.close(1u8.into(), b"authorization changed");
            }
        }
        self.data
            .peers
            .iter_mut()
            .find(|r| &r.peer.id == peer)
            .is_some_and(|r| std::mem::replace(&mut r.peer.link, Link::Offline) != Link::Offline)
    }
}

/// `<data_dir>/net/net.sqlite3`, owned exclusively through an OS lock on
/// `<data_dir>/net/LOCK` held for the node's lifetime. A second `Node::open`
/// on the same directory fails, so no other writer can exist.
pub(crate) struct Store {
    db: rusqlite::Connection,
    _lock: File,
}
impl Store {
    pub fn open(dir: &Path) -> Result<(Self, Data)> {
        let dir = dir.join("net");
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("LOCK"))?;
        ensure!(
            fs4::fs_std::FileExt::try_lock_exclusive(&lock)?,
            "this data directory is already in use by another Keel node"
        );
        let db = rusqlite::Connection::open(dir.join("net.sqlite3"))?;
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS state (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);")?;
        let mut data = read(&db)?;
        for record in &mut data.peers {
            record.peer.link = Link::Offline;
        }
        Ok((Self { db, _lock: lock }, data))
    }
    /// Re-reads the committed row inside `BEGIN IMMEDIATE`, applies `change`
    /// and commits, so a stale in-memory copy can never overwrite newer rows.
    /// Memory is replaced only after a durable commit (by the caller), so an
    /// I/O failure cannot silently grant extra access.
    pub fn update<T>(&mut self, change: impl FnOnce(&mut Data) -> Result<T>) -> Result<(Data, T)> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut data = read(&tx)?;
        let out = change(&mut data)?;
        tx.execute(
            "INSERT INTO state (id,data) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            [serde_json::to_string(&data)?],
        )?;
        tx.commit()?;
        Ok((data, out))
    }
}
fn read(db: &rusqlite::Connection) -> Result<Data> {
    let json: Option<String> = db
        .query_row("SELECT data FROM state WHERE id=1", [], |r| r.get(0))
        .optional()?;
    Ok(match json {
        Some(s) => serde_json::from_str(&s)?,
        None => Data::default(),
    })
}
/// Persists the newer of the stored and observed `last_seen` per peer.
pub(crate) fn merge_seen(data: &mut Data, seen: &[(PeerId, Option<i64>)]) {
    for record in &mut data.peers {
        if let Some((_, t)) = seen.iter().find(|(id, _)| *id == record.peer.id) {
            record.peer.last_seen = record.peer.last_seen.max(*t);
        }
    }
}
