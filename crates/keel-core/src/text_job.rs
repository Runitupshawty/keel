//! Text inside documents (Task 62): the text job reads the text of a source's documents
//! (through the extractor the program registers, `keel_preview::extract_text`) into the
//! source store's `record_text`, `record_text_body` and `record_content` (FTS5), at idle
//! priority, pausing on battery and for 5 s after user activity like hashing, with
//! checkpoints a restart resumes from. A record is read again when its size or modified
//! time changed. Each read is bounded (`TextCaps`): too big is skipped unread, and an
//! extraction past its time is abandoned on its helper thread (the record fails). The
//! record being read is noted in the store first: if a reader takes the process down (an
//! abort no panic hook sees), the next run marks that record failed instead of reading it
//! again at every start.
//!
//! **Index file contents** (`Library::set_index_contents`, on by default) and the
//! per-source switch (`Library::set_source_contents`): local folders and drives are read
//! by default; network shares, SFTP, cloud and paired-device sources only when their
//! switch is turned on (reading every document over the network is a real cost).
//! Turning it off for a source deletes the text read from it.
//!
//! The text never leaves the store: it is not logged (failures log the path and the
//! reader's short reason), and searches return at most a bounded snippet of it.

use crate::hash::background_thread;
use crate::index::{LINK, UNREADABLE};
use crate::jobs::{Job, JobCtx, JobId};
use crate::library::{Shared, Source};
use crate::{Library, SourceId, SourceKind};
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// What one extraction may read, keep and take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextCaps {
    /// Bigger files are skipped unread (PDFs: `max_pdf_bytes`).
    pub max_bytes: u64,
    pub max_pdf_bytes: u64,
    /// Characters kept per file; the rest is cut (the record is marked truncated).
    pub max_chars: usize,
    /// The reader stops here and keeps what it has (truncated); one stuck in a single call
    /// is abandoned `ABANDON_AFTER` later and the record fails.
    pub time: Duration,
}

/// The caps the text job uses.
pub const TEXT_CAPS: TextCaps = TextCaps {
    max_bytes: 8 << 20,
    max_pdf_bytes: 64 << 20,
    max_chars: 200_000,
    time: Duration::from_secs(10),
};

/// One extraction's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextOutcome {
    Done {
        text: String,
        truncated: bool,
    },
    /// No reader for this kind of file (or not text after all: a binary `.txt`).
    SkippedKind,
    /// Over the size cap.
    SkippedSize,
    /// A damaged file (or one the reader refuses: password-protected); the reason is short
    /// and never holds the document's text.
    Failed(String),
}

pub type ExtractFn = dyn Fn(&Path, &str, &TextCaps) -> TextOutcome + Send + Sync;

/// The program's text reader: the extensions it reads (lowercase, without the dot) and
/// the function (`path`, extension, caps).
#[derive(Clone)]
pub struct TextExtractor {
    pub extensions: &'static [&'static str],
    pub extract: Arc<ExtractFn>,
}

/// `record_text.status`.
pub(crate) const DONE: i64 = 0;
pub(crate) const SKIPPED_KIND: i64 = 1;
pub(crate) const SKIPPED_SIZE: i64 = 2;
pub(crate) const FAILED: i64 = 3;

/// Records per checkpoint.
const CHECKPOINT_EVERY: usize = if cfg!(test) { 10 } else { 100 };
/// Failures written to the job log (the rest are only counted).
const LOGGED_ERRORS: u64 = 20;
/// A reader past `TextCaps::time` by this much is abandoned.
const ABANDON_AFTER: Duration = Duration::from_secs(5);
/// Abandoned readers still running at most; past that every text job stops early (`Stuck`)
/// until some of them end. A reader that never ends holds its place until the program
/// restarts (ponytail: no reset per run, which would let hung threads pile up).
const MAX_ABANDONED: usize = 3;
/// `library.db` meta key of **Index file contents** ("0" = off).
const SETTING: &str = "index_contents";
/// Source store meta key of the per-source switch ("1" on, "0" off; absent: by kind).
const SOURCE_SETTING: &str = "index_contents";
/// Source store meta key: the record whose document a reader has open now. A reader that
/// takes the process down (a stack overflow or a failed allocation aborts it, and no panic
/// hook sees that) leaves it behind: the next run marks that record failed instead of
/// reading it again at every start.
const READING: &str = "text_reading";

