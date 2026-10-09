//! File operation jobs: copy / move / trash on worker threads with progress and cancel,
//! shown in a panel at the bottom of the window.

use crate::clipboard::Clipboard;
use crate::state::Msg;
use crate::worker::{send, spawn};
use crossbeam_channel::Sender;
use keel_vfs::{Conflict, Progress, Router, VPath};
use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Progress messages are sent at most this often per job.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(50);
/// Finished jobs (not failed ones) leave the panel after this long.
pub const CLEAR_AFTER: Duration = Duration::from_secs(5);

pub struct Job {
    pub id: u64,
    pub title: String,
    pub progress: Progress,
    pub cancel: Arc<AtomicBool>,
    pub done: Option<anyhow::Result<()>>,
    finished: Option<Instant>,
    remote: bool,
}

/// The error a job returns when its Cancel flag stopped it.
pub fn is_cancel(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.to_string() == "operation cancelled")
}

impl Job {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Finished and nothing to report: Ok, or stopped by Cancel. Real errors stay.
    fn clears(&self) -> bool {
        match &self.done {
            Some(Ok(())) => true,
            Some(Err(e)) => is_cancel(e),
            None => false,
        }
    }

    fn fraction(&self) -> f32 {
        let p = &self.progress;
        match (&self.done, p.total_bytes, p.total_items) {
            (Some(Ok(())), ..) => 1.0,
            (_, b, _) if b > 0 => p.done_bytes as f32 / b as f32,
            (_, _, n) if n > 0 => p.done_items as f32 / n as f32,
            _ => 0.0,
        }
    }
}

/// A copy or move waiting for its conflict policy (local or remote on either side).
#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub src: Vec<VPath>,
    pub dst: VPath,
    pub mv: bool,
}

/// Where a transfer's sources come from.
pub enum Source {
    /// Dropped or dragged paths; `true` = move.
    Paths(Vec<VPath>, bool),
    /// Ctrl+V: resolved against the system clipboard on the planning thread.
    Clipboard(Clipboard),
}

pub struct Jobs {
    pub list: Vec<Job>,
    next_id: u64,
    ctx: egui::Context,
}

