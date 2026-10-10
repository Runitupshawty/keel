//! Content identity (spec 2.10 "Identity + dedupe"): a durable hashing job and the queries
//! built on its content ids.
//!
//! Every file gets a sampled hash: BLAKE3 of its size and its first, middle and last 64 KiB
//! (the whole file up to [`WHOLE`] bytes). A record's `cas_id` is the BLAKE3 of its bytes,
//! set only once known: files up to [`WHOLE`] are read whole anyway; a larger file is hashed
//! whole when another record shares its sampled hash (both are then). So equal `cas_id`s
//! mean equal bytes; a large file whose sampled hash is unique keeps `cas_id` NULL (no other
//! file can hold its content). Duplicates, last-copy and redundancy only trust `cas_id`.
//!
//! Remote sources (SFTP; cloud only when `hash_cloud` is on) are read through their
//! provider in one pass that yields both hashes, so a remote file always gets its `cas_id`
//! and matches a local copy of the same bytes. Provider checksums (ETag, MD5, Dropbox's
//! content hash) are never used: they are other algorithms and would never match.

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
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Bytes per sample.
pub const SAMPLE: u64 = 64 * 1024;
/// Files up to this size are hashed whole: their content id is known at once.
pub const WHOLE: u64 = 3 * SAMPLE;
/// Files per checkpoint (small under test: the resume test reaches one on a slow CI
/// runner before its wait runs out, and the logic is the same).
const CHECKPOINT_EVERY: usize = if cfg!(test) { 100 } else { 1_000 };
/// How often a paused job looks again.
const PAUSE_POLL: Duration = Duration::from_millis(200);
/// How long a battery reading is trusted.
const BATTERY_TTL: Duration = Duration::from_secs(30);
/// Bytes read between stop checks when hashing a file whole.
const CHUNK: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DupGroup {
    pub cas_id: Vec<u8>,
    /// Bytes per copy.
    pub size: u64,
    /// Two or more records, by source then id; hard links of one file appear once.
    pub records: Vec<RecordRef>,
}

/// Which remote files hashing downloads (`Library::set_remote_hash_settings`), saved in
/// `library.db`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteHashSettings {
    /// SFTP and other remotes that are not cloud accounts (default on).
    pub hash_remote: bool,
    /// Google Drive, Dropbox, S3, WebDAV (default off: downloads can cost egress fees).
    pub hash_cloud: bool,
    /// Remote files bigger than this are left unhashed (`SkipReason::TooBig`; 1 GiB).
    pub remote_hash_max_bytes: u64,
}

impl Default for RemoteHashSettings {
    fn default() -> Self {
        Self {
            hash_remote: true,
            hash_cloud: false,
            remote_hash_max_bytes: 1 << 30,
        }
    }
}

/// Why a hash job left a source out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// A remote or cloud source whose hashing is off, or a paired device (its content ids
    /// are its own word).
    Remote,
    /// Remote files over `remote_hash_max_bytes` (the others are hashed).
    TooBig,
    /// A network share without `SourceDef::hash_shares`.
    Share,
    /// The root could not be reached.
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedSource {
    pub source: SourceId,
    pub label: String,
    pub reason: SkipReason,
}

/// What a hash job did (its `JobInfo::result`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashResult {
    pub hashed: u64,
    /// Files hashed whole after a sampled-hash collision.
    pub whole: u64,
    /// Files that could not be read (marked unreadable until a walk refreshes them).
    pub unreadable: u64,
    /// Earlier collisions confirmed by the re-confirm pass.
    pub reconfirmed: u64,
    pub skipped: Vec<SkippedSource>,
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

/// A remote download stopped part way: its source was removed, or its hashing turned off.
#[derive(Debug, PartialEq)]
enum Interrupted {
    Removed,
    PolicyOff,
}
impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Interrupted::Removed => "source removed",
            Interrupted::PolicyOff => "hashing turned off",
        })
    }
}
impl std::error::Error for Interrupted {}

/// A remote file whose read failed, as of the record it was read for: not read again
/// before `until` (an hour after the first failure, doubling, at most a day) unless the
/// record changes. In memory only: a restart tries once more.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Backoff {
    size: u64,
    mtime: Option<i64>,
    failures: u32,
    until: Instant,
}
const BACKOFF_FIRST: Duration = Duration::from_secs(3600);
const BACKOFF_MAX: Duration = Duration::from_secs(24 * 3600);

