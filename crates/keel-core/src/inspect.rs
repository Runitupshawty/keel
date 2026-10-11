//! The inspector (Task 59): what the library knows about one path, in one read. Its record
//! (source, volume, content id, when the index first saw it and last hashed it), the other
//! files holding its content, its history from the operation log, its tags, and a folder's
//! totals. The copies and the history are bounded by the caller's `max`.

use crate::fsid::DIR;
use crate::index::LINK;
use crate::library::{RecordRef, Source};
use crate::tags::vpath_of;
use crate::{ChildSize, Library, OpLogEntry, Redundancy, SourceId, TagId, Total, Volume};
use anyhow::Result;
use keel_vfs::VPath;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Operation log kinds that act on library paths (the history's sources).
const KINDS: &str = "'copy', 'move', 'rename', 'delete', 'tag', 'untag'";

/// One operation that touched an item, from the operation log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    pub id: i64,
    /// Unix seconds.
    pub ts: i64,
    /// `copy`, `move`, `rename`, `delete`, `tag` or `untag`.
    pub kind: String,
    /// Whether it completed (None while running).
    pub ok: Option<bool>,
    /// The item came from here: it was moved or renamed from this path, or copied from it.
    pub from: Option<String>,
    /// The item went here (moved, renamed), or a copy of it was made here.
    pub to: Option<String>,
    /// `tag` / `untag`: the tag's name.
    pub tag: Option<String>,
}

/// A folder's totals from the index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderTotals {
    /// Bytes and files below it; None until its source's first walk has finished.
    pub total: Option<Total>,
    /// Folders below it (links not counted).
    pub folders: u64,
    /// Its biggest child.
    pub biggest: Option<ChildSize>,
}

/// What the library knows about one indexed path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inspection {
    pub record: RecordRef,
    pub source_label: String,
    /// The record's real path.
    pub path: VPath,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds, as indexed.
    pub modified: Option<i64>,
    /// The volume its source is on (state, backup mark).
    pub volume: Volume,
    /// The confirmed content id (None before hashing, for a folder, or after drift).
    pub content_id: Option<[u8; 32]>,
    /// Unix seconds the index first saw it (None: before this version recorded it).
    pub first_seen: Option<i64>,
    /// Unix seconds its bytes were last hashed here (None: not hashed here).
    pub last_hashed: Option<i64>,
    /// A file's protection; `locations` are the OTHER files holding its content (device
    /// claims last), at most `max`. None for a folder.
    pub redundancy: Option<Redundancy>,
    /// More other copies exist than `max`.
    pub more_copies: bool,
    /// Its tags, Favorites included.
    pub tags: Vec<TagId>,
    /// The operations that touched it, newest first, at most `max`.
    pub history: Vec<History>,
    /// More history exists than `max`.
    pub more_history: bool,
    pub folder: Option<FolderTotals>,
}

/// `a` and `b` name the same path (letter case aside on Windows).
fn same(a: &VPath, b: &VPath) -> bool {
    if cfg!(windows) && a.scheme == b.scheme {
        a.display().to_lowercase() == b.display().to_lowercase()
    } else {
        a == b
    }
}

