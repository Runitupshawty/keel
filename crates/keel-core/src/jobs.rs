//! Durable jobs: each job's checkpoint lives in `library.db` (`job` table) and jobs that were
//! queued or running when the library closed resume on the next open (built-in kinds) or
//! when their kind is registered.

use crate::db::Pool;
use crate::library::Shared;
use crate::{Cancelled, Indexer, SourceId};
use anyhow::{Context, Result};
use keel_vfs::Router;
use parking_lot::Mutex;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

pub type JobId = i64;

/// Turns a checkpoint back into a job (`Job::restore`).
pub type Restore = fn(serde_json::Value) -> Result<Box<dyn Job>>;

pub trait Job: Send {
    fn kind(&self) -> &'static str;
    /// Does the work, calling `ctx.checkpoint` after each durable step; returns
    /// `Err(Cancelled)` (what `checkpoint` returns once stopped) when asked to stop.
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
}

/// What a running job gets: its id, stop signal, checkpointing and the library.
pub struct JobCtx {
    pub id: JobId,
    stop: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    db: Pool,
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
        self.closing.load(Ordering::SeqCst)
    }

    /// Persists `state` (and progress 0..=1). The step it describes is durable: a restart
    /// continues from here. Returns `Err(Cancelled)` once the job should stop.
    pub fn checkpoint(&self, state: serde_json::Value, progress: f32) -> Result<()> {
        self.db.get()?.execute(
            "UPDATE job SET state = ?2, progress = ?3, updated = ?4 WHERE id = ?1",
            params![self.id, state.to_string(), progress, crate::now()],
        )?;
        if self.stopping() {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    pub fn progress(&self, progress: f32) -> Result<()> {
        self.db.get()?.execute(
            "UPDATE job SET progress = ?2, updated = ?3 WHERE id = ?1",
            params![self.id, progress, crate::now()],
        )?;
        Ok(())
    }

    /// Appends a line to the job's log.
    pub fn log(&self, line: &str) -> Result<()> {
        append_log(&self.db, self.id, line)
    }

    pub fn router(&self) -> Arc<Router> {
        self.lib.router.read().clone()
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

/// The library's job runner: one thread per running job.
pub struct Jobs {
    lib: Arc<Shared>,
    kinds: Mutex<HashMap<String, Restore>>,
    running: Mutex<HashMap<JobId, Running>>,
    closing: Arc<AtomicBool>,
}

impl Jobs {
    pub(crate) fn new(lib: Arc<Shared>) -> Jobs {
        Jobs {
            lib,
            kinds: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            closing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Makes `kind` restorable by `resume_all`.
    pub fn register(&self, kind: &str, restore: Restore) {
        self.kinds.lock().insert(kind.to_owned(), restore);
    }

    /// Resumes the queued and running jobs (left by an earlier session) of every registered
    /// kind; returns their ids. Call it once the library is set up (router set, app kinds
    /// registered): a job resumed earlier would run without them.
    pub fn resume_all(&self) -> Result<Vec<JobId>> {
        let pending: Vec<(JobId, String, String)> = {
            let conn = self.lib.db.get()?;
            let mut stmt = conn.prepare(
                "SELECT id, kind, state FROM job WHERE status IN ('queued', 'running') ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut resumed = Vec::new();
        for (id, kind, state) in pending {
            let Some(restore) = self.kinds.lock().get(&kind).copied() else {
                continue;
            };
            if self.running.lock().contains_key(&id) {
                continue;
            }
            match serde_json::from_str(&state)
                .map_err(anyhow::Error::from)
                .and_then(restore)
            {
                Ok(job) => {
                    append_log(&self.lib.db, id, "resumed")?;
                    self.start(id, job)?;
                    resumed.push(id);
                }
                Err(e) => self.finish(
                    id,
                    JobStatus::Failed,
                    None,
                    Some(&format!("restore: {e:#}")),
                )?,
            }
        }
        Ok(resumed)
    }

    /// Persists `job` and starts it.
    pub fn spawn(&self, job: Box<dyn Job>) -> Result<JobId> {
        let now = crate::now();
        let id = {
            let conn = self.lib.db.get()?;
            conn.execute(
                "INSERT INTO job(kind, state, status, created, updated) VALUES (?1, ?2, 'running', ?3, ?3)",
                params![job.kind(), job.checkpoint().to_string(), now],
            )?;
            conn.last_insert_rowid()
        };
        self.start(id, job)?;
        Ok(id)
    }

    fn start(&self, id: JobId, mut job: Box<dyn Job>) -> Result<()> {
        let stop = Arc::new(AtomicBool::new(self.closing.load(Ordering::SeqCst)));
        let ctx = JobCtx {
            id,
            stop: stop.clone(),
            closing: self.closing.clone(),
            db: self.lib.db.clone(),
            lib: self.lib.clone(),
        };
        let closing = self.closing.clone();
        self.lib.db.get()?.execute(
            "UPDATE job SET status = 'running', updated = ?2 WHERE id = ?1",
            params![id, crate::now()],
        )?;
        let thread = std::thread::Builder::new()
            .name(format!("keel-job-{id}"))
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run(&ctx)))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("job panicked")));
                let state = job.checkpoint();
                let (status, error) = match result {
                    Ok(()) => (JobStatus::Done, None),
                    // Closing the library: stays running, resumes on the next open.
                    Err(_) if closing.load(Ordering::SeqCst) => (JobStatus::Running, None),
                    Err(e) if e.is::<Cancelled>() || ctx.stopping() => (JobStatus::Cancelled, None),
                    Err(e) => (JobStatus::Failed, Some(format!("{e:#}"))),
                };
                if let Err(e) = finish(&ctx.db, id, status, Some(state), error.as_deref()) {
                    tracing::warn!("job {id}: recording its end failed: {e:#}");
                }
            })?;
        let mut running = self.running.lock();
        running.retain(|_, r| !r.thread.is_finished());
        running.insert(id, Running { stop, thread });
        Ok(())
    }

    fn finish(
        &self,
        id: JobId,
        status: JobStatus,
        state: Option<serde_json::Value>,
        error: Option<&str>,
    ) -> Result<()> {
        finish(&self.lib.db, id, status, state, error)
    }

    /// Asks a running job to stop (it ends as Cancelled); a queued one is cancelled at once.
    pub fn cancel(&self, id: JobId) -> Result<()> {
        if let Some(r) = self.running.lock().get(&id) {
            r.stop.store(true, Ordering::SeqCst);
            return Ok(());
        }
        let info = self.info(id)?;
        if matches!(info.status, JobStatus::Queued | JobStatus::Running) {
            self.finish(id, JobStatus::Cancelled, None, None)?;
        }
        Ok(())
    }

    /// Waits for a job started in this session to end; returns its final record.
    pub fn wait(&self, id: JobId) -> Result<JobInfo> {
        let running = self.running.lock().remove(&id);
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
                "SELECT id, kind, status, progress, log, created, updated FROM job WHERE id = ?1",
                [id],
                row_info,
            )
            .with_context(|| format!("no job {id}"))
    }

    /// Every job, newest first.
    pub fn list(&self) -> Result<Vec<JobInfo>> {
        let conn = self.lib.db.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, status, progress, log, created, updated FROM job ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], row_info)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn running(&self) -> usize {
        self.running
            .lock()
            .values()
            .filter(|r| !r.thread.is_finished())
            .count()
    }
}

