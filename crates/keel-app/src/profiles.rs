//! Settings profiles (spec 2.6): `<config dir>/profiles/<name>/config.toml`, each with its
//! own session (`session_path`). `keel --profile <name>` picks one at start; Settings →
//! Profiles and the palette switch in place. Every disk access runs on a worker.

use crate::session::Session;
use crate::settings::{cache_dir, config_dir, Settings};
use crate::state::AppState;
use crate::tab::Tab;
use crate::theme::Themes;
use anyhow::{ensure, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use keel_vfs::{Provider, VPath};
use std::path::PathBuf;
use std::sync::RwLock;

pub const DEFAULT: &str = "default";

/// The profile this process uses; empty = `DEFAULT`.
static CURRENT: RwLock<String> = RwLock::new(String::new());

pub fn current() -> String {
    let name = CURRENT.read().unwrap_or_else(|e| e.into_inner());
    match name.is_empty() {
        true => DEFAULT.to_owned(),
        false => name.clone(),
    }
}

pub fn set_current(name: &str) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = name.to_owned();
}

pub const NAME_RULE: &str = "use letters, digits, '.', '_' or '-' (at most 64)";

/// Profile names become folder names: ASCII letters, digits, `.`, `_`, `-`; not `.`/`..`,
/// no trailing dot and no Windows device name (`con`, `nul`, `com1`, …).
pub fn valid_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    let device = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.as_bytes()[3].is_ascii_digit());
    !name.is_empty()
        && name.len() <= 64
        && !name.ends_with('.')
        && !device
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `--profile NAME` / `--profile=NAME` and the first other argument (the folder to open).
/// `Err` when the name breaks `NAME_RULE`.
pub fn parse_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> (Option<Result<String, String>>, Option<std::ffi::OsString>) {
    let (mut profile, mut folder) = (None, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = match arg.to_str() {
            Some("--profile") => args.next(),
            Some(a) if a.starts_with("--profile=") => Some(a["--profile=".len()..].into()),
            _ => {
                folder = folder.or(Some(arg));
                continue;
            }
        };
        let name = value.and_then(|v| v.into_string().ok()).unwrap_or_default();
        profile = Some(match valid_name(&name) {
            true => Ok(name),
            false => Err(format!("--profile \"{name}\": {NAME_RULE}")),
        });
    }
    (profile, folder)
}

fn root() -> PathBuf {
    config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("profiles")
}

pub fn config_path(name: &str) -> PathBuf {
    root().join(name).join("config.toml")
}

/// `<cache dir>/session.json` for the default profile (where it always was), else
/// `<cache dir>/profiles/<name>/session.json`.
pub fn session_path(name: &str) -> Option<PathBuf> {
    let cache = cache_dir()?;
    Some(match name {
        DEFAULT => cache.join("session.json"),
        _ => cache.join("profiles").join(name).join("session.json"),
    })
}

/// The profiles on disk, plus the default and the current one, sorted.
pub fn list() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n))
        .collect();
    names.extend([DEFAULT.to_owned(), current()]);
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

fn check_new(name: &str) -> Result<()> {
    ensure!(valid_name(name), "\"{name}\": {NAME_RULE}");
    ensure!(
        !root().join(name).exists() && name != DEFAULT,
        "a profile \"{name}\" already exists"
    );
    Ok(())
}

/// A new profile `name` holding `settings` (a copy of the current profile's).
pub fn create(name: &str, settings: &Settings) -> Result<()> {
    check_new(name)?;
    settings.save_to(&config_path(name))
}

/// Renames a profile that is not in use, with its session.
pub fn rename(from: &str, to: &str) -> Result<()> {
    ensure!(
        from != current(),
        "switch to another profile before renaming \"{from}\""
    );
    check_new(to)?;
    let old = root().join(from);
    if old.exists() {
        std::fs::rename(&old, root().join(to)).with_context(|| format!("rename {from}"))?;
    }
    // The session, when it is not inside the profile folder (KEEL_CONFIG_DIR puts it there).
    if let (Some(a), Some(b)) = (session_path(from), session_path(to)) {
        if a.exists() {
            std::fs::create_dir_all(b.parent().expect("session file has a folder"))?;
            std::fs::rename(&a, &b).context("move the session")?;
            if from != DEFAULT {
                let _ = std::fs::remove_dir(a.parent().expect("session file has a folder"));
            }
        }
    }
    Ok(())
}

