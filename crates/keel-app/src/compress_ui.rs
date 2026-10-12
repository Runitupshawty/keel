//! Compress (NTFS)… (Task 85, Windows): the dialog that picks the algorithm (or
//! uncompress), counts what it will touch on a worker, and starts the job
//! (`Jobs::compress`, `keel_vfs::compress`).

use crate::keys::Action;
use keel_vfs::compress::{Action as Do, Algorithm, Count};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What the job gets.
#[derive(Clone, Debug, PartialEq)]
pub struct CompressStart {
    pub paths: Vec<PathBuf>,
    pub what: Do,
    pub recurse: bool,
    /// The preview's file count (the job's total).
    pub files: u64,
}

type Counted = Arc<Mutex<Option<Result<Count, String>>>>;

pub struct CompressDialog {
    pub paths: Vec<PathBuf>,
    pub what: Do,
    pub recurse: bool,
    any_folder: bool,
    /// The count for the current `recurse`, when it arrived.
    counted: Counted,
    /// Stops the count running (a new one starts when `recurse` changes).
    stop: Arc<AtomicBool>,
    counting_recurse: bool,
}

impl Drop for CompressDialog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl CompressDialog {
    /// The dialog for local `paths` (`dirs`: which are folders); counting starts now.
    pub fn new(paths: Vec<PathBuf>, dirs: &[bool], ctx: &egui::Context) -> Self {
        let mut d = Self {
            paths,
            what: Do::Compress(Algorithm::Lznt1),
            recurse: true,
            any_folder: dirs.iter().any(|d| *d),
            counted: Arc::default(),
            stop: Arc::new(AtomicBool::new(false)),
            counting_recurse: true,
        };
        d.count(ctx);
        d
    }

    fn count(&mut self, ctx: &egui::Context) {
        self.stop.store(true, Ordering::Relaxed);
        self.stop = Arc::new(AtomicBool::new(false));
        self.counted = Arc::default();
        self.counting_recurse = self.recurse;
        let (paths, recurse) = (self.paths.clone(), self.recurse);
        let (slot, stop, ctx) = (self.counted.clone(), self.stop.clone(), ctx.clone());
        crate::worker::spawn("keel-compress-count", move || {
            let result = keel_vfs::compress::count(&paths, recurse, &stop);
            if !stop.load(Ordering::Relaxed) {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(result.map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            }
        });
    }
}

/// The dialog body; Some(action) when it starts the job.
pub fn ui(ui: &mut egui::Ui, d: &mut CompressDialog, cancel: &mut bool) -> Option<Action> {
    ui.strong("Compress (NTFS)");
    ui.label(match d.paths.as_slice() {
        [one] => format!("{}", one.display()),
        many => format!("{} items", many.len()),
    });
    ui.add_space(4.0);
    ui.radio_value(
        &mut d.what,
        Do::Compress(Algorithm::Lznt1),
        "Compress: LZNT1 (NTFS)",
    )
    .on_hover_text(
        "Any file, read and written as usual; new files in a compressed folder are compressed too",
    );
    for alg in &Algorithm::ALL[1..] {
        ui.radio_value(&mut d.what, Do::Compress(*alg), format!("Compress: {}", alg.label()))
            .on_hover_text(match alg {
                Algorithm::Lzx => "Windows Overlay Filter: the most saving, the slowest; for files that are only read (programs, games)",
                _ => "Windows Overlay Filter: files that are only read; a file written to later is stored uncompressed again",
            });
    }
    ui.radio_value(&mut d.what, Do::Uncompress, "Uncompress");
    if d.any_folder {
        ui.checkbox(&mut d.recurse, "Everything in subfolders too");
    }
    if d.recurse != d.counting_recurse {
        d.count(ui.ctx());
    }
    let counted = d.counted.lock().unwrap_or_else(|e| e.into_inner()).clone();
    ui.add_space(4.0);
    match &counted {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Counting files…");
            });
        }
        Some(Ok(c)) => {
            ui.label(format!(
                "{} ({}) in {}",
                crate::jobs::items(c.files as usize).replace("item", "file"),
                humansize::format_size(c.bytes, humansize::DECIMAL),
                match c.folders {
                    1 => "1 folder".to_owned(),
                    n => format!("{n} folders"),
                }
            ));
        }
        Some(Err(e)) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
    }
    ui.weak("Runs as a job you can cancel; the saving is measured after.");
    ui.add_space(8.0);
    let mut out = None;
    ui.horizontal(|ui| {
        let label = match d.what {
            Do::Uncompress => "Uncompress",
            Do::Compress(_) => "Compress",
        };
        let ready = matches!(counted, Some(Ok(_)));
        let go = ui.add_enabled(ready, egui::Button::new(label));
        crate::a11y::first_focus(&go);
        if go.clicked() {
            if let Some(Ok(c)) = counted {
                out = Some(Action::CompressStart(Box::new(CompressStart {
                    paths: d.paths.clone(),
                    what: d.what,
                    recurse: d.recurse,
                    files: c.files,
                })));
            }
        }
        *cancel |= ui.button("Cancel").clicked();
    });
    out
}

/// The toast when the job ends: what changed and the bytes saved.
pub fn summary(what: Do, r: &keel_vfs::compress::Report) -> String {
    use humansize::{format_size, DECIMAL};
    let files = crate::jobs::items(r.files as usize).replace("item", "file");
    let verb = match what {
        Do::Compress(alg) => format!("Compressed {files} with {}", alg.label()),
        Do::Uncompress => format!("Uncompressed {files}"),
    };
    let saved = r.saved();
    let sizes = format!(
        "{} on disk, was {}",
        format_size(r.after, DECIMAL),
        format_size(r.before, DECIMAL)
    );
    let change = if saved >= 0 {
        format!("saved {}", format_size(saved as u64, DECIMAL))
    } else {
        format!("{} more", format_size(saved.unsigned_abs(), DECIMAL))
    };
    let mut text = format!("{verb}: {sizes} ({change})");
    if r.skipped > 0 {
        text.push_str(&format!(
            "; {} left as they were (no saving, or not supported here)",
            r.skipped
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_names_the_saving() {
        let r = keel_vfs::compress::Report {
            files: 3,
            skipped: 1,
            before: 3_000_000,
            after: 1_000_000,
            ..Default::default()
        };
        let s = summary(Do::Compress(Algorithm::Lzx), &r);
        assert!(s.starts_with("Compressed 3 files with LZX"), "{s}");
        assert!(s.contains("saved 2 MB"), "{s}");
        assert!(s.contains("1 left as they were"), "{s}");
        let back = keel_vfs::compress::Report {
            files: 1,
            before: 1_000_000,
            after: 2_000_000,
            ..Default::default()
        };
        assert!(summary(Do::Uncompress, &back).contains("1 MB more"));
    }
}
