mod app;
mod clipboard;
mod crash;
mod dialogs;
mod icons;
mod jobs;
mod jump;
mod keys;
mod palette;
mod pane;
mod platform;
mod preview_panel;
mod search_tab;
mod session;
mod settings;
mod sidebar;
mod state;
mod tab;
mod theme;
mod toast;
mod view_details;
mod view_grid;
mod worker;

use keel_vfs::VPath;
use session::Session;
use settings::Settings;
use std::path::{Path, PathBuf};

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    crash::install_panic_hook();
    // pdfium sits next to the executable (copied there by build.rs).
    if let Some(dir) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
    {
        keel_preview::init_pdfium(dir);
    }
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_owned())
        .or_else(|| std::env::current_dir().ok())
        .map(VPath::local)
        .unwrap_or_else(|| VPath::local("/"));
    // Startup reads (before the window exists): settings, last session.
    let settings = Settings::load();
    let saved = Session::load();
    let mut session = saved
        .clone()
        .unwrap_or_else(|| Session::single(home.clone()));
    // `keel [folder]` opens the folder in a new tab of the left pane.
    if let Some(dir) = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .and_then(|p| std::path::absolute(p).ok())
    {
        session.panes.resize_with(2, Vec::new);
        session.panes[0].push(VPath::local(dir));
        session.active_tab[0] = session.panes[0].len() - 1;
        session.active = 0;
    }
    let missing = session.repair(&home);
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([640.0, 400.0])
        .with_title("Keel");
    let opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let boot = app::Boot {
        settings,
        session,
        missing,
        home,
        saved: Some(saved),
    };
    eframe::run_native(
        "Keel",
        opts,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, boot)))),
    )
}