/// The text counts of a source, or summed over the library (`LibraryStats::text`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextCounts {
    /// Documents whose text is indexed.
    pub documents: u64,
    /// Files skipped: no reader for their kind, or over the size cap.
    pub skipped: u64,
    /// Damaged files (and readers that ran out of time).
    pub failed: u64,
    /// Characters indexed.
    pub chars: u64,
}

impl TextCounts {
    pub(crate) fn add(&mut self, o: &TextCounts) {
        self.documents += o.documents;
        self.skipped += o.skipped;
        self.failed += o.failed;
        self.chars += o.chars;
    }
}

/// The text counts of `src`'s store.
pub(crate) fn counts(src: &Source) -> Result<TextCounts> {
    Ok(src.store.get()?.query_row(
        "SELECT coalesce(sum(status = ?1), 0), coalesce(sum(status IN (?2, ?3)), 0),
             coalesce(sum(status = ?4), 0), coalesce(sum(chars), 0) FROM record_text",
        params![DONE, SKIPPED_KIND, SKIPPED_SIZE, FAILED],
        |r| {
            Ok(TextCounts {
                documents: r.get::<_, i64>(0)? as u64,
                skipped: r.get::<_, i64>(1)? as u64,
                failed: r.get::<_, i64>(2)? as u64,
                chars: r.get::<_, i64>(3)? as u64,
            })
        },
    )?)
}

/// Whether `src` is read by default: a local folder or drive, not a network share (a UNC
/// path, a share source, or a drive mapped to a share: its volume's type).
pub(crate) fn default_on(src: &Source) -> bool {
    let Some(root) = src.def.root.to_local_path() else {
        return false;
    };
    !root.to_string_lossy().starts_with(r"\\")
        && src.def.kind != SourceKind::Share
        && !src.is_device()
        && !on_network(src, &root)
}

/// Whether `src`'s root is on a network volume, asked once the volume can be told (an
/// unplugged drive is not, until it is seen: its text stays searchable, and nothing is
/// read from a root that is not there).
fn on_network(src: &Source, root: &Path) -> bool {
    if let Some(n) = src.network.get() {
        return *n;
    }
    keel_vfs::volume_info(root).is_some_and(|v| {
        *src.network
            .get_or_init(|| v.kind == keel_vfs::VolumeType::Network)
    })
}

/// Whether the text of `src`'s documents is read and searched now.
pub(crate) fn contents_on(lib: &Shared, src: &Source) -> bool {
    lib.index_contents.load(Ordering::SeqCst) && src.contents.read().unwrap_or(default_on(src))
}

/// The per-source switch kept in the store (None: by kind).
pub(crate) fn load_switch(src_store: &crate::db::Pool) -> Result<Option<bool>> {
    Ok(src_store.meta(SOURCE_SETTING)?.map(|v| v == "1"))
}

/// The library's **Index file contents** as stored (default on).
pub(crate) fn load_setting(db: &crate::db::Pool) -> Result<bool> {
    Ok(db.meta(SETTING)?.is_none_or(|v| v != "0"))
}

/// SQL (on `record r`): a file the extractor reads, by its extension.
fn name_sql(extensions: &[&str]) -> String {
    if extensions.is_empty() {
        return "0".into();
    }
    let likes: Vec<String> = extensions
        .iter()
        .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|e| format!("r.name LIKE '%.{e}'"))
        .collect();
    format!("({})", likes.join(" OR "))
}

/// SQL (on `record r`, `record_text t`): a document to read now.
fn pending_sql(extensions: &[&str]) -> String {
    format!(
        "r.kind = 0 AND r.flags & {} = 0 AND {} AND
         (t.record IS NULL OR t.size <> r.size OR t.mtime IS NOT r.mtime)",
        UNREADABLE | LINK,
        name_sql(extensions)
    )
}

