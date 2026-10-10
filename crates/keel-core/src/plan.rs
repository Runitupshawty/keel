//! `validate -> preview -> execute` for every mutating operation. The preview is projected
//! from the index (so it works for offline sources) and falls back to the live filesystem for
//! paths the index does not know; execution re-validates, runs as a durable job and writes a
//! (redacted) op_log entry.

use crate::index::resolve;
use crate::jobs::{Job, JobCtx, JobId};
use crate::library::{relative, OfflineReason, Shared, Source, SourceStatus};
use crate::{oplog, Cancelled, Indexer, Library, SourceId};
use anyhow::{Context, Result};
use keel_vfs::{ops::Conflict, Kind, VPath};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnConflict {
    Skip,
    Overwrite,
    RenameNew,
}

impl From<OnConflict> for Conflict {
    fn from(c: OnConflict) -> Conflict {
        match c {
            OnConflict::Skip => Conflict::Skip,
            OnConflict::Overwrite => Conflict::Overwrite,
            OnConflict::RenameNew => Conflict::RenameNew,
        }
    }
}

/// A mutating operation (serializable: it is the durable state of its job).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Copy {
        src: Vec<VPath>,
        dst_dir: VPath,
        on_conflict: OnConflict,
    },
    Move {
        src: Vec<VPath>,
        dst_dir: VPath,
        on_conflict: OnConflict,
    },
    /// To the OS trash locally; other providers remove their way (`Warning::Permanent` when
    /// it cannot be undone).
    Delete {
        paths: Vec<VPath>,
    },
    Rename {
        path: VPath,
        new_name: String,
    },
}

