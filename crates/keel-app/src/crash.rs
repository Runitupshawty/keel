//! Crash log (`<cache dir>/crash.log`) and the "Keel hit a bug" dialog shown when a
//! frame panics (the app catches it in `App::update` and keeps running).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Panics logged in full per run; the next one leaves a single "suppressed" line.
const MAX_LOGGED: usize = 5;
/// At startup a larger crash.log is cut to its newest MiB.
const MAX_LOG_BYTES: usize = 1024 * 1024;

/// Serialises appends: entries from panicking threads never interleave.
static WRITE: Mutex<()> = Mutex::new(());
/// The newest entry (for the dialog's Copy button; no file read on the UI thread).
static LAST: Mutex<String> = Mutex::new(String::new());
static PANICS: AtomicUsize = AtomicUsize::new(0);

pub fn log_path() -> Option<PathBuf> {
    crate::settings::cache_dir().map(|d| d.join("crash.log"))
}

/// Trims an oversized crash.log, then appends every panic (UI or worker thread) to it and
/// runs the default hook. Called once from `main` before the window opens.
pub fn install_panic_hook() {
    let path = log_path();
    if let Some(p) = &path {
        if let Err(e) = trim(p, MAX_LOG_BYTES) {
            tracing::warn!("trim {}: {e}", p.display());
        }
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let entry = entry(info);
        *LAST.lock().unwrap_or_else(PoisonError::into_inner) = entry.clone();
        let n = PANICS.fetch_add(1, Ordering::Relaxed);
        let text = match n {
            n if n < MAX_LOGGED => entry,
            MAX_LOGGED => format!("=== {} further panics this run are not logged\n\n", now()),
            _ => return,
        };
        if let Some(path) = &path {
            if let Err(e) = append(path, &text) {
                eprintln!("could not write {}: {e}", path.display());
            }
        }
        default(info);
    }));
}

fn now() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S%.3f %z")
        .to_string()
}

/// One log entry. Release builds are stripped, so the backtrace has no symbols there:
/// the panic location goes on the first line.
fn entry(info: &std::panic::PanicHookInfo) -> String {
    let location = info
        .location()
        .map_or_else(|| "unknown location".to_owned(), ToString::to_string);
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)");
    format!(
        "=== {} Keel {} panic at {location} on thread '{}'\n{message}\n{}\n\n",
        now(),
        env!("CARGO_PKG_VERSION"),
        std::thread::current().name().unwrap_or("unnamed"),
        std::backtrace::Backtrace::force_capture(),
    )
}

/// Appends `text` with one write under a process-wide lock.
fn append(path: &Path, text: &str) -> std::io::Result<()> {
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// Keeps the newest `max` bytes of the log, starting at an entry boundary.
fn trim(path: &Path, max: usize) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > max as u64 => {}
        _ => return Ok(()),
    }
    let bytes = std::fs::read(path)?;
    let tail = &bytes[bytes.len().saturating_sub(max)..];
    let start = tail
        .windows(4)
        .position(|w| w == b"\n===")
        .map_or(0, |i| i + 1);
    crate::settings::write_atomic(path, &tail[start..]).map_err(std::io::Error::other)
}

/// The modal shown after a frame panicked. `open` goes false when dismissed.
pub fn modal(ctx: &egui::Context, open: &mut bool) {
    let path = log_path().map_or_else(|| "crash.log".to_owned(), |p| p.display().to_string());
    let r = egui::Modal::new(egui::Id::new("keel-crash")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.label(format!(
            "Keel hit a bug and reset the panes. Details were written to {path}."
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .button("Copy")
                .on_hover_text("Copy the crash report")
                .clicked()
            {
                let last = LAST.lock().unwrap_or_else(PoisonError::into_inner).clone();
                ui.ctx()
                    .copy_text(if last.is_empty() { path.clone() } else { last });
            }
            if ui.button("OK").clicked() {
                *open = false;
            }
        });
    });
    if r.should_close() {
        *open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("keel-{name}-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn concurrent_appends_never_interleave() {
        let path = tmp("crash-append");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let body = format!("{t:02}{i:02}|").repeat(200);
                        append(&path, &format!("=== start\n{body}\nend\n")).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let log = std::fs::read_to_string(&path).unwrap();
        let entries: Vec<&str> = log.split("=== start\n").skip(1).collect();
        assert_eq!(entries.len(), 400);
        for e in entries {
            let body = e.strip_suffix("\nend\n").expect("whole entry");
            assert_eq!(body, body[..5].repeat(200), "interleaved entry");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn trim_keeps_the_newest_whole_entries() {
        let path = tmp("crash-trim");
        let log: String = (0..100)
            .map(|i| format!("=== entry {i}\nxxxxxxxxxx\n"))
            .collect();
        std::fs::write(&path, &log).unwrap();
        trim(&path, 200).unwrap();
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.len() <= 200);
        assert!(kept.starts_with("=== entry"), "{kept}");
        assert!(log.ends_with(&kept));
        trim(&path, 10_000).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            kept,
            "small log untouched"
        );
        let _ = std::fs::remove_file(&path);
    }
}
