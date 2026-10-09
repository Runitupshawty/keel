//! File operation jobs: copy / move / trash / extract / add to zip on worker threads with
//! progress and cancel, shown in a panel at the bottom of the window.

use crate::clipboard::Clipboard;
use crate::state::Msg;
use crate::worker::{send, spawn};
use crossbeam_channel::Sender;
use keel_vfs::{Conflict, Progress, Router, VPath};
use std::cell::Cell;
use std::panic::AssertUnwindSafe;
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

/// A copy, move or extraction waiting for its conflict policy.
#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub src: Vec<PathBuf>,
    pub dst: PathBuf,
    pub mv: bool,
    /// Extract from this archive instead of copying `src` (then empty).
    pub extract: Option<ArchiveSrc>,
}

/// Entries of one archive to extract: `archive` is the archive file (possibly itself
/// inside an archive), `base` the folder inside it they are taken relative to, `entries`
/// normalised inner names (empty = everything under `base`).
#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveSrc {
    pub archive: VPath,
    pub base: String,
    pub entries: Vec<String>,
}

impl ArchiveSrc {
    /// The whole archive file `archive`.
    pub fn whole(archive: VPath) -> Self {
        Self {
            archive,
            base: String::new(),
            entries: Vec::new(),
        }
    }

    /// `paths` inside the archive folder `dir` (`x.zip!/sub`); None outside archives.
    pub fn picked(dir: &VPath, paths: &[VPath]) -> Option<Self> {
        let (archive, base) = dir.split_archive()?;
        let entries = paths
            .iter()
            .map(|p| p.split_archive().map(|(_, inner)| inner))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            archive,
            base,
            entries,
        })
    }
}

/// Where a transfer's sources come from.
pub enum Source {
    /// Dropped or dragged paths; `true` = move.
    Paths(Vec<PathBuf>, bool),
    /// Ctrl+V: resolved against the system clipboard on the planning thread.
    Clipboard(Clipboard),
    /// Extract (Ctrl+V of entries copied inside an archive when `clipboard`, else a
    /// context-menu extract or a drag out of an archive). Clashes are listed from the archive.
    Archive {
        src: ArchiveSrc,
        router: Arc<Router>,
        clipboard: bool,
    },
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

    pub fn start(
        &mut self,
        t: Transfer,
        conflict: Conflict,
        router: &Arc<Router>,
        tx: Sender<Msg>,
    ) -> u64 {
        if let Some(src) = t.extract {
            self.extract(src, t.dst, conflict, router.clone(), tx)
        } else if t.mv {
            self.mv(t.src, t.dst, conflict, tx)
        } else {
            self.copy(t.src, t.dst, conflict, tx)
        }
    }

    /// Extracts into `dst`, creating it first (Extract to folder).
    pub fn extract(
        &mut self,
        src: ArchiveSrc,
        dst: PathBuf,
        conflict: Conflict,
        router: Arc<Router>,
        tx: Sender<Msg>,
    ) -> u64 {
        let title = format!("Extracting {}", src.archive.name());
        self.spawn(title, tx, move |report, cancel| {
            std::fs::create_dir_all(&dst)?;
            keel_vfs::extract_under(
                &src.archive,
                &src.base,
                &src.entries,
                &dst,
                conflict,
                report,
                cancel,
                &router,
            )
        })
    }

    /// Adds `src` to `zip` (created when missing; same-named entries are replaced).
    pub fn add_to_zip(&mut self, zip: PathBuf, src: Vec<PathBuf>, tx: Sender<Msg>) -> u64 {
        let name = zip.file_name().map(|n| n.to_string_lossy().into_owned());
        let title = format!("Adding to {}", name.unwrap_or_default());
        self.spawn(title, tx, move |report, cancel| {
            keel_vfs::ops::add_to_zip(&zip, &src, "", report, cancel)
        })
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
                if cancel.load(Ordering::Relaxed) {
                    anyhow::ensure!(p.done_items > 0, "operation cancelled");
                    anyhow::bail!(
                        "Cancelled: {} of {} moved to trash",
                        p.done_items,
                        paths.len()
                    );
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
                        "{} of {} moved to trash; stopped at {}: {why}",
                        p.done_items,
                        paths.len(),
                        path.display()
                    );
                }
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

/// A file that opens as a folder (by name: `open_archive` checks the bytes later).
pub fn is_archive_file(e: &keel_vfs::Entry) -> bool {
    e.kind != keel_vfs::Kind::Dir && VPath::is_archive_name(&e.name)
}

/// `photos.tar.gz` -> `photos`: the folder Extract to folder creates.
pub fn archive_stem(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".zip", ".jar", ".7z", ".tar", ".rar",
    ]
    .iter()
    .find(|ext| lower.ends_with(*ext) && lower.len() > ext.len())
    .map_or(name, |ext| &name[..name.len() - ext.len()])
}

