//! SQLite stores: versioned migrations from `schema.sql` and a small connection pool.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use std::{
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const SCHEMA: &str = include_str!("schema.sql");
/// A pooled connection's page cache (16 MB).
pub(crate) const CACHE_SIZE: &str = "PRAGMA cache_size=-16384;";
/// Idle connections kept per store; more are opened on demand and closed when returned.
const MAX_IDLE: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    Library,
    Source,
}

impl Store {
    fn tag(self) -> &'static str {
        match self {
            Store::Library => "library",
            Store::Source => "source",
        }
    }
}

/// `(version, sql)` for `store`, ascending.
fn migrations(store: Store) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut current: Option<usize> = None;
    for line in SCHEMA.lines() {
        if let Some(header) = line.strip_prefix("-- @") {
            let mut parts = header.split_whitespace();
            let (db, version) = (parts.next(), parts.next().and_then(|v| v.parse().ok()));
            current = match (db, version) {
                (Some(db), Some(v)) if db == store.tag() => {
                    out.push((v, String::new()));
                    Some(out.len() - 1)
                }
                _ => None,
            };
        } else if let Some(i) = current {
            out[i].1.push_str(line);
            out[i].1.push('\n');
        }
    }
    out
}

/// WAL everywhere; `library.db` (jobs, checkpoints, op log) syncs every commit, source stores
/// (rebuildable by a walk) only at checkpoints.
/// Retries `f` while SQLite reports the database busy without waiting itself (switching a new
/// database to WAL, starting a write while another connection recovers the WAL), up to 5 s.
fn retry_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            r => return r,
        }
    }
}

fn configure(conn: &Connection, store: Store) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    retry_busy(|| conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())))?;
    let sync = match store {
        Store::Library => "FULL",
        Store::Source => "NORMAL",
    };
    conn.execute_batch(&format!("PRAGMA synchronous={sync}; {CACHE_SIZE}"))?;
    Ok(())
}

/// Brings `conn` to the newest version of `store`.
/// All pending migrations run in one write transaction that also reads the version, so two
/// processes opening a store at once never both migrate it.
fn migrate(conn: &Connection, store: Store) -> Result<()> {
    let conn: &Connection = conn;
    let tx = retry_busy(move || {
        rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
    })?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let all = migrations(store);
    let newest = all.last().map_or(0, |(v, _)| *v);
    anyhow::ensure!(
        version <= newest,
        "{} store is version {version}, newer than this Keel ({newest})",
        store.tag()
    );
    for (v, sql) in all.into_iter().filter(|(v, _)| *v > version) {
        tx.execute_batch(&sql)
            .with_context(|| format!("{} migration {v}", store.tag()))?;
        set_meta(&tx, "schema_version", &v.to_string())?;
        tx.pragma_update(None, "user_version", v)?;
    }
    tx.commit()?;
    Ok(())
}

/// A mutex-guarded set of connections to one database (WAL, synchronous=NORMAL, 5 s busy
/// timeout). Cheap to clone; `get` never waits for other users (SQLite does the locking).
#[derive(Clone)]
pub(crate) struct Pool(Arc<Inner>);

struct Inner {
    path: PathBuf,
    store: Store,
    idle: Mutex<Vec<Connection>>,
}

impl Pool {
    /// Opens (creating if needed) and migrates the database at `path`.
    pub(crate) fn open(path: &Path, store: Store) -> Result<Pool> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        configure(&conn, store)?;
        migrate(&conn, store)?;
        Ok(Pool(Arc::new(Inner {
            path: path.to_owned(),
            store,
            idle: Mutex::new(vec![conn]),
        })))
    }

    pub(crate) fn get(&self) -> Result<Conn<'_>> {
        let conn = match self.0.idle.lock().pop() {
            Some(conn) => conn,
            None => {
                let conn = Connection::open(&self.0.path)?;
                configure(&conn, self.0.store)?;
                conn
            }
        };
        Ok(Conn {
            pool: &self.0,
            conn: Some(conn),
        })
    }

    pub(crate) fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .get()?
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub(crate) fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.get()?;
        set_meta(&conn, key, value)
    }

    /// Closes every idle connection (so the files can be deleted on Windows).
    pub(crate) fn close_idle(&self) {
        self.0.idle.lock().clear();
    }
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        [key, value],
    )?;
    Ok(())
}

/// A pooled connection; returned to the pool on drop.
pub(crate) struct Conn<'a> {
    pool: &'a Inner,
    conn: Option<Connection>,
}

impl Deref for Conn<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection present until drop")
    }
}

impl DerefMut for Conn<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("connection present until drop")
    }
}

