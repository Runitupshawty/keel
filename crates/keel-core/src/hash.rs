//! Content identity (spec 2.10 "Identity + dedupe"): a durable hashing job and the queries
//! built on its content ids.
//!
//! Every file gets a sampled hash: BLAKE3 of its size and its first, middle and last 64 KiB
//! (the whole file up to [`WHOLE`] bytes). A record's `cas_id` is its sampled hash while that
//! is unique in the library; when another record shares it, both get the full-file BLAKE3 as
//! `cas_id`. So a content id is *confirmed* (equal ids mean equal bytes) when the file was
//! hashed whole: small files always, large ones once `cas_id` differs from `sampled_hash`.
//! Duplicates, last-copy and redundancy only trust confirmed ids.

use crate::index::{LINK, UNREADABLE};
use crate::jobs::{Job, JobCtx, JobId};
use crate::library::{RecordRef, Shared, Source};
use crate::{Cancelled, Library, SourceId};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

/// Bytes per sample.
pub const SAMPLE: u64 = 64 * 1024;
/// Files up to this size are hashed whole, so their sampled hash is a confirmed content id.
pub const WHOLE: u64 = 3 * SAMPLE;
/// Files per checkpoint.
const CHECKPOINT_EVERY: usize = 1_000;
/// How often a paused job looks again.
const PAUSE_POLL: Duration = Duration::from_millis(200);
/// How long a battery reading is trusted.
const BATTERY_TTL: Duration = Duration::from_secs(30);

/// SQL (on `record`): the content id is a whole-file hash. Literal of [`WHOLE`].
pub(crate) const CONFIRMED: &str = "(size <= 196608 OR cas_id IS NOT sampled_hash)";
const _: () = assert!(WHOLE == 196_608);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DupGroup {
    pub cas_id: Vec<u8>,
    /// Bytes per copy.
    pub size: u64,
    /// Two or more records, by source then id.
    pub records: Vec<RecordRef>,
}

/// Where a source's records physically live. Phase 7 adds drives, pools and failure domains;
/// for now one source is one volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRef {
    pub source: SourceId,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Copies {
    /// Records holding this content (at least 1: the record itself).
    pub count: u64,
    pub volumes: Vec<VolumeRef>,
}

/// The content changed (its size no longer matches the record): leave it to the indexer.
#[derive(Debug)]
struct Changed;
impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("changed since indexed")
    }
}
impl std::error::Error for Changed {}

fn changed() -> io::Error {
    io::Error::other(Changed)
}

fn open(path: &Path, size: u64) -> io::Result<File> {
    let f = File::open(path)?;
    if f.metadata()?.len() != size {
        return Err(changed());
    }
    Ok(f)
}

/// BLAKE3 of the size and the first, middle and last [`SAMPLE`] bytes (the whole file up to
/// [`WHOLE`] bytes).
pub(crate) fn sampled_hash(path: &Path, size: u64) -> io::Result<[u8; 32]> {
    let mut f = open(path, size)?;
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    if size <= WHOLE {
        let mut buf = Vec::with_capacity(size as usize);
        f.take(WHOLE + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 != size {
            return Err(changed());
        }
        h.update(&buf);
    } else {
        let mut buf = vec![0; SAMPLE as usize];
        for at in [0, size / 2 - SAMPLE / 2, size - SAMPLE] {
            f.seek(SeekFrom::Start(at))?;
            f.read_exact(&mut buf)?;
            h.update(&buf);
        }
    }
    Ok(*h.finalize().as_bytes())
}

/// BLAKE3 of the whole file.
pub(crate) fn full_hash(path: &Path, size: u64) -> io::Result<[u8; 32]> {
    let mut f = open(path, size)?;
    let mut h = blake3::Hasher::new();
    if io::copy(&mut f, &mut h)? != size {
        return Err(changed());
    }
    Ok(*h.finalize().as_bytes())
}

/// Lowest scheduling priority for the calling thread (Windows and macOS also lower its IO
/// priority). Best effort.
pub(crate) fn background_thread() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_IDLE,
        };
        // SAFETY: the pseudo handle of the calling thread.
        unsafe {
            if SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN).is_err() {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_IDLE);
            }
        }
    }
    #[cfg(target_os = "macos")]
    // SAFETY: applies to the calling thread only.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
    }
    // On Linux, nice applies per thread (who = 0 is the calling thread).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: plain syscall wrapper.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
}