/// Sends a profile that is not in use, and its session, to the OS trash.
pub fn delete(name: &str) -> Result<()> {
    ensure!(
        name != current(),
        "switch to another profile before deleting \"{name}\""
    );
    ensure!(valid_name(name), "\"{name}\": {NAME_RULE}");
    let session = session_path(name).map(|s| match name {
        DEFAULT => s,
        _ => s.parent().expect("session file has a folder").to_owned(),
    });
    for path in std::iter::once(root().join(name)).chain(session) {
        if path.exists() {
            keel_vfs::LocalProvider.remove(&VPath::local(&path))?;
        }
    }
    Ok(())
}

/// A profile read from disk, ready to apply (`AppState::apply_profile`).
pub struct Loaded {
    pub name: String,
    pub settings: Settings,
    pub session: Option<Session>,
    pub themes: Themes,
    pub notices: Vec<String>,
}

pub fn load(name: &str) -> Loaded {
    let (settings, a) = Settings::load_from(&config_path(name));
    let (session, b) = session_path(name).map_or((None, None), |p| Session::load_from(&p));
    Loaded {
        name: name.to_owned(),
        themes: Themes::load(&settings.theme),
        settings,
        session,
        notices: a.into_iter().chain(b).collect(),
    }
}

pub enum Done {
    Listed(Vec<String>),
    Loaded(Box<Loaded>),
    /// A change finished: an info toast, then the list is read again.
    Changed(String),
    Failed(String),
}

/// What a click on the Profiles page asks for.
enum Ask {
    Switch(String),
    Rename(String, String),
    Delete(String),
    New(String),
}

/// Settings → Profiles: the list and its worker.
pub struct Profiles {
    pub names: Vec<String>,
    new_name: String,
    renaming: Option<(String, String)>,
    confirm_delete: Option<String>,
    /// A worker is running; buttons wait.
    busy: bool,
    tx: Sender<Done>,
    pub rx: Receiver<Done>,
    ctx: egui::Context,
}

