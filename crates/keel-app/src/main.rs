// Release builds on Windows open no console window (debug builds keep it for logs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod a11y;
#[cfg(test)]
mod a11y_tests; // AccessKit tree and keyboard-only use
mod anim;
mod app;
mod backend; // the library in-process or in keel-daemon
mod bookmarks; // Task 76: bookmarks, filters and search history in the window
mod bulk_rename;
mod chip_row; // Task 57: library search filter chips (keel-web includes it by path)
mod chrome; // Task 65: the Finder style's title bar
mod cleanups; // Task 85: user-defined cleanups
mod clipboard;
mod clouds;
mod color_theme; // Task 47: VS Code color themes
mod compress_ui; // Task 85: Compress (NTFS)…
mod crash;
mod design; // Task 64: design tokens, appearance styles
mod devices;
mod dialogs;
mod disk_usage;
mod dropzone;
mod du_tabs; // Task 84: the Disk Usage window's analysis tabs
mod explorer_menu; // Task 85: Search in Keel / Analyze disk usage in Explorer's menu
mod filelist_ui; // Task 77: file list tabs, Export, `keel filelist`
mod finder; // Phase 14 Task 67: Quick Look, path bar, spring-loading, Go to Folder
mod finder_icons; // Task 68: the built-in Keel Finder icon theme
mod folder_ops; // Task 60: folder operation dialogs
mod folder_style; // Task 56: folder colors and icons
mod fonts; // also the web client's (keel-web includes it by path)
#[cfg(test)]
mod fuzz_tests;
mod goto; // Phase 14 Task 67
mod hardlink_dedup; // Task 85: replace duplicates with hard links
mod icon_theme;
mod icons;
mod inspector;
mod jobs;
mod journal_ui; // Phase 15: the index journal tab
mod jump;
mod keymap; // Phase 16 Task 76: configurable keys
mod keys;
mod library;
mod library_ui;
mod list_view; // Task 66: list view columns, Group by, View Options
mod media;
mod media_viewer;
mod mount_ui;
mod palette;
mod pane;
mod path_bar; // Phase 14 Task 67
mod permanent_delete; // Task 85: Delete permanently
mod platform;
mod preview_panel;
mod profiles;
mod quicklook; // Phase 14 Task 67
mod recover_cli; // Phase 17 Task 85: `keel recover`
mod remotes;
#[cfg(test)]
mod screenshots; // docs/screenshots (ignored: needs a GPU)
mod search_filters; // Task 76
mod search_history; // Task 76
mod search_settings;
mod search_tab;
mod session;
mod settings;
mod shell_menu; // Task 43: More options, the Windows shell menu
mod sidebar;
mod sidebar_glyphs; // Phase 14: the sidebar's icons
mod sidebar_prefs; // Phase 14: Favorites and hidden sections per profile
mod sidebar_remotes;
mod sizes; // Task 46: folder sizes
mod spring; // Phase 14 Task 67: spring-loaded folders
mod state;
mod system_cleanup; // Task 85: system cleanups and launchers
mod tab;
/// Task 67: Finder tag colors, dots and macOS native tags.
mod tag_dots;
mod term_pane;
mod theme;
mod toast;
mod toolbar; // Task 65: the Finder style's unified toolbar
mod trash_ui;
mod treemap; // Task 83: treemap, sunburst and flame graph
mod undo; // Ctrl+Z for file operations
mod update;
mod video_player;
mod view_columns;
mod view_details;
mod view_gallery; // Task 66: Gallery view
mod view_grid;
mod view_media;
mod view_prefs; // Task 66: per-folder view memory
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
mod journal_cli; // Phase 15: `keel journal`
mod omissions;
mod quick_search;
mod single_instance;
mod tray;
// --- end Task 24 ---
// --- Task 79 ---
mod es;
mod url_protocol;
// --- end Task 79 ---

// `crate::chips` for `chip_row` (keel-web has its own copy of the module at that path).
use keel_core::chips;
use keel_vfs::VPath;
use session::Session;
use settings::Settings;
use std::path::Path;

fn main() -> eframe::Result<()> {
    // Task 79: `keel.portable` beside the executable keeps everything beside it. First, before
    // any thread starts (it sets the folder variables). A folder other users can change is
    // refused (`notice` says why).
    keel_core::portable::apply(keel_api::private::prepare_portable);
    // --- Task 24 ---
    let cli = cli::Cli::from_env();
    // `keel --run-service --service-user <SID>`: the KeelIndex service process, which
    // only the Service Control Manager starts. Nothing else of Keel runs in it.
    #[cfg(windows)]
    if cli.run_service {
        let sid = cli.service_user.clone().unwrap_or_default();
        std::process::exit(i32::from(keel_search::run_service(&sid).is_err()));
    }
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
    if cli.register_url_protocol || cli.unregister_url_protocol {
        std::process::exit(url_protocol::cli(cli.register_url_protocol));
    }
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
    if let Some(notice) = keel_core::portable::notice() {
        tracing::warn!("{notice}");
    }
    // `keel --install-service` / `--uninstall-service`: through the UAC prompt.
    if cli.install_service || cli.uninstall_service {
        #[cfg(windows)]
        keel_vfs::desktop::attach_parent_console();
        std::process::exit(index_ui::service_cli(
            cli.install_service,
            cli.service_user.as_deref(),
        ));
    }
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
    // Task 85: `keel --system-cleanup <task> --cleanup-report <file>`: the one-shot
    // elevated helper of the system cleanups, no window.
    if let Some(task) = &cli.system_cleanup {
        let report = cli.cleanup_report.as_deref();
        std::process::exit(system_cleanup::helper_main(task, report));
    }
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_owned())
        .or_else(|| std::env::current_dir().ok())
        .map(VPath::local)
        .unwrap_or_else(|| VPath::local("/"));
    // Startup reads (before the window exists): settings, last session. Both are small
    // local files; saved folders are not checked here (a dead share would block).
    let (settings, settings_notice) = Settings::load();
    index_ui::apply_hide_unlistable(settings.index_hide_unlistable);
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
        .with_title("Keel")
        // Wayland: matches assets/keel.desktop (StartupWMClass=keel) for the icon.
        .with_app_id("keel");
    // Task 65: the Finder style's toolbar is the title bar (macOS, Windows).
    viewport = chrome::viewport(viewport, settings.style);
    match eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/keel.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => tracing::warn!("window icon: {e}"),
    }
    let opts = eframe::NativeOptions {
        viewport,
        // With KEEL_CONFIG_DIR (portable mode, tests) the window state lives there too.
        persistence_path: settings::config_dir_override().map(|d| d.join("window")),
        ..Default::default()
    };
    let boot = app::Boot {
        settings,
        session,
        notices: (keel_core::portable::notice().map(str::to_owned).into_iter())
            .chain(settings_notice)
            .chain(session_notice)
            .collect(),
        home,
        saved: Some(saved),
        // --- Task 24 ---
        request,
        server,
        searcher: None,
        native_chrome: true,
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
