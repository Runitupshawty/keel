//! Durable jobs: each job's checkpoint lives in `library.db` (`job` table) and jobs that were
//! queued or running when the library closed resume on the next open (built-in kinds) or
//! when their kind is registered.

use crate::db::Pool;
use crate::library::Shared;
use crate::{Cancelled, Indexer, SourceId};
use anyhow::{Context, Result};
use keel_vfs::Router;
use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub type JobId = i64;

/// Turns a checkpoint back into a job (`Job::restore`).
pub type Restore = fn(serde_json::Value) -> Result<Box<dyn Job>>;

pub trait Job: Send {
    fn kind(&self) -> &'static str;
    /// Does the work, calling `ctx.checkpoint` (or `ctx.cursor`) after each durable step;
    /// returns `Err(Cancelled)` (what those return once stopped) when asked to stop.
    fn run(&mut self, ctx: &JobCtx) -> Result<()>;
    /// Everything `restore` needs to continue after the last completed step.
    fn checkpoint(&self) -> serde_json::Value;
    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>>
    where
        Self: Sized;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }
    fn parse(s: &str) -> JobStatus {
        match s {
            "queued" => JobStatus::Queued,
            "running" => JobStatus::Running,
            "done" => JobStatus::Done,
            "cancelled" => JobStatus::Cancelled,
            _ => JobStatus::Failed,
        }
    }
}

/// A job's progress or end, for the app's jobs panel (`Jobs::subscribe`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    pub id: JobId,
    pub status: JobStatus,
    /// 0..=1.
    pub progress: f32,
}

/// Events a slow subscriber may fall behind by before it misses some.
const EVENT_BACKLOG: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: JobId,
    pub kind: String,
    pub status: JobStatus,
    /// 0..=1.
    pub progress: f32,
    pub log: String,
    pub created: i64,
    pub updated: i64,
    /// What the job reported (`JobCtx::set_result`), kept after it ends; a hash job's is a
    /// [`crate::HashResult`].
    pub result: Option<serde_json::Value>,
}

/// What a running job gets: its id, stop signal, checkpointing and the library.
pub struct JobCtx {
    pub id: JobId,
    stop: Arc<AtomicBool>,
    pub(crate) lib: Arc<Shared>,
}

