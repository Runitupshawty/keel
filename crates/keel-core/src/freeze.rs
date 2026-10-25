//! Freezes: dated, read-only states of a source's index, browsed like an offline source.
//! Kept copy-on-write in the source's store (schema `@source 14`): a freeze copies no row;
//! a row some freeze holds is copied into `frozen` only when it changes or leaves the index.

use crate::library::{Library, SourceId};
use crate::LibraryHit;
use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, types::Value, OptionalExtension};
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

/// Size/type/modified filters for `search_frozen`, with the same comparison semantics
/// as a library search (`search::LibraryQuery`): inclusive size bounds that only files
/// match, `kind` (`KindFilter::Folder` → folders, any other kind → files), and
/// `modified_from <= mtime < modified_before` in Unix seconds. Compared against the
/// fields the freeze holds, never live metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrozenFilters {
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub kind: Option<crate::search::KindFilter>,
    /// Unix seconds.
    pub modified_from: Option<i64>,
    /// Unix seconds, exclusive.
    pub modified_before: Option<i64>,
}

/// What `search_frozen` needs from a parsed library query: the plain substring (the words
/// and phrases, so the `ext:`/`size:`/`dm:`/`kind:` operators narrow it without matching
/// names) and the size/type/modified filters (a freeze holds `FrozenFilters`' fields,
/// nothing else). The substring is empty when the query has no plain words; `search_frozen`
/// then matches nothing (a filter-only query is unsupported, like before K14).
pub fn frozen_query(q: &crate::search::LibraryQuery) -> (String, FrozenFilters) {
    (
        q.terms
            .iter()
            .chain(&q.phrases)
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        FrozenFilters {
            min_size: q.min_size,
            max_size: q.max_size,
            kind: q.kind,
            modified_from: q.modified_from,
            modified_before: q.modified_before,
        },
    )
}

/// The records of freeze `?1`: rows unchanged since it, and the rows copied for it.
const AT: &str = "(SELECT id, parent, name, path, kind, size, mtime FROM record WHERE since < ?1
    UNION ALL SELECT id, parent, name, path, kind, size, mtime FROM frozen
        WHERE lo <= ?1 AND ?1 <= hi)";

/// mtimes are stored in nanoseconds.
const NS: i64 = 1_000_000_000;