/// Deletes every text row of `src` (the switch went off).
pub(crate) fn clear(src: &Source) -> Result<()> {
    let mut c = src.store.get()?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(&format!(
        "INSERT INTO record_content(record_content) VALUES ('delete-all');
         DELETE FROM record_text_body;
         DELETE FROM record_text;
         DELETE FROM meta WHERE key = '{READING}';"
    ))?;
    tx.commit()?;
    Ok(())
}

/// Starts a text job for `src` when contents are on for it and it has documents to read;
/// one already running goes round once more instead.
pub(crate) fn schedule(lib: &Arc<Shared>, src: &Source) -> Result<Option<JobId>> {
    if lib.closing() || !contents_on(lib, src) || src.removed.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let Some(x) = lib.text_extractor.read().clone() else {
        return Ok(None);
    };
    let pending: bool = src
        .store
        .get()?
        .prepare_cached(&format!(
            "SELECT 1 FROM record r LEFT JOIN record_text t ON t.record = r.id
             WHERE {} LIMIT 1",
            pending_sql(x.extensions)
        ))?
        .exists([])?;
    if !pending {
        return Ok(None);
    }
    let mut running = lib.text_jobs.lock();
    if let Some(again) = running.get_mut(&src.id) {
        *again = true;
        return Ok(None);
    }
    running.insert(src.id.clone(), false);
    match crate::jobs::spawn(lib, Box::new(TextJob::new(src.id.clone()))) {
        Ok(id) => Ok(Some(id)),
        Err(e) => {
            running.remove(&src.id);
            Err(e)
        }
    }
}

/// After a completed walk of `src` (index jobs and watchers).
pub(crate) fn after_walk(lib: &Arc<Shared>, src: &Source) {
    if let Err(e) = schedule(lib, src) {
        tracing::warn!("text after a walk of {}: {e:#}", src.def.label);
    }
}

/// One document to read.
struct Pending {
    id: i64,
    path: String,
    name: String,
    size: u64,
    mtime: Option<i64>,
}

impl Pending {
    fn ext(&self) -> String {
        self.name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default()
    }
}

/// The text job of one source (`Library::index_text`).
#[derive(Debug, Serialize, Deserialize)]
pub struct TextJob {
    source: SourceId,
    /// Records up to this id are done in this round.
    after: i64,
    counted: bool,
    pub total: u64,
    pub done: u64,
    pub documents: u64,
    pub skipped: u64,
    pub failed: u64,
    pub chars: u64,
    #[serde(skip)]
    battery: Option<(Instant, bool)>,
}

/// Removes the source's entry from `Shared::text_jobs` when the job ends early (an error,
/// a stop); a job that ends normally removes it itself, under the lock it checked it with.
struct Claim<'a>(&'a Shared, Option<SourceId>);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.1.take() {
            self.0.text_jobs.lock().remove(&id);
        }
    }
}

/// Why a round stopped early (not an error of the job).
#[derive(Debug)]
struct Stuck;

impl std::fmt::Display for Stuck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "readers are stuck; the rest is read once they end, or after the program restarts",
        )
    }
}

impl std::error::Error for Stuck {}

impl TextJob {
    pub const KIND: &'static str = "text";

    pub fn new(source: SourceId) -> TextJob {
        TextJob {
            source,
            after: 0,
            counted: false,
            total: 0,
            done: 0,
            documents: 0,
            skipped: 0,
            failed: 0,
            chars: 0,
            battery: None,
        }
    }

    fn progress(&self) -> f32 {
        match self.total {
            0 => 0.0,
            t => (self.done as f32 / t as f32).min(0.99),
        }
    }

