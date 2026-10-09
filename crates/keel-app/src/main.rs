mod app;
mod clipboard;
mod dialogs;
mod icons;
mod jobs;
mod keys;
mod pane;
mod platform;
mod preview_panel;
mod sidebar;
mod state;
mod tab;
mod theme;
mod toast;
mod view_details;
mod view_grid;
mod worker;

use keel_vfs::VPath;
use std::path::{Path, PathBuf};

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    // pdfium sits next to the executable (copied there by build.rs).
    if let Some(dir) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
    {
        keel_preview::init_pdfium(dir);
    }
    // `keel [folder]`; default: the user's home folder.
    let start = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_owned()))
        .and_then(|p| std::path::absolute(p).ok())
        .or_else(|| std::env::current_dir().ok())
        .map(VPath::local)
        .unwrap_or_else(|| VPath::local("/"));
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("Keel"),
        ..Default::default()
    };
    eframe::run_native(
        "Keel",
        opts,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, start)))),
    )
}
