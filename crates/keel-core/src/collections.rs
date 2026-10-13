//! Collections: named, hand-ordered lists of files and folders from any source (an album of
//! photos from three drives, the documents for a trip). Kept in `library.db`; an item is a
//! record like a recent, so a move or rename the index records keeps it, and an item whose
//! record is gone is dropped when the collection is read.

use crate::library::RecordRef;
use crate::{Library, LibraryHit, SourceId};
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

pub type CollectionId = i64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collection {
    pub id: CollectionId,
    pub name: String,
    /// Items, gone records included until the collection is next read.
    pub items: usize,
}

fn check_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty() && name.trim() == name,
        "invalid name {name:?}"
    );
    Ok(())
}

impl Library {
    /// Every collection, by name.
    pub fn collections(&self) -> Result<Vec<Collection>> {
        let c = self.shared.db.get()?;
        let mut stmt = c.prepare(
            "SELECT id, name, (SELECT count(*) FROM collection_item WHERE collection = id)
             FROM collection ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Collection {
                id: r.get(0)?,
                name: r.get(1)?,
                items: r.get::<_, i64>(2)? as usize,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn collection_exists(&self, id: CollectionId) -> Result<()> {
        let found: Option<i64> = self
            .shared
            .db
            .get()?
            .query_row("SELECT id FROM collection WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        found
            .map(|_| ())
            .with_context(|| format!("no collection {id}"))
    }

    /// Names are unique (case-insensitively).
    pub fn create_collection(&self, name: &str) -> Result<CollectionId> {
        check_name(name)?;
        let c = self.shared.db.get()?;
        c.execute("INSERT INTO collection(name) VALUES (?1)", [name])
            .with_context(|| format!("collection {name:?} already exists"))?;
        Ok(c.last_insert_rowid())
    }

    pub fn rename_collection(&self, id: CollectionId, name: &str) -> Result<()> {
        check_name(name)?;
        self.collection_exists(id)?;
        self.shared
            .db
            .get()?
            .execute(
                "UPDATE collection SET name = ?2 WHERE id = ?1",
                params![id, name],
            )
            .with_context(|| format!("collection {name:?} already exists"))?;
        Ok(())
    }

    /// Deletes a collection (never its files).
    pub fn delete_collection(&self, id: CollectionId) -> Result<()> {
        let mut c = self.shared.db.get()?;
        let tx = c.transaction()?;
        tx.execute("DELETE FROM collection_item WHERE collection = ?1", [id])?;
        let n = tx.execute("DELETE FROM collection WHERE id = ?1", [id])?;
        anyhow::ensure!(n == 1, "no collection {id}");
        tx.commit()?;
        Ok(())
    }

    /// The items in order, as records (gone ones included).
    fn item_refs(&self, id: CollectionId) -> Result<Vec<RecordRef>> {
        let c = self.shared.db.get()?;
        let mut stmt = c.prepare(
            "SELECT source, record FROM collection_item WHERE collection = ?1 ORDER BY pos",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(RecordRef {
                source: SourceId(r.get(0)?),
                id: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Writes the collection's items as exactly `items`, in that order.
    fn write_items(&self, id: CollectionId, items: &[RecordRef]) -> Result<()> {
        let mut c = self.shared.db.get()?;
        let tx = c.transaction()?;
        tx.execute("DELETE FROM collection_item WHERE collection = ?1", [id])?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO collection_item(collection, source, record, pos)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (pos, r) in items.iter().enumerate() {
                ins.execute(params![id, r.source.0, r.id, pos as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Appends records at the end; ones already in the collection stay where they are.
    /// Returns how many were added.
    pub fn add_to_collection(&self, id: CollectionId, records: &[RecordRef]) -> Result<usize> {
        self.collection_exists(id)?;
        let mut items = self.item_refs(id)?;
        let before = items.len();
        for r in records {
            if !items.contains(r) {
                items.push(r.clone());
            }
        }
        self.write_items(id, &items)?;
        Ok(items.len() - before)
    }

    /// Takes records out of the collection (never touching the files); returns how many.
    pub fn remove_from_collection(&self, id: CollectionId, records: &[RecordRef]) -> Result<usize> {
        self.collection_exists(id)?;
        let mut items = self.item_refs(id)?;
        let before = items.len();
        items.retain(|r| !records.contains(r));
        self.write_items(id, &items)?;
        Ok(before - items.len())
    }

    /// Moves `records` (in their collection order) to just before `before`, or to the end
    /// when it is None (or one of the moved records, or not in the collection). Records not
    /// in the collection are ignored.
    pub fn move_in_collection(
        &self,
        id: CollectionId,
        records: &[RecordRef],
        before: Option<&RecordRef>,
    ) -> Result<()> {
        self.collection_exists(id)?;
        let (moved, mut rest): (Vec<RecordRef>, Vec<RecordRef>) = self
            .item_refs(id)?
            .into_iter()
            .partition(|r| records.contains(r));
        let at = before
            .and_then(|b| rest.iter().position(|r| r == b))
            .unwrap_or(rest.len());
        rest.splice(at..at, moved);
        self.write_items(id, &rest)
    }

    /// The collection's files and folders in its order. Items whose record is gone (its
    /// source removed, or the file left the index) are forgotten.
    pub fn collection_items(&self, id: CollectionId) -> Result<Vec<LibraryHit>> {
        self.collection_exists(id)?;
        let mut hits = Vec::new();
        let mut gone = Vec::new();
        for r in self.item_refs(id)? {
            match self.record(&r)? {
                Some(hit) => hits.push(hit),
                None => gone.push(r),
            }
        }
        if !gone.is_empty() {
            let c = self.shared.db.get()?;
            for r in gone {
                c.execute(
                    "DELETE FROM collection_item
                     WHERE collection = ?1 AND source = ?2 AND record = ?3",
                    params![id, r.source.0, r.id],
                )?;
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
#[path = "collections_tests.rs"]
mod tests;
