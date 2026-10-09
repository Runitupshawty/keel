//! Tags (colored, nested), favorites, recents and saved views (spec 2.10).
//!
//! Tags are defined in `library.db`; every source store keeps a copy of the definitions next
//! to its `record_tag` rows, so a store that travels alone keeps its organization (and its
//! search can resolve tag names). A store that comes back from another library has its tags
//! merged in by name. Tag and record ids are never reused (AUTOINCREMENT), so a link or a
//! recent left behind never lands on another tag or file. Favorites are the reserved tag
//! [`FAVORITES`]. Recents merge what the app opened (`note_open`) with what executed
//! operations produced (op_log).

use crate::library::{RecordRef, Source};
use crate::search::{hit_of, HIT_COLUMNS};
use crate::{Library, LibraryHit, LibraryQuery};
use anyhow::{Context, Result};
use keel_vfs::VPath;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub type TagId = i64;

/// The reserved favorites tag: not listed by `tags()`, never renamed, nested or deleted.
pub const FAVORITES: TagId = 1;
/// Opened records remembered for recents.
const OPENED_KEPT: i64 = 1_000;
/// Store meta key: the library whose tags the store's `tag` table copies.
const TAGS_FROM: &str = "tags_from";
/// Attempts at a store write that another writer keeps busy (each waits the busy timeout).
const STORE_ATTEMPTS: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: TagId,
    pub name: String,
    /// Any CSS-style color string the app understands (`#e5484d`).
    pub color: Option<String>,
    pub parent: Option<TagId>,
}

/// A saved query with its layout (shown in the sidebar).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub id: i64,
    pub name: String,
    /// A [`LibraryQuery`] string.
    pub query: String,
    /// The app's layout name (`details`, `grid`, ...).
    pub layout: String,
}

fn check_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty() && name.trim() == name,
        "invalid name {name:?}"
    );
    Ok(())
}

/// A display path from the op log back to a path (None when it was redacted).
fn vpath_of(shown: &str) -> Option<VPath> {
    if shown.contains("://") {
        VPath::parse(shown).ok()
    } else if shown.starts_with('<') {
        None
    } else {
        Some(VPath::local(shown))
    }
}