impl Jobs {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            list: Vec::new(),
            next_id: 1,
            ctx,
        }
    }

    /// Copy or move through `ops::transfer`: local to local takes the OS fast path, anything
    /// involving a remote host streams with staged writes.
    pub fn start(
        &mut self,
        t: Transfer,
        conflict: Conflict,
        router: Arc<Router>,
        tx: Sender<Msg>,
    ) -> u64 {
        let verb = if t.mv { "Moving" } else { "Copying" };
        let title = format!("{verb} {} to {}", items(t.src.len()), t.dst.display());
        let remote = t.src.iter().chain([&t.dst]).any(|p| p.scheme == "sftp");
        let id = self.spawn(title, tx, move |report, cancel| {
            keel_vfs::ops::transfer(&t.src, &t.dst, t.mv, conflict, report, cancel, &router)
        });
        self.mark_remote(id, remote);
        id
    }

    fn mark_remote(&mut self, id: u64, remote: bool) {
        if let Some(job) = self.list.iter_mut().find(|j| j.id == id) {
            job.remote = remote;
        }
    }

    /// A job that touched a remote host (its failure is also toasted).
    pub fn is_remote(&self, id: u64) -> bool {
        self.list.iter().any(|j| j.id == id && j.remote)
    }

    /// Sends each path to the OS trash (remote: deletes it; the user confirmed that); stops
    /// at the first failure.
    pub fn delete(&mut self, paths: Vec<VPath>, router: Arc<Router>, tx: Sender<Msg>) -> u64 {
        let remote = paths.first().is_some_and(|p| p.scheme == "sftp");
        let (title, did) = if remote {
            (format!("Deleting {}", items(paths.len())), "deleted")
        } else {
            (
                format!("Moving {} to the trash", items(paths.len())),
                "moved to trash",
            )
        };
        let id = self.spawn(title, tx, move |report, cancel| {
            let mut p = Progress {
                done_bytes: 0,
                total_bytes: 0,
                current: String::new(),
                done_items: 0,
                total_items: paths.len(),
            };
            for path in &paths {
                if cancel.load(Ordering::Relaxed) {
                    anyhow::ensure!(p.done_items > 0, "operation cancelled");
                    anyhow::bail!("Cancelled: {} of {} {did}", p.done_items, paths.len());
                }
                p.current = path.display();
                report(p.clone());
                let removed = router
                    .provider_for(path)
                    .ok_or_else(|| anyhow::anyhow!("no provider for {}", path.display()))
                    .and_then(|provider| provider.remove(path));
                if let Err(e) = removed {
                    if p.done_items == 0 {
                        return Err(e);
                    }
                    // "nothing deleted" is about this item only; earlier ones are gone.
                    let why = format!("{e:#}").replace("; nothing deleted", "");
                    anyhow::bail!(
                        "{} of {} {did}; stopped at {}: {why}",
                        p.done_items,
                        paths.len(),
                        path.display()
                    );
                }
                p.done_items += 1;
            }
            report(p);
            Ok(())
        });
        self.mark_remote(id, remote);
        id
    }

    fn spawn(
        &mut self,
        title: String,
        tx: Sender<Msg>,
        work: impl FnOnce(&dyn Fn(Progress), &AtomicBool) -> anyhow::Result<()> + Send + 'static,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.list.push(Job {
            id,
            title,
            progress: Progress {
                done_bytes: 0,
                total_bytes: 0,
                current: String::new(),
                done_items: 0,
                total_items: 0,
            },
            cancel: cancel.clone(),
            done: None,
            finished: None,
            remote: false,
        });
        let ctx = self.ctx.clone();
        let started = spawn("keel-job", move || {
            let last = Cell::new(None::<Instant>);
            let report = |p: Progress| {
                if last.get().is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                    last.set(Some(Instant::now()));
                    send(&tx, &ctx, Msg::JobProgress { id, p });
                }
            };
            // A panicking job still answers, so its Cancel button never hangs around.
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(&report, &cancel)))
                .unwrap_or_else(|panic| {
                    let why = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    Err(anyhow::anyhow!("internal error: {why}"))
                });
            send(&tx, &ctx, Msg::JobDone { id, result });
        });
        if !started {
            self.finish(id, Err(anyhow::anyhow!("could not start a worker thread")));
        }
        id
    }

    pub fn progress(&mut self, id: u64, p: Progress) {
        if let Some(job) = self.list.iter_mut().find(|j| j.id == id) {
            job.progress = p;
        }
    }

    pub fn finish(&mut self, id: u64, result: anyhow::Result<()>) {
        if let Some(job) = self.list.iter_mut().find(|j| j.id == id) {
            if let Err(e) = &result {
                tracing::warn!("{}: {e:#}", job.title);
            }
            job.done = Some(result);
            job.finished = Some(Instant::now());
        }
    }

    /// Drops finished jobs after `CLEAR_AFTER` unless they failed; failures stay until
    /// dismissed. Cancelled jobs count as finished.
    pub fn tick(&mut self) {
        let now = Instant::now();
        self.list.retain(|j| match j.finished {
            Some(at) if j.clears() => now < at + CLEAR_AFTER,
            _ => true,
        });
        // Only jobs that will clear need a wake-up; failures stay until dismissed.
        if let Some(next) = self
            .list
            .iter()
            .filter(|j| j.clears())
            .filter_map(|j| j.finished)
            .min()
        {
            self.ctx
                .request_repaint_after((next + CLEAR_AFTER).saturating_duration_since(now));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let mut dismiss = None;
        egui::ScrollArea::vertical()
            .max_height(150.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for job in &self.list {
                    job_row(ui, job, &mut dismiss);
                }
            });
        if let Some(id) = dismiss {
            self.list.retain(|j| j.id != id);
        }
    }
}