impl Op {
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Copy { .. } => "copy",
            Op::Move { .. } => "move",
            Op::Delete { .. } => "delete",
            Op::Rename { .. } => "rename",
        }
    }

    /// The top-level paths the operation acts on, one execution step each.
    fn items(&self) -> Vec<VPath> {
        match self {
            Op::Copy { src, .. } | Op::Move { src, .. } => src.clone(),
            Op::Delete { paths } => paths.clone(),
            Op::Rename { path, .. } => vec![path.clone()],
        }
    }

    /// What the op_log records (before redaction): display paths, no provider details.
    fn payload(&self) -> serde_json::Value {
        let shown = |ps: &[VPath]| ps.iter().map(VPath::display).collect::<Vec<_>>();
        match self {
            Op::Copy {
                src,
                dst_dir,
                on_conflict,
            }
            | Op::Move {
                src,
                dst_dir,
                on_conflict,
            } => serde_json::json!({
                "src": shown(src),
                "dst": dst_dir.display(),
                "on_conflict": on_conflict,
            }),
            Op::Delete { paths } => serde_json::json!({ "paths": shown(paths) }),
            Op::Rename { path, new_name } => serde_json::json!({
                "path": path.display(),
                "new_name": new_name,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Copy,
    Move,
    Delete,
    Rename,
}

/// One projected change per top-level path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub action: Action,
    pub from: VPath,
    pub to: Option<VPath>,
    /// Files (not folders) affected, and their total size.
    pub files: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Warning {
    /// Deleting `path` removes the only indexed copy of `files` files' content (copies on
    /// lost or retired volumes do not count).
    LastCopy { path: VPath, files: u64 },
    /// `files` files under `path` are their content's only copies outside one failure
    /// domain: after the delete every copy left shares one disk (or account, or host).
    SingleDomain { path: VPath, files: u64 },
    /// Projected from the source's last generation; executing fails while it is offline.
    OfflineSource { source: SourceId, label: String },
    /// Not in the index: projected from the live filesystem.
    NotIndexed { path: VPath },
    /// The destination already has this name; `on_conflict` decides.
    Exists {
        path: VPath,
        on_conflict: OnConflict,
    },
    /// The provider deletes for good (no trash): SFTP, S3.
    Permanent { path: VPath },
    /// `files` deleted files have no content id yet, or their bytes drifted from it, so
    /// whether another copy exists is not known (no `LastCopy` can be computed for them).
    ContentUnverified { path: VPath, files: u64 },
    /// `files` deleted files' content survives only on offline or archived volumes.
    CopiesOffline { path: VPath, files: u64 },
    /// The operation changes entries inside the zip `path` (`bytes` big): the whole
    /// archive is written again beside it and then replaces it.
    RewritesArchive { path: VPath, bytes: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub op: Op,
    pub changes: Vec<Change>,
    pub warnings: Vec<Warning>,
}

/// Validates `op` and previews it (nothing is touched); `plan.execute(lib)` runs it.
pub fn validate_preview_execute(lib: &Library, op: Op) -> Result<Plan> {
    preview(&lib.shared, op)
}

/// `Plan::execute` refused because the preview no longer matches (its actions, paths or
/// kinds of warnings; counts and sizes may drift, a log file keeps growing): the fresh plan
/// is inside, for the user to confirm instead.
#[derive(Debug)]
pub struct PlanChanged(pub Box<Plan>);

impl std::fmt::Display for PlanChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the sources changed since the preview: confirm the new preview")
    }
}

impl std::error::Error for PlanChanged {}

/// A warning without its counts.
fn kind_of(w: &Warning) -> Warning {
    match w.clone() {
        Warning::LastCopy { path, .. } => Warning::LastCopy { path, files: 0 },
        Warning::SingleDomain { path, .. } => Warning::SingleDomain { path, files: 0 },
        Warning::ContentUnverified { path, .. } => Warning::ContentUnverified { path, files: 0 },
        Warning::CopiesOffline { path, .. } => Warning::CopiesOffline { path, files: 0 },
        Warning::RewritesArchive { path, .. } => Warning::RewritesArchive { path, bytes: 0 },
        w => w,
    }
}

/// Same operation, actions, paths and kinds of warnings (counts and sizes may differ).
fn same_shape(a: &Plan, b: &Plan) -> bool {
    let changes = |p: &Plan| -> Vec<(Action, VPath, Option<VPath>)> {
        p.changes
            .iter()
            .map(|c| (c.action, c.from.clone(), c.to.clone()))
            .collect()
    };
    let warnings = |p: &Plan| p.warnings.iter().map(kind_of).collect::<Vec<_>>();
    a.op == b.op && changes(a) == changes(b) && warnings(a) == warnings(b)
}

impl Plan {
    /// Runs the operation as a durable job that logs to op_log, but only while a fresh
    /// preview has the shape of this confirmed one (`PlanChanged` otherwise; a path that
    /// vanished fails too). `recheck`: preview again here, on the calling thread (it stats
    /// live paths), returning `Err(PlanChanged(fresh))`; else the job does it before its
    /// first step and fails with that error in its log.
    pub fn execute(self, lib: &Library, recheck: bool) -> Result<JobId> {
        let expect = if recheck {
            let fresh = preview(&lib.shared, self.op.clone())?;
            if !same_shape(&fresh, &self) {
                return Err(PlanChanged(Box::new(fresh)).into());
            }
            None
        } else {
            Some(self.clone())
        };
        let files = self.changes.iter().map(|c| c.files).sum();
        lib.jobs()
            .spawn(Box::new(ExecJob::new(self.op, expect, Some(files))))
    }
}

struct Probe {
    is_dir: bool,
    files: u64,
    bytes: u64,
}

fn preview(lib: &Shared, op: Op) -> Result<Plan> {
    let mut warnings = Vec::new();
    let mut changes = Vec::new();
    archive_writes(lib, &op, &mut warnings)?;
    match &op {
        Op::Copy {
            src,
            dst_dir,
            on_conflict,
        }
        | Op::Move {
            src,
            dst_dir,
            on_conflict,
        } => {
            let mv = matches!(op, Op::Move { .. });
            let verb = op.kind();
            anyhow::ensure!(!src.is_empty(), "nothing to {verb}");
            let dst = probe(lib, dst_dir, &mut warnings)?;
            anyhow::ensure!(dst.is_dir, "{} is not a folder", dst_dir.display());
            for s in src {
                anyhow::ensure!(
                    relative(s, dst_dir).is_none(),
                    "cannot {verb} {} into itself",
                    s.display()
                );
                anyhow::ensure!(
                    !(mv && s.parent().as_ref() == Some(dst_dir)),
                    "{} is already in {}",
                    s.display(),
                    dst_dir.display()
                );
                let p = probe(lib, s, &mut warnings)?;
                let to = dst_dir.join(s.name());
                if exists(lib, &to) {
                    warnings.push(Warning::Exists {
                        path: to.clone(),
                        on_conflict: *on_conflict,
                    });
                }
                changes.push(Change {
                    action: if mv { Action::Move } else { Action::Copy },
                    from: s.clone(),
                    to: Some(to),
                    files: p.files,
                    bytes: p.bytes,
                });
            }
        }
        Op::Delete { paths } => {
            anyhow::ensure!(!paths.is_empty(), "nothing to delete");
            let router = lib.router.read().clone();
            for p in paths {
                let probe = probe(lib, p, &mut warnings)?;
                let permanent = router
                    .provider_for(p)
                    .is_some_and(|pr| pr.remove_kind() == keel_vfs::RemoveKind::Permanent);
                if permanent {
                    warnings.push(Warning::Permanent { path: p.clone() });
                }
                changes.push(Change {
                    action: Action::Delete,
                    from: p.clone(),
                    to: None,
                    files: probe.files,
                    bytes: probe.bytes,
                });
            }
            last_copies(lib, paths, &mut warnings)?;
        }
        Op::Rename { path, new_name } => {
            check_name(new_name)?;
            let p = probe(lib, path, &mut warnings)?;
            let parent = path
                .parent()
                .with_context(|| format!("cannot rename {}", path.display()))?;
            let to = parent.join(new_name);
            // A case-only rename finds the same entry; the provider decides.
            let same = if cfg!(windows) {
                to.path.to_lowercase() == path.path.to_lowercase()
            } else {
                to == *path
            };
            anyhow::ensure!(same || !exists(lib, &to), "{} already exists", to.display());
            changes.push(Change {
                action: Action::Rename,
                from: path.clone(),
                to: Some(to),
                files: p.files,
                bytes: p.bytes,
            });
        }
    }
    let mut seen = Vec::new();
    warnings.retain(|w| {
        let new = !seen.contains(w);
        seen.push(w.clone());
        new
    });
    Ok(Plan {
        op,
        changes,
        warnings,
    })
}

/// Refuses writes into archives that cannot be changed (7z, tar, RAR, archives inside
/// archives or on other providers) and moves out of an archive, and warns once per zip
/// that is rewritten.
fn archive_writes(lib: &Shared, op: &Op, warnings: &mut Vec<Warning>) -> Result<()> {
    let outer = |p: &VPath| p.split_archive().map(|(o, _)| o);
    let written: Vec<&VPath> = match op {
        Op::Copy { dst_dir, .. } => vec![dst_dir],
        Op::Move { src, dst_dir, .. } => {
            for s in src.iter().filter(|s| outer(s).is_some()) {
                anyhow::ensure!(
                    outer(s) == outer(dst_dir),
                    "cannot move out of an archive (copy instead): {}",
                    s.display()
                );
            }
            vec![dst_dir]
        }
        Op::Delete { paths } => paths.iter().collect(),
        Op::Rename { path, .. } => vec![path],
    };
    for p in written {
        let Some(archive) = outer(p) else { continue };
        keel_vfs::archive::editable(p)?;
        let bytes = lib
            .router
            .read()
            .provider_for(&archive)
            .and_then(|provider| provider.stat(&archive).ok())
            .map_or(0, |e| e.size);
        warnings.push(Warning::RewritesArchive {
            path: archive,
            bytes,
        });
    }
    Ok(())
}

/// Files and bytes under a folder inside an archive (its listing is cached).
fn archive_tree(provider: &dyn keel_vfs::Provider, dir: &VPath, depth: usize) -> (u64, u64) {
    let mut total = (0, 0);
    for e in provider.list(dir).unwrap_or_default() {
        let (bytes, files) = if e.kind == Kind::Dir && depth < 256 {
            archive_tree(provider, &e.path, depth + 1)
        } else {
            (e.size, 1)
        };
        total = (total.0 + bytes, total.1 + files);
    }
    total
}

fn check_name(name: &str) -> Result<()> {
    let bad_windows = cfg!(windows)
        && (name.contains(['<', '>', ':', '"', '|', '?', '*']) || name.ends_with(['.', ' ']));
    anyhow::ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains(['/', '\\', '\0'])
            && !bad_windows,
        "invalid name {name:?}"
    );
    Ok(())
}

/// From the index (works offline), else from the live filesystem.
fn probe(lib: &Shared, p: &VPath, warnings: &mut Vec<Warning>) -> Result<Probe> {
    if let Some((src, rel)) = lib.source_for(p) {
        if matches!(*src.status.read(), SourceStatus::Offline { .. }) {
            warnings.push(Warning::OfflineSource {
                source: src.id.clone(),
                label: src.def.label.clone(),
            });
        }
        if let Some(found) = indexed(&src, &rel)? {
            return Ok(found);
        }
    }
    warnings.push(Warning::NotIndexed { path: p.clone() });
    let router = lib.router.read().clone();
    let provider = router
        .provider_for(p)
        .with_context(|| format!("no provider for {}", p.display()))?;
    let entry = provider
        .stat(p)
        .with_context(|| format!("{} does not exist", p.display()))?;
    let is_dir = entry.kind == Kind::Dir;
    let (bytes, files) = match (is_dir, p.to_local_path()) {
        (false, _) => (entry.size, 1),
        // Counts folders too; good enough for a preview of an unindexed folder.
        (true, Some(local)) => keel_vfs::plan_size(&[local]).map_or((0, 0), |(b, n)| (b, n as u64)),
        (true, None) if p.split_archive().is_some() => archive_tree(&*provider, p, 0),
        (true, None) => (0, 0),
    };
    Ok(Probe {
        is_dir,
        files,
        bytes,
    })
}

fn indexed(src: &Source, rel: &str) -> Result<Option<Probe>> {
    let c = src.store.get()?;
    let Some((id, _)) = resolve(&c, rel, src.nocase())? else {
        return Ok(None);
    };
    let (kind, files, bytes): (i64, i64, i64) = c.query_row(
        "WITH RECURSIVE sub(id) AS (
             SELECT ?1 UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id)
         SELECT (SELECT kind FROM record WHERE id = ?1),
                count(*) FILTER (WHERE kind = 0),
                coalesce(sum(size) FILTER (WHERE kind = 0), 0)
         FROM record WHERE id IN (SELECT id FROM sub)",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(Some(Probe {
        is_dir: kind == crate::fsid::DIR,
        files: files as u64,
        bytes: bytes as u64,
    }))
}

