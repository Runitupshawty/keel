//! `<cache>/index/<serial>.db`: the index persisted in SQLite (WAL) so a restart
//! loads it and catches up from the journal instead of re-reading the MFT.
//!
//! The schema is versioned with `PRAGMA user_version`: 0 is the names-only index of
//! Keel 0.22 and older, 1 adds the metadata columns. An older db is migrated when it is
//! opened (the new columns start at zero until the next full read fills them); a db
//! from a newer Keel is refused, which makes the caller rebuild.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::index::{Index, Info};
use super::usn::Changes;

/// Where the journal left off when the db was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub journal_id: u64,
    pub next_usn: i64,
    pub root: u64,
    /// "C:" when the volume was indexed (letters can move; the serial names the file).
    pub volume: String,
    /// The metadata columns were read from the MFT (false for a names-only index, or
    /// one migrated from an older Keel: sizes and dates are zero until a rebuild).
    pub full: bool,
}

/// The schema version this Keel writes.
const VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files(
    frn INTEGER PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL, is_dir INTEGER NOT NULL,
    size INTEGER NOT NULL DEFAULT 0, allocated INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL DEFAULT 0, modified INTEGER NOT NULL DEFAULT 0,
    accessed INTEGER NOT NULL DEFAULT 0, changed INTEGER NOT NULL DEFAULT 0,
    attributes INTEGER NOT NULL DEFAULT 0, links INTEGER NOT NULL DEFAULT 0,
    streams INTEGER NOT NULL DEFAULT 0, reparse INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS meta(
    journal_id INTEGER NOT NULL, next_usn INTEGER NOT NULL, root INTEGER NOT NULL,
    volume TEXT NOT NULL, full INTEGER NOT NULL DEFAULT 0);
PRAGMA user_version = 1;";

/// Columns version 1 added to `files`.
const COLUMNS_V1: [&str; 10] = [
    "size",
    "allocated",
    "created",
    "modified",
    "accessed",
    "changed",
    "attributes",
    "links",
    "streams",
    "reparse",
];

const INSERT: &str = "INSERT OR REPLACE INTO files VALUES
    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Brings a db written by an older Keel up to [`VERSION`].
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > VERSION {
        bail!("index database version {version} is newer than this Keel's ({VERSION})");
    }
    if version == VERSION {
        return Ok(());
    }
    let mut batch = String::from("BEGIN;");
    for column in COLUMNS_V1 {
        batch.push_str(&format!(
            "ALTER TABLE files ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
        ));
    }
    batch.push_str("ALTER TABLE meta ADD COLUMN full INTEGER NOT NULL DEFAULT 0;");
    batch.push_str(&format!("PRAGMA user_version = {VERSION}; COMMIT;"));
    if let Err(e) = conn.execute_batch(&batch) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e).context("migrating the index database");
    }
    Ok(())
}

fn write_meta(conn: &Connection, meta: &Meta) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM meta", [])?;
    conn.execute(
        "INSERT INTO meta VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            meta.journal_id as i64,
            meta.next_usn,
            meta.root as i64,
            meta.volume,
            meta.full
        ],
    )?;
    Ok(())
}

fn insert_row(
    insert: &mut rusqlite::Statement<'_>,
    frn: u64,
    parent: u64,
    name: &str,
    is_dir: bool,
    info: &Info,
) -> rusqlite::Result<()> {
    insert.execute(params![
        frn as i64,
        parent as i64,
        name,
        is_dir,
        info.size as i64,
        info.allocated as i64,
        info.created,
        info.modified,
        info.accessed,
        info.changed,
        info.attributes,
        info.links,
        info.streams,
        info.reparse
    ])?;
    Ok(())
}

/// Writes the whole index to `<path>.tmp`, then swaps it in for `path`.
pub(crate) fn save_full(path: &Path, index: &Index, meta: &Meta) -> anyhow::Result<()> {
    let tmp = sibling(path, ".tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut conn = Connection::open(&tmp)?;
        conn.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;")?;
        conn.execute_batch(SCHEMA)?;
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare(INSERT)?;
            for (frn, parent, name, is_dir, info) in index.rows() {
                insert_row(&mut insert, frn, parent, name, is_dir, &info)?;
            }
        }
        write_meta(&tx, meta)?;
        tx.commit()?;
        // WAL from here on: later catch-up writes do not block readers.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    }
    for stale in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(sibling(path, stale));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Writes changed rows and the new journal position.
pub(crate) fn save_changes(
    path: &Path,
    index: &Index,
    changes: &Changes,
    meta: &Meta,
) -> anyhow::Result<()> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrate(&conn)?;
    let tx = conn.transaction()?;
    {
        let mut delete = tx.prepare("DELETE FROM files WHERE frn = ?1")?;
        for frn in &changes.removes {
            delete.execute([*frn as i64])?;
        }
        let mut insert = tx.prepare(INSERT)?;
        for &frn in &changes.upserts {
            if let (Some((parent, name, is_dir)), Some(info)) = (index.get(frn), index.info(frn)) {
                insert_row(&mut insert, frn, parent, name, is_dir, &info)?;
            }
        }
    }
    write_meta(&tx, meta)?;
    tx.commit()?;
    Ok(())
}

