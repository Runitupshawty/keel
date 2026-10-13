//! Freezes: dated, read-only states of a source's index, browsed like an offline source.
//! Kept copy-on-write in the source's store (schema `@source 14`): a freeze copies no row;
//! a row some freeze holds is copied into `frozen` only when it changes or leaves the index.

use crate::library::{Library, SourceId};
use crate::LibraryHit;
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

/// One freeze of a source: when it was made and the source's counts then.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Freeze {
    pub id: i64,
    /// Unix seconds.
    pub at: i64,
    pub records: u64,
    pub files: u64,
    pub bytes: u64,
}

/// The records of freeze `?1`: rows unchanged since it, and the rows copied for it.
const AT: &str = "(SELECT id, parent, name, path, kind, size, mtime FROM record WHERE since < ?1
    UNION ALL SELECT id, parent, name, path, kind, size, mtime FROM frozen
        WHERE lo <= ?1 AND ?1 <= hi)";

fn freeze_row(r: &rusqlite::Row) -> rusqlite::Result<Freeze> {
    Ok(Freeze {
        id: r.get(0)?,
        at: r.get(1)?,
        records: r.get::<_, i64>(2)? as u64,
        files: r.get::<_, i64>(3)? as u64,
        bytes: r.get::<_, i64>(4)? as u64,
    })
}

impl Library {
    /// Freezes the source's index as it is now. Refused while a full walk runs (its rows
    /// are half old, half new).
    pub fn freeze(&self, source: &SourceId) -> Result<Freeze> {
        let src = self
            .source(source)
            .with_context(|| format!("no source {source}"))?;
        let _write = src.write.lock();
        anyhow::ensure!(
            src.pending_gen.load(Ordering::SeqCst) == 0,
            "{} is being indexed; freeze it when the walk has finished",
            src.def.label
        );
        let c = src.store.get()?;
        c.execute(
            "INSERT INTO freeze(at, records, files, bytes)
             SELECT ?1, records, files, bytes FROM counts",
            [crate::now()],
        )?;
        let id = c.last_insert_rowid();
        Ok(c.query_row(
            "SELECT id, at, records, files, bytes FROM freeze WHERE id = ?1",
            [id],
            freeze_row,
        )?)
    }