/// Live when the provider answers; from the index when the source is offline.
fn exists(lib: &Shared, p: &VPath) -> bool {
    let live = lib
        .router
        .read()
        .provider_for(p)
        .map(|provider| provider.stat(p).is_ok());
    if live == Some(true) {
        return true;
    }
    match lib.source_for(p) {
        Some((src, rel)) if matches!(*src.status.read(), SourceStatus::Offline { .. }) => src
            .store
            .get()
            .ok()
            .and_then(|c| resolve(&c, &rel, src.nocase()).ok().flatten())
            .is_some(),
        _ => false,
    }
}

/// Whether no record other than one has sampled hash `h`: content no other file holds.
fn sampled_alone(lib: &Shared, h: &[u8]) -> Result<bool> {
    let sources: Vec<Arc<Source>> = lib.sources.read().clone();
    let mut n = 0;
    for s in sources {
        n += s.store.get()?.query_row(
            "SELECT count(*) FROM (SELECT 1 FROM record WHERE sampled_hash = ?1 LIMIT 2)",
            [h],
            |r| r.get::<_, i64>(0),
        )?;
        if n > 1 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `LastCopy` for each deleted path holding files whose content no counted record outside
/// the deletion holds (by content id, or a sampled hash no other record shares);
/// `SingleDomain` for files whose remaining copies all fall in one failure domain;
/// `CopiesOffline` for files whose remaining copies are all offline; `ContentUnverified`
/// for the other files without a content id yet, or drifted from it.
fn last_copies(lib: &Shared, paths: &[VPath], warnings: &mut Vec<Warning>) -> Result<()> {
    /// (content id, deleted records with it) under one deleted path.
    type Contents = Vec<(Vec<u8>, u64)>;
    let mut deleted: HashMap<Vec<u8>, HashSet<(String, i64)>> = HashMap::new();
    // (path, its content ids, its files whose sampled hash is unique)
    let mut per_path: Vec<(VPath, Contents, u64)> = Vec::new();
    for p in paths {
        let Some((src, rel)) = lib.source_for(p) else {
            continue;
        };
        let (rows, pending) = {
            let c = src.store.get()?;
            let Some((id, _)) = resolve(&c, &rel, src.nocase())? else {
                continue;
            };
            let rows: Vec<(Vec<u8>, i64)> = c
                .prepare_cached(
                    "WITH RECURSIVE sub(id) AS (
                         SELECT ?1 UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id)
                     SELECT cas_id, id FROM record
                     WHERE id IN (SELECT id FROM sub) AND cas_id IS NOT NULL AND drift IS NULL",
                )?
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let pending: Vec<Option<Vec<u8>>> = c
                .prepare_cached(
                    "WITH RECURSIVE sub(id) AS (
                         SELECT ?1 UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id)
                     SELECT sampled_hash FROM record
                     WHERE id IN (SELECT id FROM sub) AND kind = 0
                         AND (cas_id IS NULL OR drift IS NOT NULL)",
                )?
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            (rows, pending)
        };
        let (mut unverified, mut alone) = (0, 0);
        for sampled in pending {
            match sampled {
                Some(h) if sampled_alone(lib, &h)? => alone += 1,
                _ => unverified += 1,
            }
        }
        if unverified > 0 {
            warnings.push(Warning::ContentUnverified {
                path: p.clone(),
                files: unverified,
            });
        }
        let mut cas: HashMap<Vec<u8>, u64> = HashMap::new();
        for (c, id) in rows {
            deleted
                .entry(c.clone())
                .or_default()
                .insert((src.id.0.clone(), id));
            *cas.entry(c).or_default() += 1;
        }
        per_path.push((p.clone(), cas.into_iter().collect(), alone));
    }
    // Per content: (no counted copy left, left in one failure domain only). A hard link
    // outside the deletion keeps the content too.
    // ponytail: one lookup per content id per source; batch it if deletes of huge hashed
    // trees get slow.
    let mut left: HashMap<&[u8], crate::protect::Left> = HashMap::new();
    if !deleted.is_empty() {
        let vols = crate::protect::Volumes::load(lib)?;
        for (cas, records) in &deleted {
            left.insert(cas, crate::protect::after_delete(lib, &vols, cas, records)?);
        }
    }
    for (path, cas, alone) in per_path {
        let count = |pick: fn(&crate::protect::Left) -> bool| -> u64 {
            cas.iter()
                .filter(|(c, _)| pick(&left[c.as_slice()]))
                .map(|(_, n)| n)
                .sum()
        };
        let files = alone + count(|l| l.none);
        let single = count(|l| l.one_domain);
        let offline = count(|l| l.offline);
        if files > 0 {
            warnings.push(Warning::LastCopy {
                path: path.clone(),
                files,
            });
        }
        if single > 0 {
            warnings.push(Warning::SingleDomain {
                path: path.clone(),
                files: single,
            });
        }
        if offline > 0 {
            warnings.push(Warning::CopiesOffline {
                path,
                files: offline,
            });
        }
    }
    Ok(())
}

