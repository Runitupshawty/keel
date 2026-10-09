// Release builds on Windows open no console window (debug builds keep it for logs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod clipboard;
mod clouds;
mod crash;
mod dialogs;
mod icon_theme;
mod icons;
mod jobs;
mod jump;
mod keys;
mod palette;
mod pane;
mod platform;
mod preview_panel;
mod profiles;
mod remotes;
mod search_tab;
mod session;
mod settings;
mod sidebar;
mod sidebar_remotes;
mod state;
mod tab;
mod term_pane;
mod theme;
mod toast;
mod view_details;
mod view_grid;
mod worker;
// --- Task 24 ---
mod cli;
mod dragout;
mod hotkey;
mod index_ui;
mod single_instance;
// --- end Task 24 ---

use keel_vfs::VPath;
use session::Session;
use settings::Settings;
use std::path::Path;

fn main() -> eframe::Result<()> {
    // --- Task 24 ---
    let cli = cli::Cli::from_env();
    if let Some(name) = &cli.profile {
        profiles::set_current(name);
    }
    let request = cli.request();
    // --- end Task 24 ---
    tracing_subscriber::fmt()
        .with_env_filter(format!("info,{}", keel_vfs::cloud::LOG_FILTER_HINT))
        .init();
    crash::install_panic_hook();
    // --- Task 24 ---
    // `keel --index-service <dir>`: the elevated indexer, no window.
    if let Some(dir) = &cli.index_service {
        #[cfg(windows)]
        let result = keel_search::run_index_service(dir);
        #[cfg(not(windows))]
        let result: anyhow::Result<()> = Err(anyhow::anyhow!(
            "--index-service is Windows only ({})",
            dir.display()
        ));
        std::process::exit(match result {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("index service: {e:#}");
                1
            }
        });
    }
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
    // Startup reads (before the window exists): settings, last session. Both are small
    // local files; saved folders are not checked here (a dead share would block).
    let (settings, settings_notice) = Settings::load();
    // --- Task 24 ---
    // A running Keel takes the request (FOLDER / --search) and this process exits.
    let server = if settings.single_instance {
        let name = single_instance::name(&cli::profile());
        match single_instance::claim(&name, &request, !cli.new_window) {
            single_instance::Claim::Handed => return Ok(()),
            single_instance::Claim::Server(listener) => Some(listener),
            single_instance::Claim::Alone => None,
        }
    } else {
        None
    };
    // --- end Task 24 ---
    let (saved, session_notice) = Session::load();
    let mut session = saved
        .clone()
        .unwrap_or_else(|| Session::single(home.clone()));
    session.repair(&home);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([640.0, 400.0])
        .with_title("Keel")
        // Wayland: matches assets/keel.desktop (StartupWMClass=keel) for the icon.
        .with_app_id("keel");
    match eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/keel.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => tracing::warn!("window icon: {e}"),
    }
    let opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let boot = app::Boot {
        settings,
        session,
        notices: settings_notice.into_iter().chain(session_notice).collect(),
        home,
        saved: Some(saved),
        // --- Task 24 ---
        request,
        server,
    };
    eframe::run_native(
        "Keel",
        opts,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, boot)))),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn window_icon_decodes() {
        let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/keel.png"))
            .expect("assets/keel.png is a PNG");
        assert_eq!((icon.width, icon.height), (256, 256));
    }
}