fn job_row(ui: &mut egui::Ui, job: &Job, dismiss: &mut Option<u64>) {
    {
        {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &job.done {
                        None => {
                            if ui.button("Cancel").clicked() {
                                job.cancel.store(true, Ordering::Relaxed);
                            }
                        }
                        Some(_) if job.clears() => {}
                        Some(_) => {
                            if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                                *dismiss = Some(job.id);
                            }
                        }
                    }
                    ui.add(
                        egui::ProgressBar::new(job.fraction())
                            .desired_width(180.0)
                            .show_percentage(),
                    );
                    let (text, error) = match &job.done {
                        None if job.cancelled() => ("Cancelling…".to_owned(), false),
                        None => (job.progress.current.clone(), false),
                        Some(Ok(())) if job.cancelled() => {
                            ("Done (cancel too late)".to_owned(), false)
                        }
                        Some(Ok(())) => ("Done".to_owned(), false),
                        Some(Err(e)) if is_cancel(e) => ("Cancelled".to_owned(), false),
                        Some(Err(e)) => (format!("{e:#}"), true),
                    };
                    let mut label = egui::RichText::new(text);
                    if error {
                        label = label.color(ui.visuals().error_fg_color);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(&job.title).strong()))
                            .on_hover_text(&job.title);
                        ui.add(egui::Label::new(label).truncate());
                    });
                });
            });
        }
    }
}

pub fn items(n: usize) -> String {
    match n {
        1 => "1 item".to_owned(),
        n => format!("{n} items"),
    }
}

/// Top-level names in `src` that already exist in `dst` (a stat on the remote host when
/// `dst` is remote). Blocks: workers only.
pub fn plan_conflicts(src: &[VPath], dst: &VPath, router: &Router) -> Vec<String> {
    let exists = |name: &str| match dst.to_local_path() {
        Some(dir) => std::fs::symlink_metadata(dir.join(name)).is_ok(),
        None => router
            .provider_for(dst)
            .is_some_and(|p| p.stat(&dst.join(name)).is_ok()),
    };
    src.iter()
        .map(VPath::name)
        .filter(|name| exists(name))
        .map(str::to_owned)
        .collect()
}