/// Items per batch: a batch's marks are written together before any of its items runs, a
/// copy or move transfers the batch in one call, and one cursor write ends it.
const STEP_BATCH: usize = 64;

/// Runs an `Op` in batches of top-level paths; the op_log row is written before the first
/// batch and completed at the end. The op (with the plan to check) is stored once, at
/// spawn; each batch writes only the cursor (`JobCtx::cursor`), once. A copy or move also
/// records, every few files or MiB, what its batch placed and the file it is writing
/// (`job_file` and the cursor's `unfinished`), so a resumed batch skips the files placed
/// and continues the one in progress instead of running again as a merge.
#[derive(Serialize, Deserialize)]
pub(crate) struct ExecJob {
    op: Op,
    /// The confirmed plan, checked against a fresh preview before the first step
    /// (`Plan::execute` without `recheck`).
    #[serde(default)]
    expect: Option<Plan>,
    next: usize,
    skipped: usize,
    log_id: Option<i64>,
    /// The items from `next` on that may have begun: written with the cursor before a
    /// batch's side effects, so a resumed job checks each before doing it again.
    #[serde(default)]
    marks: Vec<Started>,
    /// Copy and move: the file the running batch was writing when it last recorded.
    #[serde(default)]
    unfinished: Option<keel_vfs::ops::Unfinished>,
    /// Copy and move: files placed by the batches before `next`.
    #[serde(default)]
    files_done: u64,
    /// Copy and move: files in the confirmed plan (for "resumed at file N of M").
    #[serde(default)]
    files_total: Option<u64>,
    /// Tests: the library "closes" (the job stops where it is, as in a crash, and stays
    /// pending) once a transfer has written this many bytes.
    #[cfg(test)]
    #[serde(skip)]
    crash_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Started {
    item: usize,
    /// The copy/move target name existed before the step. Probed only where a re-run is
    /// not safe (rename on conflict: it would copy again under a new name); elsewhere true.
    target_existed: bool,
}

/// Where `item` ends up (copy, move, rename), None for a delete.
fn target(op: &Op, item: &VPath) -> Option<VPath> {
    match op {
        Op::Copy { dst_dir, .. } | Op::Move { dst_dir, .. } => Some(dst_dir.join(item.name())),
        Op::Rename { new_name, .. } => item.parent().map(|p| p.join(new_name)),
        Op::Delete { .. } => None,
    }
}

fn live(ctx: &JobCtx, p: &VPath) -> bool {
    ctx.router()
        .provider_for(p)
        .is_some_and(|provider| provider.stat(p).is_ok())
}

/// The marks of the batch starting at item `from`: up to [`STEP_BATCH`] items, cut short
/// so a resumed batch can tell what ran (no two targets with one name, no item inside
/// another, a rename-on-conflict onto a taken name only last).
fn batch_marks(ctx: &JobCtx, op: &Op, items: &[VPath], from: usize) -> Vec<Started> {
    let renames_new = matches!(
        op,
        Op::Copy {
            on_conflict: OnConflict::RenameNew,
            ..
        } | Op::Move {
            on_conflict: OnConflict::RenameNew,
            ..
        }
    );
    let mut marks: Vec<Started> = Vec::new();
    let mut names = std::collections::HashSet::new();
    for (i, item) in items.iter().enumerate().skip(from).take(STEP_BATCH) {
        let to = target(op, item);
        let clash = to
            .as_ref()
            .is_some_and(|t| !names.insert(t.name().to_lowercase()));
        let nested = marks.iter().any(|m| {
            let other = &items[m.item];
            relative(other, item).is_some() || relative(item, other).is_some()
        });
        if !marks.is_empty() && (clash || nested) {
            break;
        }
        let target_existed = to.is_some_and(|t| !renames_new || live(ctx, &t));
        marks.push(Started {
            item: i,
            target_existed,
        });
        if target_existed && renames_new {
            break;
        }
    }
    marks
}

/// Whether an item whose step may have begun before a crash certainly ran: a move, delete
/// or rename whose source is gone, a copy of a file whose target appeared (files are
/// placed whole). Anything else runs again; a copy or move then continues from what its
/// batch recorded.
fn ran(ctx: &JobCtx, op: &Op, item: &VPath, s: Started) -> bool {
    let appeared = !s.target_existed && target(op, item).is_some_and(|t| live(ctx, &t));
    match op {
        Op::Copy { .. } => appeared && !is_dir(ctx, item),
        _ => !live(ctx, item),
    }
}

fn is_dir(ctx: &JobCtx, p: &VPath) -> bool {
    ctx.router()
        .provider_for(p)
        .and_then(|provider| provider.stat(p).ok())
        .is_some_and(|e| e.kind == Kind::Dir)
}

/// Removes a copy or move's staging file (`Unfinished::staging`) that no run will continue.
fn drop_staging(ctx: &JobCtx, u: &keel_vfs::ops::Unfinished) {
    let removed = match u.staging.to_local_path() {
        Some(local) => std::fs::remove_file(local).map_err(anyhow::Error::from),
        None => ctx
            .router()
            .provider_for(&u.staging)
            .context("no provider")
            .and_then(|p| p.remove(&u.staging)),
    };
    if let Err(e) = removed {
        tracing::debug!("partial copy left behind: {e:#}");
    }
}

impl ExecJob {
    pub(crate) fn new(op: Op, expect: Option<Plan>, files_total: Option<u64>) -> ExecJob {
        ExecJob {
            op,
            expect,
            next: 0,
            skipped: 0,
            log_id: None,
            marks: Vec::new(),
            unfinished: None,
            files_done: 0,
            files_total,
            #[cfg(test)]
            crash_at: None,
        }
    }

