//! Integrity (spec 2.10 "Protection", Task 33): re-hash a random sample of confirmed files
//! and mark *drift* where the bytes changed although size, mtime and change time did not
//! (bit rot, a tool that restores timestamps). Durable, idle priority, like hashing.

use crate::hash::{background_thread, full_hash, skip_reason, wait_until_idle, SkippedSource};
use crate::jobs::{Job, JobCtx, JobId};
use crate::library::{Shared, Source};
use crate::{Cancelled, Library, SourceId};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

/// Default sample per run: 1 % of the confirmed files of each source.
pub const DEFAULT_SAMPLE_PCT: f64 = 1.0;
/// Default schedule: weekly.
pub const INTEGRITY_EVERY: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// `library.db` meta: when the last integrity check ended (unix seconds).
const LAST: &str = "integrity_last";
/// Files per checkpoint.
const CHECKPOINT_EVERY: usize = 200;

/// What an integrity check did (its `JobInfo::result`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityResult {
    /// Files re-hashed.
    pub checked: u64,
    /// Of those, files whose bytes changed under unchanged metadata (now marked).
    pub drifted: u64,
    /// Files whose metadata changed since indexed (left to the indexer).
    pub changed: u64,
    pub unreadable: u64,
    pub skipped: Vec<SkippedSource>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IntegrityJob {
    /// One source, or every source.
    pub source: Option<SourceId>,
    /// Percent of each source's confirmed files to check (1.0 = 1 %).
    pub sample_pct: f64,
    picked: bool,
    /// The sample, by source: written once (the checkpoint after the draw).
    todo: Vec<(SourceId, i64)>,
    total: u64,
    /// The cursor: `todo[next..]` is still to check (with `result`, all a step writes).
    #[serde(default)]
    next: usize,
    result: IntegrityResult,
    #[serde(skip)]
    battery: Option<(Instant, bool)>,
}

/// A sampled record as indexed: (path, size, mtime, change time, content id).
type Indexed = (String, i64, Option<i64>, Option<i64>, Vec<u8>);

enum Check {
    Same,
    Drift,
    Changed,
    Unreadable,
}

impl IntegrityJob {
    pub const KIND: &'static str = "integrity";

    pub fn new(source: Option<SourceId>, sample_pct: f64) -> IntegrityJob {
        IntegrityJob {
            source,
            sample_pct: sample_pct.clamp(0.0, 100.0),
            ..IntegrityJob::default()
        }
    }

    /// Draws the sample: `sample_pct` % (at least one) of each source's confirmed files.
    fn pick(&mut self, lib: &Shared) -> Result<()> {
        let sources: Vec<Arc<Source>> = lib.sources.read().clone();
        for s in sources
            .iter()
            .filter(|s| self.source.as_ref().is_none_or(|id| *id == s.id))
        {
            if let Some(reason) = skip_reason(s) {
                self.result.skipped.push(SkippedSource {
                    source: s.id.clone(),
                    label: s.def.label.clone(),
                    reason,
                });
                continue;
            }
            let c = s.store.get()?;
            let n: i64 = c.query_row(
                "SELECT count(*) FROM record WHERE kind = 0 AND cas_id IS NOT NULL AND drift IS NULL",
                [],
                |r| r.get(0),
            )?;
            if n == 0 {
                continue;
            }
            let take = ((n as f64 * self.sample_pct / 100.0).ceil() as i64).clamp(1, n);
            let mut stmt = c.prepare(
                "SELECT id FROM record WHERE kind = 0 AND cas_id IS NOT NULL AND drift IS NULL
                 ORDER BY random() LIMIT ?1",
            )?;
            let ids = stmt.query_map([take], |r| r.get::<_, i64>(0))?;
            for id in ids {
                self.todo.push((s.id.clone(), id?));
            }
        }
        self.total = self.todo.len() as u64;
        self.picked = true;
        Ok(())
    }

    fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.next as f32 / self.total as f32).min(0.99)
        }
    }
}

