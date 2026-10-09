//! `<cache>/index/<serial>.db`: the index persisted in SQLite (WAL) so a restart
//! loads it and catches up from the journal instead of re-reading the MFT.

use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};

use super::index::Index;
use super::usn::Changes;

/// Where the journal left off when the db was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub journal_id: u64,
    pub next_usn: i64,
    pub root: u64,
    /// "C:" when the volume was indexed (letters can move; the serial names the file).
    pub volume: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files(
    frn INTEGER PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL, is_dir INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS meta(
    journal_id INTEGER NOT NULL, next_usn INTEGER NOT NULL, root INTEGER NOT NULL, volume TEXT NOT NULL);";

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn write_meta(conn: &Connection, meta: &Meta) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM meta", [])?;
    conn.execute(
        "INSERT INTO meta VALUES (?1, ?2, ?3, ?4)",
        params![
            meta.journal_id as i64,
            meta.next_usn,
            meta.root as i64,
            meta.volume
        ],
    )?;
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
            let mut insert = tx.prepare("INSERT INTO files VALUES (?1, ?2, ?3, ?4)")?;
            for (frn, parent, name, is_dir) in index.entries() {
                insert.execute(params![frn as i64, parent as i64, name, is_dir])?;
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
    let tx = conn.transaction()?;
    {
        let mut delete = tx.prepare("DELETE FROM files WHERE frn = ?1")?;
        for frn in &changes.removes {
            delete.execute([*frn as i64])?;
        }
        let mut upsert = tx.prepare("INSERT OR REPLACE INTO files VALUES (?1, ?2, ?3, ?4)")?;
        for frn in &changes.upserts {
            if let Some((parent, name, is_dir)) = index.get(*frn) {
                upsert.execute(params![*frn as i64, parent as i64, name, is_dir])?;
            }
        }
    }
    write_meta(&tx, meta)?;
    tx.commit()?;
    Ok(())
}

/// Loads an index written by [`save_full`]; None when the file is absent.
pub(crate) fn load(path: &Path) -> anyhow::Result<Option<(Index, Meta)>> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open(path)?;
    let Some(meta) = conn
        .query_row(
            "SELECT journal_id, next_usn, root, volume FROM meta",
            [],
            |row| {
                Ok(Meta {
                    journal_id: row.get::<_, i64>(0)? as u64,
                    next_usn: row.get(1)?,
                    root: row.get::<_, i64>(2)? as u64,
                    volume: row.get(3)?,
                })
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut index = Index::new(meta.volume.clone(), meta.root);
    let mut rows = conn.prepare("SELECT frn, parent, name, is_dir FROM files")?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(2)?;
        index.upsert(
            row.get::<_, i64>(0)? as u64,
            row.get::<_, i64>(1)? as u64,
            &name,
            row.get(3)?,
        );
    }
    Ok(Some((index, meta)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_save_changes_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("keel-ntfs-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ABCD1234.db");
        let mut index = Index::new("C:", 5);
        index.upsert(10, 5, "Users", true);
        index.upsert(11, 10, "Ünïcødé.txt", false);
        index.upsert(u64::MAX - 7, 10, "high frn", false);
        let mut meta = Meta {
            journal_id: 0xDEAD_BEEF_0000_0001,
            next_usn: 1234,
            root: 5,
            volume: "C:".into(),
        };
        save_full(&path, &index, &meta).unwrap();
        // Saving again over an existing db replaces it.
        save_full(&path, &index, &meta).unwrap();

        let mut changes = Changes::default();
        index.upsert(12, 10, "new.txt", false);
        changes.upserts.insert(12);
        index.upsert(11, 5, "moved.txt", false);
        changes.upserts.insert(11);
        index.remove(u64::MAX - 7);
        changes.removes.insert(u64::MAX - 7);
        meta.next_usn = 5678;
        save_changes(&path, &index, &changes, &meta).unwrap();

        let (loaded, loaded_meta) = load(&path).unwrap().unwrap();
        assert_eq!(loaded_meta, meta);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.path(11).unwrap(), r"C:\moved.txt");
        assert_eq!(loaded.path(12).unwrap(), r"C:\Users\new.txt");
        assert!(loaded.get(u64::MAX - 7).is_none());
        assert!(load(&dir.join("missing.db")).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