/// Same folder: local paths compare as paths (separators, trailing slash), others as VPaths.
fn same_dir(a: &VPath, b: &VPath) -> bool {
    match (a.to_local_path(), b.to_local_path()) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// Resolves the sources (reading the system clipboard for a paste) and scans `dst` for
/// name clashes off the UI thread, then answers with `Msg::Planned` (or `PlanFailed`, also
/// when planning panics). False when the thread could not start.
pub fn spawn_plan(
    source: Source,
    dst: VPath,
    router: Arc<Router>,
    tx: Sender<Msg>,
    ctx: egui::Context,
) -> bool {
    spawn("keel-plan", move || {
        let from_clipboard = matches!(source, Source::Clipboard(_));
        let planned =
            std::panic::catch_unwind(AssertUnwindSafe(|| plan(source, dst, &router, &tx, &ctx)));
        if planned.is_err() {
            send(
                &tx,
                &ctx,
                Msg::PlanFailed {
                    text: "Planning the transfer failed (see crash.log)".into(),
                    from_clipboard,
                },
            );
        }
    })
}

fn plan(source: Source, dst: VPath, router: &Router, tx: &Sender<Msg>, ctx: &egui::Context) {
    let (src, mv, from_clipboard) = match source {
        Source::Paths(src, mv) => (src, mv, false),
        Source::Clipboard(clip) => match clip.resolve() {
            Some((src, cut)) => (src, cut, true),
            None => {
                send(
                    tx,
                    ctx,
                    Msg::PlanFailed {
                        text: "The clipboard holds no files".into(),
                        from_clipboard: true,
                    },
                );
                return;
            }
        },
    };
    let src: Vec<VPath> = src
        .into_iter()
        .filter(|p| !p.parent().is_some_and(|parent| same_dir(&parent, &dst)))
        .collect();
    if src.is_empty() {
        let text = if mv {
            "Already in this folder"
        } else {
            "Copying into the same folder is not supported yet"
        };
        send(
            tx,
            ctx,
            Msg::PlanFailed {
                text: text.into(),
                from_clipboard,
            },
        );
        return;
    }
    let conflicts = plan_conflicts(&src, &dst, router);
    send(
        tx,
        ctx,
        Msg::Planned {
            op: Transfer { src, dst, mv },
            conflicts,
            from_clipboard,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::create_dir_all(root.join("dst")).unwrap();
        for f in ["src/a.txt", "src/b.txt", "src/sub/c.txt"] {
            std::fs::write(root.join(f), f).unwrap();
        }
        root
    }

    #[test]
    fn copy_job_lands_files_and_reports_done() {
        let root = tree("keel-job-copy");
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut jobs = Jobs::new(egui::Context::default());
        let op = Transfer {
            src: vec![VPath::local(root.join("src"))],
            dst: VPath::local(root.join("dst")),
            mv: false,
        };
        let id = jobs.start(op, Conflict::Skip, Arc::new(Router::new()), tx);
        loop {
            match rx
                .recv_timeout(Duration::from_secs(10))
                .expect("job answers")
            {
                Msg::JobProgress { id: got, p } => {
                    assert_eq!(got, id);
                    jobs.progress(id, p);
                }
                Msg::JobDone { id: got, result } => {
                    assert_eq!(got, id);
                    result.expect("copy ok");
                    break;
                }
                _ => panic!("unexpected message"),
            }
        }
        for f in ["a.txt", "b.txt", "sub/c.txt"] {
            let copied = root.join("dst/src").join(f);
            assert_eq!(std::fs::read_to_string(copied).unwrap(), format!("src/{f}"));
        }
        assert!(root.join("src/a.txt").exists(), "copy keeps the source");
    }

    #[test]
    fn plan_conflicts_finds_existing_names() {
        let root = tree("keel-job-plan");
        std::fs::write(root.join("dst/a.txt"), "old").unwrap();
        let src = vec![
            VPath::local(root.join("src/a.txt")),
            VPath::local(root.join("src/b.txt")),
        ];
        let dst = VPath::local(root.join("dst"));
        assert_eq!(plan_conflicts(&src, &dst, &Router::new()), ["a.txt"]);
    }

    #[test]
    fn finished_jobs_clear_but_failures_stay() {
        let mut jobs = Jobs::new(egui::Context::default());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let ok = jobs.delete(Vec::new(), Arc::new(Router::new()), tx.clone());
        let bad = jobs.delete(Vec::new(), Arc::new(Router::new()), tx.clone());
        let stopped = jobs.delete(Vec::new(), Arc::new(Router::new()), tx);
        jobs.finish(ok, Ok(()));
        jobs.finish(
            bad,
            Err(anyhow::anyhow!("Could not move to trash; nothing deleted")),
        );
        jobs.finish(
            stopped,
            Err(anyhow::anyhow!("operation cancelled").context("copy a to b")),
        );
        for j in &mut jobs.list {
            j.finished = Some(Instant::now() - CLEAR_AFTER);
        }
        jobs.tick();
        assert_eq!(jobs.list.iter().map(|j| j.id).collect::<Vec<_>>(), [bad]);
        // The failure that stays never schedules a wake-up (no repaint loop).
        assert!(jobs.list.iter().all(|j| !j.clears()));
    }

    #[test]
    fn panicking_job_still_reports_done() {
        let mut jobs = Jobs::new(egui::Context::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let id = jobs.spawn("boom".into(), tx, |_, _| panic!("disk on fire"));
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::JobDone { id: got, result } => {
                assert_eq!(got, id);
                assert!(format!("{:#}", result.unwrap_err()).contains("disk on fire"));
            }
            _ => panic!("unexpected message"),
        }
    }
}
