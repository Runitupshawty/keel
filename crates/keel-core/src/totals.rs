//! Folder sizes kept with the index (Task 46): `folder_total` holds every folder's apparent
//! size and file count below it. Triggers on `record` mark the folders whose total may have
//! changed (`folder_dirty`, see schema.sql); `refresh` recomputes the marked folders from
//! their direct children, deepest first, and walks up each parent chain while a total
//! changes. Files that are links are not counted (their target is counted where it lives),
//! nor what is below a folder link (never walked); a hard link counts once per path.

use crate::fsid::{DIR, FILE};
use crate::index::LINK;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BinaryHeap, HashMap, HashSet};

/// A folder's total: apparent bytes and files below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Total {
    pub bytes: u64,
    pub files: u64,
}

/// One child of a folder with its size: a file's length, a folder's total (None while it is
/// not known yet: during the source's first walk, or for a folder link).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildSize {
    pub name: String,
    pub is_dir: bool,
    pub total: Option<Total>,
}

/// A folder's indexed total and its children's, biggest first (unknown last).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FolderSizes {
    pub total: Option<Total>,
    pub children: Vec<ChildSize>,
    /// More children exist than the `max` asked for.
    #[serde(default)]
    pub truncated: bool,
}

/// Depth of a record path ("" = the root, 0).
fn depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.matches('/').count() + 1
    }
}

/// Brings the marked folders' totals up to date in one transaction (the caller holds no
/// transaction). Many marks (a first walk, a store from before totals: over 256 and a
/// quarter of the folders) rebuild every total in one pass over the records instead.
pub(crate) fn refresh(c: &Connection) -> Result<()> {
    let dirty: i64 = c.query_row("SELECT count(*) FROM folder_dirty", [], |r| r.get(0))?;
    if dirty == 0 {
        return Ok(());
    }
    crate::index::begin_immediate(c)?;
    let result = (|| {
        let folders: i64 = c.query_row("SELECT records - files FROM counts", [], |r| r.get(0))?;
        if dirty > 256 && dirty * 4 > folders {
            rebuild(c)?;
        } else {
            let ids: Vec<i64> = c
                .prepare("SELECT record FROM folder_dirty")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            update(c, ids)?;
        }
        c.execute("DELETE FROM folder_dirty", [])?;
        Ok(())
    })();
    c.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    result
}