fn row_info(r: &rusqlite::Row) -> rusqlite::Result<JobInfo> {
    Ok(JobInfo {
        id: r.get(0)?,
        kind: r.get(1)?,
        status: JobStatus::parse(&r.get::<_, String>(2)?),
        progress: r.get::<_, f64>(3)? as f32,
        log: r.get(4)?,
        created: r.get(5)?,
        updated: r.get(6)?,
    })
}

fn finish(
    db: &Pool,
    id: JobId,
    status: JobStatus,
    state: Option<serde_json::Value>,
    error: Option<&str>,
) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE job SET status = ?2, state = coalesce(?3, state),
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

impl Drop for Jobs {
    /// Stops every job at its next checkpoint and waits; they stay `running` in the
    /// database and resume on the next open.
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        let running: Vec<_> = self.running.lock().drain().collect();
        for (_, r) in &running {
            r.stop.store(true, Ordering::SeqCst);
        }
        for (_, r) in running {
            let _ = r.thread.join();
        }
    }
}

/// A full walk of one source as a durable job (a restart walks again; walks are idempotent).
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
        Indexer::full_walk(
            &src,
            &ctx.router(),
            &|p| {
                if p.total > 0 {
                    let _ = ctx.progress((p.done as f32 / p.total as f32).min(0.99));
                }
            },
            ctx.stop_flag(),
        )
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
    use std::{path::PathBuf, time::Duration};

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
}
