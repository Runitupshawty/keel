use crate::{Grant, Peer, PeerId};
use anyhow::Result;
use iroh::{endpoint::Connection, EndpointAddr};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub peer: Peer,
    pub addr: EndpointAddr,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Data {
    pub label: String,
    pub peers: Vec<Record>,
    pub grants: Vec<Grant>,
}
pub(crate) struct Session {
    pub conn: Connection,
    pub cancel: CancellationToken,
}
pub(crate) struct State {
    db: rusqlite::Connection,
    pub data: Data,
    pub sessions: HashMap<PeerId, HashMap<usize, Session>>,
    pub closed: bool,
}
impl State {
    pub fn open(dir: &Path) -> Result<Self> {
        let dir = dir.join("net");
        std::fs::create_dir_all(&dir)?;
        let db = rusqlite::Connection::open(dir.join("net.sqlite3"))?;
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS state (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);")?;
        let json: Option<String> = db
            .query_row("SELECT data FROM state WHERE id=1", [], |r| r.get(0))
            .optional()?;
        let mut data: Data = match json {
            Some(s) => serde_json::from_str(&s)?,
            None => Data {
                label: "Keel".into(),
                ..Data::default()
            },
        };
        for record in &mut data.peers {
            record.peer.link = crate::Link::Offline;
        }
        Ok(Self {
            db,
            data,
            sessions: HashMap::new(),
            closed: false,
        })
    }
    /// A single SQLite statement is an atomic transaction. Memory is replaced only
    /// after durable commit, so an I/O failure cannot silently grant extra access.
    pub fn update(&mut self, change: impl FnOnce(&mut Data)) -> Result<()> {
        let mut data = self.data.clone();
        change(&mut data);
        let json = serde_json::to_string(&data)?;
        self.db.execute("INSERT INTO state (id,data) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data", [json])?;
        self.data = data;
        Ok(())
    }
    pub fn disconnect(&mut self, peer: &PeerId) {
        if let Some(sessions) = self.sessions.remove(peer) {
            for session in sessions.into_values() {
                session.cancel.cancel();
                session.conn.close(1u8.into(), b"authorization changed");
            }
        }
        if let Some(r) = self.data.peers.iter_mut().find(|r| &r.peer.id == peer) {
            r.peer.link = crate::Link::Offline;
        }
    }
}