impl JobCtx {
    /// Set when the job is cancelled or the library is closing; pass it to long calls.
    pub fn stop_flag(&self) -> &AtomicBool {
        &self.stop
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The stop is the library closing (the job resumes later), not a cancel.
    pub fn closing(&self) -> bool {
        self.lib.jobs.closing.load(Ordering::SeqCst)
    }

    /// Persists `state` (and progress 0..=1). The step it describes is durable: a restart
    /// continues from here. Returns `Err(Cancelled)` once the job should stop.
    pub fn checkpoint(&self, state: serde_json::Value, progress: f32) -> Result<()> {
        self.lib.db.get()?.execute(
            "UPDATE job SET state = ?2, cursor = NULL, progress = ?3, updated = ?4 WHERE id = ?1",
            params![self.id, state.to_string(), progress, crate::now()],
        )?;
        self.written(progress)
    }

    /// `checkpoint` for a job with a big fixed part (an operation's path list, stored once
    /// at spawn): persists only `cursor`, an object whose keys are laid over the last full
    /// state when the job is restored.
    pub fn cursor(&self, cursor: serde_json::Value, progress: f32) -> Result<()> {
        self.lib.db.get()?.execute(
            "UPDATE job SET cursor = ?2, progress = ?3, updated = ?4 WHERE id = ?1",
            params![self.id, cursor.to_string(), progress, crate::now()],
        )?;
        self.written(progress)
    }

    fn written(&self, progress: f32) -> Result<()> {
        self.running(progress);
        if self.stopping() {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    pub fn progress(&self, progress: f32) -> Result<()> {
        self.lib.db.get()?.execute(
            "UPDATE job SET progress = ?2, updated = ?3 WHERE id = ?1",
            params![self.id, progress, crate::now()],
        )?;
        self.running(progress);
        Ok(())
    }

    /// Records the job's structured result (`JobInfo::result`).
    pub fn set_result(&self, result: serde_json::Value) -> Result<()> {
        self.lib.db.get()?.execute(
            "UPDATE job SET result = ?2, updated = ?3 WHERE id = ?1",
            params![self.id, result.to_string(), crate::now()],
        )?;
        Ok(())
    }

    fn running(&self, progress: f32) {
        self.lib.emit(JobEvent {
            id: self.id,
            status: JobStatus::Running,
            progress,
        });
    }

    /// Appends a line to the job's log (redacted like the op log).
    pub fn log(&self, line: &str) -> Result<()> {
        let line = crate::oplog::redact_text(line, &crate::oplog::roots(&self.lib));
        append_log(&self.lib.db, self.id, &line)
    }

    pub fn router(&self) -> Arc<Router> {
        self.lib.router.read().clone()
    }

    /// Appends a finished operation to the library's op log (redacted like every entry).
    pub fn log_op(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        result: &str,
        ok: bool,
    ) -> Result<()> {
        let entry = (kind.to_owned(), payload.clone(), result.to_owned(), ok);
        crate::oplog::record_done(&self.lib, &[entry])
    }
}

fn append_log(db: &Pool, id: JobId, line: &str) -> Result<()> {
    db.get()?.execute(
        "UPDATE job SET log = log || ?2 || char(10), updated = ?3 WHERE id = ?1",
        params![id, line, crate::now()],
    )?;
    Ok(())
}

struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// The job runner's state. It lives in the library's shared state, so a job can start
/// another (a completed walk schedules hashing).
#[derive(Default)]
pub(crate) struct JobState {
    kinds: Mutex<HashMap<String, Restore>>,
    running: Mutex<HashMap<JobId, Running>>,
    pub(crate) closing: AtomicBool,
}

/// The library's job runner: one thread per running job.
pub struct Jobs {
    lib: Arc<Shared>,
}

impl Jobs {
    pub(crate) fn new(lib: Arc<Shared>) -> Jobs {
        Jobs { lib }
    }

    /// Progress and end events of every job from now on (bounded: a subscriber that does
    /// not keep up misses events; `info` has the truth).
    pub fn subscribe(&self) -> crossbeam_channel::Receiver<JobEvent> {
        let (tx, rx) = crossbeam_channel::bounded(EVENT_BACKLOG);
        self.lib.job_events.lock().push(tx);
        rx
    }

    /// Makes `kind` restorable by `resume_all`.
    pub fn register(&self, kind: &str, restore: Restore) {
        self.lib.jobs.kinds.lock().insert(kind.to_owned(), restore);
    }

    /// Resumes the queued and running jobs (left by an earlier session) of every registered
    /// kind; returns their ids. Call it once the library is set up (router set, app kinds
    /// registered): a job resumed earlier would run without them.
    pub fn resume_all(&self) -> Result<Vec<JobId>> {
        let pending: Vec<JobId> = {
            let conn = self.lib.db.get()?;
            let mut stmt = conn
                .prepare("SELECT id FROM job WHERE status IN ('queued', 'running') ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut resumed = Vec::new();
        for id in pending {
            if resume(&self.lib, id)? {
                resumed.push(id);
            }
        }
        Ok(resumed)
    }

    /// Persists `job` and starts it.
    pub fn spawn(&self, job: Box<dyn Job>) -> Result<JobId> {
        spawn(&self.lib, job)
    }

    /// Asks a running job to stop (it ends as Cancelled); a queued one is cancelled at once.
    pub fn cancel(&self, id: JobId) -> Result<()> {
        if let Some(r) = self.lib.jobs.running.lock().get(&id) {
            r.stop.store(true, Ordering::SeqCst);
            return Ok(());
        }
        let info = self.info(id)?;
        if matches!(info.status, JobStatus::Queued | JobStatus::Running) {
            finish(&self.lib.db, id, JobStatus::Cancelled, None, None)?;
        }
        Ok(())
    }

    /// Waits for a job started in this session to end; returns its final record.
    pub fn wait(&self, id: JobId) -> Result<JobInfo> {
        let running = self.lib.jobs.running.lock().remove(&id);
        if let Some(r) = running {
            let _ = r.thread.join();
        }
        self.info(id)
    }

    pub fn info(&self, id: JobId) -> Result<JobInfo> {
        self.lib
            .db
            .get()?
            .query_row(
                &format!("SELECT {INFO_COLUMNS} FROM job WHERE id = ?1"),
                [id],
                row_info,
            )
            .with_context(|| format!("no job {id}"))
    }

    /// Every job, newest first.
    pub fn list(&self) -> Result<Vec<JobInfo>> {
        let conn = self.lib.db.get()?;
        let mut stmt = conn.prepare(&format!("SELECT {INFO_COLUMNS} FROM job ORDER BY id DESC"))?;
        let rows = stmt.query_map([], row_info)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn running(&self) -> usize {
        self.lib
            .jobs
            .running
            .lock()
            .values()
            .filter(|r| !r.thread.is_finished())
            .count()
    }
}

/// Whether job `id` runs in this session.
pub(crate) fn is_running(lib: &Shared, id: JobId) -> bool {
    lib.jobs
        .running
        .lock()
        .get(&id)
        .is_some_and(|r| !r.thread.is_finished())
}

/// `state` with the keys of `cursor` (an object) laid over it.
fn merged(state: &str, cursor: Option<&str>) -> Result<serde_json::Value> {
    let mut state: serde_json::Value = serde_json::from_str(state)?;
    if let (Some(cursor), Some(obj)) = (cursor, state.as_object_mut()) {
        if let serde_json::Value::Object(c) = serde_json::from_str(cursor)? {
            obj.extend(c);
        }
    }
    Ok(state)
}

/// Restores and starts job `id` (queued or running, left by an earlier session) when its
/// kind is registered and it is not running yet; false otherwise.
pub(crate) fn resume(lib: &Arc<Shared>, id: JobId) -> Result<bool> {
    if is_running(lib, id) {
        return Ok(false);
    }
    let row: Option<(String, String, Option<String>)> = lib
        .db
        .get()?
        .query_row(
            "SELECT kind, state, cursor FROM job WHERE id = ?1 AND status IN ('queued', 'running')",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((kind, state, cursor)) = row else {
        return Ok(false);
    };
    let Some(restore) = lib.jobs.kinds.lock().get(&kind).copied() else {
        return Ok(false);
    };
    match merged(&state, cursor.as_deref()).and_then(restore) {
        Ok(job) => {
            append_log(&lib.db, id, "resumed")?;
            start(lib, id, job)?;
            Ok(true)
        }
        Err(e) => {
            let error = format!("restore: {e:#}");
            finish(&lib.db, id, JobStatus::Failed, None, Some(&error))?;
            Ok(false)
        }
    }
}

/// Persists `job` and starts it.
pub(crate) fn spawn(lib: &Arc<Shared>, job: Box<dyn Job>) -> Result<JobId> {
    let now = crate::now();
    let id = {
        let conn = lib.db.get()?;
        conn.execute(
            "INSERT INTO job(kind, state, status, created, updated) VALUES (?1, ?2, 'running', ?3, ?3)",
            params![job.kind(), job.checkpoint().to_string(), now],
        )?;
        conn.last_insert_rowid()
    };
    start(lib, id, job)?;
    Ok(id)
}

fn start(lib: &Arc<Shared>, id: JobId, mut job: Box<dyn Job>) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(lib.jobs.closing.load(Ordering::SeqCst)));
    let ctx = JobCtx {
        id,
        stop: stop.clone(),
        lib: lib.clone(),
    };
    lib.db.get()?.execute(
        "UPDATE job SET status = 'running', updated = ?2 WHERE id = ?1",
        params![id, crate::now()],
    )?;
    let thread = std::thread::Builder::new()
        .name(format!("keel-job-{id}"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run(&ctx)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("job panicked")));
            let (status, error) = match result {
                Ok(()) => (JobStatus::Done, None),
                // Closing the library: stays running, resumes on the next open.
                Err(_) if ctx.closing() => (JobStatus::Running, None),
                Err(e) if e.is::<Cancelled>() || ctx.stopping() => (JobStatus::Cancelled, None),
                Err(e) => {
                    let roots = crate::oplog::roots(&ctx.lib);
                    let error = crate::oplog::redact_text(&format!("{e:#}"), &roots);
                    (JobStatus::Failed, Some(error))
                }
            };
            // Only a job that resumes needs its state again.
            let state = (status == JobStatus::Running).then(|| job.checkpoint());
            if let Err(e) = finish(&ctx.lib.db, id, status, state, error.as_deref()) {
                tracing::warn!("job {id}: recording its end failed: {e:#}");
            }
            let progress = if status == JobStatus::Done { 1.0 } else { 0.0 };
            let progress = ctx
                .lib
                .db
                .get()
                .and_then(|c| {
                    Ok(
                        c.query_row("SELECT progress FROM job WHERE id = ?1", [id], |r| {
                            r.get::<_, f64>(0)
                        })?,
                    )
                })
                .map_or(progress, |p| p as f32);
            ctx.lib.emit(JobEvent {
                id,
                status,
                progress,
            });
        })?;
    let mut running = lib.jobs.running.lock();
    running.retain(|_, r| !r.thread.is_finished());
    running.insert(id, Running { stop, thread });
    Ok(())
}

const INFO_COLUMNS: &str = "id, kind, status, progress, log, created, updated, result";

fn row_info(r: &rusqlite::Row) -> rusqlite::Result<JobInfo> {
    Ok(JobInfo {
        id: r.get(0)?,
        kind: r.get(1)?,
        status: JobStatus::parse(&r.get::<_, String>(2)?),
        progress: r.get::<_, f64>(3)? as f32,
        log: r.get(4)?,
        created: r.get(5)?,
        updated: r.get(6)?,
        result: r
            .get::<_, Option<String>>(7)?
            .and_then(|s| serde_json::from_str(&s).ok()),
    })
}

fn finish(
    db: &Pool,
    id: JobId,
    status: JobStatus,
    state: Option<serde_json::Value>,
    error: Option<&str>,
) -> Result<()> {
    // An ended job's state (paths, possibly outside the library) is no longer needed.
    let state = match status {
        JobStatus::Queued | JobStatus::Running => state,
        _ => Some(serde_json::Value::Null),
    };
    let conn = db.get()?;
    // A full state supersedes the cursor laid over the old one.
    conn.execute(
        "UPDATE job SET status = ?2, state = coalesce(?3, state),
             cursor = CASE WHEN ?3 IS NULL THEN cursor END,
             progress = CASE WHEN ?2 = 'done' THEN 1 ELSE progress END, updated = ?4
         WHERE id = ?1",
        params![
            id,
            status.as_str(),
            state.map(|s| s.to_string()),
            crate::now()
        ],
    )?;
    drop(conn);
    if let Some(error) = error {
        append_log(db, id, error)?;
    }
    Ok(())
}

/// Stops every job at its next checkpoint (they stay `running` in the database and resume
/// on the next open) and waits up to `timeout`; false when some job is still busy (a step
/// such as a slow listing or transfer cannot be interrupted): it finishes that step on its
/// own, and the library stays locked until it has.
pub(crate) fn shutdown(lib: &Shared, timeout: Duration) -> bool {
    lib.jobs.closing.store(true, Ordering::SeqCst);
    let running: Vec<_> = lib.jobs.running.lock().drain().collect();
    for (_, r) in &running {
        r.stop.store(true, Ordering::SeqCst);
    }
    let deadline = Instant::now() + timeout;
    while running.iter().any(|(_, r)| !r.thread.is_finished()) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Ended jobs kept in `library.db` (older ones are pruned on open).
const KEEP_ENDED: i64 = 500;

/// Deletes all but the newest `KEEP_ENDED` ended jobs.
pub(crate) fn prune(db: &Pool) -> Result<()> {
    db.get()?.execute(
        "DELETE FROM job WHERE status IN ('done', 'failed', 'cancelled') AND id NOT IN (
             SELECT id FROM job WHERE status IN ('done', 'failed', 'cancelled')
             ORDER BY id DESC LIMIT ?1)",
        [KEEP_ENDED],
    )?;
    Ok(())
}

/// How long dropping a library waits for its jobs.
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A full walk of one source as a durable job (a restart walks again; walks are idempotent).
/// A completed walk schedules hashing (`Library::set_hash_after_walk`).
#[derive(Serialize, Deserialize)]
pub(crate) struct IndexJob {
    pub(crate) source: SourceId,
}

impl Job for IndexJob {
    fn kind(&self) -> &'static str {
        "index"
    }
    fn run(&mut self, ctx: &JobCtx) -> Result<()> {
        let src = ctx
            .lib
            .sources
            .read()
            .iter()
            .find(|s| s.id == self.source)
            .cloned()
            .with_context(|| format!("no source {}", self.source))?;
        let progress = |p: crate::IndexProgress| {
            if p.total > 0 {
                let _ = ctx.progress((p.done as f32 / p.total as f32).min(0.99));
            }
        };
        // A watcher's walk (each starts with one: the daemon arms a new source while its
        // index job is queued) holds the source: wait for it to end, then walk.
        loop {
            match Indexer::full_walk(&src, &ctx.router(), &progress, ctx.stop_flag()) {
                Err(e) if e.is::<crate::index::Busy>() && !ctx.stopping() => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                walked => break walked?,
            }
        }
        crate::protect::after_walk(&ctx.lib, &src);
        crate::hash::after_walk(&ctx.lib, &src);
        Ok(())
    }
    fn checkpoint(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
    fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
        Ok(Box::new(serde_json::from_value::<IndexJob>(v)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::folder;
    use crate::Library;
    use std::path::PathBuf;

    #[derive(Serialize, Deserialize)]
    struct Count {
        next: u32,
        total: u32,
        out: PathBuf,
        fail_at: Option<u32>,
    }

    impl Job for Count {
        fn kind(&self) -> &'static str {
            "count"
        }
        fn run(&mut self, ctx: &JobCtx) -> Result<()> {
            use std::io::Write;
            while self.next < self.total {
                anyhow::ensure!(Some(self.next) != self.fail_at, "boom at {}", self.next);
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&self.out)?;
                writeln!(f, "{}", self.next)?;
                self.next += 1;
                ctx.checkpoint(self.checkpoint(), self.next as f32 / self.total as f32)?;
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        }
        fn checkpoint(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap()
        }
        fn restore(v: serde_json::Value) -> Result<Box<dyn Job>> {
            Ok(Box::new(serde_json::from_value::<Count>(v)?))
        }
    }

    fn lines(path: &std::path::Path) -> Vec<u32> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect()
    }

    fn count(out: &std::path::Path, total: u32) -> Box<dyn Job> {
        Box::new(Count {
            next: 0,
            total,
            out: out.to_owned(),
            fail_at: None,
        })
    }

    #[test]
    fn a_killed_job_resumes_from_its_checkpoint() {
        let data = tempfile::tempdir().unwrap();
        let out = data.path().join("out.txt");
        let lib = Library::open(data.path(), "j").unwrap();
        lib.jobs().register("count", Count::restore);
        let id = lib.jobs().spawn(count(&out, 300)).unwrap();
        crate::index::tests::eventually("some progress", || lines(&out).len() >= 30);
        drop(lib); // the "kill": stops at the next checkpoint
        let done_before = lines(&out).len() as u32;
        assert!(done_before < 300, "stopped mid-job");

        let lib = Library::open(data.path(), "j").unwrap();
        assert_eq!(lib.jobs().info(id).unwrap().status, JobStatus::Running);
        lib.jobs().register("count", Count::restore);
        assert_eq!(lib.jobs().resume_all().unwrap(), [id]);
        let info = lib.jobs().wait(id).unwrap();
        assert_eq!(info.status, JobStatus::Done);
        assert_eq!(info.progress, 1.0);
        assert!(info.log.contains("resumed"));
        // Every step exactly once: continued at the checkpoint, not from the start.
        assert_eq!(lines(&out), (0..300).collect::<Vec<_>>());
        assert!(done_before > 0);
    }

    #[test]
    fn cancelled_and_failed_jobs_end_and_stay_ended() {
        let data = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "j").unwrap();
        lib.jobs().register("count", Count::restore);
        let out = data.path().join("c.txt");
        let id = lib.jobs().spawn(count(&out, 100_000)).unwrap();
        crate::index::tests::eventually("started", || !lines(&out).is_empty());
        lib.jobs().cancel(id).unwrap();
        assert_eq!(lib.jobs().wait(id).unwrap().status, JobStatus::Cancelled);

        let failing = lib
            .jobs()
            .spawn(Box::new(Count {
                next: 0,
                total: 10,
                out: data.path().join("f.txt"),
                fail_at: Some(3),
            }))
            .unwrap();
        let info = lib.jobs().wait(failing).unwrap();
        assert_eq!(info.status, JobStatus::Failed);
        assert!(info.log.contains("boom at 3"), "{}", info.log);
        drop(lib);

        let lib = Library::open(data.path(), "j").unwrap();
        lib.jobs().register("count", Count::restore);
        assert!(lib.jobs().resume_all().unwrap().is_empty());
        assert_eq!(lib.jobs().list().unwrap().len(), 2);
    }

    #[test]
    fn pending_jobs_wait_for_resume_all() {
        let data = tempfile::tempdir().unwrap();
        let router = Arc::new(Router::new());
        router.register(Arc::new(crate::index::tests::fake(|path: &str| {
            Ok(match path {
                "/" => vec![("x.txt".into(), false, 1)],
                _ => anyhow::bail!("no such folder {path}"),
            })
        })));
        let def = crate::SourceDef {
            label: "nas".into(),
            root: keel_vfs::VPath::parse("fake://nas/").unwrap(),
            kind: crate::SourceKind::Share,
            include_hidden: false,
            ignore: Vec::new(),
            poll_secs: None,
            hash_shares: false,
        };
        let lib = Library::open(data.path(), "j").unwrap();
        let source = lib.add_source(def).unwrap();
        // An index job left running by an earlier session.
        let state = serde_json::to_string(&IndexJob {
            source: source.clone(),
        })
        .unwrap();
        lib.shared
            .db
            .get()
            .unwrap()
            .execute(
                "INSERT INTO job(kind, state, status, created, updated)
                 VALUES ('index', ?1, 'running', 0, 0)",
                [state],
            )
            .unwrap();
        drop(lib);

        let lib = Library::open(data.path(), "j").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let id = lib.jobs().list().unwrap()[0].id;
        assert_eq!(lib.jobs().running(), 0, "nothing runs before resume_all");
        assert!(lib.jobs().info(id).unwrap().log.is_empty());
        // With the router the remote source needs, it resumes and succeeds.
        lib.set_router(router);
        assert_eq!(lib.jobs().resume_all().unwrap(), [id]);
        let info = lib.jobs().wait(id).unwrap();
        assert_eq!(info.status, JobStatus::Done, "{}", info.log);
        assert_eq!(lib.stats().records, 2);
    }

    /// Sleeps in one step without checkpoints (an uninterruptible listing).
    #[derive(Serialize, Deserialize)]
    struct Stuck;
    impl Job for Stuck {
        fn kind(&self) -> &'static str {
            "stuck"
        }
        fn run(&mut self, _: &JobCtx) -> Result<()> {
            std::thread::sleep(Duration::from_secs(2));
            Ok(())
        }
        fn checkpoint(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn restore(_: serde_json::Value) -> Result<Box<dyn Job>> {
            Ok(Box::new(Stuck))
        }
    }

    #[test]
    fn subscribers_hear_progress_and_the_end() {
        let data = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "j").unwrap();
        lib.jobs().register("count", Count::restore);
        let events = lib.jobs().subscribe();
        let id = lib
            .jobs()
            .spawn(count(&data.path().join("e.txt"), 5))
            .unwrap();
        lib.jobs().wait(id).unwrap();
        let got: Vec<JobEvent> = events.try_iter().collect();
        assert_eq!(got.len(), 6, "{got:?}");
        assert!(got.iter().all(|e| e.id == id));
        assert_eq!(got[4].progress, 1.0);
        assert_eq!(
            got[5],
            JobEvent {
                id,
                status: JobStatus::Done,
                progress: 1.0
            }
        );
        drop(events);
        let id = lib
            .jobs()
            .spawn(count(&data.path().join("f.txt"), 1))
            .unwrap();
        lib.jobs().wait(id).unwrap();
        assert!(
            lib.shared.job_events.lock().is_empty(),
            "a dropped subscriber goes"
        );
    }

    #[test]
    fn close_waits_for_jobs_up_to_a_timeout() {
        let data = tempfile::tempdir().unwrap();
        // Closed through one handle while another exists (the app's UI holds Arcs).
        let lib = Arc::new(Library::open(data.path(), "j").unwrap());
        let other = lib.clone();
        lib.jobs()
            .spawn(count(&data.path().join("c.txt"), 100_000))
            .unwrap();
        assert!(
            lib.close(Duration::from_secs(10)),
            "a checkpointing job stops"
        );
        let start = Instant::now();
        drop((lib, other));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a cheap last drop"
        );

        let lib = Library::open(data.path(), "j").unwrap();
        lib.jobs().spawn(Box::new(Stuck)).unwrap();
        let start = Instant::now();
        assert!(!lib.close(Duration::from_millis(100)));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(lib);
        // The detached job still runs: the library stays locked (no second session could
        // resume its jobs while it runs), until it ends.
        let err = Library::open(data.path(), "j").err().unwrap();
        assert!(err.to_string().contains("already open"), "{err}");
        crate::index::tests::eventually("the stuck job ends", || {
            Library::open(data.path(), "j").is_ok()
        });
    }

