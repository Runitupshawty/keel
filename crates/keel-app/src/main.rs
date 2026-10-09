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

use keel_vfs::VPath;
use session::Session;
use settings::Settings;
use std::path::{Path, PathBuf};

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(format!("info,{}", keel_vfs::cloud::LOG_FILTER_HINT))
        .init();
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
    // `keel [folder] [--profile NAME]`.
    let (profile, folder) = profiles::parse_args(std::env::args_os().skip(1));
    let profile_notice = match profile {
        Some(Ok(name)) => {
            profiles::set_current(&name);
            None
        }
        Some(Err(why)) => Some(why),
        None => None,
    };
    // Startup reads (before the window exists): settings, last session. Both are small
    // local files; saved folders are not checked here (a dead share would block).
    let (settings, settings_notice) = Settings::load();
    let (saved, session_notice) = Session::load();
    let mut session = saved
        .clone()
        .unwrap_or_else(|| Session::single(home.clone()));
    // `keel [folder]` opens the folder in a new tab of the left pane.
    if let Some(dir) = folder
        .map(PathBuf::from)
        .and_then(|p| std::path::absolute(p).ok())
    {
        session.panes.resize_with(2, Vec::new);
        session.panes[0].push(VPath::local(dir));
        session.active_tab[0] = session.panes[0].len() - 1;
        session.active = 0;
    }
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
        notices: profile_notice
            .into_iter()
            .chain(settings_notice)
            .chain(session_notice)
            .collect(),
        home,
        saved: Some(saved),
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