/// Every total from scratch: the files' sizes summed per folder, then each folder (deepest
/// first) added to its parent.
fn rebuild(c: &Connection) -> Result<()> {
    let mut acc: HashMap<i64, Total> = HashMap::new();
    let mut own = c.prepare(
        "SELECT parent, sum(size), count(*) FROM record
         WHERE kind = ?1 AND flags & ?2 = 0 AND parent IS NOT NULL GROUP BY parent",
    )?;
    for row in own.query_map(params![FILE, LINK], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (parent, bytes, files) = row?;
        acc.insert(
            parent,
            Total {
                bytes: bytes as u64,
                files: files as u64,
            },
        );
    }
    let mut dirs: Vec<(usize, i64, Option<i64>)> = c
        .prepare("SELECT id, parent, path FROM record WHERE kind = ?1 AND flags & ?2 = 0")?
        .query_map(params![DIR, LINK], |r| {
            Ok((depth(&r.get::<_, String>(2)?), r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    dirs.sort_unstable_by_key(|d| std::cmp::Reverse(d.0));
    c.execute("DELETE FROM folder_total", [])?;
    let mut put =
        c.prepare("INSERT INTO folder_total(record, bytes, files) VALUES (?1, ?2, ?3)")?;
    for (_, id, parent) in dirs {
        let t = acc.remove(&id).unwrap_or_default();
        put.execute(params![id, t.bytes as i64, t.files as i64])?;
        if let Some(p) = parent {
            let up = acc.entry(p).or_default();
            up.bytes += t.bytes;
            up.files += t.files;
        }
    }
    Ok(())
}

/// The marked folders from their direct children, deepest first; a folder whose total
/// changed queues its parent.
fn update(c: &Connection, dirty: Vec<i64>) -> Result<()> {
    let mut path_of = c.prepare_cached("SELECT path FROM record WHERE id = ?1")?;
    let mut heap = BinaryHeap::new();
    let mut queued = HashSet::new();
    for id in dirty {
        match path_of
            .query_row([id], |r| r.get::<_, String>(0))
            .optional()?
        {
            Some(path) => {
                queued.insert(id);
                heap.push((depth(&path), id));
            }
            None => {
                c.execute("DELETE FROM folder_total WHERE record = ?1", [id])?;
            }
        }
    }
    let mut info = c.prepare_cached("SELECT parent, kind, flags FROM record WHERE id = ?1")?;
    let mut sum = c.prepare_cached(
        "SELECT coalesce(sum(CASE WHEN r.kind = ?2 AND r.flags & ?3 = 0 THEN r.size
                     ELSE t.bytes END), 0),
                coalesce(sum(CASE WHEN r.kind = ?2 AND r.flags & ?3 = 0 THEN 1
                     ELSE t.files END), 0)
         FROM record r LEFT JOIN folder_total t ON r.kind = ?4 AND t.record = r.id
         WHERE r.parent = ?1",
    )?;
    let mut old = c.prepare_cached("SELECT bytes, files FROM folder_total WHERE record = ?1")?;
    let mut put = c.prepare_cached(
        "INSERT OR REPLACE INTO folder_total(record, bytes, files) VALUES (?1, ?2, ?3)",
    )?;
    while let Some((depth, id)) = heap.pop() {
        let (parent, kind, flags): (Option<i64>, i64, i64) =
            info.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if kind != DIR || flags & LINK != 0 {
            c.execute("DELETE FROM folder_total WHERE record = ?1", [id])?;
            continue;
        }
        let now: (i64, i64) =
            sum.query_row(params![id, FILE, LINK, DIR], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let was: Option<(i64, i64)> = old
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if was == Some(now) {
            continue;
        }
        put.execute(params![id, now.0, now.1])?;
        if let Some(p) = parent.filter(|p| queued.insert(*p)) {
            heap.push((depth.saturating_sub(1), p));
        }
    }
    Ok(())
}

/// The folder at `rel` with its children's sizes, biggest first (unknown last, then by
/// name): at most `max` of them, folders only with `dirs_only` (None when `rel` is not
/// indexed or not a folder).
pub(crate) fn folder_sizes(
    c: &Connection,
    rel: &str,
    nocase: bool,
    dirs_only: bool,
    max: usize,
) -> Result<Option<FolderSizes>> {
    let Some((id, _)) = crate::index::resolve(c, rel, nocase)? else {
        return Ok(None);
    };
    let kind: i64 = c.query_row("SELECT kind FROM record WHERE id = ?1", [id], |r| r.get(0))?;
    if kind != DIR {
        return Ok(None);
    }
    let total = |r: &rusqlite::Row, at: usize| -> rusqlite::Result<Option<Total>> {
        let (bytes, files): (Option<i64>, Option<i64>) = (r.get(at)?, r.get(at + 1)?);
        Ok(bytes.zip(files).map(|(b, f)| Total {
            bytes: b as u64,
            files: f as u64,
        }))
    };
    let own = c
        .query_row(
            "SELECT bytes, files FROM folder_total WHERE record = ?1",
            [id],
            |r| total(r, 0),
        )
        .optional()?
        .flatten();
    // One more than `max` tells whether there are more.
    let limit = max.saturating_add(1).min(i64::MAX as usize) as i64;
    let mut children: Vec<ChildSize> = c
        .prepare_cached(
            "SELECT r.name, r.kind, r.flags, r.size, t.bytes, t.files,
                    CASE WHEN r.kind = ?3 AND r.flags & ?4 = 0 THEN r.size
                         WHEN r.kind = ?2 AND t.files IS NOT NULL THEN t.bytes END AS s
             FROM record r
             LEFT JOIN folder_total t ON r.kind = ?2 AND t.record = r.id
             WHERE r.parent = ?1 AND (?5 = 0 OR r.kind = ?2)
             ORDER BY s IS NULL, s DESC, r.name
             LIMIT ?6",
        )?
        .query_map(params![id, DIR, FILE, LINK, dirs_only, limit], |r| {
            let (kind, flags, size): (i64, i64, i64) = (r.get(1)?, r.get(2)?, r.get(3)?);
            let total = match kind {
                FILE if flags & LINK == 0 => Some(Total {
                    bytes: size as u64,
                    files: 1,
                }),
                DIR => total(r, 4)?,
                _ => None,
            };
            Ok(ChildSize {
                name: r.get(0)?,
                is_dir: kind == DIR,
                total,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let truncated = children.len() > max;
    children.truncate(max);
    Ok(Some(FolderSizes {
        total: own,
        children,
        truncated,
    }))
}

#[cfg(test)]
#[path = "totals_tests.rs"]
mod tests;