fn backed_off(lib: &Shared, src: &Source, id: i64, size: u64, mtime: Option<i64>) -> bool {
    let backoff = lib.hash_backoff.lock();
    backoff
        .get(&(src.id.clone(), id))
        .is_some_and(|b| b.size == size && b.mtime == mtime && Instant::now() < b.until)
}

/// Records a failed read of `rec`; returns the delay before the next try.
fn back_off(lib: &Shared, src: &Source, rec: &Pending) -> Duration {
    let mut backoff = lib.hash_backoff.lock();
    let key = (src.id.clone(), rec.id);
    let failures = match backoff.get(&key) {
        Some(b) if (b.size, b.mtime) == (rec.size, rec.mtime) => b.failures + 1,
        _ => 1,
    };
    let delay = BACKOFF_FIRST
        .saturating_mul(1 << (failures - 1).min(16))
        .min(BACKOFF_MAX);
    backoff.insert(
        key,
        Backoff {
            size: rec.size,
            mtime: rec.mtime,
            failures,
            until: Instant::now() + delay,
        },
    );
    delay
}

fn open(path: &Path, size: u64) -> io::Result<File> {
    let f = File::open(path)?;
    if f.metadata()?.len() != size {
        return Err(changed());
    }
    Ok(f)
}

/// The sampled hash: BLAKE3 of the size and the first, middle and last [`SAMPLE`] bytes (the
/// whole file up to [`WHOLE`] bytes); for such a small file also its content id, BLAKE3 of
/// the bytes.
pub(crate) fn sampled_hash(path: &Path, size: u64) -> io::Result<([u8; 32], Option<[u8; 32]>)> {
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
        return Ok((
            *h.finalize().as_bytes(),
            Some(*blake3::hash(&buf).as_bytes()),
        ));
    }
    let mut buf = vec![0; SAMPLE as usize];
    for at in [0, size / 2 - SAMPLE / 2, size - SAMPLE] {
        f.seek(SeekFrom::Start(at))?;
        f.read_exact(&mut buf)?;
        h.update(&buf);
    }
    Ok((*h.finalize().as_bytes(), None))
}

