//! Running the operations an analysis tab asks for (Duplicates: keep one, hard-link the
//! others), one step at a time.
//!
//! - **With the library open** each step goes through the library's plan flow
//!   (`LibCmd::Plan`): its preview names every path and byte, and nothing runs until it is
//!   confirmed. The library shows one preview at a time, so the next step is previewed
//!   only after the previous one's job finished (done); a cancelled preview, a failed or
//!   cancelled job, or a refused execute drops the steps left.
//! - **Without it** ([`Fallback`]) the window shows the same facts (every path, the bytes
//!   moved to the trash, every link made) in one confirmation, then runs the steps in
//!   order on a worker and stops at the first failure: a file that could not go to the
//!   trash is never replaced by a link.

use crate::du_tabs::Worker;
use humansize::{format_size, DECIMAL};
use keel_core::{JobId, JobStatus, Op};
use keel_vfs::{LinkKind, VPath};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A preview or an execute the library has not answered in this long is given up.
pub const ANSWER_WAIT: Duration = Duration::from_secs(60);

/// Where the library-driven steps are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    /// A step was sent for preview; `shown` once its preview is open.
    Preview { shown: bool },
    /// The preview was confirmed; waiting for its job to start.
    Executing,
    /// The step's job runs.
    Running(JobId),
}

/// The steps through the library's plan flow.
#[derive(Debug, Default)]
pub struct Steps {
    pub queue: VecDeque<Op>,
    pub phase: Phase,
    /// Steps in this run, and the one shown or running (1-based).
    pub total: usize,
    pub current: usize,
    /// When the library was last asked (a preview, an execute).
    asked: Option<Instant>,
}

impl Steps {
    pub fn active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Takes a new run of steps; the first one to preview.
    pub fn start(&mut self, ops: Vec<Op>) -> Option<Op> {
        self.queue = ops.into();
        self.total = self.queue.len();
        self.current = 0;
        self.next()
    }

    fn next(&mut self) -> Option<Op> {
        let op = self.queue.pop_front();
        match &op {
            Some(_) => {
                self.current += 1;
                self.phase = Phase::Preview { shown: false };
                self.asked = Some(Instant::now());
            }
            None => self.stop(),
        }
        op
    }

    /// Forgets the steps left.
    pub fn stop(&mut self) {
        self.queue.clear();
        self.phase = Phase::Idle;
    }

    /// Every frame: whether the library's preview is open now. Some(message) when the
    /// steps were dropped because the user closed the preview without confirming.
    pub fn tick(&mut self, preview_open: bool) -> Option<String> {
        match self.phase {
            Phase::Preview { shown: false } if preview_open => {
                self.phase = Phase::Preview { shown: true };
            }
            Phase::Preview { shown: true } if !preview_open => {
                let left = self.queue.len();
                self.stop();
                if left > 0 {
                    return Some(format!(
                        "Preview cancelled: the {} step(s) after it were not run",
                        left
                    ));
                }
            }
            // The plan was refused at execute and previewed again.
            Phase::Executing if preview_open => self.phase = Phase::Preview { shown: true },
            Phase::Preview { shown: false } | Phase::Executing
                if self.asked.is_some_and(|t| t.elapsed() > ANSWER_WAIT) =>
            {
                self.stop();
                return Some("The library did not answer: the steps were stopped".into());
            }
            _ => {}
        }
        None
    }

    /// The library is executing the preview just confirmed.
    pub fn executing(&mut self) {
        if matches!(self.phase, Phase::Preview { .. }) {
            self.phase = Phase::Executing;
            self.asked = Some(Instant::now());
        }
    }

    /// The confirmed step's job started.
    pub fn started(&mut self, id: JobId) {
        if self.phase == Phase::Executing {
            self.phase = Phase::Running(id);
        }
    }

    /// The preview or the execute failed: the steps left are dropped.
    pub fn failed(&mut self) -> Option<String> {
        if !self.active() {
            return None;
        }
        let left = self.queue.len();
        self.stop();
        (left > 0).then(|| format!("The {left} step(s) after the failed one were not run"))
    }

    /// Job `id` ended with `status`: the next step to preview, when it was ours and done.
    /// Err(message) when it was ours and did not finish, so the rest is dropped.
    pub fn finished(&mut self, id: JobId, status: JobStatus) -> Result<Option<Op>, String> {
        if self.phase != Phase::Running(id) {
            return Ok(None);
        }
        if status == JobStatus::Done {
            return Ok(self.next());
        }
        let left = self.queue.len();
        self.stop();
        match left {
            0 => Ok(None),
            n => Err(format!("The step did not finish: the {n} step(s) after it were not run")),
        }
    }