impl Drop for Conn<'_> {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else { return };
        // A connection left inside a transaction (a panic mid-write) is not reused.
        if conn.is_autocommit() {
            let mut idle = self.pool.idle.lock();
            if idle.len() < MAX_IDLE {
                idle.push(conn);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_stores_migrate_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        for store in [Store::Library, Store::Source] {
            let path = dir.path().join(format!("{}.db", store.tag()));
            let newest = migrations(store).last().unwrap().0;
            let pool = Pool::open(&path, store).unwrap();
            assert_eq!(
                pool.meta("schema_version").unwrap(),
                Some(newest.to_string())
            );
            drop(pool);
            // Reopening runs nothing (the CREATEs would fail on existing tables).
            let pool = Pool::open(&path, store).unwrap();
            let mode: String = pool
                .get()
                .unwrap()
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            let sync: i64 = pool
                .get()
                .unwrap()
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sync, if store == Store::Library { 2 } else { 1 });
        }
    }

    #[test]
    fn concurrent_opens_migrate_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    start.wait();
                    Pool::open(&path, Store::Source).unwrap();
                });
            }
        });
        let newest = migrations(Store::Source).last().unwrap().0;
        let pool = Pool::open(&path, Store::Source).unwrap();
        assert_eq!(
            pool.meta("schema_version").unwrap(),
            Some(newest.to_string())
        );
    }

    #[test]
    fn newer_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        drop(Pool::open(&path, Store::Source).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 999)
            .unwrap();
        let err = Pool::open(&path, Store::Source).err().unwrap();
        assert!(err.to_string().contains("newer"), "{err}");
    }

    #[test]
    fn pool_reuses_and_grows() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::open(&dir.path().join("l.db"), Store::Library).unwrap();
        let conns: Vec<_> = (0..6).map(|_| pool.get().unwrap()).collect();
        for c in &conns {
            c.execute(
                "INSERT INTO op_log(ts, kind, payload, result) VALUES (0, 'x', '{}', 'ok')",
                [],
            )
            .unwrap();
        }
        drop(conns);
        assert_eq!(pool.0.idle.lock().len(), MAX_IDLE);
        let n: i64 = pool
            .get()
            .unwrap()
            .query_row("SELECT count(*) FROM op_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 6);
    }

    #[test]
    fn fts_follows_record_through_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::open(&dir.path().join("s.db"), Store::Source).unwrap();
        let c = pool.get().unwrap();
        c.execute_batch(
            "INSERT INTO record(id, name, path, kind, fs_id, gen)
                 VALUES (1, 'Café.txt', 'docs/Café.txt', 0, 'a', 1);",
        )
        .unwrap();
        let hits = |q: &str| -> i64 {
            c.query_row(
                "SELECT count(*) FROM record_fts WHERE record_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hits("cafe"), 1, "diacritics folded");
        c.execute(
            "UPDATE record SET name = 'menu.txt', path = 'docs/menu.txt' WHERE id = 1",
            [],
        )
        .unwrap();
        assert_eq!(hits("menu"), 1);
        assert_eq!(hits("cafe"), 0);
        c.execute("DELETE FROM record WHERE id = 1", []).unwrap();
        assert_eq!(hits("menu"), 0);
    }

    /// Review items 3, 15, 17: a version-4 store moves to nanosecond times, restarts its
    /// content ids and never reuses a record id again; records, links and FTS stay.
    #[test]
    fn a_version_4_store_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let conn = Connection::open(&path).unwrap();
            for (v, sql) in migrations(Store::Source)
                .into_iter()
                .filter(|(v, _)| *v <= 4)
            {
                conn.execute_batch(&sql).unwrap();
                conn.pragma_update(None, "user_version", v).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO record(id, parent, name, path, kind, size, mtime, ctime, fs_id, gen,
                     cas_id, sampled_hash)
                     VALUES (1, NULL, 'r', '', 1, 0, 1700000000, 5, 'r', 1, NULL, NULL),
                            (7, 1, 'Café.txt', 'Café.txt', 0, 3, 1700000000, 5, 'f', 1,
                             x'01', x'01');
                 INSERT INTO record_tag(record, tag) VALUES (7, 2);",
            )
            .unwrap();
        }
        let pool = Pool::open(&path, Store::Source).unwrap();
        let c = pool.get().unwrap();
        let row: (i64, bool) = c
            .query_row(
                "SELECT mtime, ctime IS NULL AND cas_id IS NULL AND sampled_hash IS NULL
                 FROM record WHERE id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (1_700_000_000_000_000_000, true));
        let links: i64 = c
            .query_row("SELECT count(*) FROM record_tag", [], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 1);
        let fts: i64 = c
            .query_row(
                "SELECT count(*) FROM record_fts WHERE record_fts MATCH 'cafe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 1);
        let counts: (i64, i64) = c
            .query_row("SELECT records, files FROM counts", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(counts, (2, 1));
        // The highest id is gone: the next record still gets a new one.
        c.execute("DELETE FROM record WHERE id = 7", []).unwrap();
        c.execute(
            "INSERT INTO record(parent, name, path, kind, fs_id, gen) VALUES (1, 'n', 'n', 0, 'n', 1)",
            [],
        )
        .unwrap();
        assert_eq!(c.last_insert_rowid(), 8);
        let counts: (i64, i64) = c
            .query_row("SELECT records, files FROM counts", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(counts, (2, 1), "counters follow");
    }
}