/// Some battery is discharging. False when it cannot be told (desktops, `power` off).
pub fn on_battery() -> bool {
    #[cfg(feature = "power")]
    return battery::Manager::new()
        .and_then(|m| m.batteries())
        .map(|bs| {
            bs.flatten()
                .any(|b| b.state() == battery::State::Discharging)
        })
        .unwrap_or(false);
    #[cfg(not(feature = "power"))]
    false
}

/// Hashes every file record without a sampled hash, source by source in id order, at idle
/// priority on its own thread; pauses while the app reports activity or on battery.
/// Remote and cloud sources are skipped (hashing would download every file), as are sources
/// whose root cannot be reached.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HashJob {
    /// Sources still to do; the first is in progress.
    sources: Vec<SourceId>,
    /// Records up to this id of the first source are done.
    after: i64,
    counted: bool,
    pub total: u64,
    pub done: u64,
    /// Files whose sampled hash collided (hashed whole).
    pub full: u64,
    pub errors: u64,
    #[serde(skip)]
    battery: Option<(Instant, bool)>,
}

/// One file record to hash.
struct Pending {
    id: i64,
    path: String,
    size: u64,
}

impl HashJob {
    fn wait_until_idle(&mut self, ctx: &JobCtx) -> Result<()> {
        loop {
            if ctx.stopping() {
                return Err(Cancelled.into());
            }
            let lib = &ctx.lib;
            let busy = lib.activity.load(Ordering::SeqCst)
                || (lib.pause_on_battery.load(Ordering::SeqCst) && self.on_battery());
            if !busy {
                return Ok(());
            }
            std::thread::sleep(PAUSE_POLL);
        }
    }

    fn on_battery(&mut self) -> bool {
        match self.battery {
            Some((at, v)) if at.elapsed() < BATTERY_TTL => v,
            _ => {
                let v = on_battery();
                self.battery = Some((Instant::now(), v));
                v
            }
        }
    }

    fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).min(0.99)
        }
    }

    /// Hashes the pending records of `src` (stops early when the source goes away).
    fn hash_source(&mut self, ctx: &JobCtx, src: &Source) -> Result<()> {
        let Some(root) = src.def.root.to_local_path() else {
            return ctx.log(&format!("{}: not local, skipped", src.def.label));
        };
        if !root.is_dir() {
            return ctx.log(&format!("{}: offline, skipped", src.def.label));
        }
        loop {
            let batch: Vec<Pending> = {
                let c = src.store.get()?;
                let mut stmt = c.prepare_cached(
                    "SELECT id, path, size FROM record
                     WHERE id > ?1 AND kind = 0 AND flags & ?2 = 0 AND sampled_hash IS NULL
                     ORDER BY id LIMIT ?3",
                )?;
                let rows = stmt.query_map(
                    params![self.after, UNREADABLE | LINK, CHECKPOINT_EVERY as i64],
                    |r| {
                        Ok(Pending {
                            id: r.get(0)?,
                            path: r.get(1)?,
                            size: r.get::<_, i64>(2)? as u64,
                        })
                    },
                )?;
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
                let path = root.join(&rec.path);
                match hash_one(&ctx.lib, src, &rec, &path) {
                    Ok(full) => self.full += u64::from(full),
                    Err(e) => match e.downcast_ref::<io::Error>() {
                        None => return Err(e),
                        Some(io) if is_changed(io) => {}
                        Some(_) if !root.is_dir() => {
                            return ctx.log(&format!("{}: went offline", src.def.label));
                        }
                        Some(io) => {
                            self.errors += 1;
                            src.store.get()?.execute(
                                "UPDATE record SET flags = flags | ?2, error = ?3 WHERE id = ?1",
                                params![rec.id, UNREADABLE, format!("hash: {io}")],
                            )?;
                        }
                    },
                }
                self.after = rec.id;
                self.done += 1;
            }
            ctx.checkpoint(self.checkpoint(), self.progress())?;
        }
    }
}

fn is_changed(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Changed>())
}