    fn cursor(&self) -> serde_json::Value {
        serde_json::json!({
            "next": self.next,
            "skipped": self.skipped,
            "log_id": self.log_id,
            "marks": self.marks,
            "unfinished": self.unfinished,
            "files_done": self.files_done,
        })
    }

    fn transfers(&self) -> bool {
        matches!(self.op, Op::Copy { .. } | Op::Move { .. })
    }

    fn count(&mut self, ctx: &JobCtx, item: &VPath, ran: bool) -> Result<()> {
        if !ran {
            self.skipped += 1;
            let line = format!("skipped (no longer exists): {}", item.display());
            ctx.log(&oplog::redact_text(&line, &oplog::roots(&ctx.lib)))?;
        }
        Ok(())
    }

    /// "resumed at file N of M" for a copy or move continuing after a crash or a close.
    fn log_resume(&self, ctx: &JobCtx) -> Result<()> {
        let placed = ctx.placed()?.values().filter(|p| p.file.is_some()).count() as u64;
        let at = self.files_done + placed + 1;
        ctx.log(&match self.files_total {
            Some(total) => format!("resumed at file {} of {total}", at.min(total.max(1))),
            None => format!("resumed at file {at}"),
        })
    }

    /// Ends a batch: the cursor moves past it (and a copy or move's record of it goes).
    fn batch_done(&mut self, ctx: &JobCtx, items: &[VPath]) -> Result<()> {
        self.marks = batch_marks(ctx, &self.op, items, self.next);
        let progress = self.next as f32 / items.len() as f32;
        if !self.transfers() {
            return ctx.cursor(self.cursor(), progress);
        }
        ctx.record_placed(&[], &self.cursor(), progress, true)?;
        if ctx.stopping() {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    fn steps(&mut self, ctx: &JobCtx, items: &[VPath], mut marked: bool) -> Result<()> {
        let n = items.len();
        // Marks left by an earlier session: their items may have begun.
        if !marked && self.transfers() {
            self.log_resume(ctx)?;
            if !self.marks.is_empty() {
                // What certainly ran is settled; the rest of the batch runs again from
                // what it recorded.
                let (mut rest, mut seeds) = (Vec::new(), Vec::new());
                for s in self.marks.clone() {
                    let item = &items[s.item];
                    if ran(ctx, &self.op, item, s) {
                        ctx.log("resumed after the step had run")?;
                        after(ctx, &self.op, std::slice::from_ref(item));
                        continue;
                    }
                    // A folder target that was not there before the batch is its own.
                    if let Some(to) = target(&self.op, item).filter(|_| !s.target_existed) {
                        if is_dir(ctx, &to) {
                            seeds.push((item.clone(), to));
                        }
                    }
                    rest.push(item.clone());
                }
                let done = self.transfer_batch(ctx, &rest, n, Some(&seeds))?;
                for (item, ran) in rest.iter().zip(done) {
                    self.count(ctx, item, ran)?;
                }
                self.next += self.marks.len();
                self.batch_done(ctx, items)?;
                marked = true;
            }
        } else if !marked {
            // Settled marks go one by one: a stop here keeps the rest for the next resume.
            while let Some(s) = self.marks.first().copied().filter(|s| s.item == self.next) {
                if ctx.stopping() {
                    return Err(Cancelled.into());
                }
                let item = &items[self.next];
                let done = if ran(ctx, &self.op, item, s) {
                    ctx.log("resumed after the step had run")?;
                    after(ctx, &self.op, std::slice::from_ref(item));
                    true
                } else {
                    step(ctx, &self.op, item)?
                };
                self.count(ctx, item, done)?;
                self.marks.remove(0);
                self.next += 1;
            }
        }
        while self.next < n {
            if ctx.stopping() {
                return Err(Cancelled.into());
            }
            if !marked {
                self.marks = batch_marks(ctx, &self.op, items, self.next);
                ctx.cursor(self.cursor(), self.next as f32 / n as f32)?;
            }
            let batch = &items[self.next..self.next + self.marks.len()];
            let done = if self.transfers() {
                self.transfer_batch(ctx, batch, n, None)?
            } else if matches!(self.op, Op::Delete { .. })
                && batch.iter().all(|i| i.split_archive().is_some())
            {
                delete_entries(ctx, &self.op, batch)?
            } else {
                batch
                    .iter()
                    .map(|item| {
                        if ctx.stopping() {
                            return Err(Cancelled.into());
                        }
                        step(ctx, &self.op, item)
                    })
                    .collect::<Result<Vec<bool>>>()?
            };
            for (item, ran) in batch.iter().zip(done) {
                self.count(ctx, item, ran)?;
            }
            // One write: this batch done, and the next one marked.
            self.next += batch.len();
            self.batch_done(ctx, items)?;
            marked = true;
        }
        // A target renamed to avoid a conflict is found by listing its folder, once.
        if let Op::Copy {
            dst_dir,
            on_conflict: OnConflict::RenameNew,
            ..
        }
        | Op::Move {
            dst_dir,
            on_conflict: OnConflict::RenameNew,
            ..
        } = &self.op
        {
            refresh_children(ctx, dst_dir);
        }
        Ok(())
    }

    /// One batch of a copy or move (`n` items in the op): one transfer of its present
    /// items, recording what it placed and the file it writes. `resumed`: the batch ran
    /// before (a crash or a close) and continues from that record, with these (item,
    /// target) folders known to be its own. Per item, false when it no longer exists
    /// (skipped).
    fn transfer_batch(
        &mut self,
        ctx: &JobCtx,
        batch: &[VPath],
        n: usize,
        resumed: Option<&[(VPath, VPath)]>,
    ) -> Result<Vec<bool>> {
        let (Op::Copy {
            dst_dir,
            on_conflict,
            ..
        }
        | Op::Move {
            dst_dir,
            on_conflict,
            ..
        }) = &self.op
        else {
            anyhow::bail!("not a copy or move");
        };
        let ran = batch
            .iter()
            .map(|item| present(ctx, item))
            .collect::<Result<Vec<bool>>>()?;
        let todo: Vec<VPath> = batch
            .iter()
            .zip(&ran)
            .filter(|(_, ran)| **ran)
            .map(|(item, _)| item.clone())
            .collect();
        if todo.is_empty() {
            return Ok(ran);
        }
        let base = self.cursor();
        let progress = self.next as f32 / n as f32;
        let mut placed = HashMap::new();
        if let Some(seeds) = resumed {
            placed = ctx.placed()?;
            for (item, target) in seeds {
                // Keyed as the transfer keys local paths (normalized).
                let key = match item.to_local_path() {
                    Some(local) => VPath::local(keel_vfs::long(&local)?),
                    None => item.clone(),
                };
                let file = None;
                let target = target.clone();
                placed.entry(key).or_insert(keel_vfs::ops::Placed {
                    target,
                    file,
                    archive: None,
                });
            }
        }
        let mut journal =
            keel_vfs::ops::Journal::new(placed, self.unfinished.take(), |placed, unfinished| {
                let mut cursor = base.clone();
                cursor["unfinished"] = serde_json::to_value(unfinished)?;
                ctx.record_placed(&placed, &cursor, progress, false)
            });
        #[cfg(test)]
        let crash_at = self.crash_at;
        let report = |_p: keel_vfs::Progress| {
            #[cfg(test)]
            if crash_at.is_some_and(|at| _p.done_bytes >= at) {
                ctx.simulate_close();
            }
        };
        let result = keel_vfs::ops::transfer_resumable(
            &todo,
            dst_dir,
            matches!(self.op, Op::Move { .. }),
            (*on_conflict).into(),
            &report,
            ctx.stop_flag(),
            &ctx.router(),
            &mut journal,
        );
        let (notes, files, left) = (
            std::mem::take(&mut journal.notes),
            journal.files,
            journal.unfinished.take(),
        );
        drop(journal);
        for note in notes {
            ctx.log(&note)?;
        }
        match result {
            Ok(()) => {
                // The file it was writing is no longer there to finish.
                if let Some(u) = left {
                    drop_staging(ctx, &u);
                }
                self.files_done += files;
                after(ctx, &self.op, &todo);
                Ok(ran)
            }
            Err(e) => {
                self.unfinished = left;
                Err(e)
            }
        }
    }
}

impl Job for ExecJob {
    fn kind(&self) -> &'static str {
        "op"
    }

    fn run(&mut self, ctx: &JobCtx) -> Result<()> {
        let lib = ctx.lib.clone();
        let items = self.op.items();
        let n = items.len();
        // The marks were written in this session: their items have not begun.
        let mut marked = false;
        let log_id = match self.log_id {
            Some(id) => id,
            None => {
                if let Some(expect) = &self.expect {
                    let now = preview(&lib, self.op.clone())?;
                    if !same_shape(&now, expect) {
                        return Err(PlanChanged(Box::new(now)).into());
                    }
                }
                let id = oplog::record(&lib, self.op.kind(), &self.op.payload(), "running")?;
                self.log_id = Some(id);
                if self.marks.is_empty() {
                    self.marks = batch_marks(ctx, &self.op, &items, self.next);
                    marked = true;
                }
                ctx.cursor(self.cursor(), 0.0)?;
                id
            }
        };
        let result = self.steps(ctx, &items, marked);
        let summary = match &result {
            Ok(()) if self.skipped == 0 => "ok".to_owned(),
            Ok(()) => format!("{} skipped", self.skipped),
            // Resumes on the next open (a partial copy stays for it).
            Err(_) if ctx.closing() => return result,
            Err(e) if e.is::<Cancelled>() || ctx.stopping() => {
                format!("cancelled after {} of {}", self.next, n)
            }
            Err(e) => format!("failed after {} of {}: {e:#}", self.next, n),
        };
        // Ends here: nothing will continue a partial copy.
        if let Some(u) = self.unfinished.take() {
            drop_staging(ctx, &u);
        }
        oplog::set_result(&lib, log_id, &summary, result.is_ok())?;
        result
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<ExecJob>(v)?))
    }
}