/// The entries of `entries` (newest first) that touched `path`, at most `max`, and whether
/// there were more. A move or rename to a known path makes its origin known too, so the
/// history follows an item back through its renames and moves (not through copies: a
/// copy's origin is another file).
pub(crate) fn history_of(
    entries: impl IntoIterator<Item = OpLogEntry>,
    path: &VPath,
    max: usize,
) -> (Vec<History>, bool) {
    let mut known = vec![path.clone()];
    let mut out = Vec::new();
    for e in entries {
        let str_of = |k: &str| e.payload.get(k).and_then(Value::as_str).and_then(vpath_of);
        let list = |k: &str| -> Vec<VPath> {
            (e.payload
                .get(k)
                .and_then(Value::as_array)
                .into_iter()
                .flatten())
            .filter_map(|s| s.as_str().and_then(vpath_of))
            .collect()
        };
        // (from, to) per item; `to` None for deletes and tags.
        let items: Vec<(VPath, Option<VPath>)> = match e.kind.as_str() {
            "copy" | "move" => match str_of("dst") {
                Some(dst) => list("src")
                    .into_iter()
                    .map(|s| {
                        let to = dst.join(s.name());
                        (s, Some(to))
                    })
                    .collect(),
                None => Vec::new(),
            },
            "rename" => {
                let name = e.payload.get("new_name").and_then(Value::as_str);
                let moved = str_of("path").and_then(|p| {
                    let to = p.parent()?.join(name?);
                    Some((p, Some(to)))
                });
                moved.into_iter().collect()
            }
            _ => list("paths").into_iter().map(|p| (p, None)).collect(),
        };
        let is_known = |p: &VPath| known.iter().any(|k| same(k, p));
        let mut hit = None;
        for (from, to) in items {
            match to {
                Some(to) if is_known(&to) => {
                    hit = Some((Some(from.display()), None));
                    if e.kind != "copy" {
                        known.push(from);
                    }
                    break;
                }
                Some(to) if is_known(&from) => {
                    hit = Some((None, Some(to.display())));
                    break;
                }
                None if is_known(&from) => {
                    hit = Some((None, None));
                    break;
                }
                _ => {}
            }
        }
        let Some((from, to)) = hit else { continue };
        if out.len() == max {
            return (out, true);
        }
        out.push(History {
            id: e.id,
            ts: e.ts,
            tag: e
                .payload
                .get("tag")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind: e.kind,
            ok: e.ok,
            from,
            to,
        });
    }
    (out, false)
}

/// Folders below folder record `id` (itself and folder links not counted).
fn folder_count(c: &Connection, id: i64) -> Result<u64> {
    let n: i64 = c.query_row(
        "WITH RECURSIVE d(id) AS (SELECT ?1 UNION ALL
             SELECT r.id FROM record r JOIN d ON r.parent = d.id
             WHERE r.kind = ?2 AND r.flags & ?3 = 0)
         SELECT count(*) - 1 FROM d",
        params![id, DIR, LINK],
        |r| r.get(0),
    )?;
    Ok(n.max(0) as u64)
}

/// A record row as `inspect` reads it.
type Row = (
    String,
    i64,
    i64,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<i64>,
);

