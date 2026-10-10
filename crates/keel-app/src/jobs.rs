//! File operation jobs: copy / move / trash / extract / add to zip on worker threads with
//! progress and cancel, shown in a panel at the bottom of the window.

use crate::clipboard::Clipboard;
use crate::state::Msg;
use crate::worker::{send, spawn};
use crossbeam_channel::Sender;
use keel_vfs::{Conflict, Progress, Router, VPath};
use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
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

    /// Finished and nothing to report: Ok with nothing skipped, or stopped by Cancel.
    /// Real errors and skipped items stay until dismissed.
    fn clears(&self) -> bool {
        match &self.done {
            Some(Ok(())) => self.progress.skipped == 0,
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

/// A copy, move or extraction waiting for its conflict policy (local or remote on either
/// side; extraction only into local folders).
#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub src: Vec<VPath>,
    pub dst: VPath,
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
    Paths(Vec<VPath>, bool),
    /// Ctrl+V: resolved against the system clipboard on the planning thread.
    Clipboard(Clipboard),
    /// Extract (Ctrl+V of entries copied inside an archive when `clipboard`, else a
    /// context-menu extract or a drag out of an archive). Clashes are listed from the archive.
    Archive { src: ArchiveSrc, clipboard: bool },
}

pub struct Jobs {
    pub list: Vec<Job>,
    next_id: u64,
    ctx: egui::Context,
    /// Setting: local transfers touching the same drive run one after another.
    pub one_per_drive: bool,
}

impl Jobs {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            list: Vec::new(),
            next_id: 1,
            ctx,
            one_per_drive: false,
        }
    }

    /// Copy or move through `ops::transfer`: local to local takes the OS fast path, anything
    /// involving a remote host streams with staged writes. Extractions go through `extract`.
    pub fn start(
        &mut self,
        t: Transfer,
        conflict: Conflict,
        router: Arc<Router>,
        tx: Sender<Msg>,
    ) -> u64 {
        if let Some(src) = t.extract {
            return self.extract(src, t.dst, conflict, router, tx);
        }
        let verb = if t.mv { "Moving" } else { "Copying" };
        let title = format!("{verb} {} to {}", items(t.src.len()), t.dst.display());
        let remote = t.src.iter().chain([&t.dst]).any(crate::remotes::is_network);
        let queue = self.one_per_drive;
        let id = self.spawn(title, tx, move |report, cancel| {
            let _drives = if queue {
                let keys = t
                    .src
                    .iter()
                    .chain([&t.dst])
                    .filter_map(volume_key)
                    .collect();
                Some(DriveLock::acquire(keys, report, cancel)?)
            } else {
                None
            };
            keel_vfs::ops::transfer(&t.src, &t.dst, t.mv, conflict, report, cancel, &router)
        });
        self.mark_remote(id, remote);
        id
    }

    /// Extracts into `dst`, creating it first (Extract to folder).
    pub fn extract(
        &mut self,
        src: ArchiveSrc,
        dst: VPath,
        conflict: Conflict,
        router: Arc<Router>,
        tx: Sender<Msg>,
    ) -> u64 {
        let title = format!("Extracting {}", src.archive.name());
        self.spawn(title, tx, move |report, cancel| {
            let dst = dst.to_local_path().ok_or_else(|| {
                anyhow::anyhow!("Extract into remote folders is not supported yet")
            })?;
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
            keel_vfs::ops::add_to_archive(&zip, &src, "", report, cancel)
        })
    }

    /// Restores, permanently deletes or empties the Recycle Bin / Trash (a `trash://` tab).
    pub fn trash_op(&mut self, op: TrashOp, tx: Sender<Msg>) -> u64 {
        let title = match &op {
            TrashOp::Restore(p) => format!("Restoring {}", items(p.len())),
            TrashOp::Purge(p) => format!("Deleting {} permanently", items(p.len())),
            TrashOp::Empty => format!("Emptying the {}", keel_vfs::trashbin::label()),
        };
        self.spawn(title, tx, move |_, _| {
            let bin = keel_vfs::TrashProvider;
            match op {
                TrashOp::Restore(p) => bin.restore_paths(&p),
                TrashOp::Purge(p) => bin.purge_paths(&p),
                TrashOp::Empty => bin.empty().map(drop),
            }
        })
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
        // Entries of a zip: one rewrite of the archive for all of them, progress by bytes.
        if let Some((archive, _)) = paths.first().and_then(VPath::split_archive) {
            if paths.iter().all(|p| p.split_archive().is_some()) {
                let title = format!("Deleting {} from {}", items(paths.len()), archive.name());
                return self.spawn(title, tx, move |report, cancel| {
                    keel_vfs::ops::remove_entries(&paths, report, cancel)
                });
            }
        }
        let remote = paths.first().is_some_and(crate::remotes::is_network);
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
                skipped: 0,
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
                skipped: 0,
            },
            cancel: cancel.clone(),
            done: None,
            finished: None,
            remote: false,
        });
        let ctx = self.ctx.clone();
        let started = spawn("keel-job", move || {
            let last = Cell::new(None::<Instant>);
            // The newest report held back by the throttle; sent before JobDone so the
            // final counts (skipped items) always arrive.
            let held = RefCell::new(None::<Progress>);
            let report = |p: Progress| {
                if last.get().is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                    last.set(Some(Instant::now()));
                    held.borrow_mut().take();
                    send(&tx, &ctx, Msg::JobProgress { id, p });
                } else {
                    *held.borrow_mut() = Some(p);
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
            if let Some(p) = held.take() {
                send(&tx, &ctx, Msg::JobProgress { id, p });
            }
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
                        Some(Ok(())) if job.progress.skipped > 0 => (
                            format!(
                                "Done; skipped {} already there",
                                items(job.progress.skipped)
                            ),
                            false,
                        ),
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

/// A file `add_to_archive` can extend (zip, 7z, tar, tar.gz; not rar).
pub fn is_addable_archive(e: &keel_vfs::Entry) -> bool {
    e.kind != keel_vfs::Kind::Dir && crate::settings::ArchiveFormat::of_name(&e.name).is_some()
}

/// "Add to <archive>…" for a selection: exactly one addable archive plus at least one other
/// item, all in the archive's folder. Returns the archive and the others.
pub fn add_target<'a>(
    targets: &[&'a keel_vfs::Entry],
) -> Option<(&'a keel_vfs::Entry, Vec<&'a keel_vfs::Entry>)> {
    let mut archives = targets.iter().copied().filter(|e| is_addable_archive(e));
    let (Some(archive), None) = (archives.next(), archives.next()) else {
        return None;
    };
    let others: Vec<_> = targets
        .iter()
        .copied()
        .filter(|e| e.path != archive.path)
        .collect();
    let dir = archive.path.parent();
    (!others.is_empty() && others.iter().all(|e| e.path.parent() == dir))
        .then_some((archive, others))
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

/// A Recycle Bin / Trash job (`trash_op`).
pub enum TrashOp {
    Restore(Vec<VPath>),
    Purge(Vec<VPath>),
    Empty,
}

pub fn items(n: usize) -> String {
    match n {
        1 => "1 item".to_owned(),
        n => format!("{n} items"),
    }
}

/// The volume a local path is on: on Windows its volume GUID (the same through `subst`
/// drives, junctions and folder mount points; `keel_vfs::desktop::volume_id`) or share root,
/// else the path prefix (`c:`, `\\server\share`) when it can't be read; the device number
/// elsewhere. A path that does not exist yet counts by its nearest existing parent. None for
/// remote paths. May touch the disk: workers only.
pub fn volume_key(p: &VPath) -> Option<String> {
    let local = p.to_local_path()?;
    #[cfg(windows)]
    {
        let real = local.ancestors().find_map(keel_vfs::desktop::volume_id);
        real.or_else(|| match local.components().next()? {
            std::path::Component::Prefix(pre) => {
                Some(pre.as_os_str().to_string_lossy().to_lowercase())
            }
            _ => None,
        })
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        let mut probe = local.as_path();
        loop {
            if let Ok(m) = std::fs::metadata(probe) {
                return Some(m.dev().to_string());
            }
            probe = probe.parent()?;
        }
    }
}

/// Transfers holding or waiting for drives (setting "one transfer per drive"), in ticket
/// order: (ticket, drives).
struct DriveQueue {
    next: u64,
    queue: Vec<(u64, Vec<String>)>,
}

static DRIVES: parking_lot::Mutex<DriveQueue> = parking_lot::Mutex::new(DriveQueue {
    next: 0,
    queue: Vec::new(),
});
/// Signalled whenever a transfer leaves the queue.
static DRIVES_FREED: parking_lot::Condvar = parking_lot::Condvar::new();

/// Holds drives for a transfer (its ticket in the queue); released on drop.
pub struct DriveLock(u64);

impl DriveLock {
    /// Takes a ticket for `keys` (drives) and waits until no earlier ticket shares one of
    /// them, showing that it waits: transfers on one drive run in the order they asked, none
    /// waits forever behind later ones, and transfers on other drives are not held up.
    /// Cancel stops the wait.
    pub fn acquire(
        mut keys: Vec<String>,
        report: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> anyhow::Result<Self> {
        keys.sort();
        keys.dedup();
        let mut drives = DRIVES.lock();
        let ticket = drives.next;
        drives.next += 1;
        drives.queue.push((ticket, keys.clone()));
        let mut told = false;
        loop {
            let blocked = drives
                .queue
                .iter()
                .take_while(|(t, _)| *t != ticket)
                .any(|(_, held)| held.iter().any(|k| keys.contains(k)));
            if !blocked {
                return Ok(Self(ticket));
            }
            if cancel.load(Ordering::Relaxed) {
                drives.queue.retain(|(t, _)| *t != ticket);
                DRIVES_FREED.notify_all();
                anyhow::bail!("operation cancelled");
            }
            if !told {
                told = true;
                report(Progress {
                    current: "Waiting for another transfer on this drive…".into(),
                    ..Progress::default()
                });
            }
            // Wakes on a release, or after a while to look at `cancel`.
            DRIVES_FREED.wait_for(&mut drives, Duration::from_millis(100));
        }
    }
}

impl Drop for DriveLock {
    fn drop(&mut self) {
        DRIVES.lock().queue.retain(|(t, _)| *t != self.0);
        DRIVES_FREED.notify_all();
    }
}

/// Top-level names in `src` that already exist in `dst` (a stat on the remote host when
/// `dst` is remote). Local names go through the extended-length form, so long paths and
/// names ending in a dot or space are found too. Blocks: workers only.
pub fn plan_conflicts(src: &[VPath], dst: &VPath, router: &Router) -> Vec<String> {
    let exists = |name: &str| match dst.to_local_path() {
        Some(dir) => {
            keel_vfs::long(&dir.join(name)).is_ok_and(|p| std::fs::symlink_metadata(p).is_ok())
        }
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

/// Top-level names an extraction of `src` would create that already exist in `dst`.
fn archive_conflicts(
    src: &ArchiveSrc,
    dst: &VPath,
    router: &Router,
) -> anyhow::Result<Vec<String>> {
    let dst = dst
        .to_local_path()
        .ok_or_else(|| anyhow::anyhow!("Extract into remote folders is not supported yet"))?;
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
        .filter(|name| {
            keel_vfs::long(&dst.join(name)).is_ok_and(|p| std::fs::symlink_metadata(p).is_ok())
        })
        .collect())
}

/// The paths an `ArchiveSrc` names (the children of its folder when it names none).
fn archive_paths(src: &ArchiveSrc, router: &Router) -> anyhow::Result<Vec<VPath>> {
    let dir = VPath::join_archive(&src.archive, &src.base);
    if !src.entries.is_empty() {
        return Ok(src
            .entries
            .iter()
            .map(|e| VPath::join_archive(&src.archive, e))
            .collect());
    }
    let provider = router
        .provider_for(&dir)
        .ok_or_else(|| anyhow::anyhow!("no provider for {}", dir.display()))?;
    Ok(provider.list(&dir)?.into_iter().map(|e| e.path).collect())
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
        let from_clipboard = matches!(
            source,
            Source::Clipboard(_)
                | Source::Archive {
                    clipboard: true,
                    ..
                }
        );
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

/// `spawn_plan`'s body, for callers already on a worker (the folder picker).
pub fn plan(source: Source, dst: VPath, router: &Router, tx: &Sender<Msg>, ctx: &egui::Context) {
    let (src, mv, from_clipboard) = match source {
        // Into a folder inside a zip: entries are copied over (raw within one zip), not
        // extracted.
        Source::Archive { src, clipboard } if dst.split_archive().is_some() => {
            match archive_paths(&src, router) {
                Ok(paths) => (paths, false, clipboard),
                Err(e) => {
                    let text = format!("{e:#}");
                    let from_clipboard = clipboard;
                    return send(
                        tx,
                        ctx,
                        Msg::PlanFailed {
                            text,
                            from_clipboard,
                        },
                    );
                }
            }
        }
        Source::Archive { src, clipboard } => {
            let msg = match archive_conflicts(&src, &dst, router) {
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
    // Entries from inside an archive are extracted (one pass over the archive), unless
    // they go into a zip; they never move out of it.
    if let Some(picked) = src
        .first()
        .and_then(VPath::parent)
        .and_then(|dir| ArchiveSrc::picked(&dir, &src))
    {
        let into_same = dst.split_archive().map(|(a, _)| a) == Some(picked.archive.clone());
        if mv && !into_same {
            let text = "Moving out of an archive is not supported: copy, then delete".into();
            return send(
                tx,
                ctx,
                Msg::PlanFailed {
                    text,
                    from_clipboard,
                },
            );
        }
        if dst.split_archive().is_none() {
            let clipboard = from_clipboard;
            return plan(
                Source::Archive {
                    src: picked,
                    clipboard,
                },
                dst,
                router,
                tx,
                ctx,
            );
        }
    }
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
    fn file(dir: &str, name: &str, kind: keel_vfs::Kind) -> keel_vfs::Entry {
        keel_vfs::Entry {
            path: VPath::local(PathBuf::from(dir).join(name)),
            name: name.into(),
            kind,
            size: 0,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: String::new(),
        }
    }

    #[test]
    fn add_to_entry_needs_one_archive_and_siblings() {
        use keel_vfs::Kind::{Dir, File};
        let ok = |sel: &[keel_vfs::Entry]| {
            let r: Vec<&keel_vfs::Entry> = sel.iter().collect();
            add_target(&r).map(|(a, o)| (a.name.clone(), o.len()))
        };
        let zip = file("/d", "a.zip", File);
        let txt = file("/d", "b.txt", File);
        let folder = file("/d", "sub", Dir);
        assert_eq!(
            ok(&[txt.clone(), zip.clone(), folder]),
            Some(("a.zip".into(), 2))
        );
        for name in ["a.7z", "a.tar", "a.tar.gz", "a.TGZ"] {
            assert!(
                ok(&[file("/d", name, File), txt.clone()]).is_some(),
                "{name}"
            );
        }
        // Nothing else selected, rar, two archives, other folders, a folder named like one.
        assert_eq!(ok(std::slice::from_ref(&zip)), None);
        assert_eq!(ok(&[file("/d", "a.rar", File), txt.clone()]), None);
        assert_eq!(
            ok(&[zip.clone(), file("/d", "b.7z", File), txt.clone()]),
            None
        );
        assert_eq!(ok(&[zip.clone(), file("/e", "b.txt", File)]), None);
        assert_eq!(ok(&[file("/d", "x.zip", Dir), txt]), None);
    }

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
            extract: None,
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
        let src = vec![
            VPath::local(root.join("src/a.txt")),
            VPath::local(root.join("src/b.txt")),
        ];
        let dst = VPath::local(root.join("dst"));
        assert_eq!(plan_conflicts(&src, &dst, &Router::new()), ["a.txt"]);
    }

    /// Polish backlog: clashes past MAX_PATH and names with a trailing dot are found.
    #[test]
    fn plan_conflicts_sees_long_and_trailing_dot_names() {
        let root = tree("keel-job-plan-long");
        let mut deep = root.join("dst");
        while deep.as_os_str().len() < 300 {
            deep = deep.join("a-rather-long-folder-name");
        }
        let long = |p: &std::path::Path| keel_vfs::long(p).unwrap();
        std::fs::create_dir_all(long(&deep)).unwrap();
        for name in ["x.txt", "dot."] {
            std::fs::write(long(&deep.join(name)), "old").unwrap();
        }
        let src = vec![
            VPath::local(root.join("src/x.txt")),
            VPath::local(root.join("src/dot.")),
            VPath::local(root.join("src/new.txt")),
        ];
        let found = plan_conflicts(&src, &VPath::local(&deep), &Router::new());
        assert_eq!(found, ["x.txt", "dot."]);
        let _ = std::fs::remove_dir_all(long(&root));
    }

    /// Polish backlog: a job's last progress (with its skipped count) always arrives,
    /// and a job that skipped items stays in the panel.
    #[test]
    fn skipped_items_are_reported_and_kept() {
        let mut jobs = Jobs::new(egui::Context::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let id = jobs.spawn("copy".into(), tx, |report, _| {
            for skipped in 1..=3 {
                report(Progress {
                    skipped,
                    ..Progress::default()
                });
            }
            Ok(())
        });
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::JobProgress { id, p } => jobs.progress(id, p),
                Msg::JobDone { id, result } => {
                    jobs.finish(id, result);
                    break;
                }
                _ => panic!("unexpected message"),
            }
        }
        let job = jobs.list.iter().find(|j| j.id == id).unwrap();
        assert_eq!(job.progress.skipped, 3, "the throttled last report arrived");
        assert!(!job.clears());
    }

    #[test]
    fn drive_lock_serialises_transfers_on_one_drive() {
        let none = AtomicBool::new(false);
        let first = DriveLock::acquire(vec!["test-drive-q".into()], &|_| {}, &none).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let waiter = std::thread::spawn(move || {
            let waited = AtomicBool::new(false);
            let lock = DriveLock::acquire(
                vec!["test-drive-q".into(), "test-drive-r".into()],
                &|p| waited.store(p.current.contains("Waiting"), Ordering::Relaxed),
                &AtomicBool::new(false),
            )
            .unwrap();
            tx.send(waited.load(Ordering::Relaxed)).unwrap();
            drop(lock);
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "waits"
        );
        drop(first);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "said it waited"
        );
        waiter.join().unwrap();
        // Cancel stops a wait.
        let _held = DriveLock::acquire(vec!["test-drive-s".into()], &|_| {}, &none).unwrap();
        let cancelled = AtomicBool::new(true);
        assert!(DriveLock::acquire(vec!["test-drive-s".into()], &|_| {}, &cancelled).is_err());
        assert!(volume_key(&VPath::local(std::env::temp_dir())).is_some());
        assert!(volume_key(&VPath::parse("sftp://h/x").unwrap()).is_none());
    }

    /// Waiters on one drive get it in the order they asked; a waiter blocked on a busy
    /// drive holds up later waiters that share a drive with it, but not unrelated ones.
    #[test]
    fn drive_lock_is_first_come_first_served() {
        let none = AtomicBool::new(false);
        let first = DriveLock::acquire(vec!["fifo-a".into()], &|_| {}, &none).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut waiters = Vec::new();
        for (n, keys) in [
            (1, vec!["fifo-a"]),
            (2, vec!["fifo-a", "fifo-b"]),
            (3, vec!["fifo-b"]),
        ] {
            let tx = tx.clone();
            let keys: Vec<String> = keys.into_iter().map(str::to_owned).collect();
            waiters.push(std::thread::spawn(move || {
                let lock = DriveLock::acquire(keys, &|_| {}, &AtomicBool::new(false)).unwrap();
                tx.send(n).unwrap();
                std::thread::sleep(Duration::from_millis(50));
                drop(lock);
            }));
            // Each has taken its ticket before the next asks.
            std::thread::sleep(Duration::from_millis(150));
        }
        // 3 waits behind 2 (fifo-b), which waits behind 1 and `first` (fifo-a), although
        // fifo-b itself is free.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        // An unrelated drive goes at once.
        drop(DriveLock::acquire(vec!["fifo-c".into()], &|_| {}, &none).unwrap());
        drop(first);
        let order: Vec<i32> = (0..3)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        assert_eq!(order, [1, 2, 3]);
        for w in waiters {
            w.join().unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn volume_key_is_the_real_volume() {
        let tmp = std::env::temp_dir();
        let key = volume_key(&VPath::local(&tmp)).unwrap();
        assert!(key.starts_with(r"\\?\volume{"), "{key}");
        // Not there yet: its nearest existing parent's volume.
        let new = tmp.join("keel-no-such-dir").join("x");
        assert_eq!(volume_key(&VPath::local(new)), Some(key.clone()));
        let root = tmp.ancestors().last().unwrap();
        assert_eq!(volume_key(&VPath::local(root)), Some(key));
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