    /// "Step 2 of 3: …" for the window.
    pub fn status(&self) -> Option<String> {
        let what = match self.phase {
            Phase::Idle => return None,
            Phase::Preview { .. } => "confirm or cancel its preview",
            Phase::Executing => "starting",
            Phase::Running(_) => "running",
        };
        Some(format!("Step {} of {}: {what}", self.current, self.total))
    }
}

/// One line per step, with every path: what the confirmation shows.
pub fn describe(ops: &[Op]) -> Vec<String> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            Op::Delete { paths } => {
                let bytes: u64 = (paths.iter())
                    .filter_map(|p| p.to_local_path())
                    .filter_map(|p| std::fs::symlink_metadata(p).ok())
                    .map(|m| m.len())
                    .sum();
                out.push(format!(
                    "Move {} item(s), {}, to the trash:",
                    paths.len(),
                    format_size(bytes, DECIMAL)
                ));
                out.extend(paths.iter().map(|p| format!("    {}", p.display())));
            }
            Op::Link { target, link, kind } => out.push(format!(
                "Make {} {} pointing at {}",
                match kind {
                    LinkKind::Hardlink => "the hard link",
                    _ => "the link",
                },
                link.display(),
                target.display()
            )),
            other => out.push(format!("Not supported without the library: {other:?}")),
        }
    }
    out
}

/// Whether every step can run without the library (local trash and links only).
pub fn runs_without_library(ops: &[Op]) -> bool {
    let local = |p: &VPath| p.to_local_path().is_some();
    ops.iter().all(|op| match op {
        Op::Delete { paths } => paths.iter().all(local),
        Op::Link { target, link, .. } => local(target) && local(link),
        _ => false,
    })
}

/// Runs `ops` in order, stopping at the first failure; the summary of what was done.
pub fn run(ops: &[Op], handle: &keel_vfs::du::ScanHandle) -> anyhow::Result<String> {
    let (mut trashed, mut linked) = (0usize, 0usize);
    let done = |t: usize, l: usize| format!("{t} item(s) moved to the trash, {l} link(s) made");
    for op in ops {
        if handle.is_cancelled() {
            anyhow::bail!("Stopped: {}", done(trashed, linked));
        }
        let step = match op {
            Op::Delete { paths } => paths.iter().try_for_each(|p| {
                keel_vfs::local::trash(p)?;
                trashed += 1;
                anyhow::Ok(())
            }),
            Op::Link { target, link, kind } => (|| {
                let (Some(t), Some(l)) = (target.to_local_path(), link.to_local_path()) else {
                    anyhow::bail!("not a local path");
                };
                keel_vfs::fsattr::make_link(&t, &l, *kind)?;
                linked += 1;
                anyhow::Ok(())
            })(),
            other => Err(anyhow::anyhow!("cannot run without the library: {other:?}")),
        };
        if let Err(e) = step {
            anyhow::bail!("{e:#}; stopped after {}", done(trashed, linked));
        }
    }
    Ok(done(trashed, linked))
}

/// The confirmation and the run without the library.
pub struct Fallback {
    pub ops: Vec<Op>,
    pub lines: Vec<String>,
    pub job: Option<Worker<String>>,
}

impl Fallback {
    pub fn new(ops: Vec<Op>) -> Self {
        let lines = describe(&ops);
        Fallback {
            ops,
            lines,
            job: None,
        }
    }

    /// The modal; Some(result line) when it closes (None in it: cancelled).
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<Option<String>> {
        if let Some(job) = &self.job {
            if let Some(r) = job.poll() {
                return Some(Some(match r {
                    Ok(done) => format!("Done: {done}"),
                    Err(e) => format!("{e:#}"),
                }));
            }
        }
        let mut answer = None;
        let can_run = runs_without_library(&self.ops);
        let modal = egui::Modal::new(egui::Id::new("keel-du-steps")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading("Run these steps?");
            ui.label(
                "The library is not open, so Keel runs them itself, in this order, and stops at \
                 the first one that fails. Items go to the Recycle Bin or Trash.",
            );
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .show(ui, |ui| {
                    for line in &self.lines {
                        ui.label(line);
                    }
                });
            ui.add_space(8.0);
            match &self.job {
                Some(_) => {
                    ui.horizontal(|ui| {
                        crate::anim::spinner(ui);
                        ui.label("Running the steps…");
                    });
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(100));
                }
                None => {
                    ui.horizontal(|ui| {
                        let button = ui.add_enabled(can_run, egui::Button::new("Run"));
                        if button.clicked() {
                            let ops = self.ops.clone();
                            self.job = Some(Worker::spawn(ui.ctx(), "keel-du-steps", move |h| {
                                run(&ops, &h)
                            }));
                        }
                        if ui.button("Cancel").clicked() {
                            answer = Some(None);
                        }
                    });
                }
            }
        });
        if self.job.is_none() && modal.should_close() && answer.is_none() {
            answer = Some(None);
        }
        answer
    }
}
