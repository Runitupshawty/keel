//! User settings (`<config dir>/profiles/default/config.toml`), the Settings window, and the
//! persistence worker that writes settings and the session off the UI thread.

use crate::session::Session;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

/// Writes wait this long after the last change.
pub const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Built-in themes offered in the Settings window.
pub const THEMES: &[&str] = &["dark", "light"];

const MB: u64 = 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub theme: String,
    pub icon_theme: String,
    pub show_hidden: bool,
    pub dual: bool,
    pub preview_open: bool,
    pub sidebar_width: f32,
    pub preview_width: f32,
    /// Files larger than this are not previewed (keel-preview's own 64 MB cap still applies).
    pub max_preview_mb: u64,
    pub terminal_shell: String,
    pub terminal_follow_cwd: bool,
    pub terminal_height: f32,
    /// Grid thumbnails for files on remote hosts (each one is a download).
    pub remote_thumbnails: bool,
    /// `[[remotes]]`: SFTP hosts. Passwords and passphrases live in the OS keychain only.
    pub remotes: Vec<keel_vfs::RemoteHost>,
    /// `[[clouds]]`: cloud accounts, non-secret fields only. Tokens and keys live in the OS
    /// keychain only.
    pub clouds: Vec<keel_vfs::CloudAccount>,
    // --- Task 24 ---
    /// Global hotkey that brings Keel forward ("" = none), e.g. "Ctrl+Alt+K".
    pub hotkey: String,
    /// A second `keel` hands its folder / search to the running one and exits.
    pub single_instance: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            icon_theme: "default".into(),
            show_hidden: false,
            dual: true,
            preview_open: false,
            sidebar_width: 210.0,
            preview_width: 380.0,
            max_preview_mb: keel_preview::MAX_PREVIEW_BYTES / MB,
            terminal_shell: String::new(),
            terminal_follow_cwd: true,
            terminal_height: 220.0,
            remote_thumbnails: false,
            remotes: Vec::new(),
            clouds: Vec::new(),
            hotkey: "Ctrl+Alt+K".into(),
            single_instance: true,
        }
    }
}

/// `KEEL_CONFIG_DIR`, else `%APPDATA%\Keel`, `~/Library/Application Support/Keel`,
/// `~/.config/keel` (spec 2.9).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KEEL_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(dir.into());
    }
    let base = directories::BaseDirs::new()?;
    Some(base.config_dir().join(app_dir()))
}

/// Where session.json and crash.log live: `KEEL_CONFIG_DIR` too when set (one folder for a
/// portable or test setup), else `%LOCALAPPDATA%\Keel`, `~/Library/Caches/Keel`,
/// `~/.cache/keel` (spec 2.9). eframe keeps its own window state (`app.ron`) in its
/// storage folder; `KEEL_CONFIG_DIR` does not move that.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KEEL_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(dir.into());
    }
    let base = directories::BaseDirs::new()?;
    Some(base.cache_dir().join(app_dir()))
}

