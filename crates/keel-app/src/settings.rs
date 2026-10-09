//! User settings (`<config dir>/profiles/default/config.toml`), the Settings window, and the
//! persistence worker that writes settings and the session off the UI thread.

use crate::session::Session;
use crossbeam_channel::{RecvTimeoutError, Sender};
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

/// `%LOCALAPPDATA%\Keel`, `~/Library/Caches/Keel`, `~/.cache/keel` (spec 2.9).
pub fn cache_dir() -> Option<PathBuf> {
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
            .join("default")
            .join("config.toml")
    }

    /// Defaults when the file is missing or broken (a broken file is logged and only
    /// overwritten after the next change).
    pub fn load() -> Settings {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| tracing::warn!("{}: {e}", path.display()))
                .unwrap_or_default(),
            Err(_) => Settings::default(),
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

/// Writes a sibling temp file, then renames it over the target, so a crash mid-write
/// never leaves a truncated file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

enum Save {
    Settings(Settings),
    Session(Session),
}

/// The persistence worker: keeps the newest settings and session and writes them
/// `SAVE_DEBOUNCE` after the last change, and once more when the app exits.
pub struct Persist {
    tx: Option<Sender<Save>>,
    thread: Option<JoinHandle<()>>,
    last_settings: Settings,
    last_session: Option<Session>,
}

impl Persist {
    /// `settings` and `session` are what is on disk already (nothing to write yet).
    pub fn new(settings: Settings, session: Option<Session>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<Save>();
        let thread = std::thread::Builder::new()
            .name("keel-persist".into())
            .spawn(move || {
                let (mut settings, mut session) = (None::<Settings>, None::<Session>);
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
                                tracing::error!("save settings: {e:#}");
                            }
                            if let Some(Err(e)) = session.take().map(|s| s.save()) {
                                tracing::error!("save session: {e:#}");
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

    /// On exit: queue the final state, then wait for the worker to write it.
    pub fn finish(&mut self, settings: &Settings, session: Option<Session>) {
        self.update(settings, session);
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The Settings window (Ctrl+,). Returns true when the theme was changed.
pub fn window(ctx: &egui::Context, open: &mut bool, s: &mut Settings) -> bool {
    let mut theme_changed = false;
    egui::Window::new("Settings")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
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
            Settings::default(),
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
        assert_eq!(Settings::load(), s);
        // Missing keys take defaults and unknown keys are ignored.
        std::fs::write(Settings::path(), "theme = \"light\"\nfuture_key = 1\n").unwrap();
        let loaded = Settings::load();
        assert_eq!(loaded.theme, "light");
        assert!(loaded.dual);
        std::env::remove_var("KEEL_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