/// Whether `item` exists; fails when it is missing because its source cannot be reached
/// (the rest of the operation is not run against it).
fn present(ctx: &JobCtx, item: &VPath) -> Result<bool> {
    let router = ctx.router();
    let provider = router
        .provider_for(item)
        .with_context(|| format!("no provider for {}", item.display()))?;
    if provider.stat(item).is_ok() {
        return Ok(true);
    }
    if let Some((src, _)) = ctx.lib.source_for(item) {
        let reachable = router
            .provider_for(&src.def.root)
            .is_some_and(|p| p.stat(&src.def.root).is_ok());
        if !reachable {
            *src.status.write() = SourceStatus::Offline {
                last_seen: src
                    .store
                    .meta("last_full_walk")?
                    .and_then(|t| t.parse().ok()),
                reason: OfflineReason::Unreachable,
            };
            anyhow::bail!("source {} is offline", src.def.label);
        }
    }
    Ok(false)
}

/// Deletes of entries inside zips: one rewrite of each archive for the whole batch. Per
/// item, false when it no longer exists (skipped).
fn delete_entries(ctx: &JobCtx, op: &Op, batch: &[VPath]) -> Result<Vec<bool>> {
    let ran = batch
        .iter()
        .map(|item| present(ctx, item))
        .collect::<Result<Vec<bool>>>()?;
    let todo: Vec<VPath> = batch
        .iter()
        .zip(&ran)
        .filter(|(_, ran)| **ran)
        .map(|(item, _)| item.clone())
        .collect();
    if !todo.is_empty() {
        keel_vfs::ops::remove_entries(&todo, &|_| {}, ctx.stop_flag())?;
        after(ctx, op, &todo);
    }
    Ok(ran)
}