/// Hashes one record (sampled; whole when another record shares the sampled hash, which is
/// then hashed whole too). Returns whether it was hashed whole because of a collision.
fn hash_one(lib: &Shared, src: &Source, rec: &Pending, path: &Path) -> Result<bool> {
    let sampled = sampled_hash(path, rec.size)?;
    let collides = rec.size > WHOLE && confirm_others(lib, src, rec.id, &sampled)?;
    let cas = if collides {
        full_hash(path, rec.size)?
    } else {
        sampled
    };
    // Guarded: a record the indexer changed meanwhile keeps its reset hashes.
    src.store.get()?.execute(
        "UPDATE record SET sampled_hash = ?2, cas_id = ?3 WHERE id = ?1 AND size = ?4 AND kind = 0",
        params![rec.id, &sampled[..], &cas[..], rec.size as i64],
    )?;
    Ok(collides)
}

/// Whether any other record has `sampled` as its sampled hash; those not yet hashed whole are
/// hashed whole now (records on sources that cannot be read stay unconfirmed).
fn confirm_others(lib: &Shared, me: &Source, my_id: i64, sampled: &[u8]) -> Result<bool> {
    let sources: Vec<Arc<Source>> = lib.sources.read().clone();
    let mut any = false;
    for s in &sources {
        let others: Vec<(i64, String, i64, bool)> = {
            let c = s.store.get()?;
            let mut stmt = c.prepare_cached(&format!(
                "SELECT id, path, size, {CONFIRMED} FROM record WHERE sampled_hash = ?1"
            ))?;
            let rows = stmt.query_map([sampled], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, rel, size, confirmed) in others {
            if s.id == me.id && id == my_id {
                continue;
            }
            any = true;
            if confirmed {
                continue;
            }
            let Some(path) = s.absolute(&rel).to_local_path() else {
                continue;
            };
            match full_hash(&path, size as u64) {
                Ok(full) => {
                    s.store.get()?.execute(
                        "UPDATE record SET cas_id = ?2 WHERE id = ?1 AND sampled_hash = ?3",
                        params![id, &full[..], sampled],
                    )?;
                }
                Err(e) => tracing::debug!("confirm {}: {e}", path.display()),
            }
        }
    }
    Ok(any)
}

impl Job for HashJob {
    fn kind(&self) -> &'static str {
        "hash"
    }

    fn run(&mut self, ctx: &JobCtx) -> Result<()> {
        background_thread();
        let lib = ctx.lib.clone();
        let source = |id: &SourceId| lib.sources.read().iter().find(|s| &s.id == id).cloned();
        if !self.counted {
            for id in &self.sources {
                if let Some(s) = source(id) {
                    self.total += s.store.get()?.query_row(
                        "SELECT count(*) FROM record
                         WHERE kind = 0 AND flags & ?1 = 0 AND sampled_hash IS NULL",
                        [UNREADABLE | LINK],
                        |r| r.get::<_, i64>(0),
                    )? as u64;
                }
            }
            self.counted = true;
            ctx.checkpoint(self.checkpoint(), 0.0)?;
        }
        while let Some(id) = self.sources.first().cloned() {
            // A removed source is simply skipped.
            if let Some(src) = source(&id) {
                self.hash_source(ctx, &src)?;
            }
            self.sources.remove(0);
            self.after = 0;
            ctx.checkpoint(self.checkpoint(), self.progress())?;
        }
        ctx.log(&format!(
            "hashed {} files, {} whole after a collision, {} unreadable",
            self.done, self.full, self.errors
        ))?;
        crate::library::count_unique(&lib)?;
        Ok(())
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<HashJob>(v)?))
    }
}

/// Confirmed records holding `cas`, per source (sources without one left out).
pub(crate) fn copies(lib: &Shared, cas: &[u8]) -> Result<Vec<(Arc<Source>, u64)>> {
    let sources: Vec<Arc<Source>> = lib.sources.read().clone();
    let mut out = Vec::new();
    for s in sources {
        let n: i64 = s.store.get()?.query_row(
            &format!("SELECT count(*) FROM record WHERE cas_id = ?1 AND kind = 0 AND {CONFIRMED}"),
            [cas],
            |r| r.get(0),
        )?;
        if n > 0 {
            out.push((s, n as u64));
        }
    }
    Ok(out)
}

