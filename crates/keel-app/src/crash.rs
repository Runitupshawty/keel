//! Crash log (`<cache dir>/crash.log`) and the "Keel hit a bug" dialog shown when a
//! frame panics (the app catches it in `App::update` and keeps running).

use std::io::Write;
use std::path::{Path, PathBuf};

pub fn log_path() -> Option<PathBuf> {
    crate::settings::cache_dir().map(|d| d.join("crash.log"))
}

/// Appends every panic (UI or worker thread) to the crash log, then runs the default hook.
pub fn install_panic_hook() {
    install_hook(log_path());
}

fn install_hook(path: Option<PathBuf>) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = &path {
            if let Err(e) = append(path, info) {
                eprintln!("could not write {}: {e}", path.display());
            }
        }
        default(info);
    }));
}

fn append(path: &Path, info: &std::panic::PanicHookInfo) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let thread = std::thread::current();
    writeln!(
        f,
        "=== {} Keel {} panic on thread '{}'\n{info}\n{}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z"),
        env!("CARGO_PKG_VERSION"),
        thread.name().unwrap_or("unnamed"),
        std::backtrace::Backtrace::force_capture(),
    )
}

/// The modal shown after a frame panicked. `open` goes false when dismissed.
pub fn modal(ctx: &egui::Context, open: &mut bool) {
    let path = log_path().map_or_else(|| "crash.log".to_owned(), |p| p.display().to_string());
    let r = egui::Modal::new(egui::Id::new("keel-crash")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.heading("Keel hit a bug");
        ui.label("Details in crash.log. The tab that failed was closed; the rest keeps working.");
        ui.add_space(4.0);
        ui.monospace(&path);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .button("Copy")
                .on_hover_text("Copy the log path")
                .clicked()
            {
                ui.ctx().copy_text(path.clone());
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
    #[test]
    fn hook_writes_the_crash_log() {
        let path = std::env::temp_dir().join(format!("keel-crash-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        super::install_hook(Some(path.clone()));
        let joined = std::thread::Builder::new()
            .name("crash-test".into())
            .spawn(|| panic!("crash hook test"))
            .unwrap()
            .join();
        // Back to the default hook for the other tests.
        let _ = std::panic::take_hook();
        assert!(joined.is_err());
        let log = std::fs::read_to_string(&path).expect("crash.log written");
        assert!(log.contains("crash hook test"), "{log}");
        assert!(log.contains("thread 'crash-test'"), "{log}");
        let _ = std::fs::remove_file(&path);
    }
}