    /// The source's freezes, newest first.
    pub fn freezes(&self, source: &SourceId) -> Result<Vec<Freeze>> {
        let src = self
            .source(source)
            .with_context(|| format!("no source {source}"))?;
        let c = src.store.get()?;
        let mut stmt =
            c.prepare("SELECT id, at, records, files, bytes FROM freeze ORDER BY id DESC")?;
        let rows = stmt.query_map([], freeze_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes a freeze and the copied rows no other freeze holds.
    pub fn delete_freeze(&self, source: &SourceId, id: i64) -> Result<()> {
        let src = self
            .source(source)
            .with_context(|| format!("no source {source}"))?;
        let _write = src.write.lock();
        let c = src.store.get()?;
        let tx = c.unchecked_transaction()?;
        let n = tx.execute("DELETE FROM freeze WHERE id = ?1", [id])?;
        anyhow::ensure!(n == 1, "{} has no freeze {id}", src.def.label);
        tx.execute(
            "DELETE FROM frozen WHERE NOT EXISTS
                 (SELECT 1 FROM freeze f WHERE f.id BETWEEN frozen.lo AND frozen.hi)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The children of a folder (`rel` relative to the source root) as freeze `id` holds
    /// them, folders first, by name (like `list_children`).
    pub fn list_frozen(&self, source: &SourceId, id: i64, rel: &str) -> Result<Vec<LibraryHit>> {
        let src = self
            .source(source)
            .with_context(|| format!("no source {source}"))?;
        let c = src.store.get()?;
        let known = c
            .query_row("SELECT 1 FROM freeze WHERE id = ?1", [id], |_| Ok(()))
            .optional()?;
        anyhow::ensure!(known.is_some(), "{} has no freeze {id}", src.def.label);
        let collate = if src.nocase() { "COLLATE NOCASE" } else { "" };
        let mut dir: Option<i64> = c
            .query_row(
                &format!("SELECT id FROM {AT} WHERE parent IS NULL ORDER BY id LIMIT 1"),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let mut child = c.prepare(&format!(
            "SELECT id FROM {AT} WHERE parent = ?2 AND name = ?3 {collate} ORDER BY id LIMIT 1"
        ))?;
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            let Some(parent) = dir else { break };
            dir = child
                .query_row(params![id, parent, part], |r| r.get(0))
                .optional()?;
        }
        let Some(dir) = dir else {
            anyhow::bail!("{rel} is not in this freeze of {}", src.def.label);
        };
        let mut stmt = c.prepare(&format!(
            "SELECT {} FROM {AT} r WHERE r.parent = ?2
             ORDER BY r.kind = 1 DESC, r.name COLLATE NOCASE",
            crate::search::HIT_COLUMNS
        ))?;
        let rows = stmt.query_map(params![id, dir], |r| crate::search::hit_of(&src, r, 0.0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use crate::library::tests::folder;
    use crate::Library;
    use std::fs;

    fn names(hits: Vec<crate::LibraryHit>) -> Vec<String> {
        hits.into_iter().map(|h| h.name).collect()
    }

    /// A freeze keeps the listing of its time through edits, renames, deletes and new
    /// files, copies only the rows that changed, and goes with its copies when deleted.
    #[test]
    fn freezes_keep_an_old_state_copy_on_write() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("drive");
        fs::create_dir_all(root.join("docs")).unwrap();
        for i in 0..20 {
            fs::write(root.join(format!("docs/f{i}.txt")), "x").unwrap();
        }
        fs::write(root.join("a.txt"), "a").unwrap();
        let lib = Library::open(data.path(), "f").unwrap();
        let id = lib.add_source(folder("d", &root)).unwrap();
        let src = lib.source(&id).unwrap();
        let walk = || crate::index::tests::walk(&src, &lib.router()).unwrap();
        walk();
        // Files only: NTFS updates a folder's modified time in its parent's listing lazily, so
        // when a walk sees a folder change is up to the file system.
        let copied = || -> i64 {
            (src.store.get().unwrap())
                .query_row(
                    "SELECT count(*) FROM frozen WHERE kind != ?1",
                    [crate::fsid::DIR],
                    |r| r.get(0),
                )
                .unwrap()
        };

        let one = lib.freeze(&id).unwrap();
        assert_eq!((one.files, one.records), (21, 23));
        walk();
        assert_eq!(copied(), 0, "an unchanged walk copies nothing");

        fs::write(root.join("a.txt"), "longer").unwrap();
        fs::rename(root.join("docs/f0.txt"), root.join("docs/g0.txt")).unwrap();
        fs::remove_file(root.join("docs/f1.txt")).unwrap();
        fs::write(root.join("new.txt"), "n").unwrap();
        walk();
        // a.txt, f0 (renamed) and f1 (removed): the 18 untouched files stay shared.
        assert_eq!(copied(), 3);
        let two = lib.freeze(&id).unwrap();

        let frozen = |f: i64, rel: &str| names(lib.list_frozen(&id, f, rel).unwrap());
        assert_eq!(frozen(one.id, ""), ["docs", "a.txt"]);
        let docs = frozen(one.id, "docs");
        assert!(docs.contains(&"f0.txt".into()) && docs.contains(&"f1.txt".into()));
        assert!(!docs.contains(&"g0.txt".into()));
        let a = &lib.list_frozen(&id, one.id, "").unwrap()[1];
        assert_eq!(a.size, 1);
        assert_eq!(frozen(two.id, ""), ["docs", "a.txt", "new.txt"]);
        assert!(frozen(two.id, "docs").contains(&"g0.txt".into()));
        assert_eq!(
            frozen(two.id, "docs"),
            names(lib.list_children(&id, "docs").unwrap())
        );
        assert!(lib.list_frozen(&id, one.id, "nope").is_err());
        assert!(lib.list_frozen(&id, 99, "").is_err());

        assert_eq!(
            lib.freezes(&id)
                .unwrap()
                .iter()
                .map(|f| f.id)
                .collect::<Vec<_>>(),
            [two.id, one.id]
        );
        lib.delete_freeze(&id, one.id).unwrap();
        assert_eq!(
            copied(),
            0,
            "copies only the deleted freeze held go with it"
        );
        assert!(lib.delete_freeze(&id, one.id).is_err());
        assert_eq!(frozen(two.id, ""), ["docs", "a.txt", "new.txt"]);
    }
}
