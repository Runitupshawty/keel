//! Settings profiles (spec 2.6): `<config dir>/profiles/<name>/config.toml`, each with its
//! own session (`session_path`). `keel --profile <name>` picks one at start; Settings →
//! Profiles and the palette switch in place. Every disk access runs on a worker.
//! Named sessions (saved tabs and views, palette "Save session as…" / "Open session: …")
//! are the profile's too: `sessions/<name>.json` next to its session.json.

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

pub const NAME_RULE: &str = "use letters, digits, '.', '_' or '-' (not first; at most 64)";

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
        && !name.starts_with('-')
        && !device
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
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

/// The named sessions of profile `name`, next to its session.json.
pub fn sessions_dir(name: &str) -> Option<PathBuf> {
    Some(session_path(name)?.with_file_name("sessions"))
}

/// The current profile's session `name` (named like a profile: it is a file name).
pub fn named_session_path(name: &str) -> Result<PathBuf> {
    ensure!(
        valid_name(name),
        "\"{name}\": {}",
        crate::i18n::tr("profiles.name_rule")
    );
    let dir = sessions_dir(&current()).context("no cache folder")?;
    Ok(dir.join(format!("{name}.json")))
}

/// The current profile's named sessions, sorted.
pub fn sessions() -> Vec<String> {
    let dir = sessions_dir(&current());
    let mut names: Vec<String> = dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".json").map(str::to_owned))
        .filter(|n| valid_name(n))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// The palette's named-session commands.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionCmd {
    /// Ask for a name, then `Save`.
    SaveAs,
    /// Save the open tabs and views as `name` (replacing a session of that name).
    Save(String),
    /// Open its tabs and views instead of the open ones.
    Open(String),
    /// Ask, then `DeleteNow`.
    Delete(String),
    DeleteNow(String),
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
    // The session and named sessions, when they are not inside the profile folder
    // (KEEL_CONFIG_DIR puts them there).
    let pairs = [
        (session_path(from), session_path(to)),
        (sessions_dir(from), sessions_dir(to)),
    ];
    for (a, b) in pairs {
        if let (Some(a), Some(b)) = (a, b) {
            if a.exists() {
                std::fs::create_dir_all(b.parent().expect("session file has a folder"))?;
                std::fs::rename(&a, &b).context("move the session")?;
            }
        }
    }
    if let Some(a) = session_path(from).filter(|_| from != DEFAULT) {
        let _ = std::fs::remove_dir(a.parent().expect("session file has a folder"));
    }
    Ok(())
}

/// Sends a profile that is not in use, and its sessions, to the OS trash.
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
    // The default profile's named sessions are beside its session.json, not in a folder.
    let named = sessions_dir(name).filter(|_| name == DEFAULT);
    for path in std::iter::once(root().join(name))
        .chain(session)
        .chain(named)
    {
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
        themes: Themes::load(),
        settings,
        session,
        notices: a.into_iter().chain(b).collect(),
    }
}

