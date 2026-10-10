//! The sidecar job: makes `Thumb256` + `Meta` sidecars for every image and video record of a
//! source (thumbnails at 1024 px and video strips are made on demand), fills the source
//! store's `media` table, at idle priority on its own thread, pausing on battery like hashing
//! and for 1 s after user activity (a window asked for it, and its notes come every 4 s).

use crate::hash::background_thread;
use crate::index::{LINK, UNREADABLE};
use crate::jobs::{Job, JobCtx, JobId};
use crate::library::Source;
use crate::media::{self, MediaMeta};
use crate::sidecars::{self, SidecarKey, SidecarKind, Sidecars};
use crate::{Library, SourceId};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Instant,
};

/// Records per checkpoint.
// Small under test: the resume test reaches a checkpoint on a slow CI runner in time.
const CHECKPOINT_EVERY: usize = if cfg!(test) { 50 } else { 500 };
/// Errors written to the job log (the rest are only counted).
const LOGGED_ERRORS: u64 = 20;

#[derive(Debug, Serialize, Deserialize)]
pub struct SidecarJob {
    source: SourceId,
    /// The sidecar store (`<data dir>/sidecars`).
    root: PathBuf,
    /// Records up to this id are done.
    after: i64,
    counted: bool,
    pub total: u64,
    pub done: u64,
    /// Records whose sidecars were (re)made or moved.
    pub made: u64,
    /// Records that failed (corrupt files, which are recorded in their `meta.json`, and
    /// transient failures, which the next run retries).
    pub errors: u64,
    #[serde(skip)]
    battery: Option<(Instant, bool)>,
}

/// One media record.
struct Pending {
    id: i64,
    path: String,
    size: u64,
    mtime: i64,
    cas: Option<Vec<u8>>,
}

impl SidecarJob {
    pub const KIND: &'static str = "sidecar";

    pub fn new(source: SourceId, sidecars: &Path) -> SidecarJob {
        SidecarJob {
            source,
            root: sidecars.to_owned(),
            after: 0,
            counted: false,
            total: 0,
            done: 0,
            made: 0,
            errors: 0,
            battery: None,
        }
    }

    /// Pauses for 1 s after user activity (always) and on battery like hashing.
    fn wait_until_idle(&mut self, ctx: &JobCtx) -> Result<()> {
        crate::hash::wait_until_idle(ctx, &mut self.battery, crate::library::SIDECAR_PAUSE)
    }

    fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).min(0.99)
        }
    }
}

/// Makes (or moves) one record's sidecars and stores its `media` row. Returns whether
/// anything was made or moved.
fn process(sidecars: &Sidecars, src: &Source, root: &Path, rec: &Pending) -> Result<bool> {
    let path = root.join(&rec.path);
    let cas = rec
        .cas
        .as_deref()
        .and_then(|c| <[u8; 32]>::try_from(c).ok());
    let key = SidecarKey::local(&path, rec.mtime, rec.size, cas);
    let (dir, pdir) = (key.dir_name(), key.by_path().dir_name());
    let row: Option<(String, String)> = src
        .store
        .get()?
        .query_row(
            "SELECT key, pkey FROM media WHERE record = ?1",
            [rec.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    // The thumbnail is there, or recorded as impossible.
    let done = |m: &MediaMeta| {
        m.failure(SidecarKind::Thumb256).is_some()
            || sidecars.root().join(&dir).join("thumb-256.webp").is_file()
    };
    if row.as_ref().is_some_and(|(k, _)| *k == dir) && sidecars.meta(&key).is_some_and(|m| done(&m))
    {
        return Ok(false);
    }
    // The record's content id appeared or changed since its sidecars were made (same path,
    // mtime and size, so the same content): move them instead of decoding again.
    let mut changed = false;
    let old = row.filter(|(_, p)| *p == pdir).map(|(k, _)| k);
    for from in old.iter().map(String::as_str).chain([pdir.as_str()]) {
        if sidecars.relink_dir(from, &key)? {
            changed = true;
            break;
        }
    }
    let meta_path = sidecars.ensure(&key, SidecarKind::Meta, &path)?;
    let mut meta: MediaMeta = serde_json::from_slice(&std::fs::read(meta_path)?)?;
    let mut failed = None;
    if !done(&meta) {
        changed = true;
        if let Err(e) = sidecars.ensure(&key, SidecarKind::Thumb256, &path) {
            meta = sidecars.meta(&key).unwrap_or(meta);
            failed = Some(e);
        }
    }
    let c = src.store.get()?;
    c.execute(
        "INSERT OR REPLACE INTO media(record, key, pkey, width, height, orientation, taken_at,
             duration_ms, camera, gps_lat, gps_lon)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            rec.id,
            dir,
            pdir,
            meta.width,
            meta.height,
            meta.orientation,
            meta.taken_at,
            meta.duration_ms.map(|d| d as i64),
            meta.camera,
            meta.gps.map(|g| g.0),
            meta.gps.map(|g| g.1),
        ],
    )?;
    c.execute("DELETE FROM media_fts WHERE rowid = ?1", [rec.id])?;
    if meta.camera.is_some() || !meta.keywords.is_empty() {
        c.execute(
            "INSERT INTO media_fts(rowid, camera, keywords) VALUES (?1, ?2, ?3)",
            params![rec.id, meta.camera, meta.keywords.join(" ")],
        )?;
    }
    let recorded = meta
        .error
        .as_deref()
        .or_else(|| meta.failure(SidecarKind::Thumb256));
    if let Some(error) = recorded.filter(|_| failed.is_none()) {
        // Recorded now or earlier: counted, not retried.
        return Err(media::corrupt(error));
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(changed),
    }
}

