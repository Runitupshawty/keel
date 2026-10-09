//! `validate -> preview -> execute` for every mutating operation. The preview is projected
//! from the index (so it works for offline sources) and falls back to the live filesystem for
//! paths the index does not know; execution re-validates, runs as a durable job and writes a
//! (redacted) op_log entry.

use crate::index::{resolve, ChangeEvent};
use crate::jobs::{Job, JobCtx, JobId};
use crate::library::{relative, Shared, Source, SourceStatus};
use crate::{oplog, Cancelled, Indexer, Library, SourceId};
use anyhow::{Context, Result};
use keel_vfs::{ops::Conflict, Kind, VPath};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// Deleting `path` removes the only indexed copy of `files` files' content.
    LastCopy { path: VPath, files: u64 },
    /// Projected from the source's last generation; execution waits for it to come back.
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
    /// `files` deleted files have no content id yet, so whether another copy exists is not
    /// known (no `LastCopy` can be computed for them).
    ContentUnverified { path: VPath, files: u64 },
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

impl Plan {
    /// Re-validates against the current state (a path that vanished fails here), then runs
    /// the operation as a durable job that logs to op_log. A preview that no longer matches
    /// is noted in the job log.
    pub fn execute(self, lib: &Library) -> Result<JobId> {
        let fresh = preview(&lib.shared, self.op.clone())?;
        lib.jobs().spawn(Box::new(ExecJob {
            diverged: fresh.changes != self.changes,
            op: fresh.op,
            next: 0,
            skipped: 0,
            log_id: None,
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
                to.path.eq_ignore_ascii_case(&path.path)
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

/// `LastCopy` for each deleted path holding files whose content id has no confirmed record
/// outside the deletion; `ContentUnverified` for files without a content id yet.
fn last_copies(lib: &Shared, paths: &[VPath], warnings: &mut Vec<Warning>) -> Result<()> {
    /// (content id, files with it) under one deleted path.
    type Contents = Vec<(Vec<u8>, u64)>;
    let mut deleted: HashMap<Vec<u8>, u64> = HashMap::new();
    let mut per_path: Vec<(VPath, Contents)> = Vec::new();
    for p in paths {
        let Some((src, rel)) = lib.source_for(p) else {
            continue;
        };
        let c = src.store.get()?;
        let Some((id, _)) = resolve(&c, &rel, src.nocase())? else {
            continue;
        };
        let mut stmt = c.prepare_cached(
            "WITH RECURSIVE sub(id) AS (
                 SELECT ?1 UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id)
             SELECT cas_id, count(*) FROM record
             WHERE id IN (SELECT id FROM sub) AND cas_id IS NOT NULL GROUP BY cas_id",
        )?;
        let cas: Contents = stmt
            .query_map([id], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<_>>()?;
        let unverified: i64 = c
            .prepare_cached(
                "WITH RECURSIVE sub(id) AS (
                     SELECT ?1 UNION ALL SELECT r.id FROM record r JOIN sub ON r.parent = sub.id)
                 SELECT count(*) FROM record
                 WHERE id IN (SELECT id FROM sub) AND kind = 0 AND cas_id IS NULL",
            )?
            .query_row([id], |r| r.get(0))?;
        if unverified > 0 {
            warnings.push(Warning::ContentUnverified {
                path: p.clone(),
                files: unverified as u64,
            });
        }
        for (c, n) in &cas {
            *deleted.entry(c.clone()).or_default() += n;
        }
        per_path.push((p.clone(), cas));
    }
    if deleted.is_empty() {
        return Ok(());
    }
    // Copies elsewhere count only with a confirmed (whole-file) content id: an unconfirmed
    // shared sampled hash is not proof of a second copy.
    // ponytail: one count per content id per source; batch it if deletes of huge hashed
    // trees get slow.
    let mut total: HashMap<&[u8], u64> = HashMap::new();
    for cas in deleted.keys() {
        let n = crate::hash::copies(lib, cas)?.iter().map(|(_, n)| n).sum();
        total.insert(cas, n);
    }
    for (path, cas) in per_path {
        let files: u64 = cas
            .iter()
            .filter(|(c, _)| total[c.as_slice()] <= deleted[c])
            .map(|(_, n)| n)
            .sum();
        if files > 0 {
            warnings.push(Warning::LastCopy { path, files });
        }
    }
    Ok(())
}

/// Runs an `Op` one top-level path per step; the op_log row is written before the first
/// step and completed at the end.
#[derive(Serialize, Deserialize)]
pub(crate) struct ExecJob {
    op: Op,
    next: usize,
    skipped: usize,
    log_id: Option<i64>,
    diverged: bool,
}

impl Job for ExecJob {
    fn kind(&self) -> &'static str {
        "op"
    }

    fn run(&mut self, ctx: &JobCtx) -> Result<()> {
        let lib = ctx.lib.clone();
        let log_id = match self.log_id {
            Some(id) => id,
            None => {
                let id = oplog::record(&lib, self.op.kind(), &self.op.payload(), "running")?;
                self.log_id = Some(id);
                if self.diverged {
                    ctx.log("the sources changed since the preview")?;
                }
                ctx.checkpoint(self.checkpoint(), 0.0)?;
                id
            }
        };
        let items = self.op.items();
        let result = (|| -> Result<()> {
            while self.next < items.len() {
                if ctx.stopping() {
                    return Err(Cancelled.into());
                }
                let item = &items[self.next];
                if !step(ctx, &self.op, item)? {
                    self.skipped += 1;
                    let line = format!("skipped (no longer exists): {}", item.display());
                    ctx.log(&oplog::redact_text(&line, &oplog::roots(&lib)))?;
                }
                self.next += 1;
                ctx.checkpoint(self.checkpoint(), self.next as f32 / items.len() as f32)?;
            }
            Ok(())
        })();
        let summary = match &result {
            Ok(()) if self.skipped == 0 => "ok".to_owned(),
            Ok(()) => format!("ok, {} skipped", self.skipped),
            // Resumes on the next open.
            Err(_) if ctx.closing() => return result,
            Err(e) if e.is::<Cancelled>() || ctx.stopping() => {
                format!("cancelled after {} of {}", self.next, items.len())
            }
            Err(e) => format!("failed after {} of {}: {e:#}", self.next, items.len()),
        };
        oplog::set_result(&lib, log_id, &summary)?;
        result
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<ExecJob>(v)?))
    }
}

/// One top-level path; false when it no longer exists (skipped).
fn step(ctx: &JobCtx, op: &Op, item: &VPath) -> Result<bool> {
    let router = ctx.router();
    let provider = router
        .provider_for(item)
        .with_context(|| format!("no provider for {}", item.display()))?;
    if provider.stat(item).is_err() {
        return Ok(false);
    }
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
            let mv = matches!(op, Op::Move { .. });
            keel_vfs::ops::transfer(
                std::slice::from_ref(item),
                dst_dir,
                mv,
                (*on_conflict).into(),
                &|_| {},
                ctx.stop_flag(),
                &router,
            )?;
            // Destination first: a same-volume move is then found by identity.
            reindex(ctx, &dst_dir.join(item.name()), true);
            refresh_children(ctx, dst_dir);
            if mv {
                reindex(ctx, item, false);
            }
        }
        Op::Delete { .. } => {
            provider.remove(item)?;
            reindex(ctx, item, false);
        }
        Op::Rename { new_name, .. } => {
            let to = item
                .parent()
                .with_context(|| format!("cannot rename {}", item.display()))?
                .join(new_name);
            provider.rename(item, &to)?;
            reindex(ctx, &to, true);
            reindex(ctx, item, false);
        }
    }
    Ok(true)
}

/// Brings the index up to date for a local path the job changed (the watcher, if any,
/// would get there too; remote sources catch up at their next poll).
fn reindex(ctx: &JobCtx, p: &VPath, present: bool) {
    let Some((src, _)) = ctx.lib.source_for(p) else {
        return;
    };
    if p.to_local_path().is_none() {
        return;
    }
    let ev = if present {
        ChangeEvent::Changed(p.clone())
    } else {
        ChangeEvent::Removed(p.clone())
    };
    if let Err(e) = Indexer::apply_change(&src, ev) {
        tracing::debug!("reindex after op: {e:#}");
    }
}

/// Indexes children of a local folder that the index does not have yet (a copy that
/// renamed its target to avoid a conflict).
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
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let known = src
            .store
            .get()
            .ok()
            .and_then(|c| resolve(&c, &format!("{prefix}{name}"), src.nocase()).ok())
            .flatten()
            .is_some();
        if !known {
            reindex(ctx, &dir.join(&name), true);
        }
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
