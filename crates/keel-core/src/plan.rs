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
        lib.jobs().spawn(Box::new(ExecJob {
            op: self.op,
            expect,
            next: 0,
            skipped: 0,
            log_id: None,
            marks: Vec::new(),
        }))
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
    let entry = router
        .provider_for(p)
        .with_context(|| format!("no provider for {}", p.display()))?
        .stat(p)
        .with_context(|| format!("{} does not exist", p.display()))?;
    let is_dir = entry.kind == Kind::Dir;
    let (bytes, files) = match (is_dir, p.to_local_path()) {
        (false, _) => (entry.size, 1),
        // Counts folders too; good enough for a preview of an unindexed folder.
        (true, Some(local)) => keel_vfs::plan_size(&[local]).map_or((0, 0), |(b, n)| (b, n as u64)),
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
/// spawn; each batch writes only the cursor (`JobCtx::cursor`), once.
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Started {
    item: usize,
    /// The copy/move target name existed before the step. Probed only where a re-run is
    /// not safe (rename on conflict: it would copy again under a new name); elsewhere true,
    /// so a begun step simply runs again (skip and overwrite are idempotent, and a skip
    /// merges into a folder copied halfway).
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

/// What a resumed job does with an item whose step may have begun before a crash.
enum Resume {
    /// The step happened: only the index catches up.
    Done,
    /// It began (its target appeared): run it again as a merge that skips what exists, so
    /// a folder copy killed halfway gets its remaining files.
    Merge,
    Run,
}

/// A move, delete or rename happened when its source is gone; a copy of a file when its
/// target appeared (files are placed whole). A folder copy or move whose target appeared
/// may be partial: merged. (A copy that renames on conflict onto a name that existed
/// cannot tell; it runs again.)
fn resume(ctx: &JobCtx, op: &Op, item: &VPath, s: Started) -> Resume {
    let appeared = !s.target_existed && target(op, item).is_some_and(|t| live(ctx, &t));
    let is_dir = || {
        ctx.router()
            .provider_for(item)
            .and_then(|p| p.stat(item).ok())
            .is_some_and(|e| e.kind == Kind::Dir)
    };
    match op {
        Op::Copy { .. } if appeared && is_dir() => Resume::Merge,
        Op::Copy { .. } if appeared => Resume::Done,
        Op::Copy { .. } => Resume::Run,
        _ if !live(ctx, item) => Resume::Done,
        Op::Move { .. } if appeared => Resume::Merge,
        _ => Resume::Run,
    }
}

impl ExecJob {
    fn cursor(&self) -> serde_json::Value {
        serde_json::json!({
            "next": self.next,
            "skipped": self.skipped,
            "log_id": self.log_id,
            "marks": self.marks,
        })
    }

    fn count(&mut self, ctx: &JobCtx, item: &VPath, ran: bool) -> Result<()> {
        if !ran {
            self.skipped += 1;
            let line = format!("skipped (no longer exists): {}", item.display());
            ctx.log(&oplog::redact_text(&line, &oplog::roots(&ctx.lib)))?;
        }
        Ok(())
    }

    fn steps(&mut self, ctx: &JobCtx, items: &[VPath], mut marked: bool) -> Result<()> {
        let n = items.len();
        // Marks left by an earlier session: their items may have begun, each is settled
        // on its own.
        if !marked {
            // Settled marks go one by one: a stop here keeps the rest for the next resume.
            while let Some(s) = self.marks.first().copied().filter(|s| s.item == self.next) {
                if ctx.stopping() {
                    return Err(Cancelled.into());
                }
                let item = &items[self.next];
                let ran = match resume(ctx, &self.op, item, s) {
                    Resume::Done => {
                        ctx.log("resumed after the step had run")?;
                        after(ctx, &self.op, std::slice::from_ref(item));
                        true
                    }
                    Resume::Merge => {
                        ctx.log("resumed a step that had begun: merging")?;
                        step(ctx, &self.op, item, Some(OnConflict::Skip))?
                    }
                    Resume::Run => step(ctx, &self.op, item, None)?,
                };
                self.count(ctx, item, ran)?;
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
            let ran = run_batch(ctx, &self.op, batch)?;
            for (item, ran) in batch.iter().zip(ran) {
                self.count(ctx, item, ran)?;
            }
            // One write: this batch done, and the next one marked.
            self.next += batch.len();
            self.marks = batch_marks(ctx, &self.op, items, self.next);
            marked = true;
            ctx.cursor(self.cursor(), self.next as f32 / n as f32)?;
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
            // Resumes on the next open.
            Err(_) if ctx.closing() => return result,
            Err(e) if e.is::<Cancelled>() || ctx.stopping() => {
                format!("cancelled after {} of {}", self.next, n)
            }
            Err(e) => format!("failed after {} of {}: {e:#}", self.next, n),
        };
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

/// One batch; per item, false when it no longer exists (skipped). A copy or move is one
/// transfer of the batch's present items.
fn run_batch(ctx: &JobCtx, op: &Op, batch: &[VPath]) -> Result<Vec<bool>> {
    let (Op::Copy {
        dst_dir,
        on_conflict,
        ..
    }
    | Op::Move {
        dst_dir,
        on_conflict,
        ..
    }) = op
    else {
        return batch
            .iter()
            .map(|item| {
                if ctx.stopping() {
                    return Err(Cancelled.into());
                }
                step(ctx, op, item, None)
            })
            .collect();
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
    if !todo.is_empty() {
        keel_vfs::ops::transfer(
            &todo,
            dst_dir,
            matches!(op, Op::Move { .. }),
            (*on_conflict).into(),
            &|_| {},
            ctx.stop_flag(),
            &ctx.router(),
        )?;
        after(ctx, op, &todo);
    }
    Ok(ran)
}

/// One top-level path (with `conflict` instead of the op's own, when given); false when it
/// no longer exists (skipped).
fn step(ctx: &JobCtx, op: &Op, item: &VPath, conflict: Option<OnConflict>) -> Result<bool> {
    if !present(ctx, item)? {
        return Ok(false);
    }
    let router = ctx.router();
    match op {
        Op::Copy {
            dst_dir,
            on_conflict,
            ..
        }
        | Op::Move {
            dst_dir,
            on_conflict,
            ..
        } => {
            keel_vfs::ops::transfer(
                std::slice::from_ref(item),
                dst_dir,
                matches!(op, Op::Move { .. }),
                conflict.unwrap_or(*on_conflict).into(),
                &|_| {},
                ctx.stop_flag(),
                &router,
            )?;
        }
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