pub enum Done {
    /// The profiles, and the current profile's named sessions.
    Listed(Vec<String>, Vec<String>),
    /// A named session read from disk, to open.
    Session(String, Box<Session>),
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
        self.run(|| Ok(Done::Listed(list(), sessions())));
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
                            if ui
                                .add_enabled(ok, egui::Button::new(crate::i18n::tr("menu.rename")))
                                .clicked()
                            {
                                ask = Some(Ask::Rename(name.clone(), to.trim().to_owned()));
                            }
                            if ui.button(crate::i18n::tr("dialogs.cancel")).clicked() {
                                ask = Some(Ask::Rename(String::new(), String::new()));
                            }
                        });
                    } else if self.confirm_delete.as_ref() == Some(name) {
                        ui.label(
                            crate::i18n::tr("profiles.delete_confirm").replace("{name}", name),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .button(crate::i18n::tr("profiles.move_to_trash"))
                                .on_hover_text(crate::i18n::tr("profiles.trash_tip"))
                                .clicked()
                            {
                                ask = Some(Ask::Delete(name.clone()));
                            }
                            if ui.button(crate::i18n::tr("dialogs.cancel")).clicked() {
                                ask = Some(Ask::Delete(String::new()));
                            }
                        });
                    } else {
                        match mine {
                            true => ui.strong(
                                crate::i18n::tr("profiles.current").replace("{name}", name),
                            ),
                            false => ui.label(name),
                        };
                        ui.add_enabled_ui(!self.busy && !mine, |ui| {
                            ui.horizontal(|ui| {
                                if ui.button(crate::i18n::tr("profiles.switch")).clicked() {
                                    ask = Some(Ask::Switch(name.clone()));
                                }
                                if ui.button(crate::i18n::tr("menu.rename")).clicked() {
                                    self.renaming = Some((name.clone(), name.clone()));
                                }
                                if ui.button(crate::i18n::tr("menu.delete")).clicked() {
                                    self.confirm_delete = Some(name.clone());
                                }
                            })
                            .response
                            .on_disabled_hover_text(crate::i18n::tr("profiles.switch_first"));
                        });
                    }
                    ui.end_row();
                }
            });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_name)
                    .hint_text(crate::i18n::tr("profiles.new_name_hint"))
                    .desired_width(160.0),
            );
            let name = self.new_name.trim();
            let ok = !self.busy && valid_name(name);
            if ui
                .add_enabled(ok, egui::Button::new(crate::i18n::tr("profiles.new")))
                .on_hover_text(crate::i18n::tr("profiles.new_tip"))
                .clicked()
            {
                ask = Some(Ask::New(name.to_owned()));
            }
        });
        if !self.new_name.trim().is_empty() && !valid_name(self.new_name.trim()) {
            ui.weak(crate::i18n::tr("profiles.name_rule"));
        }
        ui.add_space(4.0);
        ui.weak(crate::i18n::tr("profiles.start_hint"));
        match ask {
            None => {}
            Some(Ask::Switch(name)) => self.switch(&name),
            Some(Ask::Rename(from, to)) => {
                self.renaming = None;
                if !from.is_empty() {
                    let text = crate::i18n::tr("profiles.renamed")
                        .replace("{from}", &from)
                        .replace("{to}", &to);
                    self.run(move || rename(&from, &to).map(|()| Done::Changed(text)));
                }
            }
            Some(Ask::Delete(name)) => {
                self.confirm_delete = None;
                if !name.is_empty() {
                    let text = crate::i18n::tr("profiles.trashed").replace("{name}", &name);
                    self.run(move || delete(&name).map(|()| Done::Changed(text)));
                }
            }
            Some(Ask::New(name)) => {
                self.new_name.clear();
                let settings = s.clone();
                let text = crate::i18n::tr("profiles.created").replace("{name}", &name);
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
                Done::Listed(names, sessions) => {
                    let current = current();
                    self.palette.profiles =
                        names.iter().filter(|n| **n != current).cloned().collect();
                    self.palette.sessions = sessions;
                    self.profiles.names = names;
                }
                Done::Session(name, mut session) => {
                    session.repair(&self.home);
                    self.apply_session(*session);
                    self.toasts
                        .info(crate::i18n::tr("profiles.session_opened").replace("{name}", &name));
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
        // The single instance answers under the new profile's name from now on; a profile
        // another Keel already runs is not opened twice.
        if let Some(server) = &mut self.instance {
            if let Err(e) = server.rebind(&crate::single_instance::name(&name)) {
                tracing::info!("single instance for the new profile: {e}");
                self.toasts
                    .error(crate::i18n::tr("profiles.open_elsewhere").replace("{name}", &name));
                return;
            }
        }
        set_current(&name);
        self.find.history.follow(&name); // Task 76
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
        // Task 23: its stash too, else the old one would be saved into its session.json.
        self.dropzone.set_items(std::mem::take(&mut session.stash));
        self.apply_session(session);
        // Undo is per window and profile: the old profile's operations are not offered.
        self.undo.clear();
        self.jobs.undo_generation = self.undo.generation();
        for notice in notices {
            self.toasts.error(notice);
        }
        self.toasts
            .info(crate::i18n::tr("profiles.profile_now").replace("{name}", &name));
    }
}

impl AppState {
    /// Opens the tabs of `session` (repaired) instead of the open ones, with its views and
    /// column chains (listed by the next tick); else the old ones would be saved into the
    /// session.json (Task 23). The drop zone's stash stays.
    pub fn apply_session(&mut self, session: Session) {
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
        crate::view_columns::restore_session(&mut self.panes, &session.views, &session.columns);
        self.active = if self.dual { session.active } else { 0 };
    }

    /// The palette's named sessions; the disk is read and written on the profile worker.
    pub fn session_cmd(&mut self, cmd: SessionCmd) {
        let name = match &cmd {
            SessionCmd::SaveAs => {
                self.dialog = Some(crate::dialogs::Dialog::SessionName {
                    text: String::new(),
                    taken: self.palette.sessions.clone(),
                    focus: true,
                });
                return;
            }
            SessionCmd::Save(n)
            | SessionCmd::Open(n)
            | SessionCmd::Delete(n)
            | SessionCmd::DeleteNow(n) => n.clone(),
        };
        // The path is the current profile's now, also when a switch follows.
        let path = match named_session_path(&name) {
            Ok(path) => path,
            Err(e) => return self.toasts.error(format!("{e:#}")),
        };
        match cmd {
            SessionCmd::SaveAs => {}
            SessionCmd::Save(_) => {
                let session = Session::of_tabs(self);
                let text = crate::i18n::tr("profiles.session_saved").replace("{name}", &name);
                self.profiles.run(move || {
                    session.save_to(&path)?;
                    Ok(Done::Changed(text))
                });
            }
            SessionCmd::Open(_) => {
                let missing = crate::i18n::tr("profiles.no_session").replace("{name}", &name);
                self.profiles.run(move || match Session::load_from(&path) {
                    (Some(session), _) => Ok(Done::Session(name, Box::new(session))),
                    (None, notice) => Err(anyhow::anyhow!(notice.unwrap_or(missing))),
                });
            }
            SessionCmd::Delete(_) => {
                self.dialog = Some(crate::dialogs::Dialog::Confirm {
                    text: crate::i18n::tr("profiles.delete_session").replace("{name}", &name),
                    yes: crate::i18n::tr("profiles.move_to_trash"),
                    on_yes: crate::keys::Action::Session(SessionCmd::DeleteNow(name)),
                });
            }
            SessionCmd::DeleteNow(_) => {
                let text = crate::i18n::tr("profiles.session_trashed").replace("{name}", &name);
                self.profiles.run(move || {
                    keel_vfs::LocalProvider.remove(&VPath::local(&path))?;
                    Ok(Done::Changed(text))
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["default", "work", "my.profile-2_x", "Home"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "../x",
            "x.",
            "con",
            "NUL.txt",
            "com1",
            "a b",
            "é",
            "-x",
            "--profile",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name("console") && valid_name("com"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn create_rename_delete_switch_round_trip() {
        let _env = crate::settings::TEST_ENV.lock();
        let _restore = crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR"]);
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
        // Views, column chains and the stash are the profile's too.
        let tabs = Session {
            panes: vec![
                vec![VPath::local(&dir)],
                vec![VPath::local(dir.join("profiles"))],
            ],
            active: 1,
            active_tab: [0, 0],
            views: [
                crate::pane::ViewMode::Columns,
                crate::pane::ViewMode::Details,
            ],
            columns: vec![vec![vec![VPath::local(dir.join("profiles"))]], vec![vec![]]],
            stash: vec![VPath::local(dir.join("stashed.txt"))],
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
        state
            .dropzone
            .missing
            .insert(VPath::local(dir.join("stashed.txt")));
        state.apply_profile(load(DEFAULT));
        assert_eq!(current(), DEFAULT);
        assert_eq!(state.settings, mine, "default saved the same settings");
        assert_eq!(state.panes[0].view, crate::pane::ViewMode::Details);
        assert!(state.panes[0].tabs[0].columns.cols.is_empty());
        assert!(state.dropzone.items().is_empty() && state.dropzone.missing.is_empty());
        delete("job").unwrap();
        assert_eq!(list(), ["default"]);
        assert!(!dir.join("profiles").join("job").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs the profile worker's answers until `done` holds (5 s at most).
    fn tick_until(state: &mut AppState, done: impl Fn(&AppState) -> bool) -> bool {
        for _ in 0..500 {
            state.profiles_tick();
            if done(state) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn named_sessions_save_open_delete_and_follow_the_profile() {
        let _env = crate::settings::TEST_ENV.lock();
        let _restore = crate::settings::EnvRestore::save(&["KEEL_CONFIG_DIR"]);
        let dir = std::env::temp_dir().join(format!("keel-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::env::set_var("KEEL_CONFIG_DIR", &dir);
        set_current(DEFAULT);
        let (home, a) = (VPath::local(&dir), VPath::local(dir.join("a")));
        let mut state = AppState::new(
            egui::Context::default(),
            std::sync::Arc::new(keel_vfs::Router::new()),
            home.clone(),
        );
        assert!(sessions().is_empty());
        assert!(named_session_path("../x").is_err(), "a file name");

        // Save: the open tabs and views, next to the profile's session.json.
        state.panes[0].tabs.push(Tab::new(a.clone()));
        state.panes[0].active = 1;
        state.panes[1].view = crate::pane::ViewMode::Grid;
        let saved = Session::of_tabs(&state);
        state.session_cmd(SessionCmd::Save("photos".into()));
        assert!(tick_until(&mut state, |s| s.palette.sessions == ["photos"]));
        let file = dir.join("sessions").join("photos.json");
        assert_eq!(named_session_path("photos").unwrap(), file);
        assert_eq!(Session::load_from(&file).0, Some(saved.clone()));

        // Open: its tabs replace the open ones; the drop zone's stash stays.
        state.dropzone.set_items(vec![a.clone()]);
        state.apply_session(Session::single(home.clone()));
        state.session_cmd(SessionCmd::Open("photos".into()));
        assert!(tick_until(&mut state, |s| s.panes[0].tabs.len() == 2));
        assert_eq!(Session::of_tabs(&state), saved);
        assert_eq!(state.dropzone.items(), std::slice::from_ref(&a));
        state.session_cmd(SessionCmd::Open("gone".into()));
        assert!(tick_until(&mut state, |s| !s.toasts.list.is_empty()));

        // Renaming the default profile takes its named sessions along; deleting trashes them.
        create("work", &Settings::default()).unwrap();
        state.apply_profile(load("work"));
        state.profiles.relist(); // as `Done::Loaded` does
        assert!(tick_until(&mut state, |s| s.palette.sessions.is_empty()));
        rename(DEFAULT, "old").unwrap();
        assert!(!file.exists());
        set_current("old");
        assert_eq!(sessions(), ["photos"]);
        set_current("work");

        // Delete asks first, then trashes the file.
        state.apply_profile(load("old"));
        state.session_cmd(SessionCmd::Delete("photos".into()));
        assert!(matches!(
            &state.dialog,
            Some(crate::dialogs::Dialog::Confirm { on_yes, .. })
                if *on_yes == crate::keys::Action::Session(SessionCmd::DeleteNow("photos".into()))
        ));
        state.session_cmd(SessionCmd::DeleteNow("photos".into()));
        assert!(tick_until(&mut state, |s| !sessions()
            .contains(&"photos".into())
            && s.palette.sessions.is_empty()));

        set_current(DEFAULT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