/// Add to "<name>.zip": one target names it (a file without its extension), several take
/// the folder's name.
pub fn zip_name(tab: &crate::tab::Tab) -> String {
    let stem = match tab.targets().as_slice() {
        [one] if one.kind == keel_vfs::Kind::Dir => one.name.clone(),
        [one] => match one.name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
            _ => one.name.clone(),
        },
        _ => match tab.dir.name() {
            "" => "Archive".to_owned(),
            name => name.trim_end_matches(':').to_owned(),
        },
    };
    format!("{stem}.zip")
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

/// Top-level names an extraction of `src` would create that already exist in `dst`.
fn archive_conflicts(src: &ArchiveSrc, dst: &Path, router: &Router) -> anyhow::Result<Vec<String>> {
    let names: Vec<String> = if src.entries.is_empty() {
        let dir = VPath::join_archive(&src.archive, &src.base);
        let provider = router
            .provider_for(&dir)
            .ok_or_else(|| anyhow::anyhow!("no provider for {}", dir.display()))?;
        provider.list(&dir)?.into_iter().map(|e| e.name).collect()
    } else {
        let last = |e: &String| e.rsplit('/').next().unwrap_or(e).to_owned();
        src.entries.iter().map(last).collect()
    };
    Ok(names
        .into_iter()
        .filter(|name| std::fs::symlink_metadata(dst.join(name)).is_ok())
        .collect())
}

/// Resolves the sources (reading the system clipboard for a paste) and scans `dst` for
/// name clashes off the UI thread, then answers with `Msg::Planned` (or `PlanFailed`, also
/// when planning panics). False when the thread could not start.
pub fn spawn_plan(source: Source, dst: PathBuf, tx: Sender<Msg>, ctx: egui::Context) -> bool {
    spawn("keel-plan", move || {
        let from_clipboard = matches!(
            source,
            Source::Clipboard(_)
                | Source::Archive {
                    clipboard: true,
                    ..
                }
        );
        let planned = std::panic::catch_unwind(AssertUnwindSafe(|| plan(source, dst, &tx, &ctx)));
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

/// `spawn_plan`'s body, for callers already on a worker (the folder picker).
pub fn plan(source: Source, dst: PathBuf, tx: &Sender<Msg>, ctx: &egui::Context) {
    let (src, mv, from_clipboard) = match source {
        Source::Archive {
            src,
            router,
            clipboard,
        } => {
            let msg = match archive_conflicts(&src, &dst, &router) {
                Ok(conflicts) => Msg::Planned {
                    op: Transfer {
                        src: Vec::new(),
                        dst,
                        mv: false,
                        extract: Some(src),
                    },
                    conflicts,
                    from_clipboard: clipboard,
                },
                Err(e) => Msg::PlanFailed {
                    text: format!("{e:#}"),
                    from_clipboard: clipboard,
                },
            };
            return send(tx, ctx, msg);
        }
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
    let conflicts = plan_conflicts(&src, &dst);
    send(
        tx,
        ctx,
        Msg::Planned {
            op: Transfer {
                src,
                dst,
                mv,
                extract: None,
            },
            conflicts,
            from_clipboard,
        },
    );
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
    fn archive_names() {
        assert_eq!(archive_stem("Photos.TAR.GZ"), "Photos");
        assert_eq!(archive_stem("a.b.zip"), "a.b");
        assert_eq!(archive_stem(".zip"), ".zip");
        let dir = VPath::parse("mem://t/work").unwrap();
        let mut tab = crate::tab::Tab::new(dir.clone());
        tab.set_entries(vec![
            crate::tab::test_entry(&dir, "notes.txt", keel_vfs::Kind::File, 1),
            crate::tab::test_entry(&dir, "src", keel_vfs::Kind::Dir, 0),
        ]);
        tab.visible(false);
        tab.cursor = Some("notes.txt".into());
        assert_eq!(zip_name(&tab), "notes.zip");
        tab.cursor = Some("src".into());
        assert_eq!(zip_name(&tab), "src.zip");
        tab.select_all();
        assert_eq!(zip_name(&tab), "work.zip");
        assert_eq!(ArchiveSrc::picked(&dir, &[dir.join("src")]), None);
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