    /// One round over the pending documents of `src` (ids after `after`).
    fn round(&mut self, ctx: &JobCtx, src: &Source, x: &TextExtractor) -> Result<()> {
        let lib = &ctx.lib;
        let label = &src.def.label;
        if !contents_on(lib, src) {
            return ctx.log(&format!("{label}: file contents are not indexed, skipped"));
        }
        let root = src.def.root.to_local_path();
        let reachable = match &root {
            Some(r) => r.is_dir(),
            None => (ctx.router().provider_for(&src.def.root))
                .is_some_and(|p| p.stat(&src.def.root).is_ok()),
        };
        if !reachable {
            return ctx.log(&format!("{label}: offline, skipped"));
        }
        let filter = pending_sql(x.extensions);
        if !self.counted {
            self.total = src.store.get()?.query_row(
                &format!(
                    "SELECT count(*) FROM record r LEFT JOIN record_text t ON t.record = r.id
                     WHERE r.id > ?1 AND {filter}"
                ),
                [self.after],
                |r| r.get::<_, i64>(0),
            )? as u64
                + self.done;
            self.counted = true;
            ctx.checkpoint(self.checkpoint(), self.progress())?;
        }
        loop {
            let batch: Vec<Pending> = {
                let c = src.store.get()?;
                let mut stmt = c.prepare_cached(&format!(
                    "SELECT r.id, r.path, r.name, r.size, r.mtime
                     FROM record r LEFT JOIN record_text t ON t.record = r.id
                     WHERE r.id > ?1 AND {filter} ORDER BY r.id LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![self.after, CHECKPOINT_EVERY as i64], |r| {
                    Ok(Pending {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        name: r.get(2)?,
                        size: r.get::<_, i64>(3)? as u64,
                        mtime: r.get(4)?,
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
                crate::hash::wait_until_idle(
                    ctx,
                    &mut self.battery,
                    crate::library::ACTIVITY_PAUSE,
                )?;
                // The switch may have gone off meanwhile: it applies from the next file.
                if !contents_on(lib, src) {
                    return ctx.log(&format!("{label}: file contents are not indexed, stopped"));
                }
                let outcome = read(ctx, src, root.as_deref(), &rec, x)?;
                if let TextOutcome::Failed(why) = &outcome {
                    self.failed += 1;
                    if self.failed <= LOGGED_ERRORS {
                        ctx.log(&format!("{}: {}", rec.path, short(why)))?;
                    }
                }
                match &outcome {
                    TextOutcome::Done { text, .. } => {
                        self.documents += 1;
                        self.chars += text.chars().count() as u64;
                    }
                    TextOutcome::SkippedKind | TextOutcome::SkippedSize => self.skipped += 1,
                    TextOutcome::Failed(_) => {}
                }
                retrying(ctx, || save(lib, src, &rec, outcome.clone()))?;
                self.after = rec.id;
                self.done += 1;
            }
            ctx.checkpoint(self.checkpoint(), self.progress())?;
        }
    }
}

/// The text of one document: read locally, or downloaded (up to the cap) into the
/// store's folder first; the reader runs on a helper thread at idle priority, abandoned
/// when it takes `ABANDON_AFTER` past its time.
fn read(
    ctx: &JobCtx,
    src: &Source,
    root: Option<&Path>,
    rec: &Pending,
    x: &TextExtractor,
) -> Result<TextOutcome> {
    let caps = *ctx.lib.text_caps.read();
    let ext = rec.ext();
    let cap = if ext == "pdf" {
        caps.max_pdf_bytes
    } else {
        caps.max_bytes
    };
    if rec.size > cap {
        return Ok(TextOutcome::SkippedSize);
    }
    if ctx.lib.text_abandoned.load(Ordering::SeqCst) >= MAX_ABANDONED {
        return Err(Stuck.into());
    }
    let (path, scratch) = match root {
        Some(root) => (root.join(&rec.path), None),
        None => match download(ctx, src, rec, &ext, cap) {
            Ok(Some(p)) => (p.clone(), Some(p)),
            Ok(None) => return Ok(TextOutcome::SkippedSize),
            Err(e) => return Ok(TextOutcome::Failed(short(&format!("{e:#}")))),
        },
    };
    // Cleared by `save`; left behind only when the reader takes the process down.
    retrying(ctx, || src.store.set_meta(READING, &rec.id.to_string()))?;
    let (tx, rx) = crossbeam_channel::bounded(1);
    // Set by whichever comes first: the reader ending, or the job giving up on it.
    let settled = Arc::new(AtomicBool::new(false));
    let (f, ended) = (x.extract.clone(), settled.clone());
    let abandoned = ctx.lib.text_abandoned.clone();
    std::thread::Builder::new()
        .name("keel-text".into())
        .spawn(move || {
            background_thread();
            let caught = keel_vfs::quiet_panic::quiet(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&path, &ext, &caps)))
            });
            let out = caught.unwrap_or_else(|_| TextOutcome::Failed("the reader crashed".into()));
            if let Some(p) = scratch {
                let _ = std::fs::remove_file(p);
            }
            if ended.swap(true, Ordering::SeqCst) {
                abandoned.fetch_sub(1, Ordering::SeqCst);
            }
            let _ = tx.send(out);
        })
        .context("starting a text reader")?;
    match rx.recv_timeout(caps.time + ABANDON_AFTER) {
        Ok(out) => Ok(out),
        Err(_) if settled.swap(true, Ordering::SeqCst) => Ok(rx
            .recv()
            .unwrap_or_else(|_| TextOutcome::Failed("the reader crashed".into()))),
        Err(_) => {
            ctx.lib.text_abandoned.fetch_add(1, Ordering::SeqCst);
            Ok(TextOutcome::Failed("timed out".into()))
        }
    }
}

/// A remote document copied into `src`'s store folder (None: longer than `cap`). The
/// reader deletes it when done.
fn download(
    ctx: &JobCtx,
    src: &Source,
    rec: &Pending,
    ext: &str,
    cap: u64,
) -> Result<Option<PathBuf>> {
    let path = src.absolute(&rec.path);
    let provider = (ctx.router().provider_for(&path)).context("remote unavailable")?;
    let mut reader = provider.read(&path)?;
    let file = src
        .store_dir()
        .join(format!("text-{}.{ext}", crate::random_id()?));
    let mut out = std::fs::File::create(&file)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    let result = (|| -> Result<bool> {
        loop {
            if ctx.stopping() {
                return Err(crate::Cancelled.into());
            }
            let n = match reader.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => r?,
            };
            if n == 0 {
                return Ok(true);
            }
            total += n as u64;
            if total > cap {
                return Ok(false);
            }
            out.write_all(&buf[..n])?;
        }
    })();
    drop(out);
    match result {
        Ok(true) => Ok(Some(file)),
        other => {
            let _ = std::fs::remove_file(&file);
            other.map(|_| None)
        }
    }
}

