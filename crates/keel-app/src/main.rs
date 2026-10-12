// Release builds on Windows open no console window (debug builds keep it for logs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod a11y;
#[cfg(test)]
mod a11y_tests; // AccessKit tree and keyboard-only use
mod anim;
mod app;
mod backend; // the library in-process or in keel-daemon
mod bulk_rename;
mod chip_row; // Task 57: library search filter chips (keel-web includes it by path)
mod clipboard;
mod clouds;
mod color_theme; // Task 47: VS Code color themes
mod crash;
mod devices;
mod dialogs;
mod dropzone;
mod folder_ops; // Task 60: folder operation dialogs
mod folder_style; // Task 56: folder colors and icons
mod fonts; // also the web client's (keel-web includes it by path)
#[cfg(test)]
mod fuzz_tests;
mod icon_theme;
mod icons;
mod inspector;
mod jobs;
mod jump;
mod keys;
mod library;
mod library_ui;
mod list_view; // Task 66: list view columns, Group by, View Options
mod media;
mod media_viewer;
mod mount_ui;
mod palette;
mod pane;
mod platform;
mod preview_panel;
mod profiles;
mod remotes;
#[cfg(test)]
mod screenshots; // docs/screenshots (ignored: needs a GPU)
mod search_tab;
mod session;
mod settings;
mod shell_menu; // Task 43: More options, the Windows shell menu
mod sidebar;
mod sidebar_remotes;
mod sizes; // Task 46: folder sizes
mod state;
mod tab;
mod term_pane;
mod theme;
mod toast;
mod trash_ui;
mod undo; // Ctrl+Z for file operations
mod update;
mod video_player;
mod view_columns;
mod view_details;
mod view_grid;
mod view_media;
#[cfg(test)]
mod walkthrough_tests; // the QA walkthrough (scripts/walkthrough.sh)
mod worker;
mod wsl; // Task 51: the sidebar's WSL section, `keel` inside WSL
#[cfg(any(target_os = "linux", test))]
mod x11_clipboard;
// --- Task 24 ---
mod cli;
mod commands; // Task 37
mod dragout;
mod hotkey;
mod index_ui;
mod single_instance;
// --- end Task 24 ---

// `crate::chips` for `chip_row` (keel-web has its own copy of the module at that path).
use keel_core::chips;
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
    // pdfium sits next to the executable (copied there by build.rs); a
    // command's library reads PDF text with it too (Task 62).
    if let Some(dir) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
    {
        keel_preview::init_pdfium(dir);
    }
    // --- Task 37 ---
    // `keel <subcommand>`: no window, output on the shell's console.
    if let Some(cmd) = cli.command.clone() {
        #[cfg(windows)]
        if !matches!(cmd, cli::Command::Mcp { .. }) {
            keel_vfs::desktop::attach_parent_console();
        }
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(keel_api::crash::log_filter("warn"))
            .init();
        std::process::exit(commands::run(cmd, &cli::profile(), cli.json));
    }
    // --- end Task 37 ---
    if cli.check_update {
        #[cfg(windows)]
        keel_vfs::desktop::attach_parent_console();
        std::process::exit(update::cli());
    }
    // Task 51: inside WSL, `keel <folder>` opens it in the Windows Keel.
    #[cfg(target_os = "linux")]
    if let Some(code) = wsl::hand_off(&cli) {
        std::process::exit(code);
    }
    tracing_subscriber::fmt()
        .with_env_filter(keel_api::crash::log_filter("info"))
        .with_writer(|| keel_api::crash::tee(std::io::stdout()))
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
    // FOLDER as this instance opens it (a file: its folder, the file selected).
    let request = request.checked().unwrap_or_else(|e| {
        tracing::warn!("{e}");
        cli::Request::default()
    });
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
        searcher: None,
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