fn info_of(row: &Row<'_>) -> rusqlite::Result<Info> {
    Ok(Info {
        size: row.get::<_, i64>(4)? as u64,
        allocated: row.get::<_, i64>(5)? as u64,
        created: row.get(6)?,
        modified: row.get(7)?,
        accessed: row.get(8)?,
        changed: row.get(9)?,
        attributes: row.get::<_, i64>(10)? as u32,
        links: row.get::<_, i64>(11)? as u16,
        streams: row.get::<_, i64>(12)? as u16,
        reparse: row.get::<_, i64>(13)? as u32,
    })
}

/// Loads an index written by [`save_full`] (migrating an older db first), with its
/// folder totals counted; None when the file is absent.
pub(crate) fn load(path: &Path) -> anyhow::Result<Option<(Index, Meta)>> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrate(&conn)?;
    let Some(meta) = conn
        .query_row(
            "SELECT journal_id, next_usn, root, volume, full FROM meta",
            [],
            |row| {
                Ok(Meta {
                    journal_id: row.get::<_, i64>(0)? as u64,
                    next_usn: row.get(1)?,
                    root: row.get::<_, i64>(2)? as u64,
                    volume: row.get(3)?,
                    full: row.get(4)?,
                })
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut index = Index::new(meta.volume.clone(), meta.root);
    index.begin_bulk();
    let mut rows = conn.prepare(
        "SELECT frn, parent, name, is_dir, size, allocated, created, modified, accessed,
            changed, attributes, links, streams, reparse FROM files",
    )?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let frn = row.get::<_, i64>(0)? as u64;
        let name: String = row.get(2)?;
        index.upsert(frn, row.get::<_, i64>(1)? as u64, &name, row.get(3)?);
        index.set_info(frn, info_of(row)?);
    }
    index.settle();
    Ok(Some((index, meta)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-ntfs-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn full_save_changes_and_load_round_trip() {
        let dir = temp("round");
        let path = dir.join("ABCD1234.db");
        let mut index = Index::new("C:", 5);
        index.upsert(10, 5, "Users", true);
        index.upsert(11, 10, "Ünïcødé.txt", false);
        index.upsert(u64::MAX - 7, 10, "high frn", false);
        let info = Info {
            size: u64::MAX - 1,
            allocated: 1 << 40,
            created: -5,
            modified: i64::MAX,
            accessed: 3,
            changed: 4,
            attributes: 0x8000_0020,
            links: 65_535,
            streams: 2,
            reparse: 0xA000_000C,
        };
        index.set_info(11, info);
        index.settle();
        let mut meta = Meta {
            journal_id: 0xDEAD_BEEF_0000_0001,
            next_usn: 1234,
            root: 5,
            volume: "C:".into(),
            full: true,
        };
        save_full(&path, &index, &meta).unwrap();
        // Saving again over an existing db replaces it.
        save_full(&path, &index, &meta).unwrap();

        let mut changes = Changes::default();
        index.upsert(12, 10, "new.txt", false);
        index.set_info(
            12,
            Info {
                size: 99,
                ..Info::default()
            },
        );
        changes.upserts.insert(12);
        index.upsert(11, 5, "moved.txt", false);
        changes.upserts.insert(11);
        index.remove(u64::MAX - 7);
        changes.removes.insert(u64::MAX - 7);
        index.settle();
        meta.next_usn = 5678;
        save_changes(&path, &index, &changes, &meta).unwrap();

        let (loaded, loaded_meta) = load(&path).unwrap().unwrap();
        assert_eq!(loaded_meta, meta);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.path(11).unwrap(), r"C:\moved.txt");
        assert_eq!(loaded.path(12).unwrap(), r"C:\Users\new.txt");
        assert!(loaded.get(u64::MAX - 7).is_none());
        assert_eq!(loaded.info(11), Some(info));
        assert_eq!(loaded.info(12).unwrap().size, 99);
        // Folder totals are counted again on load.
        assert_eq!(loaded.folder(10).unwrap().size, 99);
        assert_eq!(loaded.folder(10).unwrap().files, 1);
        assert!(load(&dir.join("missing.db")).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A db written by Keel 0.22 (names only, no version) loads with zero metadata and
    /// is migrated on disk; one from a newer Keel is refused.
    #[test]
    fn older_databases_migrate_and_newer_ones_are_refused() {
        let dir = temp("migrate");
        let path = dir.join("OLD.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files(frn INTEGER PRIMARY KEY, parent INTEGER NOT NULL,
                    name TEXT NOT NULL, is_dir INTEGER NOT NULL);
                 CREATE TABLE meta(journal_id INTEGER NOT NULL, next_usn INTEGER NOT NULL,
                    root INTEGER NOT NULL, volume TEXT NOT NULL);
                 INSERT INTO files VALUES (10, 5, 'Users', 1), (11, 10, 'a.txt', 0);
                 INSERT INTO meta VALUES (7, 100, 5, 'D:');",
            )
            .unwrap();
        }
        let (index, mut meta) = load(&path).unwrap().unwrap();
        assert_eq!(index.path(11).unwrap(), r"D:\Users\a.txt");
        assert_eq!(index.info(11), Some(Info::default()));
        assert!(!meta.full);
        assert_eq!(meta.next_usn, 100);
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, VERSION);
        // Loading again (already migrated) and writing changes work.
        meta.next_usn = 200;
        save_changes(&path, &index, &Changes::default(), &meta).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().1.next_usn, 200);

        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 2")
            .unwrap();
        let err = load(&path).err().unwrap().to_string();
        assert!(err.contains("newer"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
