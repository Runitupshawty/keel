//! The Disk Usage window's analysis tabs (Phase 17 Task 84): Largest files, Duplicates,
//! Search, Storage analytics and Permissions. Each tab is a `State` the window keeps and a
//! `ui(ui, tree, state)` that draws it for a scan; the models are in `keel_vfs::du`.
//!
//! A tab never changes the disk or app state itself: it returns an [`Outcome`] for the
//! window (select an item in the linked views, or preview operations through the library's
//! plan flow, `LibCmd::Plan`). Slow work (hashing, reading permissions, saving reports)
//! runs on a worker thread with progress and Cancel.

pub mod duplicates;
pub mod largest;
pub mod permissions;
pub mod search;
pub mod storage;

#[cfg(test)]
mod tests;

use crate::a11y::Named;
use egui_extras::{Column, TableBuilder};
use humansize::{format_size, DECIMAL};
use keel_vfs::du::analysis::{FileRow, SizeRange};
use keel_vfs::du::{NodeId, ScanHandle, Tree};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// What a tab asks of the Disk Usage window.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Select this item in the tree, the treemap and the other views ("jump to tree").
    Select(NodeId),
    /// Preview and confirm these operations one after another through the library's plan
    /// flow (`LibCmd::Plan`: the preview names every path and byte before anything runs);
    /// the next one only after the previous ran.
    Plans(Vec<keel_core::Op>),
}

impl Outcome {
    /// The app actions for the first step (the window queues the rest).
    pub fn first_action(&self) -> Option<crate::keys::Action> {
        match self {
            Outcome::Plans(ops) => Some(crate::keys::Action::Library(
                crate::library::LibCmd::Plan(ops.first()?.clone()),
            )),
            Outcome::Select(_) => None,
        }
    }
}

/// A job on a worker thread whose progress and Cancel go through a [`ScanHandle`]; dropping
/// it cancels the job.
pub struct Worker<T> {
    rx: mpsc::Receiver<anyhow::Result<T>>,
    pub handle: ScanHandle,
}

impl<T: Send + 'static> Worker<T> {
    pub fn spawn(
        ctx: &egui::Context,
        name: &str,
        work: impl FnOnce(ScanHandle) -> anyhow::Result<T> + Send + 'static,
    ) -> Self {
        let handle = ScanHandle::new();
        let (tx, rx) = mpsc::channel();
        let (h, ctx) = (handle.clone(), ctx.clone());
        crate::worker::spawn(name, move || {
            let _ = tx.send(work(h));
            ctx.request_repaint();
        });
        Worker { rx, handle }
    }

    /// The result once the job is done.
    pub fn poll(&self) -> Option<anyhow::Result<T>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("the job stopped"))),
        }
    }

    /// Progress (`what` the files count, e.g. "files hashed"), bytes read, the current
    /// path and a Cancel button.
    pub fn progress(&self, ui: &mut egui::Ui, what: &str) {
        let p = self.handle.progress();
        ui.horizontal(|ui| {
            crate::anim::spinner(ui);
            let mut text = format!("{} {what}", crate::library::count(p.files));
            if p.bytes > 0 {
                text += &format!(", {} read", format_size(p.bytes, DECIMAL));
            }
            ui.label(text);
            let cancel = ui.add_enabled(!p.cancelled, egui::Button::new("Cancel"));
            if cancel.named("Cancel").clicked() {
                self.handle.cancel();
            }
        });
        if !p.current.as_os_str().is_empty() {
            ui.add(
                egui::Label::new(egui::RichText::new(p.current.display().to_string()).weak())
                    .truncate(),
            );
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.handle.cancel();
    }
}

/// True (and remembers it) when `tree` is not the scan seen last: a new scan or a refresh
/// (the window swaps in a new `Arc` for either).
pub fn changed(seen: &mut Option<Arc<Tree>>, tree: &Arc<Tree>) -> bool {
    if seen.as_ref().is_some_and(|old| Arc::ptr_eq(old, tree)) {
        return false;
    }
    *seen = Some(tree.clone());
    true
}

/// Minimum and maximum size fields, as typed ("10 MB"; empty for no bound; see
/// [`keel_vfs::du::analysis::parse_size`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SizeFields {
    pub min: String,
    pub max: String,
}

impl SizeFields {
    /// The two fields: (one changed, Enter pressed in one).
    pub fn ui(&mut self, ui: &mut egui::Ui) -> (bool, bool) {
        let enter =
            |r: &egui::Response| r.lost_focus() && r.ctx.input(|i| i.key_pressed(egui::Key::Enter));
        ui.label("Size from");
        let a = ui.add(
            egui::TextEdit::singleline(&mut self.min)
                .desired_width(70.0)
                .hint_text("0"),
        );
        let a = a.named("Minimum size");
        ui.label("to");
        let b = ui.add(
            egui::TextEdit::singleline(&mut self.max)
                .desired_width(70.0)
                .hint_text("any"),
        );
        let b = b.named("Maximum size");
        (a.changed() || b.changed(), enter(&a) || enter(&b))
    }