/// BLAKE3 of the whole file, in 1 MiB reads; `Err(Cancelled)` once `stop` is set.
pub(crate) fn full_hash(path: &Path, size: u64, stop: &AtomicBool) -> Result<[u8; 32]> {
    let mut f = open(path, size)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0; CHUNK];
    let mut read = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        h.update(&buf[..n]);
        read += n as u64;
    }
    if read != size {
        return Err(changed().into());
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

/// Returns once the app is idle (and not on battery, unless that is allowed); `battery`
/// caches the last reading. `Err(Cancelled)` when the job is asked to stop.
/// Waits while the user was active within the last `pause` (zero: never) or the machine
/// runs on battery (when the library pauses on battery).
pub(crate) fn wait_until_idle(
    ctx: &JobCtx,
    battery: &mut Option<(Instant, bool)>,
    pause: Duration,
) -> Result<()> {
    loop {
        if ctx.stopping() {
            return Err(Cancelled.into());
        }
        let lib = &ctx.lib;
        let mut discharging = || match *battery {
            Some((at, v)) if at.elapsed() < BATTERY_TTL => v,
            _ => {
                let v = on_battery();
                *battery = Some((Instant::now(), v));
                v
            }
        };
        let busy = (!pause.is_zero() && lib.busy_within(pause))
            || (lib.pause_on_battery.load(Ordering::SeqCst) && discharging());
        if !busy {
            return Ok(());
        }
        std::thread::sleep(PAUSE_POLL);
    }
}

/// Why `src` is not hashed now, if it is not.
pub(crate) fn skip_reason(src: &Source) -> Option<SkipReason> {
    let Some(root) = src.def.root.to_local_path() else {
        return Some(SkipReason::Remote);
    };
    if !src.hashable_share() {
        return Some(SkipReason::Share);
    }
    (!root.is_dir()).then_some(SkipReason::Offline)
}

fn is_cloud(src: &Source) -> bool {
    src.def.root.scheme == "cloud" || src.def.kind == crate::SourceKind::Cloud
}

/// Whether the remote policy lets the files of `src`, a source that is not local, be
/// downloaded now. Cheap (no network): checked again before every file and read.
fn remote_allowed(lib: &Shared, src: &Source) -> bool {
    let settings = *lib.remote_hash_settings.read();
    let allowed = match is_cloud(src) {
        true => settings.hash_cloud,
        false => settings.hash_remote,
    };
    allowed && !src.is_device()
}

/// The largest record size hashing reads from `src` (remote sources: the size cap).
fn size_cap(lib: &Shared, src: &Source) -> i64 {
    match src.def.root.to_local_path() {
        Some(_) => i64::MAX,
        None => {
            let max = lib.remote_hash_settings.read().remote_hash_max_bytes;
            max.min(i64::MAX as u64) as i64
        }
    }
}

/// `skip_reason` for hashing, which also reads remote sources under the remote policy
/// (integrity checks stay local-only). A remote root that cannot be stat'ed is offline.
fn hash_skip_reason(lib: &Shared, src: &Source) -> Option<SkipReason> {
    if src.def.root.to_local_path().is_some() {
        return skip_reason(src);
    }
    if !remote_allowed(lib, src) {
        return Some(SkipReason::Remote);
    }
    let provider = lib.router.read().provider_for(&src.def.root);
    if provider.is_none_or(|p| p.stat(&src.def.root).is_err()) {
        return Some(SkipReason::Offline);
    }
    None
}

/// (sampled hash, content id) of a remote record, read once through its provider in
/// [`CHUNK`] reads: the samples are the same bytes `sampled_hash` reads locally. Drift
/// (size or mtime differ from the record, before or after) is `Changed`; a removed source
/// or hashing turned off stops the read (`Interrupted`).
fn remote_hash(
    ctx: &JobCtx,
    src: &Source,
    rec: &Pending,
    battery: &mut Option<(Instant, bool)>,
) -> Result<([u8; 32], [u8; 32])> {
    // ponytail: one remote file at a time across all hosts (so at most one per host);
    // per-host locks if several hosts should stream at once.
    let _serial = loop {
        hash_wait(ctx, battery)?;
        if let Some(guard) = ctx.lib.remote_hash_io.try_lock() {
            break guard;
        }
        std::thread::sleep(PAUSE_POLL);
    };
    let path = src.absolute(&rec.path);
    let provider = ctx
        .lib
        .router
        .read()
        .provider_for(&path)
        .context("remote unavailable")?;
    let check = || -> Result<()> {
        let e = provider.stat(&path)?;
        if e.kind != keel_vfs::Kind::File
            || e.is_link
            || e.size != rec.size
            || e.modified.map(crate::unix_ns) != rec.mtime
        {
            return Err(changed().into());
        }
        Ok(())
    };
    check()?;
    let mut reader = provider.read(&path)?;
    let mut full = blake3::Hasher::new();
    let ranges = if rec.size <= WHOLE {
        vec![(0, rec.size)]
    } else {
        vec![
            (0, SAMPLE),
            (rec.size / 2 - SAMPLE / 2, SAMPLE),
            (rec.size - SAMPLE, SAMPLE),
        ]
    };
    let mut samples: Vec<Vec<u8>> = ranges
        .iter()
        .map(|(_, len)| Vec::with_capacity(*len as usize))
        .collect();
    let mut buf = vec![0; CHUNK];
    let mut offset = 0;
    loop {
        hash_wait(ctx, battery)?;
        if src.removed.load(Ordering::SeqCst) {
            return Err(Interrupted::Removed.into());
        }
        if !remote_allowed(&ctx.lib, src) {
            return Err(Interrupted::PolicyOff.into());
        }
        // One extra byte detects growth without downloading an unbounded changing file.
        let limit = (rec.size.saturating_sub(offset).saturating_add(1)).min(CHUNK as u64) as usize;
        let n = match reader.read(&mut buf[..limit]) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            break;
        }
        if offset + n as u64 > rec.size {
            return Err(changed().into());
        }
        full.update(&buf[..n]);
        for ((start, len), sample) in ranges.iter().zip(&mut samples) {
            let lo = offset.max(*start);
            let hi = (offset + n as u64).min(start + len);
            if lo < hi {
                sample.extend_from_slice(&buf[(lo - offset) as usize..(hi - offset) as usize]);
            }
        }
        offset += n as u64;
    }
    if offset != rec.size {
        return Err(changed().into());
    }
    check()?;
    let mut sampled = blake3::Hasher::new();
    sampled.update(&rec.size.to_le_bytes());
    for sample in samples {
        sampled.update(&sample);
    }
    Ok((*sampled.finalize().as_bytes(), *full.finalize().as_bytes()))
}