impl Profiles {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut me = Self {
            names: vec![current()],
            new_name: String::new(),
            renaming: None,
            confirm_delete: None,
            busy: false,
            tx,
            rx,
            ctx,
        };
        me.relist();
        me
    }

    fn run(&mut self, job: impl FnOnce() -> Result<Done> + Send + 'static) {
        self.busy = true;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("keel-profiles".into())
            .spawn(move || {
                let done = job().unwrap_or_else(|e| Done::Failed(format!("{e:#}")));
                let _ = tx.send(done);
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            self.busy = false;
            tracing::error!("spawn keel-profiles: {e}");
        }
    }

    pub fn relist(&mut self) {
        self.run(|| Ok(Done::Listed(list())));
    }

    /// Reads profile `name` on a worker; `AppState::profiles_tick` applies it.
    pub fn switch(&mut self, name: &str) {
        if name == current() || !valid_name(name) {
            return;
        }
        let name = name.to_owned();
        self.run(move || Ok(Done::Loaded(Box::new(load(&name)))));
    }

    pub fn settings_page(&mut self, ui: &mut egui::Ui, s: &Settings) {
        let current = current();
        let mut ask = None;
        egui::Grid::new("profiles-grid")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                for name in &self.names {
                    let mine = *name == current;
                    if let Some((_, to)) = self.renaming.as_mut().filter(|(f, _)| f == name) {
                        ui.add(egui::TextEdit::singleline(to).desired_width(160.0));
                        ui.horizontal(|ui| {
                            let ok = valid_name(to.trim());
                            if ui.add_enabled(ok, egui::Button::new("Rename")).clicked() {
                                ask = Some(Ask::Rename(name.clone(), to.trim().to_owned()));
                            }
                            if ui.button("Cancel").clicked() {
                                ask = Some(Ask::Rename(String::new(), String::new()));
                            }
                        });
                    } else if self.confirm_delete.as_ref() == Some(name) {
                        ui.label(format!("Delete \"{name}\"?"));
                        ui.horizontal(|ui| {
                            if ui
                                .button("Move to trash")
                                .on_hover_text("Its settings and saved tabs go to the trash")
                                .clicked()
                            {
                                ask = Some(Ask::Delete(name.clone()));
                            }
                            if ui.button("Cancel").clicked() {
                                ask = Some(Ask::Delete(String::new()));
                            }
                        });
                    } else {
                        match mine {
                            true => ui.strong(format!("{name}  (current)")),
                            false => ui.label(name),
                        };
                        ui.add_enabled_ui(!self.busy && !mine, |ui| {
                            ui.horizontal(|ui| {
                                if ui.button("Switch").clicked() {
                                    ask = Some(Ask::Switch(name.clone()));
                                }
                                if ui.button("Rename").clicked() {
                                    self.renaming = Some((name.clone(), name.clone()));
                                }
                                if ui.button("Delete").clicked() {
                                    self.confirm_delete = Some(name.clone());
                                }
                            })
                            .response
                            .on_disabled_hover_text("Switch to another profile first");
                        });
                    }
                    ui.end_row();
                }
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_name)
                    .hint_text("new profile name")
                    .desired_width(160.0),
            );
            let name = self.new_name.trim();
            let ok = !self.busy && valid_name(name);
            if ui
                .add_enabled(ok, egui::Button::new("New"))
                .on_hover_text("A copy of the current profile's settings")
                .clicked()
            {
                ask = Some(Ask::New(name.to_owned()));
            }
        });
        if !self.new_name.trim().is_empty() && !valid_name(self.new_name.trim()) {
            ui.weak(NAME_RULE);
        }
        ui.add_space(4.0);
        ui.weak("keel --profile <name> starts with a profile");
        match ask {
            None => {}
            Some(Ask::Switch(name)) => self.switch(&name),
            Some(Ask::Rename(from, to)) => {
                self.renaming = None;
                if !from.is_empty() {
                    let text = format!("Profile \"{from}\" renamed to \"{to}\"");
                    self.run(move || rename(&from, &to).map(|()| Done::Changed(text)));
                }
            }
            Some(Ask::Delete(name)) => {
                self.confirm_delete = None;
                if !name.is_empty() {
                    let text = format!("Profile \"{name}\" moved to the trash");
                    self.run(move || delete(&name).map(|()| Done::Changed(text)));
                }
            }
            Some(Ask::New(name)) => {
                self.new_name.clear();
                let settings = s.clone();
                let text = format!("Profile \"{name}\" created");
                self.run(move || create(&name, &settings).map(|()| Done::Changed(text)));
            }
        }
    }
}

impl AppState {
    /// Per frame: answers from the profile worker.
    pub fn profiles_tick(&mut self) {
        while let Ok(done) = self.profiles.rx.try_recv() {
            self.profiles.busy = false;
            match done {
                Done::Listed(names) => {
                    let current = current();
                    self.palette.profiles =
                        names.iter().filter(|n| **n != current).cloned().collect();
                    self.profiles.names = names;
                }
                Done::Loaded(loaded) => {
                    self.apply_profile(*loaded);
                    self.profiles.relist();
                }
                Done::Changed(text) => {
                    self.toasts.info(text);
                    self.profiles.relist();
                }
                Done::Failed(text) => self.toasts.error(text),
            }
        }
    }