    #[test]
    fn ended_jobs_are_pruned_on_open() {
        let data = tempfile::tempdir().unwrap();
        let lib = Library::open(data.path(), "j").unwrap();
        {
            let c = lib.shared.db.get().unwrap();
            c.execute(
                "INSERT INTO job(kind, state, status, created, updated)
                 VALUES ('x', 'null', 'running', 0, 0)",
                [],
            )
            .unwrap();
            for _ in 0..KEEP_ENDED + 10 {
                c.execute(
                    "INSERT INTO job(kind, state, status, created, updated)
                     VALUES ('x', 'null', 'done', 0, 0)",
                    [],
                )
                .unwrap();
            }
        }
        drop(lib);
        let lib = Library::open(data.path(), "j").unwrap();
        let jobs = lib.jobs().list().unwrap();
        assert_eq!(jobs.len() as i64, KEEP_ENDED + 1);
        assert!(
            jobs.iter().any(|j| j.status == JobStatus::Running),
            "pending kept"
        );
        assert_eq!(jobs[0].id, KEEP_ENDED + 11, "the newest kept");
    }

    #[test]
    fn index_job_walks_a_source() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        std::fs::write(files.path().join("a.txt"), "a").unwrap();
        let lib = Library::open(data.path(), "j").unwrap();
        let id = lib.add_source(folder("f", files.path())).unwrap();
        let job = lib.index(&id).unwrap();
        assert_eq!(lib.jobs().wait(job).unwrap().status, JobStatus::Done);
        assert_eq!(
            lib.source(&id).unwrap().generation.load(Ordering::SeqCst),
            1
        );
        assert_eq!(lib.stats().records, 2);
    }

    /// The daemon arms a watcher (which starts with a walk) on a new source while its
    /// index job is queued: the job waits for that walk instead of failing.
    #[test]
    fn an_index_job_waits_for_a_walk_already_running() {
        let data = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        std::fs::write(files.path().join("a.txt"), b"a").unwrap();
        let lib = Library::open(data.path(), "j").unwrap();
        let id = lib.add_source(folder("F", files.path())).unwrap();
        let src = lib.source(&id).unwrap();
        // Another walk holds the source.
        src.pending_gen.store(u64::MAX, Ordering::SeqCst);
        let job = lib.index(&id).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let info = lib.jobs().info(job).unwrap();
        assert!(
            matches!(info.status, JobStatus::Queued | JobStatus::Running),
            "{:?}: {}",
            info.status,
            info.log
        );
        src.pending_gen.store(0, Ordering::SeqCst);
        let info = lib.jobs().wait(job).unwrap();
        assert_eq!(info.status, JobStatus::Done, "{}", info.log);
        assert_eq!(lib.stats().records, 2);
        let names: Vec<String> = (lib.list_children(&id, "").unwrap())
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(names, ["a.txt"], "listed as soon as the job is done");
    }
}