fn hash_wait(ctx: &JobCtx, battery: &mut Option<(Instant, bool)>) -> Result<()> {
    let pause = if ctx.lib.hash_on_activity.load(Ordering::SeqCst) {
        crate::library::ACTIVITY_PAUSE
    } else {
        Duration::ZERO
    };
    wait_until_idle(ctx, battery, pause)
}

/// Hashes every file record without a sampled hash, source by source in id order, at idle
/// priority on its own thread; pauses while the app reports activity or on battery.
/// Remote files stream through their provider under the download policy. Network shares
/// need explicit permission; unreachable sources are skipped. The skips are in
/// the job's [`HashResult`]. Ends with a pass that confirms sampled hashes shared by
/// records that are still unconfirmed (a stop between the two halves of a collision, a
/// source that was offline).
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
    #[serde(default)]
    reconfirmed: u64,
    #[serde(default)]
    skipped: Vec<SkippedSource>,
    #[serde(skip)]
    battery: Option<(Instant, bool)>,
}

/// One file record to hash, with the metadata the hash is valid for.
struct Pending {
    id: i64,
    path: String,
    size: u64,
    mtime: Option<i64>,
    ctime: Option<i64>,
}

impl HashJob {
    fn new(lib: &Shared) -> HashJob {
        HashJob {
            sources: lib.sources.read().iter().map(|s| s.id.clone()).collect(),
            ..HashJob::default()
        }
    }

    fn wait_until_idle(&mut self, ctx: &JobCtx) -> Result<()> {
        let pause = match ctx.lib.hash_on_activity.load(Ordering::SeqCst) {
            true => crate::library::ACTIVITY_PAUSE,
            false => Duration::ZERO,
        };
        wait_until_idle(ctx, &mut self.battery, pause)
    }

    fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).min(0.99)
        }
    }

    fn skip(&mut self, ctx: &JobCtx, src: &Source, reason: SkipReason) -> Result<()> {
        let why = match reason {
            SkipReason::Remote if src.is_device() => "paired device",
            SkipReason::Remote if is_cloud(src) => "cloud hashing is off",
            SkipReason::Remote => "remote hashing is off",
            SkipReason::TooBig => "over the remote size cap",
            SkipReason::Share => "network share (hash_shares is off)",
            SkipReason::Offline => "offline",
        };
        self.note_skip(src, reason);
        ctx.log(&format!("{}: {why}, skipped", src.def.label))
    }

    /// Lists `src` under `reason` in the result (once).
    fn note_skip(&mut self, src: &Source, reason: SkipReason) {
        if !self
            .skipped
            .iter()
            .any(|s| s.source == src.id && s.reason == reason)
        {
            self.skipped.push(SkippedSource {
                source: src.id.clone(),
                label: src.def.label.clone(),
                reason,
            });
        }
    }

    /// Hashes the pending records of `src` (stops early when the source goes away).
    fn hash_source(&mut self, ctx: &JobCtx, src: &Source) -> Result<()> {
        if let Some(reason) = hash_skip_reason(&ctx.lib, src) {
            return self.skip(ctx, src, reason);
        }
        let root = src.def.root.to_local_path();
        loop {
            let batch: Vec<Pending> = {
                let c = src.store.get()?;
                let mut stmt = c.prepare_cached(
                    "SELECT id, path, size, mtime, ctime FROM record
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
                            mtime: r.get(3)?,
                            ctime: r.get(4)?,
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
                // The policy may have changed meanwhile: it applies from the next file.
                if root.is_none() && !remote_allowed(&ctx.lib, src) {
                    return self.skip(ctx, src, SkipReason::Remote);
                }
                if root.is_none() && backed_off(&ctx.lib, src, rec.id, rec.size, rec.mtime) {
                    self.after = rec.id;
                    continue;
                }
                if root.is_none()
                    && rec.size > ctx.lib.remote_hash_settings.read().remote_hash_max_bytes
                {
                    self.note_skip(src, SkipReason::TooBig);
                    let (label, path) = (&src.def.label, &rec.path);
                    ctx.log(&format!(
                        "{label}: {path} is over the remote size cap, skipped"
                    ))?;
                    self.after = rec.id;
                    continue;
                }
                let result = match &root {
                    Some(root) => hash_one(ctx, src, &rec, &root.join(&rec.path)),
                    None => {
                        remote_hash(ctx, src, &rec, &mut self.battery).and_then(|(sampled, cas)| {
                            save_hash(src, &rec, &sampled, Some(cas))?;
                            ctx.lib
                                .hash_backoff
                                .lock()
                                .remove(&(src.id.clone(), rec.id));
                            Ok(false)
                        })
                    }
                };
                match result {
                    Ok(full) => self.full += u64::from(full),
                    Err(e) if e.is::<Cancelled>() => return Err(e),
                    // As for a local source: the job goes on with the other sources.
                    Err(e) if e.downcast_ref::<Interrupted>() == Some(&Interrupted::Removed) => {
                        return Ok(())
                    }
                    Err(e) if e.is::<Interrupted>() => {
                        return self.skip(ctx, src, SkipReason::Remote);
                    }
                    Err(e) if root.is_none() => {
                        if e.downcast_ref::<io::Error>().is_some_and(is_changed) {
                            // The indexer will refresh this record.
                        } else if let Some(reason) = hash_skip_reason(&ctx.lib, src) {
                            return self.skip(ctx, src, reason);
                        } else {
                            // Not marked unreadable (a walk would clear that): tried again
                            // after a growing delay, or once the record changes.
                            self.errors += 1;
                            let hours = back_off(&ctx.lib, src, &rec).as_secs() / 3600;
                            ctx.log(&format!(
                                "{}: {}: {e:#} (tried again in {hours} h)",
                                src.def.label, rec.path
                            ))?;
                        }
                    }
                    Err(e) => match e.downcast_ref::<io::Error>() {
                        None => return Err(e),
                        Some(io) if is_changed(io) => {}
                        Some(_) if root.as_ref().is_some_and(|r| !r.is_dir()) => {
                            return self.skip(ctx, src, SkipReason::Offline);
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

    /// Hashes whole the unconfirmed records whose sampled hash another record shares.
    fn reconfirm(&mut self, ctx: &JobCtx) -> Result<()> {
        let sources: Vec<Arc<Source>> = ctx.lib.sources.read().clone();
        let scratch = Connection::open("")?;
        scratch.execute_batch(
            "CREATE TABLE c(sampled BLOB, src INTEGER, id INTEGER, path TEXT, size INTEGER,
                 confirmed INTEGER)",
        )?;
        for (i, s) in sources.iter().enumerate() {
            let path = s.store_dir().join("source.db");
            scratch.execute("ATTACH DATABASE ?1 AS s", [path.to_string_lossy()])?;
            let copied = scratch.execute(
                "INSERT INTO c SELECT sampled_hash, ?1, id, path, size, cas_id IS NOT NULL
                 FROM s.record WHERE kind = 0 AND size > ?2 AND sampled_hash IS NOT NULL",
                params![i as i64, WHOLE as i64],
            );
            scratch.execute("DETACH DATABASE s", [])?;
            copied?;
        }
        let todo: Vec<(usize, i64, String, i64, Vec<u8>)> = {
            let mut stmt = scratch.prepare(
                "SELECT src, id, path, size, sampled FROM c WHERE NOT confirmed AND sampled IN
                     (SELECT sampled FROM c GROUP BY sampled HAVING count(*) > 1)
                 ORDER BY src, id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)? as usize,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (i, id, rel, size, sampled) in todo {
            let s = &sources[i];
            if hash_skip_reason(&ctx.lib, s).is_some() || s.removed.load(Ordering::SeqCst) {
                continue;
            }
            let Some(path) = s.absolute(&rel).to_local_path() else {
                continue;
            };
            self.wait_until_idle(ctx)?;
            match full_hash(&path, size as u64, ctx.stop_flag()) {
                Ok(full) => {
                    self.reconfirmed += s.store.get()?.execute(
                        "UPDATE record SET cas_id = ?2
                         WHERE id = ?1 AND sampled_hash = ?3 AND cas_id IS NULL",
                        params![id, &full[..], sampled],
                    )? as u64;
                }
                Err(e) if e.is::<Cancelled>() => return Err(e),
                Err(e) => tracing::debug!("reconfirm {}: {e:#}", path.display()),
            }
        }
        Ok(())
    }

    fn result(&self) -> HashResult {
        HashResult {
            hashed: self.done,
            whole: self.full,
            unreadable: self.errors,
            reconfirmed: self.reconfirmed,
            skipped: self.skipped.clone(),
        }
    }
}

fn is_changed(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Changed>())
}

/// Hashes one record (sampled; whole when another record shares the sampled hash, which is
/// then hashed whole too). Returns whether it was hashed whole because of a collision.
fn hash_one(ctx: &JobCtx, src: &Source, rec: &Pending, path: &Path) -> Result<bool> {
    let (sampled, small) = sampled_hash(path, rec.size)?;
    let (cas, collided) = match small {
        Some(cas) => (Some(cas), false),
        None if confirm_others(ctx, src, rec.id, &sampled)? => {
            (Some(full_hash(path, rec.size, ctx.stop_flag())?), true)
        }
        None => (None, false),
    };
    save_hash(src, rec, &sampled, cas)?;
    Ok(collided)
}

fn save_hash(src: &Source, rec: &Pending, sampled: &[u8; 32], cas: Option<[u8; 32]>) -> Result<()> {
    // Guarded: a record the indexer changed meanwhile keeps its reset hashes.
    src.store.get()?.execute(
        "UPDATE record SET sampled_hash = ?2, cas_id = ?3
         WHERE id = ?1 AND kind = 0 AND size = ?4 AND sampled_hash IS NULL
             AND mtime IS ?5 AND ctime IS ?6",
        params![
            rec.id,
            &sampled[..],
            cas.as_ref().map(|c| &c[..]),
            rec.size as i64,
            rec.mtime,
            rec.ctime
        ],
    )?;
    Ok(())
}

/// Whether any other record has `sampled` as its sampled hash; those not yet hashed whole are
/// hashed whole now (records on sources that are not hashed stay unconfirmed).
fn confirm_others(ctx: &JobCtx, me: &Source, my_id: i64, sampled: &[u8]) -> Result<bool> {
    let sources: Vec<Arc<Source>> = ctx.lib.sources.read().clone();
    let mut any = false;
    for s in &sources {
        let others: Vec<(i64, String, i64, bool)> = {
            let c = s.store.get()?;
            let mut stmt = c.prepare_cached(
                "SELECT id, path, size, cas_id IS NOT NULL FROM record WHERE sampled_hash = ?1",
            )?;
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
            if confirmed || hash_skip_reason(&ctx.lib, s).is_some() {
                continue;
            }
            let Some(path) = s.absolute(&rel).to_local_path() else {
                continue;
            };
            match full_hash(&path, size as u64, ctx.stop_flag()) {
                Ok(full) => {
                    s.store.get()?.execute(
                        "UPDATE record SET cas_id = ?2
                         WHERE id = ?1 AND sampled_hash = ?3 AND cas_id IS NULL",
                        params![id, &full[..], sampled],
                    )?;
                }
                Err(e) if e.is::<Cancelled>() => return Err(e),
                Err(e) => tracing::debug!("confirm {}: {e:#}", path.display()),
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
        loop {
            if !self.counted {
                for id in &self.sources {
                    if let Some(s) = source(id) {
                        self.total += s.store.get()?.query_row(
                            "SELECT count(*) FROM record
                             WHERE kind = 0 AND flags & ?1 = 0 AND sampled_hash IS NULL
                                 AND size <= ?2",
                            params![UNREADABLE | LINK, size_cap(&lib, &s)],
                            |r| r.get::<_, i64>(0),
                        )? as u64;
                    }
                }
                self.counted = true;
                ctx.checkpoint(self.checkpoint(), self.progress())?;
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
            // Hashing was asked for again meanwhile (a walk found new files): once more.
            let mut slot = lib.hash_job.lock();
            if lib.hash_again.swap(false, Ordering::SeqCst) {
                drop(slot);
                *self = HashJob {
                    total: self.total,
                    done: self.done,
                    full: self.full,
                    errors: self.errors,
                    skipped: std::mem::take(&mut self.skipped),
                    ..HashJob::new(&lib)
                };
                continue;
            }
            if *slot == Some(ctx.id) {
                *slot = None;
            }
            // Still running for a while (reconfirm, counts): `schedule` must not count on
            // it, it no longer looks at `hash_again`.
            lib.hash_finishing.store(ctx.id, Ordering::SeqCst);
            break;
        }
        self.reconfirm(ctx)?;
        ctx.log(&format!(
            "hashed {} files, {} whole after a collision, {} unreadable",
            self.done, self.full, self.errors
        ))?;
        ctx.set_result(serde_json::to_value(self.result())?)?;
        crate::library::count_unique(&lib)?;
        crate::protect::recount(&lib)?;
        Ok(())
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<HashJob>(v)?))
    }
}

/// The newest queued or running hash job in `library.db`.
fn pending_hash_job(lib: &Shared) -> Result<Option<JobId>> {
    Ok(lib
        .db
        .get()?
        .query_row(
            "SELECT id FROM job WHERE kind = 'hash' AND status IN ('queued', 'running')
             ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// `Library::hash`: the hash job that is running (it goes over every source once more when
/// it is done), a pending one left by an earlier session (resumed now), else a new one.
pub(crate) fn schedule(lib: &Arc<Shared>) -> Result<JobId> {
    let mut slot = lib.hash_job.lock();
    let pending = match *slot {
        Some(id) if crate::jobs::is_running(lib, id) => Some(id),
        _ => pending_hash_job(lib)?,
    };
    let finishing = lib.hash_finishing.load(Ordering::SeqCst);
    let id = match pending.filter(|id| *id != finishing) {
        Some(id) if crate::jobs::is_running(lib, id) || crate::jobs::resume(lib, id)? => {
            lib.hash_again.store(true, Ordering::SeqCst);
            id
        }
        _ => crate::jobs::spawn(lib, Box::new(HashJob::new(lib)))?,
    };
    *slot = Some(id);
    Ok(id)
}

/// Starts hashing after a completed walk of `src` when that is on and `src` has files to
/// hash: not over the remote size cap, and not waiting out a failed read.
pub(crate) fn after_walk(lib: &Arc<Shared>, src: &Source) {
    if !lib.hash_after_walk.load(Ordering::SeqCst)
        || lib.closing()
        || hash_skip_reason(lib, src).is_some()
    {
        return;
    }
    // One more row than there are backed-off records: at least one of them is not.
    let backed = lib
        .hash_backoff
        .lock()
        .keys()
        .filter(|k| k.0 == src.id)
        .count();
    let unhashed = src.store.get().and_then(|c| {
        let mut stmt = c.prepare_cached(
            "SELECT id, size, mtime FROM record
             WHERE kind = 0 AND flags & ?1 = 0 AND sampled_hash IS NULL AND size <= ?2
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![UNREADABLE | LINK, size_cap(lib, src), backed as i64 + 1],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?)),
        )?;
        let rows: Vec<(i64, u64, Option<i64>)> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .any(|(id, size, mtime)| !backed_off(lib, src, id, size, mtime)))
    });
    match unhashed {
        Ok(true) => {
            if let Err(e) = schedule(lib) {
                tracing::warn!("hashing after a walk of {}: {e:#}", src.def.label);
            }
        }
        Ok(false) => {}
        Err(e) => tracing::warn!("hashing after a walk of {}: {e:#}", src.def.label),
    }
}

impl Library {
    /// Hashes every file still without a sampled hash, as a durable idle-priority job: the
    /// one already running (it goes over every source once more when it is done), else a
    /// new one.
    pub fn hash(&self) -> Result<JobId> {
        schedule(&self.shared)
    }

    /// Current remote hashing policy (defaults: remotes on, cloud off, 1 GiB cap).
    pub fn remote_hash_settings(&self) -> RemoteHashSettings {
        *self.shared.remote_hash_settings.read()
    }

    /// Saves the remote hashing policy. It applies at once: a running job stops reading
    /// from sources whose hashing was turned off (the file in progress included), a new cap
    /// applies from the next file.
    pub fn set_remote_hash_settings(&self, settings: RemoteHashSettings) -> Result<()> {
        let mut current = self.shared.remote_hash_settings.write();
        self.shared
            .db
            .set_meta("remote_hash_settings", &serde_json::to_string(&settings)?)?;
        *current = settings;
        Ok(())
    }

    /// Tells the background jobs the user is busy (call it on every input): integrity jobs
    /// pause for the next 5 s, hashing too unless `set_hash_idle_only(false)`, sidecar jobs
    /// for the next 1 s.
    pub fn note_activity(&self) {
        let until = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| {
                (d + crate::library::ACTIVITY_PAUSE).as_millis() as u64
            });
        self.shared.busy_until.fetch_max(until, Ordering::SeqCst);
    }

    /// Whether hashing pauses on user activity like the other background jobs (default on;
    /// off: it runs through input, the "pause on battery" / "always" policies).
    pub fn set_hash_idle_only(&self, on: bool) {
        self.shared.hash_on_activity.store(on, Ordering::SeqCst);
    }

    /// `set_hash_idle_only`'s setting.
    pub fn hash_idle_only(&self) -> bool {
        self.shared.hash_on_activity.load(Ordering::SeqCst)
    }

    /// Whether `note_activity` was called within the last 5 s (background jobs pause).
    pub fn user_active(&self) -> bool {
        self.shared.busy()
    }

    /// Whether hashing pauses on battery power (default on).
    pub fn set_pause_on_battery(&self, pause: bool) {
        self.shared.pause_on_battery.store(pause, Ordering::SeqCst);
    }

    /// Groups of two or more files of at least `min_size` bytes with the same content id,
    /// across every source but device sources; biggest first. Hard links of one file count as one file.
    pub fn duplicates(&self, min_size: u64) -> Result<Vec<DupGroup>> {
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        // A private on-disk scratch database (SQLite spills it to a temp file).
        let scratch = Connection::open("")?;
        scratch.execute_batch(
            "CREATE TABLE c(cas BLOB, src INTEGER, id INTEGER, size INTEGER, file TEXT)",
        )?;
        for (i, s) in sources.iter().enumerate() {
            // A device's claims are not copies (nor files "Keep one" may weigh).
            if s.is_device() {
                continue;
            }
            let path = s.store_dir().join("source.db");
            scratch.execute("ATTACH DATABASE ?1 AS s", [path.to_string_lossy()])?;
            let copied = scratch.execute(
                "INSERT INTO c SELECT cas_id, ?1, id, size,
                     CASE WHEN substr(fs_id, 1, 2) <> 'h:' THEN fs_id ELSE ?1 || ':' || id END
                 FROM s.record WHERE kind = 0 AND cas_id IS NOT NULL AND drift IS NULL
                     AND size >= ?2 ORDER BY id",
                params![i as i64, min_size as i64],
            );
            scratch.execute("DETACH DATABASE s", [])?;
            copied?;
        }
        scratch.execute_batch("CREATE INDEX c_cas ON c(cas, file)")?;
        // One record per file (the first of its hard links), in groups of 2+ files.
        let mut stmt = scratch.prepare(
            "SELECT cas, src, id, size FROM c
             WHERE rowid IN (SELECT min(rowid) FROM c GROUP BY cas, file)
                 AND cas IN (SELECT cas FROM c GROUP BY cas HAVING count(DISTINCT file) > 1)
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

    /// True unless another file is known to hold the same content (also true while the
    /// record has no content id yet: no other copy is known). A hard link of the same file
    /// is not another copy, nor is a copy on a lost or retired volume.
    pub fn last_copy(&self, record: &RecordRef) -> Result<bool> {
        Ok(self.redundancy(record)?.copies <= 1)
    }
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