    pub fn range(&self) -> Result<SizeRange, String> {
        SizeRange::parse(&self.min, &self.max)
    }
}

pub fn size_text(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}

/// Unix nanoseconds as local time.
pub fn time_text(ns: Option<i64>) -> String {
    ns.and_then(|n| {
        let t = chrono::DateTime::from_timestamp(
            n.div_euclid(1_000_000_000),
            n.rem_euclid(1_000_000_000) as u32,
        )?;
        Some(
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
        )
    })
    .unwrap_or_default()
}

pub fn error_line(ui: &mut egui::Ui, text: &str) {
    if !text.is_empty() {
        ui.colored_label(ui.visuals().error_fg_color, text);
    }
}

/// A last column of [`file_table`]: its title and what to draw in it for row `i`.
pub type ExtraColumn<'a> = (&'a str, &'a mut dyn FnMut(&mut egui::Ui, usize));

/// A table of files: Name, Size, Size on disk, Modified, Folder. A click on a row returns
/// it (to select it in the tree); `extra` draws a last column (a row's buttons).
pub fn file_table(
    ui: &mut egui::Ui,
    salt: &str,
    rows: &[FileRow],
    mut extra: Option<ExtraColumn<'_>>,
) -> Option<NodeId> {
    let mut picked = None;
    let mut table = TableBuilder::new(ui)
        .id_salt(salt)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::initial(220.0).at_least(80.0).clip(true))
        .column(Column::initial(80.0))
        .column(Column::initial(80.0))
        .column(Column::initial(120.0))
        .column(Column::remainder().at_least(120.0).clip(true));
    if extra.is_some() {
        table = table.column(Column::auto());
    }
    table
        .header(20.0, |mut h| {
            for title in ["Name", "Size", "On disk", "Modified", "Folder"] {
                h.col(|ui| {
                    ui.strong(title);
                });
            }
            if let Some((title, _)) = &extra {
                h.col(|ui| {
                    ui.strong(*title);
                });
            }
        })
        .body(|body| {
            body.rows(22.0, rows.len(), |mut row| {
                let i = row.index();
                let f = &rows[i];
                let name = f
                    .path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                row.col(|ui| {
                    let r = ui.selectable_label(false, &name);
                    let r = r.named(&format!(
                        "{name}, {}: select in the tree",
                        size_text(f.bytes)
                    ));
                    if r.on_hover_text(f.path.display().to_string()).clicked() {
                        picked = Some(f.node);
                    }
                });
                row.col(|ui| {
                    ui.label(size_text(f.bytes));
                });
                row.col(|ui| {
                    ui.label(size_text(f.allocated));
                });
                row.col(|ui| {
                    ui.label(time_text(f.modified_ns));
                });
                row.col(|ui| {
                    let dir = f
                        .path
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    ui.add(egui::Label::new(egui::RichText::new(dir).weak()).truncate());
                });
                if let Some((_, draw)) = &mut extra {
                    row.col(|ui| draw(ui, i));
                }
            });
        });
    picked
}

/// Saving a report: the OS dialog and the writing run on a worker; a report is written
/// beside its destination and then moved over it, so a failed save leaves an older report
/// as it was.
#[derive(Default)]
pub struct Export {
    job: Option<Worker<Option<PathBuf>>>,
    pub status: String,
}

impl Export {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Asks where to save `name` (a `kind` file, extension `ext`) and writes it there.
    pub fn start(
        &mut self,
        ctx: &egui::Context,
        name: String,
        kind: &'static str,
        ext: &'static str,
        write: impl FnOnce(&mut dyn std::io::Write) -> anyhow::Result<()> + Send + 'static,
    ) {
        if self.busy() {
            return;
        }
        self.status.clear();
        self.job = Some(Worker::spawn(ctx, "keel-du-export", move |_| {
            // The OS dialog blocks: on this worker, never the UI thread.
            let Some(path) = rfd::FileDialog::new()
                .set_title("Save report")
                .set_file_name(name)
                .add_filter(kind, &[ext])
                .save_file()
            else {
                return Ok(None);
            };
            save(&path, write)?;
            Ok(Some(path))
        }));
    }

    /// The result of the last save.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if let Some(job) = &self.job {
            match job.poll() {
                None => {
                    ui.horizontal(|ui| {
                        crate::anim::spinner(ui);
                        ui.label("Saving the report…");
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Some(r) => {
                    self.status = match r {
                        Ok(Some(p)) => format!("Saved {}", p.display()),
                        Ok(None) => String::new(),
                        Err(e) => format!("Could not save the report: {e:#}"),
                    };
                    self.job = None;
                }
            }
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
    }
}

/// Writes a report to `path` through a temp file beside it.
pub fn save(
    path: &std::path::Path,
    write: impl FnOnce(&mut dyn std::io::Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("no file name"))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(".keel-part");
    let temp = path.with_file_name(temp_name);
    let result = (|| {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&temp)?);
        write(&mut out)?;
        out.flush()?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&temp, path)?;
        anyhow::Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