/// One delete or rename of a top-level path; false when it no longer exists (skipped).
fn step(ctx: &JobCtx, op: &Op, item: &VPath) -> Result<bool> {
    if !present(ctx, item)? {
        return Ok(false);
    }
    let router = ctx.router();
    match op {
        Op::Copy { .. } | Op::Move { .. } => anyhow::bail!("copies and moves run in batches"),
        Op::Delete { .. } => router
            .provider_for(item)
            .with_context(|| format!("no provider for {}", item.display()))?
            .remove(item)?,
        Op::Rename { .. } => {
            let to =
                target(op, item).with_context(|| format!("cannot rename {}", item.display()))?;
            router
                .provider_for(item)
                .with_context(|| format!("no provider for {}", item.display()))?
                .rename(item, &to)?;
        }
    }
    after(ctx, op, std::slice::from_ref(item));
    Ok(true)
}

/// Updates the index for what steps changed: destinations first (a same-volume move is
/// then found by identity; a folder copied into an existing one is walked whole), then the
/// paths that went away.
fn after(ctx: &JobCtx, op: &Op, items: &[VPath]) {
    let targets: Vec<VPath> = items.iter().filter_map(|i| target(op, i)).collect();
    reindex(ctx, &targets, true);
    if !matches!(op, Op::Copy { .. }) {
        reindex(ctx, items, false);
    }
}