/// Where an executed operation left records (copy/move destinations, rename targets).
fn op_targets(kind: &str, payload: &serde_json::Value) -> Vec<VPath> {
    let str_of = |k: &str| payload.get(k).and_then(|v| v.as_str()).and_then(vpath_of);
    match kind {
        "copy" | "move" => {
            let Some(dst) = str_of("dst") else {
                return Vec::new();
            };
            payload
                .get("src")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().and_then(vpath_of))
                .map(|s| dst.join(s.name()))
                .collect()
        }
        "rename" => {
            let new_name = payload.get("new_name").and_then(|v| v.as_str());
            match (str_of("path").and_then(|p| p.parent()), new_name) {
                (Some(parent), Some(name)) => vec![parent.join(name)],
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

impl Library {
    fn tag_rows(&self) -> Result<Vec<Tag>> {
        let c = self.shared.db.get()?;
        let mut stmt =
            c.prepare("SELECT id, name, color, parent FROM tag ORDER BY name COLLATE NOCASE, id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
                color: r.get(2)?,
                parent: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces the source store's copy of the tag definitions.
    fn mirror_tags(&self, src: &Source, tags: &[Tag]) -> Result<()> {
        let mut c = src.store.get()?;
        let tx = c.transaction()?;
        tx.execute("DELETE FROM tag", [])?;
        {
            let mut ins =
                tx.prepare("INSERT INTO tag(id, name, color, parent) VALUES (?1, ?2, ?3, ?4)")?;
            for t in tags {
                ins.execute(params![t.id, t.name, t.color, t.parent])?;
            }
        }
        crate::db::set_meta(&tx, TAGS_FROM, &self.id.0)?;
        tx.commit()?;
        Ok(())
    }

    /// The library tag named `name` (case-insensitively) under `parent`, created when
    /// missing.
    fn tag_named(&self, name: &str, color: Option<&str>, parent: Option<TagId>) -> Result<TagId> {
        let c = self.shared.db.get()?;
        let found: Option<TagId> = c
            .query_row(
                "SELECT id FROM tag WHERE coalesce(parent, 0) = coalesce(?2, 0)
                     AND name = ?1 COLLATE NOCASE",
                params![name, parent],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = found {
            return Ok(id);
        }
        c.execute(
            "INSERT INTO tag(name, color, parent) VALUES (?1, ?2, ?3)",
            params![name, color, parent],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Brings every store's tags in line with the library before the library's copy
    /// overwrites them: a store last mirrored from another library has its tags merged in by
    /// name (created here when missing) and its links moved to them; links of this library's
    /// tags that no longer exist are dropped.
    pub(crate) fn reconcile_tags(&self) -> Result<()> {
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        for src in &sources {
            let from = src.store.meta(TAGS_FROM)?;
            let foreign = from.as_deref().is_some_and(|f| f != self.id.0);
            let known: Vec<TagId> = self.tag_rows()?.iter().map(|t| t.id).collect();
            let mut c = src.store.get()?;
            if !foreign {
                c.execute(
                    "DELETE FROM record_tag WHERE tag NOT IN (SELECT value FROM json_each(?1))",
                    [serde_json::to_string(&known)?],
                )?;
                continue;
            }
            let theirs: Vec<Tag> = {
                let mut stmt = c.prepare("SELECT id, name, color, parent FROM tag")?;
                let rows = stmt.query_map([], |r| {
                    Ok(Tag {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        color: r.get(2)?,
                        parent: r.get(3)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            // Parents first; a parent that never resolves (a cycle, a missing row) leaves
            // its children at the top level.
            let mut map: HashMap<TagId, TagId> = HashMap::from([(FAVORITES, FAVORITES)]);
            let mut left: Vec<&Tag> = theirs.iter().filter(|t| t.id != FAVORITES).collect();
            while !left.is_empty() {
                let ready = left
                    .iter()
                    .position(|t| t.parent.is_none_or(|p| map.contains_key(&p)))
                    .unwrap_or(0);
                let t = left.remove(ready);
                let parent = t.parent.and_then(|p| map.get(&p).copied());
                let id = self.tag_named(&t.name, t.color.as_deref(), parent)?;
                map.insert(t.id, id);
            }
            // Move the links: to negative ids first (old and new ids may overlap).
            let tx = c.transaction()?;
            let case: String = map
                .iter()
                .map(|(old, new)| format!(" WHEN {old} THEN {}", -new))
                .collect();
            if !map.is_empty() {
                tx.execute(
                    &format!(
                        "INSERT OR IGNORE INTO record_tag(record, tag)
                         SELECT record, CASE tag{case} END FROM record_tag
                         WHERE tag IN ({})",
                        map.keys().map(i64::to_string).collect::<Vec<_>>().join(",")
                    ),
                    [],
                )?;
            }
            tx.execute("DELETE FROM record_tag WHERE tag > 0", [])?;
            tx.execute("UPDATE record_tag SET tag = -tag", [])?;
            crate::db::set_meta(&tx, TAGS_FROM, &self.id.0)?;
            tx.commit()?;
        }
        self.mirror_all()
    }

    fn mirror_all(&self) -> Result<()> {
        let tags = self.tag_rows()?;
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        for s in &sources {
            self.mirror_tags(s, &tags)?;
        }
        Ok(())
    }

    fn tag_exists(&self, id: TagId) -> Result<()> {
        let found: Option<i64> = self
            .shared
            .db
            .get()?
            .query_row("SELECT id FROM tag WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        found.map(|_| ()).with_context(|| format!("no tag {id}"))
    }

    fn editable(&self, id: TagId) -> Result<()> {
        anyhow::ensure!(id != FAVORITES, "Favorites cannot be changed");
        self.tag_exists(id)
    }

    /// `id` and every tag nested below it.
    fn tag_tree(&self, id: TagId) -> Result<Vec<TagId>> {
        let c = self.shared.db.get()?;
        let mut stmt = c.prepare(
            "WITH RECURSIVE t(id) AS (
                 SELECT ?1 UNION SELECT tag.id FROM tag JOIN t ON tag.parent = t.id)
             SELECT id FROM t",
        )?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every tag except Favorites, by name.
    pub fn tags(&self) -> Result<Vec<Tag>> {
        let mut tags = self.tag_rows()?;
        tags.retain(|t| t.id != FAVORITES);
        Ok(tags)
    }

    /// Names are unique among siblings (case-insensitively).
    pub fn create_tag(
        &self,
        name: &str,
        color: Option<&str>,
        parent: Option<TagId>,
    ) -> Result<TagId> {
        check_name(name)?;
        if let Some(p) = parent {
            self.editable(p)?;
        }
        let id = {
            let c = self.shared.db.get()?;
            c.execute(
                "INSERT INTO tag(name, color, parent) VALUES (?1, ?2, ?3)",
                params![name, color, parent],
            )
            .with_context(|| format!("tag {name:?} already exists"))?;
            c.last_insert_rowid()
        };
        self.mirror_all()?;
        Ok(id)
    }

    pub fn rename_tag(&self, id: TagId, name: &str) -> Result<()> {
        check_name(name)?;
        self.editable(id)?;
        self.shared
            .db
            .get()?
            .execute("UPDATE tag SET name = ?2 WHERE id = ?1", params![id, name])
            .with_context(|| format!("tag {name:?} already exists"))?;
        self.mirror_all()
    }

    pub fn recolor_tag(&self, id: TagId, color: Option<&str>) -> Result<()> {
        self.editable(id)?;
        self.shared.db.get()?.execute(
            "UPDATE tag SET color = ?2 WHERE id = ?1",
            params![id, color],
        )?;
        self.mirror_all()
    }

    /// Moves a tag under `parent` (None: top level); a tag cannot go below itself.
    pub fn nest_tag(&self, id: TagId, parent: Option<TagId>) -> Result<()> {
        self.editable(id)?;
        if let Some(p) = parent {
            self.editable(p)?;
            anyhow::ensure!(
                !self.tag_tree(id)?.contains(&p),
                "a tag cannot be nested below itself"
            );
        }
        self.shared
            .db
            .get()?
            .execute(
                "UPDATE tag SET parent = ?2 WHERE id = ?1",
                params![id, parent],
            )
            .context("a sibling tag already has that name")?;
        self.mirror_all()
    }

    /// Deletes a tag and its record links; its nested tags move up to its parent. The links
    /// go first (retried while a store is busy): a failure leaves the tag in place, to be
    /// deleted again, never links without their tag.
    pub fn delete_tag(&self, id: TagId) -> Result<()> {
        self.editable(id)?;
        {
            // Checked before anything goes: the nested tags must fit one level up.
            let c = self.shared.db.get()?;
            let clash: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM tag a JOIN tag b
                     ON coalesce(b.parent, 0) = coalesce((SELECT parent FROM tag WHERE id = ?1), 0)
                     AND b.name = a.name COLLATE NOCASE AND b.id <> ?1
                 WHERE a.parent = ?1)",
                [id],
                |r| r.get(0),
            )?;
            anyhow::ensure!(!clash, "a nested tag's name clashes one level up");
        }
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        for s in &sources {
            let mut attempt = 1;
            loop {
                let deleted = s
                    .store
                    .get()
                    .and_then(|c| Ok(c.execute("DELETE FROM record_tag WHERE tag = ?1", [id])?));
                match deleted {
                    Ok(_) => break,
                    Err(_) if attempt < STORE_ATTEMPTS => attempt += 1,
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("remove tag {id} from source {}", s.def.label)
                        })
                    }
                }
            }
        }
        {
            let mut c = self.shared.db.get()?;
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE tag SET parent = (SELECT parent FROM tag WHERE id = ?1) WHERE parent = ?1",
                [id],
            )
            .context("a nested tag's name clashes one level up")?;
            tx.execute("DELETE FROM tag WHERE id = ?1", [id])?;
            tx.commit()?;
        }
        self.mirror_all()
    }

    /// Applies (`on`) or removes a tag on records, one transaction per source. Records that
    /// no longer exist are skipped.
    pub fn set_tag(&self, tag: TagId, records: &[RecordRef], on: bool) -> Result<()> {
        self.tag_exists(tag)?;
        let tags = self.tag_rows()?;
        let mut by_source: Vec<(Arc<Source>, Vec<i64>)> = Vec::new();
        for r in records {
            match by_source.iter_mut().find(|(s, _)| s.id == r.source) {
                Some((_, ids)) => ids.push(r.id),
                None => {
                    let src = self
                        .source(&r.source)
                        .with_context(|| format!("no source {}", r.source))?;
                    by_source.push((src, vec![r.id]));
                }
            }
        }
        for (src, ids) in by_source {
            if on {
                self.mirror_tags(&src, &tags)?;
            }
            let mut c = src.store.get()?;
            let tx = c.transaction()?;
            {
                let mut stmt = tx.prepare(if on {
                    "INSERT OR IGNORE INTO record_tag(record, tag)
                     SELECT id, ?2 FROM record WHERE id = ?1"
                } else {
                    "DELETE FROM record_tag WHERE record = ?1 AND tag = ?2"
                })?;
                for id in ids {
                    stmt.execute(params![id, tag])?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }

    /// The tags on a record (Favorites included).
    pub fn tags_of(&self, record: &RecordRef) -> Result<Vec<TagId>> {
        let src = self
            .source(&record.source)
            .with_context(|| format!("no source {}", record.source))?;
        let c = src.store.get()?;
        let mut stmt = c.prepare("SELECT tag FROM record_tag WHERE record = ?1 ORDER BY tag")?;
        let rows = stmt.query_map([record.id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records carrying `tag` or a tag nested below it, by name.
    pub fn records_with_tag(&self, tag: TagId) -> Result<Vec<LibraryHit>> {
        self.tag_exists(tag)?;
        let ids = self
            .tag_tree(tag)?
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        let mut hits = Vec::new();
        for src in &sources {
            let c = src.store.get()?;
            let mut stmt = c.prepare(&format!(
                "SELECT {HIT_COLUMNS} FROM record r
                 WHERE r.id IN (SELECT record FROM record_tag WHERE tag IN ({ids}))"
            ))?;
            let rows = stmt.query_map([], |r| hit_of(src, r, 0.0))?;
            hits.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        hits.sort_by_key(|h| h.name.to_lowercase());
        Ok(hits)
    }

    pub fn set_favorite(&self, records: &[RecordRef], on: bool) -> Result<()> {
        self.set_tag(FAVORITES, records, on)
    }

    pub fn favorites(&self) -> Result<Vec<LibraryHit>> {
        self.records_with_tag(FAVORITES)
    }

    /// One record as a hit (score 0), None when it no longer exists.
    pub fn record(&self, record: &RecordRef) -> Result<Option<LibraryHit>> {
        let Some(src) = self.source(&record.source) else {
            return Ok(None);
        };
        let c = src.store.get()?;
        Ok(c.query_row(
            &format!("SELECT {HIT_COLUMNS} FROM record r WHERE r.id = ?1"),
            [record.id],
            |r| hit_of(&src, r, 0.0),
        )
        .optional()?)
    }

    /// Remembers that the app opened a record (for recents).
    pub fn note_open(&self, record: &RecordRef) -> Result<()> {
        let c = self.shared.db.get()?;
        c.execute(
            "INSERT OR REPLACE INTO opened(source, record, ts) VALUES (?1, ?2, ?3)",
            params![record.source.0, record.id, crate::now()],
        )?;
        c.execute(
            "DELETE FROM opened WHERE rowid NOT IN
                 (SELECT rowid FROM opened ORDER BY ts DESC, rowid DESC LIMIT ?1)",
            [OPENED_KEPT],
        )?;
        Ok(())
    }

    /// Recently opened records and records produced by executed operations (copy and move
    /// destinations, renamed items), newest first, each once. Opened records that are gone
    /// are forgotten.
    pub fn recents(&self, limit: usize) -> Result<Vec<LibraryHit>> {
        // (ts, sequence within its kind, record)
        let mut events: Vec<(i64, i64, RecordRef)> = Vec::new();
        {
            let c = self.shared.db.get()?;
            let mut stmt = c.prepare(
                "SELECT source, record, ts, rowid FROM opened ORDER BY ts DESC, rowid DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit as i64], |r| {
                Ok((
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    RecordRef {
                        source: crate::SourceId(r.get(0)?),
                        id: r.get(1)?,
                    },
                ))
            })?;
            events.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        for entry in crate::oplog::entries(&self.shared, limit.max(1) * 4)? {
            if entry.ok != Some(true) {
                continue;
            }
            for target in op_targets(&entry.kind, &entry.payload) {
                let Some((src, rel)) = self.source_for(&target) else {
                    continue;
                };
                let c = src.store.get()?;
                if let Some((id, _)) = crate::index::resolve(&c, &rel, src.nocase())? {
                    let record = RecordRef {
                        source: src.id.clone(),
                        id,
                    };
                    events.push((entry.ts, entry.id, record));
                }
            }
        }
        events.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (_, _, record) in events {
            if out.len() >= limit {
                break;
            }
            if seen.insert(record.clone()) {
                match self.record(&record)? {
                    Some(hit) => out.push(hit),
                    None => gone.push(record),
                }
            }
        }
        if !gone.is_empty() {
            let c = self.shared.db.get()?;
            for r in gone {
                c.execute(
                    "DELETE FROM opened WHERE source = ?1 AND record = ?2",
                    params![r.source.0, r.id],
                )?;
            }
        }
        Ok(out)
    }

    /// Saves a view; `query` must parse as a [`LibraryQuery`].
    pub fn create_view(&self, name: &str, query: &str, layout: &str) -> Result<View> {
        check_name(name)?;
        LibraryQuery::parse(query, 0)?;
        let c = self.shared.db.get()?;
        c.execute(
            "INSERT INTO view(name, query, layout) VALUES (?1, ?2, ?3)",
            params![name, query, layout],
        )?;
        Ok(View {
            id: c.last_insert_rowid(),
            name: name.to_owned(),
            query: query.to_owned(),
            layout: layout.to_owned(),
        })
    }

    /// Saved views, in creation order.
    pub fn views(&self) -> Result<Vec<View>> {
        let c = self.shared.db.get()?;
        let mut stmt = c.prepare("SELECT id, name, query, layout FROM view ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(View {
                id: r.get(0)?,
                name: r.get(1)?,
                query: r.get(2)?,
                layout: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_view(&self, view: &View) -> Result<()> {
        check_name(&view.name)?;
        LibraryQuery::parse(&view.query, 0)?;
        let n = self.shared.db.get()?.execute(
            "UPDATE view SET name = ?2, query = ?3, layout = ?4 WHERE id = ?1",
            params![view.id, view.name, view.query, view.layout],
        )?;
        anyhow::ensure!(n == 1, "no view {}", view.id);
        Ok(())
    }

    pub fn delete_view(&self, id: i64) -> Result<()> {
        let n = self
            .shared
            .db
            .get()?
            .execute("DELETE FROM view WHERE id = ?1", [id])?;
        anyhow::ensure!(n == 1, "no view {id}");
        Ok(())
    }
}

#[cfg(test)]
#[path = "tags_tests.rs"]
mod tests;