fn app_dir() -> &'static str {
    if cfg!(target_os = "linux") {
        "keel"
    } else {
        "Keel"
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("profiles")
            .join(crate::cli::profile()) // Task 24: `--profile`
            .join("config.toml")
    }

    /// Defaults when the file is missing or broken. A broken file is renamed to
    /// `config.toml.bad` first (so saving defaults cannot destroy it) and reported in the
    /// returned notice, for a toast.
    pub fn load() -> (Settings, Option<String>) {
        match read_config(&Self::path(), |t| {
            toml::from_str(t).map_err(|e| e.to_string())
        }) {
            Ok(s) => (s.unwrap_or_default(), None),
            Err(notice) => (Settings::default(), Some(notice)),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write_atomic(&Self::path(), toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn max_preview_bytes(&self) -> u64 {
        self.max_preview_mb
            .clamp(1, keel_preview::MAX_PREVIEW_BYTES / MB)
            * MB
    }
}

/// Reads and parses a config file (a leading UTF-8 BOM is ignored). `Ok(None)` when there
/// is no file. A file that cannot be read or parsed is renamed to `<name>.bad` and the
/// error is a notice for the user.
pub fn read_config<T>(
    path: &Path,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> Result<Option<T>, String> {
    let why = match std::fs::read_to_string(path) {
        Ok(text) => match parse(text.strip_prefix('\u{feff}').unwrap_or(&text)) {
            Ok(value) => return Ok(Some(value)),
            Err(e) => e,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => e.to_string(),
    };
    tracing::warn!("{}: {why}", path.display());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    Err(match std::fs::rename(path, &bad) {
        Ok(()) => format!("{name} could not be read; kept as {name}.bad, using defaults"),
        Err(e) => format!("{name} could not be read ({why}); using defaults: {e}"),
    })
}

/// Writes a per-process temp file next to `path`, flushes it to disk, then renames it over
/// the target, so a crash or power loss mid-write never leaves a truncated file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

enum Save {
    Settings(Settings),
    Session(Session),
}

/// The persistence worker: keeps the newest settings and session and writes them
/// `SAVE_DEBOUNCE` after the last change, and once more when the app exits.
pub struct Persist {
    tx: Option<Sender<Save>>,
    /// The first failed write of the run, for a toast.
    errors: Receiver<String>,
    thread: Option<JoinHandle<()>>,
    last_settings: Settings,
    last_session: Option<Session>,
}

impl Persist {
    /// `settings` and `session` are what is on disk already (nothing to write yet).
    pub fn new(settings: Settings, session: Option<Session>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<Save>();
        let (err_tx, errors) = crossbeam_channel::bounded::<String>(1);
        let thread = std::thread::Builder::new()
            .name("keel-persist".into())
            .spawn(move || {
                let (mut settings, mut session) = (None::<Settings>, None::<Session>);
                let mut reported = false;
                let mut report = |what: &str, e: anyhow::Error| {
                    tracing::error!("save {what}: {e:#}");
                    if !std::mem::replace(&mut reported, true) {
                        let _ = err_tx.try_send(format!("Could not save {what}: {e:#}"));
                    }
                };
                loop {
                    let msg = if settings.is_some() || session.is_some() {
                        rx.recv_timeout(SAVE_DEBOUNCE)
                    } else {
                        rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
                    };
                    match msg {
                        Ok(Save::Settings(s)) => settings = Some(s),
                        Ok(Save::Session(s)) => session = Some(s),
                        Err(why) => {
                            if let Some(Err(e)) = settings.take().map(|s| s.save()) {
                                report("settings", e);
                            }
                            if let Some(Err(e)) = session.take().map(|s| s.save()) {
                                report("the open tabs", e);
                            }
                            if why == RecvTimeoutError::Disconnected {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|e| tracing::error!("spawn keel-persist: {e}"))
            .ok();
        Self {
            tx: Some(tx),
            errors,
            thread,
            last_settings: settings,
            last_session: session,
        }
    }

    /// Queues whatever changed since the last call (cheap: a few fields and paths).
    /// `session` None leaves the saved session alone (after a crash reset).
    pub fn update(&mut self, settings: &Settings, session: Option<Session>) {
        let Some(tx) = &self.tx else { return };
        if *settings != self.last_settings {
            self.last_settings = settings.clone();
            let _ = tx.send(Save::Settings(settings.clone()));
        }
        if let Some(session) = session.filter(|s| self.last_session.as_ref() != Some(s)) {
            let _ = tx.send(Save::Session(session.clone()));
            self.last_session = Some(session);
        }
    }

    /// A failed write, once per run.
    pub fn error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }

    /// On exit: queue the final state, then wait for the worker to write it.
    pub fn finish(&mut self, settings: &Settings, session: Option<Session>) {
        self.update(settings, session);
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Pages of the Settings window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    General,
    Remotes,
    Cloud,
}

/// The Settings window (Ctrl+,): General, Remotes and Cloud pages. Returns true when the
/// theme was changed.
pub fn window(
    ctx: &egui::Context,
    open: &mut bool,
    s: &mut Settings,
    remotes: &mut crate::remotes::Remotes,
    clouds: &mut crate::clouds::Clouds,
    tx: &Sender<crate::state::Msg>,
) -> bool {
    let mut theme_changed = false;
    egui::Window::new("Settings")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut remotes.page, Page::General, "General");
                ui.selectable_value(&mut remotes.page, Page::Remotes, "Remotes");
                ui.selectable_value(&mut remotes.page, Page::Cloud, "Cloud");
            });
            ui.separator();
            if remotes.page != Page::General {
                match remotes.page {
                    Page::Remotes => remotes.settings_page(ui, s, tx),
                    _ => clouds.settings_page(ui, s, tx),
                }
                ui.add_space(4.0);
                ui.weak(format!("Saved to {}", Settings::path().display()));
                return;
            }
            egui::Grid::new("settings-grid")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Theme");
                    egui::ComboBox::from_id_salt("settings-theme")
                        .selected_text(s.theme.as_str())
                        .show_ui(ui, |ui| {
                            for name in THEMES {
                                if ui.selectable_label(s.theme == *name, *name).clicked()
                                    && s.theme != *name
                                {
                                    s.theme = (*name).to_owned();
                                    theme_changed = true;
                                }
                            }
                        });
                    ui.end_row();
                    ui.label("Hidden files");
                    ui.checkbox(&mut s.show_hidden, "Show");
                    ui.end_row();
                    ui.label("Layout");
                    ui.checkbox(&mut s.dual, "Dual pane");
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut s.preview_open, "Preview panel");
                    ui.end_row();
                    ui.label("Max preview size");
                    ui.add(
                        egui::Slider::new(
                            &mut s.max_preview_mb,
                            1..=keel_preview::MAX_PREVIEW_BYTES / MB,
                        )
                        .suffix(" MB"),
                    );
                    ui.end_row();
                    // --- Task 24 ---
                    ui.label("Global hotkey");
                    crate::hotkey::field(ui, &mut s.hotkey);
                    ui.end_row();
                    ui.label("Instances");
                    ui.checkbox(&mut s.single_instance, "Reuse the running window")
                        .on_hover_text("`keel <folder>` opens a tab here (from the next start)");
                    ui.end_row();
                });
            ui.add_space(4.0);
            ui.weak(format!("Saved to {}", Settings::path().display()));
        });
    theme_changed
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn terminal_settings_upgrade_and_round_trip() {
        let defaults: Settings = toml::from_str("theme = 'light'").unwrap();
        assert!(defaults.terminal_follow_cwd);
        assert_eq!(defaults.terminal_height, 220.0);
        let custom = Settings {
            terminal_shell: "cmd.exe".into(),
            terminal_follow_cwd: false,
            terminal_height: 310.0,
            ..defaults
        };
        assert_eq!(
            toml::from_str::<Settings>(&toml::to_string(&custom).unwrap()).unwrap(),
            custom
        );
    }

    #[test]
    fn round_trip_under_keel_config_dir() {
        let dir = std::env::temp_dir().join(format!("keel-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KEEL_CONFIG_DIR", &dir);
        assert_eq!(
            Settings::path(),
            dir.join("profiles").join("default").join("config.toml")
        );
        assert_eq!(
            Settings::load(),
            (Settings::default(), None),
            "missing file = defaults"
        );
        let s = Settings {
            theme: "light".into(),
            show_hidden: true,
            dual: false,
            preview_open: true,
            sidebar_width: 250.0,
            max_preview_mb: 8,
            ..Settings::default()
        };
        s.save().unwrap();
        assert_eq!(Settings::load(), (s, None));
        // Missing keys take defaults, unknown keys are ignored, a BOM is skipped.
        let path = Settings::path();
        std::fs::write(&path, "\u{feff}theme = \"light\"\nfuture_key = 1\n").unwrap();
        let (loaded, notice) = Settings::load();
        assert_eq!((loaded.theme.as_str(), notice), ("light", None));
        assert!(loaded.dual);
        // A broken file is set aside before anything can overwrite it.
        std::fs::write(&path, "theme = [").unwrap();
        let (loaded, notice) = Settings::load();
        assert_eq!(loaded, Settings::default());
        assert!(notice.unwrap().contains("config.toml.bad"));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(path.with_file_name("config.toml.bad")).unwrap(),
            "theme = ["
        );
        // session.json and crash.log follow KEEL_CONFIG_DIR.
        assert_eq!(super::cache_dir(), Some(dir.clone()));
        std::env::remove_var("KEEL_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
