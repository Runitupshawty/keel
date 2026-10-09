//! File operation jobs: copy / move / trash on worker threads with progress and cancel,
//! shown in a panel at the bottom of the window.

use crate::clipboard::Clipboard;
use crate::state::Msg;
use crate::worker::{send, spawn};
use crossbeam_channel::Sender;
use keel_vfs::{Conflict, Progress, Router, VPath};
use std::cell::Cell;
use std::path::{Path, PathBuf};
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
}

impl Job {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
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

/// A copy or move waiting for its conflict policy.
#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub src: Vec<PathBuf>,
    pub dst: PathBuf,
    pub mv: bool,
}

/// Where a transfer's sources come from.
pub enum Source {
    /// Dropped or dragged paths; `true` = move.
    Paths(Vec<PathBuf>, bool),
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

    pub fn copy(
        &mut self,
        src: Vec<PathBuf>,
        dst: PathBuf,
        conflict: Conflict,
        tx: Sender<Msg>,
    ) -> u64 {
        let title = format!("Copying {} to {}", items(src.len()), dst.display());
        self.spawn(title, tx, move |report, cancel| {
            keel_vfs::copy_local(&src, &dst, conflict, report, cancel)
        })
    }

    pub fn mv(
        &mut self,
        src: Vec<PathBuf>,
        dst: PathBuf,
        conflict: Conflict,
        tx: Sender<Msg>,
    ) -> u64 {
        let title = format!("Moving {} to {}", items(src.len()), dst.display());
        self.spawn(title, tx, move |report, cancel| {
            keel_vfs::move_local(&src, &dst, conflict, report, cancel)
        })
    }

    pub fn start(&mut self, t: Transfer, conflict: Conflict, tx: Sender<Msg>) -> u64 {
        if t.mv {
            self.mv(t.src, t.dst, conflict, tx)
        } else {
            self.copy(t.src, t.dst, conflict, tx)
        }
    }

    /// Sends each path to the OS trash; stops at the first failure.
    pub fn delete(&mut self, paths: Vec<VPath>, router: Arc<Router>, tx: Sender<Msg>) -> u64 {
        let title = format!("Moving {} to the trash", items(paths.len()));
        self.spawn(title, tx, move |report, cancel| {
            let mut p = Progress {
                done_bytes: 0,
                total_bytes: 0,
                current: String::new(),
                done_items: 0,
                total_items: paths.len(),
            };
            for path in &paths {
                anyhow::ensure!(!cancel.load(Ordering::Relaxed), "operation cancelled");
                p.current = path.display();
                report(p.clone());
                router
                    .provider_for(path)
                    .ok_or_else(|| anyhow::anyhow!("no provider for {}", path.display()))?
                    .remove(path)?;
                p.done_items += 1;
            }
            report(p);
            Ok(())
        })
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
        });
        let ctx = self.ctx.clone();
        spawn("keel-job", move || {
            let last = Cell::new(None::<Instant>);
            let report = |p: Progress| {
                if last.get().is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                    last.set(Some(Instant::now()));
                    send(&tx, &ctx, Msg::JobProgress { id, p });
                }
            };
            let result = work(&report, &cancel);
            send(&tx, &ctx, Msg::JobDone { id, result });
        });
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
        self.list.retain(|j| match (&j.done, j.finished) {
            (Some(Err(_)), _) if !j.cancelled() => true,
            (Some(_), Some(at)) => now < at + CLEAR_AFTER,
            _ => true,
        });
        if let Some(next) = self.list.iter().filter_map(|j| j.finished).min() {
            self.ctx
                .request_repaint_after((next + CLEAR_AFTER).saturating_duration_since(now));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let mut dismiss = None;
        for job in &self.list {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &job.done {
                        None => {
                            if ui.button("Cancel").clicked() {
                                job.cancel.store(true, Ordering::Relaxed);
                            }
                        }
                        Some(Err(_)) => {
                            if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                                dismiss = Some(job.id);
                            }
                        }
                        Some(Ok(())) => {}
                    }
                    ui.add(
                        egui::ProgressBar::new(job.fraction())
                            .desired_width(180.0)
                            .show_percentage(),
                    );
                    let (text, error) = match &job.done {
                        _ if job.cancelled() => ("Cancelled".to_owned(), false),
                        Some(Err(e)) => (format!("{e:#}"), true),
                        Some(Ok(())) => ("Done".to_owned(), false),
                        None => (job.progress.current.clone(), false),
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
        if let Some(id) = dismiss {
            self.list.retain(|j| j.id != id);
        }
    }
}

pub fn items(n: usize) -> String {
    match n {
        1 => "1 item".to_owned(),
        n => format!("{n} items"),
    }
}

/// Top-level names in `src` that already exist in `dst`.
pub fn plan_conflicts(src: &[PathBuf], dst: &Path) -> Vec<String> {
    src.iter()
        .filter_map(|p| p.file_name())
        .filter(|name| std::fs::symlink_metadata(dst.join(name)).is_ok())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

/// Resolves the sources (reading the system clipboard for a paste) and scans `dst` for
/// name clashes off the UI thread, then answers with `Msg::Planned`.
pub fn spawn_plan(source: Source, dst: PathBuf, tx: Sender<Msg>, ctx: egui::Context) {
    spawn("keel-plan", move || {
        let (src, mv, from_clipboard) = match source {
            Source::Paths(src, mv) => (src, mv, false),
            Source::Clipboard(clip) => match clip.resolve() {
                Some((src, cut)) => (src, cut, true),
                None => {
                    send(&tx, &ctx, Msg::Toast("The clipboard holds no files".into()));
                    return;
                }
            },
        };
        let src: Vec<PathBuf> = src
            .into_iter()
            .filter(|p| p.parent() != Some(dst.as_path()))
            .collect();
        if src.is_empty() {
            let text = if mv {
                "Already in this folder"
            } else {
                "Copying into the same folder is not supported yet"
            };
            send(&tx, &ctx, Msg::Toast(text.into()));
            return;
        }
        let conflicts = plan_conflicts(&src, &dst);
        send(
            &tx,
            &ctx,
            Msg::Planned {
                op: Transfer { src, dst, mv },
                conflicts,
                from_clipboard,
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let id = jobs.copy(vec![root.join("src")], root.join("dst"), Conflict::Skip, tx);
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
        let src = vec![root.join("src/a.txt"), root.join("src/b.txt")];
        assert_eq!(plan_conflicts(&src, &root.join("dst")), ["a.txt"]);
    }

    #[test]
    fn finished_jobs_clear_but_failures_stay() {
        let mut jobs = Jobs::new(egui::Context::default());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let ok = jobs.delete(Vec::new(), Arc::new(Router::new()), tx.clone());
        let bad = jobs.delete(Vec::new(), Arc::new(Router::new()), tx);
        jobs.finish(ok, Ok(()));
        jobs.finish(
            bad,
            Err(anyhow::anyhow!("Could not move to trash; nothing deleted")),
        );
        for j in &mut jobs.list {
            j.finished = Some(Instant::now() - CLEAR_AFTER);
        }
        jobs.tick();
        assert_eq!(jobs.list.iter().map(|j| j.id).collect::<Vec<_>>(), [bad]);
    }
}