/// Re-hashes record `id` of `src` and compares.
fn check(ctx: &JobCtx, src: &Source, id: i64) -> Result<Option<Check>> {
    let row: Option<Indexed> = src
        .store
        .get()?
        .query_row(
            "SELECT path, size, mtime, ctime, cas_id FROM record
             WHERE id = ?1 AND kind = 0 AND cas_id IS NOT NULL AND drift IS NULL",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    // Gone, re-hashed or already marked since the sample was drawn.
    let Some((rel, size, mtime, ctime, cas)) = row else {
        return Ok(None);
    };
    let Some(path) = src.absolute(&rel).to_local_path() else {
        return Ok(None);
    };
    let same_meta = || {
        crate::fsid::stat(&path)
            .map(|now| (now.size, now.mtime, now.ctime) == (size, mtime, ctime))
            .ok()
    };
    match same_meta() {
        None => return Ok(Some(Check::Unreadable)),
        Some(false) => return Ok(Some(Check::Changed)),
        Some(true) => {}
    }
    let full = match full_hash(&path, size as u64, ctx.stop_flag()) {
        Ok(full) => full,
        Err(e) if e.is::<Cancelled>() => return Err(e),
        Err(e) => {
            tracing::debug!("integrity {}: {e:#}", path.display());
            return Ok(Some(Check::Unreadable));
        }
    };
    if full[..] == cas[..] {
        return Ok(Some(Check::Same));
    }
    // Written to while being read: metadata moved on, the indexer will see it.
    if same_meta() != Some(true) {
        return Ok(Some(Check::Changed));
    }
    let marked = src.store.get()?.execute(
        "UPDATE record SET drift = ?2
         WHERE id = ?1 AND cas_id = ?3 AND size = ?4 AND mtime IS ?5 AND ctime IS ?6",
        params![id, crate::now(), cas, size, mtime, ctime],
    )?;
    Ok(Some(if marked == 1 {
        Check::Drift
    } else {
        Check::Changed
    }))
}

impl Job for IntegrityJob {
    fn kind(&self) -> &'static str {
        Self::KIND
    }

    fn run(&mut self, ctx: &JobCtx) -> Result<()> {
        background_thread();
        let lib = ctx.lib.clone();
        if !self.picked {
            self.pick(&lib)?;
            ctx.checkpoint(self.checkpoint(), 0.0)?;
        }
        let mut since = 0;
        while let Some((sid, id)) = self.todo.get(self.next).cloned() {
            let src = lib.sources.read().iter().find(|s| s.id == sid).cloned();
            let usable =
                src.filter(|s| !s.removed.load(Ordering::SeqCst) && skip_reason(s).is_none());
            if let Some(src) = usable {
                wait_until_idle(ctx, &mut self.battery, true)?;
                match check(ctx, &src, id)? {
                    Some(Check::Same) => self.result.checked += 1,
                    Some(Check::Drift) => {
                        self.result.checked += 1;
                        self.result.drifted += 1;
                        ctx.log(&format!("{}: drift in record {id}", src.def.label))?;
                    }
                    Some(Check::Changed) => self.result.changed += 1,
                    Some(Check::Unreadable) => self.result.unreadable += 1,
                    None => {}
                }
            }
            self.next += 1;
            since += 1;
            if since >= CHECKPOINT_EVERY {
                since = 0;
                // The sample stays as written; only the cursor and the counts move.
                let cursor = serde_json::json!({ "next": self.next, "result": self.result });
                ctx.cursor(cursor, self.progress())?;
            }
        }
        lib.db.set_meta(LAST, &crate::now().to_string())?;
        crate::protect::recount(&lib)?;
        ctx.log(&format!(
            "checked {} files: {} drifted, {} changed since indexed, {} unreadable",
            self.result.checked, self.result.drifted, self.result.changed, self.result.unreadable
        ))?;
        ctx.set_result(serde_json::to_value(&self.result)?)?;
        Ok(())
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<IntegrityJob>(v)?))
    }
}

/// A queued or running integrity job in `library.db`.
fn pending(lib: &Shared) -> Result<Option<JobId>> {
    Ok(lib
        .db
        .get()?
        .query_row(
            "SELECT id FROM job WHERE kind = ?1 AND status IN ('queued', 'running')
             ORDER BY id DESC LIMIT 1",
            [IntegrityJob::KIND],
            |r| r.get(0),
        )
        .optional()?)
}

impl Library {
    /// Re-hashes `sample_pct` % of the confirmed files of `source` (or of every source) as
    /// a durable idle-priority job; drift is marked on the records (`ProtectionSummary`).
    pub fn integrity(&self, source: Option<SourceId>, sample_pct: f64) -> Result<JobId> {
        self.jobs()
            .spawn(Box::new(IntegrityJob::new(source, sample_pct)))
    }

    /// Starts an integrity check of every source when the last one ended `every` ago or
    /// longer and none is pending (the first call only starts the clock). Call it now and
    /// then (cheap).
    pub fn schedule_integrity(&self, sample_pct: f64, every: Duration) -> Result<Option<JobId>> {
        if pending(&self.shared)?.is_some() {
            return Ok(None);
        }
        // A new library's first check is one interval away (its files are being hashed).
        let Some(last) = self
            .shared
            .db
            .meta(LAST)?
            .and_then(|t| t.parse::<i64>().ok())
        else {
            self.shared.db.set_meta(LAST, &crate::now().to_string())?;
            return Ok(None);
        };
        if crate::now() - last < every.as_secs() as i64 {
            return Ok(None);
        }
        self.integrity(None, sample_pct).map(Some)
    }
}