impl Library {
    /// What the library knows about `path` (a real path or a `library://` one): None when
    /// it is in no source or not indexed (yet). Copies and history at most `max` each.
    /// Reads only.
    pub fn inspect(&self, path: &VPath, max: usize) -> Result<Option<Inspection>> {
        let found: Option<(Arc<Source>, String)> = match keel_vfs::library::split(path) {
            Some((id, rel)) => self
                .source(&SourceId(id.to_owned()))
                .map(|s| (s, rel.to_owned())),
            None => self.source_for(path),
        };
        let Some((src, rel)) = found else {
            return Ok(None);
        };
        let (id, row, folder) = {
            let c = src.store.get()?;
            let Some((id, _)) = crate::index::resolve(&c, &rel, src.nocase())? else {
                return Ok(None);
            };
            let row: Row = c.query_row(
                "SELECT path, kind, size, mtime, CASE WHEN drift IS NULL THEN cas_id END, seen,
                     hashed
                 FROM record WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?;
            let folder = if row.1 == DIR {
                let sizes = crate::totals::folder_sizes(&c, &row.0, src.nocase(), false, 1)?;
                Some(FolderTotals {
                    total: sizes.as_ref().and_then(|s| s.total),
                    folders: folder_count(&c, id)?,
                    biggest: sizes.and_then(|s| s.children.into_iter().next()),
                })
            } else {
                None
            };
            (id, row, folder)
        };
        let (rel, kind, size, mtime, cas, seen, hashed) = row;
        let record = RecordRef {
            source: src.id.clone(),
            id,
        };
        let real = src.absolute(&rel);
        let (redundancy, more_copies) = if kind == DIR {
            (None, false)
        } else {
            let mut r = self.redundancy(&record)?;
            r.locations.retain(|l| l.record != record);
            let more = r.locations.len() > max;
            r.locations.truncate(max);
            (Some(r), more)
        };
        let (history, more_history) = self.history(&real, max)?;
        Ok(Some(Inspection {
            source_label: src.def.label.clone(),
            path: real,
            is_dir: kind == DIR,
            size: size.max(0) as u64,
            modified: mtime.map(|ns| ns.div_euclid(1_000_000_000)),
            volume: self.volume_of(&src)?,
            content_id: cas.and_then(|c| c.try_into().ok()),
            first_seen: seen,
            last_hashed: hashed,
            redundancy,
            more_copies,
            tags: self.tags_of(&record)?,
            history,
            more_history,
            folder,
            record,
        }))
    }

    /// The operation log entries that touched `path`, newest first (`history_of`).
    fn history(&self, path: &VPath, max: usize) -> Result<(Vec<History>, bool)> {
        let c = self.shared.db.get()?;
        let mut stmt = c.prepare(&format!(
            "SELECT id, ts, kind, payload, result, ok FROM op_log WHERE kind IN ({KINDS})
             ORDER BY id DESC"
        ))?;
        // ponytail: scans the file operations and tag changes newest first until `max` are
        // found; an index of paths per entry if a log ever grows past a quick scan.
        let rows = stmt.query_map([], crate::oplog::row)?;
        Ok(history_of(rows.map_while(Result::ok), path, max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: i64, kind: &str, payload: Value) -> OpLogEntry {
        OpLogEntry {
            id,
            ts: id * 10,
            kind: kind.into(),
            payload,
            result: "ok".into(),
            ok: Some(true),
        }
    }

    fn p(s: &str) -> String {
        if cfg!(windows) {
            format!(r"D:\lib\{}", s.replace('/', r"\"))
        } else {
            format!("/lib/{s}")
        }
    }

    /// Newest first: tagged, copied out, renamed into place after a move, itself a copy;
    /// an unrelated delete and the copy's origin's own history are left out.
    #[test]
    fn history_follows_moves_and_renames_but_not_copies() {
        let log = vec![
            entry(
                7,
                "tag",
                json!({"tag": "receipts", "paths": [p("b/new.txt")]}),
            ),
            entry(6, "copy", json!({"src": [p("b/new.txt")], "dst": p("c")})),
            entry(
                5,
                "rename",
                json!({"path": p("b/old.txt"), "new_name": "new.txt"}),
            ),
            entry(4, "delete", json!({"paths": [p("other.txt")]})),
            entry(3, "move", json!({"src": [p("a/old.txt")], "dst": p("b")})),
            entry(
                2,
                "copy",
                json!({"src": [p("orig/old.txt")], "dst": p("a")}),
            ),
            entry(
                1,
                "move",
                json!({"src": [p("x/old.txt")], "dst": p("orig")}),
            ),
        ];
        let (h, more) = history_of(log, &VPath::local(p("b/new.txt")), 50);
        assert!(!more);
        let ids: Vec<i64> = h.iter().map(|e| e.id).collect();
        assert_eq!(ids, [7, 6, 5, 3, 2]);
        assert_eq!(h[0].tag.as_deref(), Some("receipts"));
        assert_eq!(h[1].from, None);
        assert_eq!(h[1].to, Some(p("c/new.txt")));
        assert_eq!(h[2].from, Some(p("b/old.txt")));
        assert_eq!(h[3].from, Some(p("a/old.txt")));
        assert_eq!(h[4].from, Some(p("orig/old.txt")));
    }

    #[test]
    fn history_is_bounded() {
        let log: Vec<OpLogEntry> = (1..=120)
            .rev()
            .map(|i| {
                let kind = if i % 2 == 0 { "tag" } else { "untag" };
                entry(i, kind, json!({"tag": "t", "paths": [p("f.txt")]}))
            })
            .collect();
        let f = VPath::local(p("f.txt"));
        let (h, more) = history_of(log.clone(), &f, 50);
        assert_eq!((h.len(), more), (50, true));
        assert_eq!(h[0].id, 120, "newest first");
        let (h, more) = history_of(log, &f, 1000);
        assert_eq!((h.len(), more), (120, false));
        // Redacted paths never match.
        let gone = entry(1, "delete", json!({"paths": [crate::oplog::OUTSIDE]}));
        assert!(history_of([gone], &f, 50).0.is_empty());
    }
}