/// A reader's reason, short: whatever a reader says goes into the job log through this, so
/// a reason that quotes the document (a reader's own error message) keeps at most a few
/// words of it.
fn short(why: &str) -> String {
    let mut s: String = why.chars().take(120).collect();
    if s.len() < why.len() {
        s.push('…');
    }
    s
}

/// A write, again after a pause while the store stays busy past its 5 s timeout (a walk
/// or another job writing a big batch on a loaded machine), up to 3 times.
fn retrying(ctx: &JobCtx, mut write: impl FnMut() -> Result<()>) -> Result<()> {
    let busy = |e: &anyhow::Error| {
        e.chain().any(|c| {
            matches!(c.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(f, _))
                    if f.code == rusqlite::ErrorCode::DatabaseBusy
                        || f.code == rusqlite::ErrorCode::DatabaseLocked)
        })
    };
    let mut tries = 0;
    loop {
        match write() {
            Err(e) if busy(&e) && tries < 3 && !ctx.stopping() => {
                tries += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
            r => return r,
        }
    }
}

/// Writes one outcome: the status row, and the text into the body and the index (the
/// old text out first). A record the walk removed meanwhile is left alone, and nothing is
/// written once contents are off for the source.
fn save(lib: &Shared, src: &Source, rec: &Pending, outcome: TextOutcome) -> Result<()> {
    let mut c = src.store.get()?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM meta WHERE key = ?1", [READING])?;
    // The switch may have gone off since the read. Checked under the store's write lock:
    // turning it off sets it first and then deletes the text under that lock.
    if !contents_on(lib, src) {
        return Ok(tx.commit()?);
    }
    let old: Option<String> = tx
        .query_row(
            "SELECT text FROM record_text_body WHERE record = ?1",
            [rec.id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        tx.execute(
            "INSERT INTO record_content(record_content, rowid, text) VALUES ('delete', ?1, ?2)",
            params![rec.id, old],
        )?;
        tx.execute("DELETE FROM record_text_body WHERE record = ?1", [rec.id])?;
    }
    let exists = tx
        .prepare_cached("SELECT 1 FROM record WHERE id = ?1")?
        .exists([rec.id])?;
    if !exists {
        tx.execute("DELETE FROM record_text WHERE record = ?1", [rec.id])?;
        return Ok(tx.commit()?);
    }
    let (status, chars, truncated) = match outcome {
        TextOutcome::Done { text, truncated } => {
            // The snippets' emphasis marks: a document's own would show as matched words.
            let marks = [crate::search::MARK_ON, crate::search::MARK_OFF];
            let text = match text.contains(marks) {
                true => text.replace(marks, ""),
                false => text,
            };
            let chars = text.chars().count() as i64;
            if !text.trim().is_empty() {
                tx.execute(
                    "INSERT INTO record_text_body(record, text) VALUES (?1, ?2)",
                    params![rec.id, text],
                )?;
                tx.execute(
                    "INSERT INTO record_content(rowid, text) VALUES (?1, ?2)",
                    params![rec.id, text],
                )?;
            }
            (DONE, chars, truncated)
        }
        TextOutcome::SkippedKind => (SKIPPED_KIND, 0, false),
        TextOutcome::SkippedSize => (SKIPPED_SIZE, 0, false),
        TextOutcome::Failed(_) => (FAILED, 0, false),
    };
    tx.execute(
        "INSERT OR REPLACE INTO record_text(record, mtime, size, chars, truncated, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![rec.id, rec.mtime, rec.size as i64, chars, truncated, status],
    )?;
    tx.commit()?;
    Ok(())
}

/// Copies of remote documents (`download`) whose reader never deleted them: the program
/// stopped while it read. Swept when the job starts (it is the only one of its source).
fn sweep(src: &Source) {
    let Ok(dir) = std::fs::read_dir(src.store_dir()) else {
        return;
    };
    for e in dir.flatten() {
        if e.file_name().to_string_lossy().starts_with("text-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The record a reader had open when the program last stopped (`READING`), saved as
/// failed with its size and time so it is not read again until it changes. Its path.
fn crashed(lib: &Shared, src: &Source) -> Result<Option<String>> {
    let Some(id) = src.store.meta(READING)? else {
        return Ok(None);
    };
    let rec = src
        .store
        .get()?
        .query_row(
            "SELECT id, path, name, size, mtime FROM record WHERE id = ?1",
            [id.parse::<i64>().unwrap_or(-1)],
            |r| {
                Ok(Pending {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name: r.get(2)?,
                    size: r.get::<_, i64>(3)? as u64,
                    mtime: r.get(4)?,
                })
            },
        )
        .optional()?;
    match rec {
        Some(rec) => {
            save(lib, src, &rec, TextOutcome::Failed(String::new()))?;
            Ok(Some(rec.path))
        }
        None => {
            (src.store.get()?).execute("DELETE FROM meta WHERE key = ?1", [READING])?;
            Ok(None)
        }
    }
}

impl Job for TextJob {
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
        let Some(x) = ctx.lib.text_extractor.read().clone() else {
            return ctx.log("no text reader in this program: skipped");
        };
        ctx.lib
            .text_jobs
            .lock()
            .entry(src.id.clone())
            .or_insert(false);
        let mut claim = Claim(&ctx.lib, Some(src.id.clone()));
        sweep(&src);
        if let Some(path) = crashed(&ctx.lib, &src)? {
            self.failed += 1;
            ctx.log(&format!(
                "{path}: the program stopped while reading it; not read again until it changes"
            ))?;
        }
        loop {
            match self.round(ctx, &src, &x) {
                Err(e) if e.is::<Stuck>() => {
                    ctx.log(&format!("{e}"))?;
                    break;
                }
                r => r?,
            }
            // Asked for again while it ran (a walk, a switch turned on): once more.
            let mut running = ctx.lib.text_jobs.lock();
            if running.get(&src.id) == Some(&true) {
                running.insert(src.id.clone(), false);
                drop(running);
                (self.after, self.counted) = (0, false);
                continue;
            }
            running.remove(&src.id);
            claim.1 = None;
            break;
        }
        ctx.log(&format!(
            "{} documents read: {} indexed ({} characters), {} skipped, {} failed",
            self.done, self.documents, self.chars, self.skipped, self.failed
        ))
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<TextJob>(v)?))
    }
}

impl Library {
    /// Registers the program's text reader (keel-app and keel-daemon pass
    /// `keel_preview::text_extractor()`); without one no text is read.
    pub fn set_text_extractor(&self, x: TextExtractor) {
        *self.shared.text_extractor.write() = Some(x);
    }

    /// Starts the text job of every source with documents to read (the program calls it
    /// once the library is open; walks call it for their source).
    pub fn start_text_jobs(&self) -> Vec<JobId> {
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        let mut out = Vec::new();
        for s in sources {
            match schedule(&self.shared, &s) {
                Ok(Some(id)) => out.push(id),
                Ok(None) => {}
                Err(e) => tracing::warn!("text of {}: {e:#}", s.def.label),
            }
        }
        out
    }

    /// Starts reading the text of `source`'s documents now (None: nothing to read, contents
    /// are off for it, or its job runs and goes round once more).
    pub fn index_text(&self, source: &SourceId) -> Result<Option<JobId>> {
        let src = self
            .source(source)
            .with_context(|| format!("no source {source}"))?;
        schedule(&self.shared, &src)
    }

    /// **Index file contents** (Settings → Library): on by default.
    pub fn index_contents(&self) -> bool {
        self.shared.index_contents.load(Ordering::SeqCst)
    }

    /// Turns **Index file contents** on (starting the text jobs) or off (deleting the text
    /// read so far from every source).
    pub fn set_index_contents(&self, on: bool) -> Result<()> {
        self.shared
            .db
            .set_meta(SETTING, if on { "1" } else { "0" })?;
        self.shared.index_contents.store(on, Ordering::SeqCst);
        if on {
            self.start_text_jobs();
            return Ok(());
        }
        let sources: Vec<Arc<Source>> = self.shared.sources.read().clone();
        for s in sources {
            clear(&s)?;
        }
        Ok(())
    }

    /// A source's switch (None: by its kind, on for local folders and drives) and whether
    /// its contents are read now (the switch and **Index file contents** both on).
    pub fn source_contents(&self, id: &SourceId) -> Result<(Option<bool>, bool)> {
        let src = self.source(id).with_context(|| format!("no source {id}"))?;
        let switch = *src.contents.read();
        Ok((switch, contents_on(&self.shared, &src)))
    }

    /// Sets a source's switch (None: back to its kind's default). Off deletes the text
    /// read from it; on starts its text job.
    pub fn set_source_contents(&self, id: &SourceId, on: Option<bool>) -> Result<()> {
        let src = self.source(id).with_context(|| format!("no source {id}"))?;
        match on {
            Some(on) => src
                .store
                .set_meta(SOURCE_SETTING, if on { "1" } else { "0" })?,
            None => {
                src.store
                    .get()?
                    .execute("DELETE FROM meta WHERE key = ?1", [SOURCE_SETTING])?;
            }
        }
        *src.contents.write() = on;
        if contents_on(&self.shared, &src) {
            schedule(&self.shared, &src)?;
        } else {
            clear(&src)?;
        }
        Ok(())
    }

    /// Tests: smaller caps.
    #[cfg(test)]
    pub(crate) fn set_text_caps(&self, caps: TextCaps) {
        *self.shared.text_caps.write() = caps;
    }
}

#[cfg(test)]
#[path = "text_job_tests.rs"]
pub(crate) mod tests;