/// The filter's ` AND ...` conditions for `search_frozen`, numbered from the next
/// placeholder of `args`, with the comparison semantics of `search::LibraryQuery::filters`
/// (size bounds inclusive and files-only, modified in Unix seconds): `None` when the
/// size range is empty, nothing matches.
fn filter_conds(f: &FrozenFilters, args: &mut Vec<Value>) -> Option<String> {
    if f.min_size.zip(f.max_size).is_some_and(|(a, b)| a > b) {
        return None;
    }
    let mut sql = String::new();
    match f.kind {
        Some(crate::search::KindFilter::Folder) => sql.push_str(" AND r.kind = 1"),
        Some(_) => sql.push_str(" AND r.kind = 0"),
        None => {}
    }
    if f.min_size.is_some() || f.max_size.is_some() {
        sql.push_str(" AND r.kind = 0");
    }
    for (bound, op) in [(f.min_size, ">="), (f.max_size, "<=")] {
        if let Some(b) = bound {
            args.push(Value::Integer(b.min(i64::MAX as u64) as i64));
            sql.push_str(&format!(" AND r.size {op} ?{}", args.len()));
        }
    }
    for (bound, op) in [(f.modified_from, ">="), (f.modified_before, "<")] {
        if let Some(t) = bound {
            args.push(Value::Integer(t.saturating_mul(NS)));
            sql.push_str(&format!(" AND r.mtime {op} ?{}", args.len()));
        }
    }
    Some(sql)
}

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

    /// Name or path search inside freeze `id` (`query` a case-insensitive substring),
    /// scoped to `folder` (a source-relative path like `list_frozen`'s `rel`; empty is
    /// the whole source) and narrowed by `filters` (size/type/modified): only the `AT`
    /// rows, so it returns the names and the size/type/modified fields as the freeze
    /// holds them, never live metadata. At most `max` hits, folders first, by name,
    /// then path.
    pub fn search_frozen(
        &self,
        source: &SourceId,
        id: i64,
        query: &str,
        folder: &str,
        filters: &FrozenFilters,
        max: usize,
    ) -> Result<Vec<LibraryHit>> {
        let src = self
            .source(source)
            .with_context(|| format!("no source {source}"))?;
        let c = src.store.get()?;
        let known = c
            .query_row("SELECT 1 FROM freeze WHERE id = ?1", [id], |_| Ok(()))
            .optional()?;
        anyhow::ensure!(known.is_some(), "{} has no freeze {id}", src.def.label);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = format!("%{}%", crate::search::like_escape(query));
        // `AT` binds ?1, so the outer placeholders number from ?2 (positional args).
        let mut args: Vec<Value> = vec![Value::Integer(id), Value::Text(pattern)];
        let Some(filters_sql) = filter_conds(filters, &mut args) else {
            return Ok(Vec::new());
        };
        // The folder itself and its contents; escaped so a `_` or `%` in the name is
        // literal, and the trailing slash keeps same-prefix siblings (docs2) out.
        let scope = if folder.is_empty() {
            String::new()
        } else {
            args.push(Value::Text(folder.to_string()));
            let eq = args.len();
            args.push(Value::Text(crate::search::like_escape(folder) + "/%"));
            format!(
                " AND (r.path = ?{eq} OR r.path LIKE ?{} ESCAPE '\\')",
                args.len()
            )
        };
        args.push(Value::Integer(max as i64));
        let limit_n = args.len();
        let mut stmt = c.prepare(&format!(
            "SELECT {} FROM {AT} r
             WHERE (r.name LIKE ?2 OR r.path LIKE ?2 ESCAPE '\\'){filters_sql}{scope}
             ORDER BY r.kind = 1 DESC, r.name COLLATE NOCASE, r.path COLLATE NOCASE
             LIMIT ?{limit_n}",
            crate::search::HIT_COLUMNS
        ))?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            crate::search::hit_of(&src, r, 0.0)
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::{frozen_query, FrozenFilters, NS};
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

        let find = |f: i64, q: &str| {
            names(
                lib.search_frozen(&id, f, q, "", &FrozenFilters::default(), 100)
                    .unwrap(),
            )
        };
        assert_eq!(
            find(one.id, "f0"),
            ["f0.txt"],
            "rename: the older freeze keeps the old name"
        );
        assert_eq!(find(one.id, "g0"), Vec::<String>::new());
        assert_eq!(find(two.id, "g0"), ["g0.txt"]);
        assert_eq!(
            find(one.id, "docs/f1.txt"),
            ["f1.txt"],
            "delete: the older freeze keeps the file"
        );
        assert_eq!(find(one.id, "new"), Vec::<String>::new());
        assert_eq!(
            find(one.id, ""),
            Vec::<String>::new(),
            "empty query: no hits"
        );
        assert_eq!(
            lib.search_frozen(&id, two.id, "f", "", &FrozenFilters::default(), 2)
                .unwrap()
                .len(),
            2,
            "max bounds the hits"
        );
        assert!(
            lib.search_frozen(&id, 99, "a", "", &FrozenFilters::default(), 1)
                .is_err(),
            "unknown freeze id errors"
        );

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

    /// A scoped freeze search stays inside `folder`: same-prefix siblings and `_`/`%`
    /// in the folder name stay out, the folder itself is in, and a deep match is found
    /// past unrelated rows (scope before limit, ties ordered by path).
    #[test]
    fn search_frozen_scopes_to_a_folder() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("drive");
        for dir in [
            "docs", "docs/a", "docs/b", "docs/sub", "docs2", "my_docs", "myXdocs", "100%", "100x",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for p in [
            "docs/notes.txt",
            "docs/a/notes.txt",
            "docs/b/notes.txt",
            "docs/sub/deep.txt",
            "docs2/notes.txt",
            "my_docs/notes.txt",
            "myXdocs/notes.txt",
            "100%/notes.txt",
            "100x/notes.txt",
        ] {
            fs::write(root.join(p), "x").unwrap();
        }
        for i in 0..5 {
            fs::write(root.join(format!("zz{i}.txt")), "x").unwrap();
        }
        let lib = Library::open(data.path(), "f").unwrap();
        let id = lib.add_source(folder("d", &root)).unwrap();
        let src = lib.source(&id).unwrap();
        crate::index::tests::walk(&src, &lib.router()).unwrap();
        let freeze = lib.freeze(&id).unwrap();
        let root_path = src.def.root.path.clone();
        let find = |q: &str, scope: &str, max: usize| {
            lib.search_frozen(&id, freeze.id, q, scope, &FrozenFilters::default(), max)
                .unwrap()
                .into_iter()
                .map(|h| h.path.path.strip_prefix(&root_path).unwrap()[1..].to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            find("notes", "docs", 100),
            ["docs/a/notes.txt", "docs/b/notes.txt", "docs/notes.txt"],
            "same-prefix sibling docs2 stays out; name ties order by path"
        );
        assert_eq!(
            find("notes", "my_docs", 100),
            ["my_docs/notes.txt"],
            "_ in the folder name is literal, not a wildcard"
        );
        assert_eq!(
            find("notes", "100%", 100),
            ["100%/notes.txt"],
            "% in the folder name is literal, not a wildcard"
        );
        assert_eq!(
            find("deep", "docs", 1),
            ["docs/sub/deep.txt"],
            "a matching descendant is found past unrelated rows, scope before limit"
        );
        assert_eq!(find("zz", "docs", 100), Vec::<String>::new());
        assert_eq!(find("notes", "nope", 100), Vec::<String>::new());
        assert_eq!(
            find("my_docs", "my_docs", 100),
            ["my_docs", "my_docs/notes.txt"],
            "the scope includes the folder itself, folders first"
        );
    }

    /// Size/type/modified filters match the fields the freeze holds, not live metadata:
    /// a file edited after freezing (new size, new mtime) still matches its historical
    /// values and not the live ones; size bounds are inclusive and files-only (a folder
    /// never matches a size filter), and modified is `from <= mtime < before`.
    #[test]
    fn search_frozen_filters_use_stored_values() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("drive");
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(root.join("data/readme.txt"), "r").unwrap();
        fs::write(root.join("big.txt"), vec![b'x'; 2000]).unwrap();
        fs::write(root.join("small.txt"), "s").unwrap();
        let lib = Library::open(data.path(), "f").unwrap();
        let id = lib.add_source(folder("d", &root)).unwrap();
        let src = lib.source(&id).unwrap();
        let walk = || crate::index::tests::walk(&src, &lib.router()).unwrap();
        walk();
        // Known historical mtimes in the store: big at t0, small two days later.
        let t0 = 1_700_000_000i64;
        let c = src.store.get().unwrap();
        c.execute(
            "UPDATE record SET mtime = ?1 WHERE name = 'big.txt'",
            [t0 * NS],
        )
        .unwrap();
        c.execute(
            "UPDATE record SET mtime = ?1 WHERE name = 'small.txt'",
            [(t0 + 2 * 86_400) * NS],
        )
        .unwrap();
        let one = lib.freeze(&id).unwrap();

        // Edit live after freezing: big.txt grows to 5000 bytes with a fresh mtime.
        fs::write(root.join("big.txt"), vec![b'y'; 5000]).unwrap();
        walk();

        let find = |f: &FrozenFilters, q: &str| {
            names(lib.search_frozen(&id, one.id, q, "", f, 100).unwrap())
        };
        // 1000..=3000 fits the stored 2000 but not the live 5000: the historical size
        // match survives the live edit.
        assert_eq!(
            find(
                &FrozenFilters {
                    min_size: Some(1000),
                    max_size: Some(3000),
                    ..Default::default()
                },
                "big"
            ),
            ["big.txt"]
        );
        assert_eq!(
            find(
                &FrozenFilters {
                    min_size: Some(4000),
                    ..Default::default()
                },
                "big"
            ),
            Vec::<String>::new(),
            "the live size is not what the freeze holds"
        );
        // Modified: from <= mtime < before, against the stored mtimes. The live mtime
        // of big.txt (just now) is far past t0 + 1 day.
        assert_eq!(
            find(
                &FrozenFilters {
                    modified_from: Some(t0),
                    modified_before: Some(t0 + 86_400),
                    ..Default::default()
                },
                "big"
            ),
            ["big.txt"]
        );
        assert_eq!(
            find(
                &FrozenFilters {
                    modified_from: Some(t0 + 86_400),
                    ..Default::default()
                },
                "mall"
            ),
            ["small.txt"]
        );
        assert_eq!(
            find(
                &FrozenFilters {
                    modified_before: Some(t0),
                    ..Default::default()
                },
                "big"
            ),
            Vec::<String>::new(),
            "before is exclusive"
        );
        // Type: folders and files; a size filter is files-only, so the folder `data`
        // (query "data" also matches its child via the path) never matches.
        assert_eq!(
            find(
                &FrozenFilters {
                    kind: Some(crate::KindFilter::Folder),
                    ..Default::default()
                },
                "data"
            ),
            ["data"]
        );
        assert_eq!(
            find(
                &FrozenFilters {
                    kind: Some(crate::KindFilter::File),
                    ..Default::default()
                },
                "big"
            ),
            ["big.txt"]
        );
        assert_eq!(
            find(
                &FrozenFilters {
                    min_size: Some(1),
                    ..Default::default()
                },
                "data"
            ),
            ["readme.txt"],
            "size semantics: files only, the folder stays out"
        );
        assert_eq!(
            find(
                &FrozenFilters {
                    min_size: Some(3000),
                    max_size: Some(2000),
                    ..Default::default()
                },
                "big"
            ),
            Vec::<String>::new(),
            "an empty size range matches nothing"
        );
    }

    /// `frozen_query` keeps the plain words as the substring (the operators narrow it
    /// without matching names) and lifts the size/type/modified filters for `search_frozen`.
    #[test]
    fn frozen_query_keeps_words_and_filters() {
        use crate::search::LibraryQuery;
        let lq = LibraryQuery::parse("invoice size:>1mb kind:folder dm:>=2026-01-01", 0).unwrap();
        let (substring, f) = frozen_query(&lq);
        assert_eq!(substring, "invoice", "the operators are not the substring");
        assert_eq!(
            f.min_size,
            Some((1 << 20) + 1),
            "strict `>` lifts the inclusive bound"
        );
        assert_eq!(f.kind, Some(crate::search::KindFilter::Folder));
        assert!(f.modified_from.is_some() && f.modified_before.is_none());
        // No plain words: an empty substring (a filter-only query matches nothing in a freeze).
        let op = LibraryQuery::parse("ext:txt", 0).unwrap();
        assert_eq!(frozen_query(&op).0, "");
    }
}
