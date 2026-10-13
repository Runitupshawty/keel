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

/// The newest schema version of `store` this Keel knows.
pub(crate) fn newest(store: Store) -> u32 {
    migrations(store).last().map_or(0, |(v, _)| *v)
}

/// A trigger or view: `(type, name, sql)`.
pub(crate) type CodeObject = (String, String, Option<String>);

/// The triggers and views in `conn`, by name.
pub(crate) fn code_objects(conn: &Connection) -> Result<Vec<CodeObject>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, sql FROM sqlite_master WHERE type IN ('trigger', 'view') ORDER BY name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The triggers and views (`code_objects`) a store of `store` holds at `version`, made by
/// this Keel's migrations up to it in memory; None for a version it never had.
pub(crate) fn reference_objects(store: Store, version: u32) -> Result<Option<Vec<CodeObject>>> {
    let all = migrations(store);
    if !all.iter().any(|(v, _)| *v == version) {
        return Ok(None);
    }
    let conn = Connection::open_in_memory()?;
    for (_, sql) in all.iter().filter(|(v, _)| *v <= version) {
        conn.execute_batch(sql)?;
    }
    code_objects(&conn).map(Some)
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
        let io = crate::hash::StoreIo::enter();
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
            _io: io,
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
    // Dropped after the connection is back in the pool.
    _io: crate::hash::StoreIo,
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

    /// Branches that each added a migration must not share (or reorder) a version number:
    /// the runner applies them in file order and takes the last as the newest.
    #[test]
    fn migration_versions_ascend() {
        for store in [Store::Library, Store::Source] {
            let versions: Vec<u32> = migrations(store).iter().map(|m| m.0).collect();
            assert!(
                versions.windows(2).all(|w| w[0] < w[1]),
                "{}: {versions:?}",
                store.tag()
            );
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

    /// Task 62: a version-11 store (0.21.0) gains the text tables; its records stay, and a
    /// record leaving the index takes its text along (body, index and status row).
    #[test]
    fn a_version_11_store_gains_text_inside_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let conn = Connection::open(&path).unwrap();
            for (v, sql) in migrations(Store::Source)
                .into_iter()
                .filter(|(v, _)| *v <= 11)
            {
                conn.execute_batch(&sql).unwrap();
                conn.pragma_update(None, "user_version", v).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO record(id, parent, name, path, kind, size, fs_id, gen)
                     VALUES (1, NULL, 'r', '', 1, 0, 'r', 1),
                            (2, 1, 'a.txt', 'a.txt', 0, 5, 'f', 1),
                            (3, 1, 'b.txt', 'b.txt', 0, 5, 'g', 1);",
            )
            .unwrap();
        }
        let pool = Pool::open(&path, Store::Source).unwrap();
        let c = pool.get().unwrap();
        let version: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(version >= 12);
        let n: i64 = c
            .query_row("SELECT count(*) FROM record", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        for (id, text) in [(2, "Keel Test Document"), (3, "another Keel page")] {
            c.execute(
                "INSERT INTO record_text(record, mtime, size, chars, status) VALUES (?1, 0, 5, 9, 0)",
                [id],
            )
            .unwrap();
            c.execute(
                "INSERT INTO record_text_body(record, text) VALUES (?1, ?2)",
                rusqlite::params![id, text],
            )
            .unwrap();
            c.execute(
                "INSERT INTO record_content(rowid, text) VALUES (?1, ?2)",
                rusqlite::params![id, text],
            )
            .unwrap();
        }
        let hits = |w: &str| -> i64 {
            c.query_row(
                "SELECT count(*) FROM record_content WHERE record_content MATCH ?1",
                [w],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hits("keel"), 2);
        c.execute("DELETE FROM record WHERE id = 2", []).unwrap();
        assert_eq!(hits("keel"), 1);
        assert_eq!(hits("document"), 0);
        let left: (i64, i64) = c
            .query_row(
                "SELECT (SELECT count(*) FROM record_text), (SELECT count(*) FROM record_text_body)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(left, (1, 1));
        let ok: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        c.execute(
            "INSERT INTO record_content(record_content, rank) VALUES ('integrity-check', 1)",
            [],
        )
        .unwrap();
    }

    /// Freezes: a version-13 store keeps its records, all of them in a first freeze, and a
    /// change after it copies the old row for that freeze.
    #[test]
    fn a_version_13_store_gains_freezes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let conn = Connection::open(&path).unwrap();
            for (v, sql) in migrations(Store::Source)
                .into_iter()
                .filter(|(v, _)| *v <= 13)
            {
                conn.execute_batch(&sql).unwrap();
                conn.pragma_update(None, "user_version", v).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO record(id, parent, name, path, kind, size, fs_id, gen)
                     VALUES (1, NULL, 'r', '', 1, 0, 'r', 1),
                            (2, 1, 'a.txt', 'a.txt', 0, 5, 'f', 1);",
            )
            .unwrap();
        }
        let pool = Pool::open(&path, Store::Source).unwrap();
        let c = pool.get().unwrap();
        let version: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(version >= 14);
        let since: i64 = c
            .query_row("SELECT sum(since) FROM record", [], |r| r.get(0))
            .unwrap();
        assert_eq!(since, 0, "old rows belong to every freeze");
        c.execute_batch(
            "INSERT INTO freeze(at, records, files, bytes) VALUES (0, 2, 1, 5);
             UPDATE record SET size = 9 WHERE id = 2;",
        )
        .unwrap();
        let frozen: (i64, i64, i64) = c
            .query_row("SELECT lo, hi, size FROM frozen WHERE id = 2", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(frozen, (1, 1, 5));
    }

    /// Stores written by v0.7.0 (library v6, source v7; migrations never change once released)
    /// reach the newest versions with their rows intact.
    #[test]
    fn v0_7_0_stores_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let old = |store: Store, upto: u32, rows: &str| {
            let path = dir.path().join(format!("{}.db", store.tag()));
            let conn = Connection::open(&path).unwrap();
            for (v, sql) in migrations(store).into_iter().filter(|(v, _)| *v <= upto) {
                conn.execute_batch(&sql).unwrap();
                conn.pragma_update(None, "user_version", v).unwrap();
            }
            conn.execute_batch(rows).unwrap();
            path
        };
        let lib = old(
            Store::Library,
            6,
            "INSERT INTO source(id, def, created, volume_id) VALUES ('s1', '{}', 0, 'v1');
             INSERT INTO volume(id, label, kind, domain, domain_set, backup)
                 VALUES ('v1', 'C:', 'fixed', 'disk:a', 'disk:mine', 1);",
        );
        let src = old(
            Store::Source,
            7,
            "INSERT INTO record(id, parent, name, path, kind, size, fs_id, gen, cas_id, drift)
                 VALUES (1, NULL, 'r', '', 1, 0, 'r', 1, NULL, NULL),
                        (2, 1, 'a.txt', 'a.txt', 0, 5, 'f', 1, x'aa', 1700000000);",
        );
        let pool = Pool::open(&lib, Store::Library).unwrap();
        let newest = migrations(Store::Library).last().unwrap().0;
        let version: u32 = pool
            .get()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, newest);
        let row: (String, String, bool) = pool
            .get()
            .unwrap()
            .query_row(
                "SELECT s.volume_id, v.domain_set, v.backup FROM source s JOIN volume v
                     ON v.id = s.volume_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("v1".into(), "disk:mine".into(), true));

        let pool = Pool::open(&src, Store::Source).unwrap();
        let newest = migrations(Store::Source).last().unwrap().0;
        assert!(newest >= 8);
        let version: u32 = pool
            .get()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, newest);
        let row: (Vec<u8>, i64, Option<Vec<u8>>) = pool
            .get()
            .unwrap()
            .query_row(
                "SELECT cas_id, drift, remote_cas FROM record WHERE id = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (vec![0xaa], 1_700_000_000, None));
    }

    /// Task 76: saved views of a version-8 library become library bookmarks at the top level,
    /// in creation order, with the default sort, columns, match toggles and no shortcut.
    #[test]
    fn version_8_views_become_bookmarks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            for (v, sql) in migrations(Store::Library)
                .into_iter()
                .filter(|(v, _)| *v <= 8)
            {
                conn.execute_batch(&sql).unwrap();
                conn.pragma_update(None, "user_version", v).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO view(id, name, query, layout) VALUES (3, 'PDFs', 'ext:pdf', 'grid'),
                     (5, 'Big', 'size:>1g', 'details');",
            )
            .unwrap();
        }
        let pool = Pool::open(&path, Store::Library).unwrap();
        let c = pool.get().unwrap();
        let mut stmt = c
            .prepare(&format!(
                "SELECT {} FROM view ORDER BY folder, position, id",
                crate::tags::VIEW_COLUMNS
            ))
            .unwrap();
        let views: Vec<crate::View> = stmt
            .query_map([], crate::tags::view_of)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let want = |id, name: &str, query: &str, layout: &str| crate::View {
            id,
            name: name.into(),
            query: query.into(),
            layout: layout.into(),
            position: id,
            ..crate::View::default()
        };
        assert_eq!(
            views,
            [
                want(3, "PDFs", "ext:pdf", "grid"),
                want(5, "Big", "size:>1g", "details")
            ]
        );
        assert_eq!(views[0].backend, crate::ViewBackend::Library);
    }
}