/// `(cas_id, confirmed)` of a record.
fn content_of(c: &Connection, id: i64) -> Result<(Option<Vec<u8>>, bool)> {
    c.query_row(
        &format!("SELECT cas_id, {CONFIRMED} FROM record WHERE id = ?1"),
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .with_context(|| format!("no record {id}"))
}

impl Library {
    /// Hashes every file still without a content id, as a durable idle-priority job.
    pub fn hash(&self) -> Result<JobId> {
        let sources = self
            .shared
            .sources
            .read()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        self.jobs().spawn(Box::new(HashJob {
            sources,
            ..HashJob::default()
        }))
    }

    /// Set by the app while the user is interacting: hashing pauses until it is cleared.
    pub fn activity(&self) -> &std::sync::atomic::AtomicBool {
        &self.shared.activity
    }

    /// Whether hashing pauses on battery power (default on).
    pub fn set_pause_on_battery(&self, pause: bool) {
        self.shared.pause_on_battery.store(pause, Ordering::SeqCst);
    }

    /// Groups of two or more files of at least `min_size` bytes with the same confirmed
    /// content id, across every source; biggest first.
    pub fn duplicates(&self, min_size: u64) -> Result<Vec<DupGroup>> {
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        // A private on-disk scratch database (SQLite spills it to a temp file).
        let scratch = Connection::open("")?;
        scratch.execute_batch("CREATE TABLE c(cas BLOB, src INTEGER, id INTEGER, size INTEGER)")?;
        for (i, s) in sources.iter().enumerate() {
            let path = s.store_dir().join("source.db");
            scratch.execute("ATTACH DATABASE ?1 AS s", [path.to_string_lossy()])?;
            let copied = scratch.execute(
                &format!(
                    "INSERT INTO c SELECT cas_id, ?1, id, size FROM s.record
                     WHERE kind = 0 AND cas_id IS NOT NULL AND size >= ?2 AND {CONFIRMED}"
                ),
                params![i as i64, min_size as i64],
            );
            scratch.execute("DETACH DATABASE s", [])?;
            copied?;
        }
        scratch.execute_batch("CREATE INDEX c_cas ON c(cas)")?;
        let mut stmt = scratch.prepare(
            "SELECT cas, src, id, size FROM c
             WHERE cas IN (SELECT cas FROM c GROUP BY cas HAVING count(*) > 1)
             ORDER BY size DESC, cas, src, id",
        )?;
        let mut rows = stmt.query([])?;
        let mut groups: Vec<DupGroup> = Vec::new();
        while let Some(r) = rows.next()? {
            let (cas, src, id, size): (Vec<u8>, i64, i64, i64) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            let record = RecordRef {
                source: sources[src as usize].id.clone(),
                id,
            };
            match groups.last_mut() {
                Some(g) if g.cas_id == cas => g.records.push(record),
                _ => groups.push(DupGroup {
                    cas_id: cas,
                    size: size as u64,
                    records: vec![record],
                }),
            }
        }
        Ok(groups)
    }

    /// True unless another record is known to hold the same content (also true while the
    /// record has no confirmed content id yet: no other copy is known).
    pub fn last_copy(&self, record: &RecordRef) -> Result<bool> {
        Ok(self.redundancy(record)?.count <= 1)
    }

    /// How many records hold this record's content, and on which volumes.
    pub fn redundancy(&self, record: &RecordRef) -> Result<Copies> {
        let src = self
            .source(&record.source)
            .with_context(|| format!("no source {}", record.source))?;
        let (cas, confirmed) = content_of(&*src.store.get()?, record.id)?;
        let alone = || Copies {
            count: 1,
            volumes: vec![VolumeRef {
                source: src.id.clone(),
                label: src.def.label.clone(),
            }],
        };
        let Some(cas) = cas.filter(|_| confirmed) else {
            return Ok(alone());
        };
        let copies = copies(&self.shared, &cas)?;
        if copies.is_empty() {
            return Ok(alone());
        }
        Ok(Copies {
            count: copies.iter().map(|(_, n)| n).sum(),
            volumes: copies
                .into_iter()
                .map(|(s, _)| VolumeRef {
                    source: s.id.clone(),
                    label: s.def.label.clone(),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