/// Brings the index up to date for local paths the job changed, one transaction per
/// source (the watcher, if any, would get there too; remote sources catch up at their next
/// poll). `walk`: walk folders whole.
fn reindex(ctx: &JobCtx, paths: &[VPath], walk: bool) {
    let mut by_source: Vec<(Arc<Source>, Vec<VPath>)> = Vec::new();
    for p in paths.iter().filter(|p| p.to_local_path().is_some()) {
        let Some((src, _)) = ctx.lib.source_for(p) else {
            continue;
        };
        match by_source.iter_mut().find(|(s, _)| s.id == src.id) {
            Some((_, ps)) => ps.push(p.clone()),
            None => by_source.push((src, vec![p.clone()])),
        }
    }
    for (src, ps) in by_source {
        if let Err(e) = Indexer::apply_paths(&src, &ps, walk) {
            tracing::debug!("reindex after op: {e:#}");
        }
    }
}

/// Indexes children of a local folder that the index does not have yet (targets a copy
/// renamed to avoid a conflict).
fn refresh_children(ctx: &JobCtx, dir: &VPath) {
    let (Some((src, rel)), Some(local)) = (ctx.lib.source_for(dir), dir.to_local_path()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(local) else {
        return;
    };
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    let Ok(c) = src.store.get() else {
        return;
    };
    let unknown: Vec<VPath> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            resolve(&c, &format!("{prefix}{name}"), src.nocase())
                .ok()
                .flatten()
                .is_none()
        })
        .map(|name| dir.join(&name))
        .collect();
    drop(c);
    reindex(ctx, &unknown, true);
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