/// SQL (on `record`): the name has a media extension.
fn media_name_sql() -> String {
    let likes: Vec<String> = media::IMAGE_EXTENSIONS
        .iter()
        .chain(media::VIDEO_EXTENSIONS)
        .map(|e| format!("name LIKE '%.{e}'"))
        .collect();
    format!("({})", likes.join(" OR "))
}

impl SidecarJob {
    fn run_source(&mut self, ctx: &JobCtx, src: &Source, sidecars: &Sidecars) -> Result<()> {
        let Some(root) = src.def.root.to_local_path() else {
            return ctx.log(&format!("{}: not local, skipped", src.def.label));
        };
        if !root.is_dir() {
            return ctx.log(&format!("{}: offline, skipped", src.def.label));
        }
        let filter = format!(
            "kind = 0 AND flags & {} = 0 AND {}",
            UNREADABLE | LINK,
            media_name_sql()
        );
        if !self.counted {
            self.total = src.store.get()?.query_row(
                &format!("SELECT count(*) FROM record WHERE {filter}"),
                [],
                |r| r.get::<_, i64>(0),
            )? as u64;
            self.counted = true;
            ctx.checkpoint(self.checkpoint(), 0.0)?;
        }
        loop {
            let batch: Vec<Pending> = {
                let c = src.store.get()?;
                let mut stmt = c.prepare_cached(&format!(
                    "SELECT id, path, size, coalesce(mtime, 0), cas_id FROM record
                     WHERE id > ?1 AND {filter} ORDER BY id LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![self.after, CHECKPOINT_EVERY as i64], |r| {
                    Ok(Pending {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        size: r.get::<_, i64>(2)? as u64,
                        mtime: r.get(3)?,
                        cas: r.get(4)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            if batch.is_empty() {
                return Ok(());
            }
            for rec in batch {
                if src.removed.load(Ordering::SeqCst) {
                    return Ok(());
                }
                self.wait_until_idle(ctx)?;
                match process(sidecars, src, &root, &rec) {
                    Ok(made) => self.made += u64::from(made),
                    Err(e) if e.is::<rusqlite::Error>() => return Err(e),
                    Err(e) => {
                        self.errors += 1;
                        if self.errors <= LOGGED_ERRORS {
                            ctx.log(&format!("{}: {e:#}", rec.path))?;
                        }
                    }
                }
                self.after = rec.id;
                self.done += 1;
            }
            sidecars.evict_to_budget(&|_| false);
            ctx.checkpoint(self.checkpoint(), self.progress())?;
        }
    }
}

impl Job for SidecarJob {
    fn kind(&self) -> &'static str {
        Self::KIND
    }

    fn run(&mut self, ctx: &JobCtx) -> Result<()> {
        background_thread();
        let src = ctx
            .lib
            .sources
            .read()
            .iter()
            .find(|s| s.id == self.source)
            .cloned();
        let Some(src) = src else {
            return ctx.log(&format!("no source {}: skipped", self.source));
        };
        let sidecars = sidecars::shared(&self.root)?;
        self.run_source(ctx, &src, &sidecars)?;
        ctx.log(&format!(
            "{} media files: {} made or moved, {} failed",
            self.done, self.made, self.errors
        ))
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<SidecarJob>(v)?))
    }
}

impl Library {
    /// Makes the missing sidecars of `source`'s images and videos, as a durable idle-priority
    /// job (resumed after a restart like the other library jobs).
    pub fn media_job(&self, source: &SourceId) -> Result<JobId> {
        anyhow::ensure!(self.source(source).is_some(), "no source {source}");
        self.jobs().spawn(Box::new(SidecarJob::new(
            source.clone(),
            &sidecars::sidecar_root(self)?,
        )))
    }
}

#[cfg(test)]
#[path = "sidecar_job_tests.rs"]
mod tests;