    /// Switches to `loaded` in place: its settings, theme, remotes and clouds, and its tabs
    /// instead of the open ones. The old profile's last state was queued for saving (to its
    /// own files) by the frame before.
    pub fn apply_profile(&mut self, loaded: Loaded) {
        let Loaded {
            name,
            settings,
            session,
            themes,
            notices,
        } = loaded;
        set_current(&name);
        let mut session = session.unwrap_or_else(|| Session::single(self.home.clone()));
        session.repair(&self.home);
        self.remotes.sync(&self.router, &settings.remotes);
        self.clouds.sync(&self.router, &settings.clouds);
        self.dual = settings.dual;
        self.show_hidden = settings.show_hidden;
        self.preview.reset();
        self.preview.open = settings.preview_open;
        self.preview.max_bytes = settings.max_preview_bytes();
        self.jobs.one_per_drive = settings.one_transfer_per_drive;
        self.themes = themes;
        self.theme = self.themes.get(&settings.theme);
        self.theme.apply(&self.ctx);
        self.settings = settings;
        for (p, (dirs, active)) in session
            .panes
            .into_iter()
            .zip(session.active_tab)
            .enumerate()
        {
            self.panes[p].tabs = dirs.into_iter().map(Tab::new).collect();
            self.panes[p].active = active;
            for t in 0..self.panes[p].tabs.len() {
                self.list(p, t);
            }
        }
        self.active = if self.dual { session.active } else { 0 };
        for notice in notices {
            self.toasts.error(notice);
        }
        self.toasts.info(format!("Profile: {name}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_arguments() {
        for ok in ["default", "work", "my.profile-2_x", "Home"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "", ".", "..", "a/b", "a\\b", "../x", "x.", "con", "NUL.txt", "com1", "a b", "é",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name("console") && valid_name("com"));
        assert!(!valid_name(&"x".repeat(65)));
        let args = |a: &[&str]| parse_args(a.iter().map(std::ffi::OsString::from));
        assert_eq!(args(&[]), (None, None));
        assert_eq!(
            args(&["D:\\x", "--profile", "work"]),
            (Some(Ok("work".into())), Some("D:\\x".into()))
        );
        assert_eq!(args(&["--profile=job"]).0, Some(Ok("job".into())));
        assert!(args(&["--profile", "../etc"]).0.unwrap().is_err());
        assert!(args(&["--profile"]).0.unwrap().is_err());
    }

    #[test]
    fn create_rename_delete_switch_round_trip() {
        let _env = crate::settings::TEST_ENV.lock();
        let dir = std::env::temp_dir().join(format!("keel-profiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KEEL_CONFIG_DIR", &dir);
        set_current(DEFAULT);

        let mine = Settings {
            theme: "light".into(),
            icon_theme: "PKief.material-icon-theme".into(),
            ..Settings::default()
        };
        mine.save_to(&Settings::path()).unwrap();
        assert_eq!(list(), ["default"]);
        create("work", &mine).unwrap();
        assert!(create("work", &mine).is_err(), "exists");
        assert!(create("default", &mine).is_err(), "exists");
        assert!(create("bad/name", &mine).is_err(), "invalid");
        assert_eq!(list(), ["default", "work"]);
        assert_eq!(Settings::load_from(&config_path("work")).0, mine);

        // Each profile has its own session file.
        let work_session = session_path("work").unwrap();
        assert_eq!(
            work_session,
            dir.join("profiles").join("work").join("session.json")
        );
        assert_eq!(session_path(DEFAULT).unwrap(), dir.join("session.json"));
        let tabs = Session {
            panes: vec![
                vec![VPath::local(&dir)],
                vec![VPath::local(dir.join("profiles"))],
            ],
            active: 1,
            active_tab: [0, 0],
        };
        tabs.save_to(&work_session).unwrap();

        assert!(rename(DEFAULT, "x").is_err(), "in use");
        assert!(rename("work", "a/b").is_err(), "invalid");
        rename("work", "job").unwrap();
        assert_eq!(list(), ["default", "job"]);
        assert!(!dir.join("profiles").join("work").exists());

        // Switch in place: settings, paths and tabs follow the profile.
        let start = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(
            egui::Context::default(),
            std::sync::Arc::new(keel_vfs::Router::new()),
            start,
        );
        state.apply_profile(load("job"));
        assert_eq!(current(), "job");
        assert_eq!(Settings::path(), config_path("job"));
        assert_eq!(Session::path(), session_path("job"));
        assert_eq!(state.settings, mine);
        assert!(!state.theme.dark, "light theme applied");
        assert_eq!(
            state.panes[1].tabs[0].dir,
            VPath::local(dir.join("profiles"))
        );
        assert_eq!(state.active, 1);
        assert_eq!(Session::of(&state), tabs);
        assert!(delete("job").is_err(), "in use");

        // Back to the default profile: the other one can go.
        state.apply_profile(load(DEFAULT));
        assert_eq!(current(), DEFAULT);
        assert_eq!(state.settings, mine, "default saved the same settings");
        delete("job").unwrap();
        assert_eq!(list(), ["default"]);
        assert!(!dir.join("profiles").join("job").exists());

        std::env::remove_var("KEEL_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
