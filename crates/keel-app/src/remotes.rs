//! SFTP remotes in the app: hosts from `[[remotes]]` in config.toml registered with the
//! Router, connection status and host-key prompts from the providers, the host editor
//! (Settings → Remotes), and the remote-only actions (connect, disconnect, ssh in the
//! terminal, name search, download progress). No IO here runs on the UI thread.

use crate::settings::Settings;
use crate::state::{AppState, Msg};
use crate::tab::Tab;
use crate::worker::{send, spawn};
use crossbeam_channel::{Receiver, Sender};
use egui::{Id, Modal};
use keel_vfs::{ConnStatus, Kind, Provider, RemoteAuth, RemoteEvent, RemoteHost, Router};
use keel_vfs::{SftpProvider, VPath};
use parking_lot::RwLock;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Downloads larger than this show a progress toast.
pub const BIG_DOWNLOAD: u64 = 8 * 1024 * 1024;
/// A remote Ctrl+F looks at no more entries than this.
pub const SEARCH_LIMIT: usize = 20_000;
/// A failed host's visible tabs try to list again this often.
pub const RETRY: Duration = Duration::from_secs(10);
/// The provider waits this long for a host-key answer, then gives up.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Live SFTP providers by host id, shared with the preview and launch workers (they need
/// `local_copy_with_progress`, which the Router's `dyn Provider` does not offer).
pub type SftpMap = Arc<RwLock<HashMap<String, Arc<SftpProvider>>>>;

/// Sidebar right-click commands on a host (`Add` opens an empty editor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteCmd {
    Connect,
    Disconnect,
    Edit,
    Terminal,
    CopyAddress,
    Add,
}

pub struct HostKeyPrompt {
    pub host_id: String,
    pub fingerprint: String,
    reply: Sender<bool>,
    since: Instant,
}

pub struct Remotes {
    /// The hosts the providers were built from (the last synced `settings.remotes`).
    hosts: Vec<RemoteHost>,
    pub sftp: SftpMap,
    pub status: HashMap<String, (ConnStatus, String)>,
    events: Sender<RemoteEvent>,
    pub prompts: VecDeque<HostKeyPrompt>,
    pub editor: Option<Editor>,
    /// The Settings window shows the Remotes page.
    pub page: bool,
    confirm_remove: Option<String>,
    retry_at: HashMap<String, Instant>,
    /// The newest remote search; older walks stop early.
    pub search_gen: Arc<AtomicU64>,
}

impl Remotes {
    /// Provider events are forwarded into the app's message channel with a repaint, so a
    /// host-key prompt shows at once.
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        let (events, rx) = crossbeam_channel::unbounded::<RemoteEvent>();
        spawn("keel-remote-events", move || {
            for event in rx {
                send(&tx, &ctx, Msg::Remote(event));
            }
        });
        Self {
            hosts: Vec::new(),
            sftp: Arc::default(),
            status: HashMap::new(),
            events,
            prompts: VecDeque::new(),
            editor: None,
            page: false,
            confirm_remove: None,
            retry_at: HashMap::new(),
            search_gen: Arc::default(),
        }
    }

    /// Registers new or changed hosts with the router and drops removed ones; replaced
    /// sessions are closed on a worker. Cheap when nothing changed (every frame).
    pub fn sync(&mut self, router: &Router, hosts: &[RemoteHost]) {
        if self.hosts == hosts {
            return;
        }
        let mut old = Vec::new();
        let mut map = self.sftp.write();
        for h in hosts {
            if self.hosts.contains(h) && map.contains_key(&h.id) {
                continue;
            }
            let provider = Arc::new(SftpProvider::new(h.clone(), self.events.clone()));
            router.register_remote_provider(h.id.clone(), provider.clone());
            old.extend(map.insert(h.id.clone(), provider));
            self.status
                .insert(h.id.clone(), (ConnStatus::Disconnected, String::new()));
        }
        map.retain(|id, provider| {
            let keep = hosts.iter().any(|h| h.id == *id);
            if !keep {
                router.unregister_remote(id);
                old.push(provider.clone());
            }
            keep
        });
        drop(map);
        self.status
            .retain(|id, _| hosts.iter().any(|h| h.id == *id));
        self.hosts = hosts.to_vec();
        if !old.is_empty() {
            spawn("keel-disconnect", move || {
                old.iter().for_each(|p| p.disconnect())
            });
        }
    }

    pub fn host(&self, id: &str) -> Option<&RemoteHost> {
        self.hosts.iter().find(|h| h.id == id)
    }

    pub fn label<'a>(&'a self, id: &'a str) -> &'a str {
        self.host(id).map_or(id, |h| h.label.as_str())
    }

    pub fn state(&self, id: &str) -> Option<ConnStatus> {
        self.status.get(id).map(|s| s.0)
    }

    /// The line above a remote tab's listing: connecting / reconnecting, or the remote
    /// search note. None for local tabs and connected hosts.
    pub fn banner(&self, tab: &Tab) -> Option<String> {
        if tab.dir.scheme != "sftp" {
            return None;
        }
        let id = tab.dir.authority.as_str();
        let label = self.label(id);
        if tab.is_search() {
            return Some(format!(
                "Remote search: names under {} on {label} (first {SEARCH_LIMIT} entries)",
                tab.dir.path
            ));
        }
        Some(match self.state(id)? {
            ConnStatus::Connected => return None,
            ConnStatus::Connecting => format!("Connecting to {label}…"),
            ConnStatus::Failed => format!("Reconnecting to {label}… (showing the last listing)"),
            ConnStatus::Disconnected if tab.loading => format!("Connecting to {label}…"),
            ConnStatus::Disconnected => format!("Disconnected from {label}; F5 reconnects"),
        })
    }

    /// Answers the oldest host-key prompt (the provider's worker is waiting on it).
    pub fn answer(&mut self, trust: bool) {
        if let Some(p) = self.prompts.pop_front() {
            let _ = p.reply.send(trust);
        }
    }

    /// Host-key prompt and host editor, every frame. The editor saves into
    /// `settings.remotes` (written by the persistence worker) and secrets into the keychain.
    pub fn modals(&mut self, ctx: &egui::Context, settings: &mut Settings, tx: &Sender<Msg>) {
        self.host_key_modal(ctx);
        if let Some(editor) = &mut self.editor {
            match editor.ui(ctx, &settings.remotes) {
                EditorOutcome::Open => {}
                EditorOutcome::Cancel => self.editor = None,
                EditorOutcome::Save(host, secret) => {
                    match settings.remotes.iter_mut().find(|h| h.id == host.id) {
                        Some(slot) => *slot = (*host).clone(),
                        None => settings.remotes.push((*host).clone()),
                    }
                    if let Some((kind, value)) = secret {
                        store_secret(host.id.clone(), kind, value, tx.clone(), ctx.clone());
                    }
                    self.editor = None;
                }
            }
        }
    }

    fn host_key_modal(&mut self, ctx: &egui::Context) {
        // The provider gave up after PROMPT_TIMEOUT; a late answer would go nowhere.
        self.prompts.retain(|p| p.since.elapsed() < PROMPT_TIMEOUT);
        let Some(p) = self.prompts.front() else {
            return;
        };
        let label = self.label(&p.host_id).to_owned();
        let mut answer = None;
        let modal = Modal::new(Id::new("keel-host-key")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.strong(format!("First connection to {label}"));
            ui.add_space(4.0);
            ui.label(
                "This host's key is not in ~/.ssh/known_hosts yet. Check that the fingerprint \
                 matches the server's before trusting it:",
            );
            ui.add_space(4.0);
            ui.monospace(&p.fingerprint);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Trust and connect").clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        // Esc or a click outside is a Cancel: never trust by accident.
        if modal.should_close() {
            answer.get_or_insert(false);
        }
        match answer {
            Some(trust) => self.answer(trust),
            None => ctx.request_repaint_after(Duration::from_secs(1)),
        }
    }

    /// Settings → Remotes: the host list with Add / Edit / Remove.
    pub fn settings_page(&mut self, ui: &mut egui::Ui, s: &mut Settings, tx: &Sender<Msg>) {
        ui.checkbox(&mut s.remote_thumbnails, "Thumbnails for remote files")
            .on_hover_text("Grid view then downloads every image and video it shows");
        ui.add_space(6.0);
        if s.remotes.is_empty() {
            ui.weak("No remote hosts yet.");
        }
        let mut remove = None;
        egui::Grid::new("settings-remotes")
            .num_columns(4)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for h in &s.remotes {
                    ui.strong(&h.label);
                    ui.label(format!("{}@{}:{}", h.user, h.host, h.port));
                    ui.weak(auth_name(&h.auth));
                    ui.horizontal(|ui| {
                        if ui.button("Edit…").clicked() {
                            self.editor = Some(Editor::edit(h));
                        }
                        let confirming = self.confirm_remove.as_deref() == Some(h.id.as_str());
                        let text = if confirming {
                            "Really remove?"
                        } else {
                            "Remove"
                        };
                        if ui.button(text).clicked() {
                            if confirming {
                                remove = Some(h.id.clone());
                            } else {
                                self.confirm_remove = Some(h.id.clone());
                            }
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some(id) = remove {
            s.remotes.retain(|h| h.id != id);
            self.confirm_remove = None;
            let (tx, ctx) = (tx.clone(), ui.ctx().clone());
            spawn("keel-keychain", move || {
                keel_vfs::sftp::auth::forget_secrets(&id);
                send(&tx, &ctx, Msg::Info("Remote host removed".into()));
            });
        }
        ui.add_space(6.0);
        if ui.button("Add host…").clicked() {
            self.editor = Some(Editor::default());
        }
    }
}

fn auth_name(a: &RemoteAuth) -> &'static str {
    match a {
        RemoteAuth::Agent => "SSH agent",
        RemoteAuth::KeyFile { .. } => "Key file",
        RemoteAuth::PasswordInKeyring => "Password (keychain)",
    }
}

/// Writes a password or passphrase to the OS keychain on a worker; never to config.
fn store_secret(
    host_id: String,
    kind: &'static str,
    value: String,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    spawn("keel-keychain", move || {
        let msg = match keel_vfs::sftp::auth::store_secret(&host_id, kind, &value) {
            Ok(()) => Msg::Info(format!("The {kind} is saved in the OS keychain")),
            Err(e) => Msg::Toast(format!("Could not save the {kind}: {e:#}")),
        };
        send(&tx, &ctx, msg);
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Agent,
    KeyFile,
    Password,
}

/// The Add / Edit host dialog. `secret` (password or key passphrase) only ever goes to the
/// keychain.
pub struct Editor {
    /// None: a new host (the id is made from the label on save).
    pub id: Option<String>,
    pub label: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub auth: AuthKind,
    /// Empty = the default keys in ~/.ssh.
    pub key_path: String,
    pub passphrase_in_keyring: bool,
    pub secret: String,
    pub home: String,
    pub bookmarks: Vec<(String, String)>,
    pub error: Option<String>,
    picker: Option<Receiver<Option<PathBuf>>>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            id: None,
            label: String::new(),
            host: String::new(),
            port: "22".into(),
            user: String::new(),
            auth: AuthKind::KeyFile,
            key_path: String::new(),
            passphrase_in_keyring: false,
            secret: String::new(),
            home: String::new(),
            bookmarks: Vec::new(),
            error: None,
            picker: None,
        }
    }
}

/// A password or passphrase for the keychain: (`"password"`/`"passphrase"`, value).
pub type Secret = Option<(&'static str, String)>;

pub enum EditorOutcome {
    Open,
    Cancel,
    Save(Box<RemoteHost>, Secret),
}

/// Host names and IPs (IPv6 in or out of brackets); no spaces or shell characters, since the
/// terminal's `ssh` line is built from them.
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && !h.starts_with('-')
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_:[]%".contains(c))
}

fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && !u.starts_with('-')
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// A stable id slug from the label, unique among `taken`.
pub fn slug(label: &str, taken: &[RemoteHost]) -> String {
    let mut base: String = label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    base = base
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if base.is_empty() {
        base = "host".into();
    }
    let free = |id: &str| taken.iter().all(|h| h.id != id);
    if free(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| free(id))
        .expect("an unused suffix")
}

impl Editor {
    pub fn edit(h: &RemoteHost) -> Self {
        let (auth, key_path, passphrase_in_keyring) = match &h.auth {
            RemoteAuth::Agent => (AuthKind::Agent, String::new(), false),
            RemoteAuth::KeyFile {
                path,
                passphrase_in_keyring,
            } => (
                AuthKind::KeyFile,
                path.display().to_string(),
                *passphrase_in_keyring,
            ),
            RemoteAuth::PasswordInKeyring => (AuthKind::Password, String::new(), false),
        };
        Self {
            id: Some(h.id.clone()),
            label: h.label.clone(),
            host: h.host.clone(),
            port: h.port.to_string(),
            user: h.user.clone(),
            auth,
            key_path,
            passphrase_in_keyring,
            home: h.home.clone().unwrap_or_default(),
            bookmarks: h.bookmarks.clone(),
            ..Self::default()
        }
    }

    /// The host to save and the secret for the keychain (`"password"`/`"passphrase"`,
    /// value), or why the form is not complete.
    pub fn build(&self, existing: &[RemoteHost]) -> Result<(RemoteHost, Secret), String> {
        let label = self.label.trim();
        let host = self.host.trim();
        let user = self.user.trim();
        if label.is_empty() {
            return Err("Enter a label".into());
        }
        if !valid_host(host) {
            return Err("Enter a host name or IP address (no spaces)".into());
        }
        let port: u16 = match self.port.trim().parse() {
            Ok(p) if p > 0 => p,
            _ => return Err("The port must be 1-65535".into()),
        };
        if !valid_user(user) {
            return Err("Enter a user name (letters, digits, . _ -)".into());
        }
        let home = self.home.trim();
        if !home.is_empty() && !home.starts_with('/') {
            return Err("The initial folder must start with /".into());
        }
        let mut bookmarks = Vec::new();
        for (name, path) in &self.bookmarks {
            let (name, path) = (name.trim(), path.trim());
            if name.is_empty() && path.is_empty() {
                continue;
            }
            if !path.starts_with('/') {
                return Err(format!("Bookmark \"{name}\": the path must start with /"));
            }
            let name = if name.is_empty() { path } else { name };
            bookmarks.push((name.to_owned(), path.to_owned()));
        }
        let new = self.id.is_none();
        let secret = (!self.secret.is_empty()).then(|| self.secret.clone());
        let (auth, secret) = match self.auth {
            AuthKind::Agent => (RemoteAuth::Agent, None),
            AuthKind::KeyFile => (
                RemoteAuth::KeyFile {
                    path: PathBuf::from(self.key_path.trim()),
                    passphrase_in_keyring: self.passphrase_in_keyring,
                },
                secret
                    .filter(|_| self.passphrase_in_keyring)
                    .map(|s| ("passphrase", s)),
            ),
            AuthKind::Password => {
                if new && secret.is_none() {
                    return Err("Enter the password (it is kept in the OS keychain)".into());
                }
                (
                    RemoteAuth::PasswordInKeyring,
                    secret.map(|s| ("password", s)),
                )
            }
        };
        if new && self.auth == AuthKind::KeyFile && self.passphrase_in_keyring && secret.is_none() {
            return Err("Enter the key passphrase (it is kept in the OS keychain)".into());
        }
        let id = self.id.clone().unwrap_or_else(|| slug(label, existing));
        Ok((
            RemoteHost {
                id,
                label: label.to_owned(),
                host: host.to_owned(),
                port,
                user: user.to_owned(),
                auth,
                home: (!home.is_empty()).then(|| home.to_owned()),
                bookmarks,
            },
            secret,
        ))
    }

    fn ui(&mut self, ctx: &egui::Context, existing: &[RemoteHost]) -> EditorOutcome {
        if let Some(Ok(picked)) = self.picker.as_ref().map(Receiver::try_recv) {
            self.picker = None;
            if let Some(path) = picked {
                self.key_path = path.display().to_string();
            }
        }
        let mut outcome = EditorOutcome::Open;
        let title = if self.id.is_some() {
            "Edit remote host"
        } else {
            "Add remote host"
        };
        let modal = Modal::new(Id::new("keel-remote-editor")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.strong(title);
            ui.add_space(6.0);
            egui::Grid::new("remote-editor")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    let field = |ui: &mut egui::Ui, name: &str, text: &mut String, hint: &str| {
                        ui.label(name);
                        ui.add(
                            egui::TextEdit::singleline(text)
                                .hint_text(hint)
                                .desired_width(300.0),
                        );
                        ui.end_row();
                    };
                    field(ui, "Label", &mut self.label, "Mac mini");
                    field(ui, "Host", &mut self.host, "host name or IP");
                    field(ui, "Port", &mut self.port, "22");
                    field(ui, "User", &mut self.user, "");
                    ui.label("Sign in with");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.auth, AuthKind::KeyFile, "Key file");
                        ui.radio_value(&mut self.auth, AuthKind::Agent, "SSH agent");
                        ui.radio_value(&mut self.auth, AuthKind::Password, "Password");
                    });
                    ui.end_row();
                    match self.auth {
                        AuthKind::KeyFile => {
                            ui.label("Key file");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.key_path)
                                        .hint_text("default keys in ~/.ssh")
                                        .desired_width(220.0),
                                );
                                let busy = self.picker.is_some();
                                if ui
                                    .add_enabled(!busy, egui::Button::new("Browse…"))
                                    .clicked()
                                {
                                    self.picker = Some(pick_key(ctx.clone()));
                                }
                            });
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(
                                &mut self.passphrase_in_keyring,
                                "Key has a passphrase (kept in the OS keychain)",
                            );
                            ui.end_row();
                            if self.passphrase_in_keyring {
                                ui.label("Passphrase");
                                secret_field(ui, &mut self.secret, self.id.is_some());
                                ui.end_row();
                            }
                        }
                        AuthKind::Password => {
                            ui.label("Password");
                            secret_field(ui, &mut self.secret, self.id.is_some());
                            ui.end_row();
                        }
                        AuthKind::Agent => {}
                    }
                    field(ui, "Initial folder", &mut self.home, "/ (optional)");
                });
            ui.add_space(4.0);
            ui.label("Bookmarks");
            let mut drop_row = None;
            for (i, (name, path)) in self.bookmarks.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("label")
                            .desired_width(120.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(path)
                            .hint_text("/remote/path")
                            .desired_width(250.0),
                    );
                    if ui.small_button("✕").clicked() {
                        drop_row = Some(i);
                    }
                });
            }
            if let Some(i) = drop_row {
                self.bookmarks.remove(i);
            }
            if ui.small_button("Add bookmark").clicked() {
                self.bookmarks.push(Default::default());
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    match self.build(existing) {
                        Ok((host, secret)) => outcome = EditorOutcome::Save(Box::new(host), secret),
                        Err(e) => self.error = Some(e),
                    }
                }
                if ui.button("Cancel").clicked() {
                    outcome = EditorOutcome::Cancel;
                }
            });
        });
        if matches!(outcome, EditorOutcome::Open) && modal.should_close() && self.picker.is_none() {
            outcome = EditorOutcome::Cancel;
        }
        outcome
    }
}

fn secret_field(ui: &mut egui::Ui, secret: &mut String, editing: bool) {
    let hint = if editing {
        "leave empty to keep the stored one"
    } else {
        ""
    };
    ui.add(
        egui::TextEdit::singleline(secret)
            .password(true)
            .hint_text(hint)
            .desired_width(300.0),
    );
}

/// The OS file dialog on a worker (it blocks until closed).
fn pick_key(ctx: egui::Context) -> Receiver<Option<PathBuf>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let started = spawn("keel-pick-key", move || {
        let mut dialog = rfd::FileDialog::new().set_title("SSH private key");
        if let Some(ssh) = directories::BaseDirs::new().map(|b| b.home_dir().join(".ssh")) {
            dialog = dialog.set_directory(ssh);
        }
        let _ = tx.send(dialog.pick_file());
        ctx.request_repaint();
    });
    if !started {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = tx.send(None);
        return rx;
    }
    rx
}

/// `sftp://<id><home>`, `/` when the host has no initial folder.
pub fn home_of(h: &RemoteHost) -> VPath {
    remote_path(&h.id, h.home.as_deref().unwrap_or("/"))
}

pub fn remote_path(id: &str, path: &str) -> VPath {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    VPath {
        scheme: "sftp".into(),
        authority: id.into(),
        path,
    }
}

/// `sftp://user@host[:port]` for "Copy address".
pub fn address(h: &RemoteHost) -> String {
    let host = if h.host.contains(':') && !h.host.starts_with('[') {
        format!("[{}]", h.host)
    } else {
        h.host.clone()
    };
    match h.port {
        22 => format!("sftp://{}@{host}", h.user),
        port => format!("sftp://{}@{host}:{port}", h.user),
    }
}

/// The line typed into the terminal pane, or None when the config holds characters that
/// could do more than name a host (it is hand-editable).
pub fn ssh_command(h: &RemoteHost) -> Option<String> {
    let host = h.host.trim_start_matches('[').trim_end_matches(']');
    (valid_host(host) && valid_user(&h.user)).then(|| match h.port {
        22 => format!("ssh {}@{host}", h.user),
        port => format!("ssh -p {port} {}@{host}", h.user),
    })
}

pub fn delete_text(n: usize, label: &str) -> String {
    format!(
        "Delete {} on {label}? This cannot be undone.",
        crate::jobs::items(n)
    )
}

/// Materialises `path` for preview / open-with. Remote files report progress as
/// `Msg::Download` when larger than `BIG_DOWNLOAD`. Blocks: workers only.
pub fn materialise(
    router: &Router,
    sftp: &SftpMap,
    path: &VPath,
    tx: &Sender<Msg>,
    ctx: &egui::Context,
) -> anyhow::Result<PathBuf> {
    let remote = (path.scheme == "sftp")
        .then(|| sftp.read().get(&path.authority).cloned())
        .flatten();
    let Some(provider) = remote else {
        return router
            .provider_for(path)
            .ok_or_else(|| anyhow::anyhow!("no provider for {}", path.display()))?
            .local_copy(path);
    };
    let name = path.name().to_owned();
    let last = Cell::new(None::<Instant>);
    provider.local_copy_with_progress(path, &|p| {
        let done = p.done_items == p.total_items;
        let due = last
            .get()
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(250));
        if p.total_bytes > BIG_DOWNLOAD && (done || due) {
            last.set(Some(Instant::now()));
            send(
                tx,
                ctx,
                Msg::Download {
                    name: name.clone(),
                    done: if done { p.total_bytes } else { p.done_bytes },
                    total: p.total_bytes,
                },
            );
        }
    })
}

/// Breadth-first name search under `root`: case-insensitive substring, at most `limit`
/// entries looked at and `max_hits` returned. Folders that cannot be listed are skipped
/// (except `root`). `stop` ends the walk early (a newer search started).
pub fn search_names(
    provider: &dyn Provider,
    root: &VPath,
    query: &str,
    limit: usize,
    max_hits: usize,
    stop: &dyn Fn() -> bool,
) -> anyhow::Result<Vec<keel_search::Hit>> {
    let needle = query.to_lowercase();
    let mut queue = VecDeque::from([root.clone()]);
    let (mut seen, mut hits) = (0, Vec::new());
    while let Some(dir) = queue.pop_front() {
        if stop() {
            break;
        }
        let entries = match provider.list(&dir) {
            Ok(e) => e,
            Err(e) if dir == *root => return Err(e),
            Err(_) => continue,
        };
        for e in entries {
            seen += 1;
            if e.name.to_lowercase().contains(&needle) && hits.len() < max_hits {
                hits.push(keel_search::Hit {
                    path: e.path.clone(),
                    is_dir: e.kind == Kind::Dir,
                    size: e.size,
                    modified: e.modified,
                });
            }
            if e.kind == Kind::Dir && !e.is_link {
                queue.push_back(e.path);
            }
            if seen >= limit || hits.len() >= max_hits {
                return Ok(hits);
            }
        }
    }
    Ok(hits)
}

pub fn spawn_search(
    router: Arc<Router>,
    root: VPath,
    text: String,
    id: u64,
    newest: Arc<AtomicU64>,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    newest.store(id, Ordering::Relaxed);
    spawn("keel-remote-search", move || {
        let result = router
            .provider_for(&root)
            .ok_or_else(|| anyhow::anyhow!("no provider for {}", root.display()))
            .and_then(|p| {
                search_names(
                    p.as_ref(),
                    &root,
                    &text,
                    SEARCH_LIMIT,
                    crate::search_tab::MAX_HITS as usize,
                    &|| newest.load(Ordering::Relaxed) != id,
                )
            });
        send(&tx, &ctx, Msg::Search { id, result });
    });
}

/// Remote parts of `AppState` (kept here so state.rs only has small hooks).
impl AppState {
    pub fn remote_event(&mut self, event: RemoteEvent) {
        match event {
            RemoteEvent::Status {
                host_id,
                status,
                detail,
            } => {
                let Some(label) = self.remotes.host(&host_id).map(|h| h.label.clone()) else {
                    return;
                };
                let before = self
                    .remotes
                    .status
                    .insert(host_id.clone(), (status, detail.clone()))
                    .map(|s| s.0);
                match status {
                    ConnStatus::Connected => {
                        self.remotes.retry_at.remove(&host_id);
                        self.toasts.info(format!("Connected to {label}"));
                        // Tabs that failed while the host was away list again.
                        for p in 0..2 {
                            for t in 0..self.panes[p].tabs.len() {
                                let tab = &self.panes[p].tabs[t];
                                if tab.dir.scheme == "sftp"
                                    && tab.dir.authority == host_id
                                    && tab.error.is_some()
                                {
                                    self.list(p, t);
                                }
                            }
                        }
                    }
                    ConnStatus::Failed => self.toasts.error(format!("{label}: {detail}")),
                    ConnStatus::Disconnected if before == Some(ConnStatus::Connected) => {
                        self.toasts.info(format!("Disconnected from {label}"))
                    }
                    _ => {}
                }
            }
            RemoteEvent::HostKeyPrompt {
                host_id,
                fingerprint,
                reply,
            } => self.remotes.prompts.push_back(HostKeyPrompt {
                host_id,
                fingerprint,
                reply,
                since: Instant::now(),
            }),
        }
    }

    pub fn download_progress(&mut self, name: String, done: u64, total: u64) {
        let prefix = format!("Downloading {name}");
        let text = if done >= total {
            format!("Downloaded {name}")
        } else {
            let size = humansize::format_size(total, humansize::DECIMAL);
            format!("{prefix}: {}% of {size}", done * 100 / total.max(1))
        };
        self.toasts.replace(&prefix, text);
    }

    /// Per frame: config → router, sidebar rows, thumbnail setting, and retrying the
    /// visible tabs of a failed host every `RETRY`.
    pub fn remote_tick(&mut self) {
        self.remotes.sync(&self.router, &self.settings.remotes);
        self.sidebar.remotes =
            crate::sidebar_remotes::rows(&self.settings.remotes, &self.remotes.status);
        self.thumbs.remote = self.settings.remote_thumbnails;
        let now = Instant::now();
        for p in 0..if self.dual { 2 } else { 1 } {
            let t = self.panes[p].active;
            let tab = &self.panes[p].tabs[t];
            if tab.dir.scheme != "sftp" || tab.is_search() || tab.loading || tab.error.is_none() {
                continue;
            }
            let id = tab.dir.authority.clone();
            if self.remotes.state(&id) != Some(ConnStatus::Failed) {
                continue;
            }
            let at = *self
                .remotes
                .retry_at
                .entry(id.clone())
                .or_insert(now + RETRY);
            if at <= now {
                self.remotes.retry_at.insert(id, now + RETRY);
                self.list(p, t);
            } else {
                self.ctx.request_repaint_after(at - now);
            }
        }
    }

    pub fn remote_cmd(&mut self, id: String, cmd: RemoteCmd) {
        if cmd == RemoteCmd::Add {
            self.remotes.editor = Some(Editor::default());
            return;
        }
        let Some(host) = self.remotes.host(&id).cloned() else {
            return self
                .toasts
                .error("That remote host is no longer configured");
        };
        let provider = self.remotes.sftp.read().get(&id).cloned();
        match cmd {
            RemoteCmd::Connect | RemoteCmd::Disconnect => {
                let Some(provider) = provider else { return };
                // Both block on the network (connect may wait on a host-key prompt).
                spawn("keel-connect", move || {
                    if cmd == RemoteCmd::Connect {
                        let _ = provider.connect();
                    } else {
                        provider.disconnect();
                    }
                });
            }
            RemoteCmd::Edit => self.remotes.editor = Some(Editor::edit(&host)),
            RemoteCmd::Terminal => match ssh_command(&host) {
                Some(line) => {
                    let cwd = self
                        .home
                        .to_local_path()
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_default();
                    self.terminal
                        .type_line(&line, cwd, &self.settings, &self.tx, &self.ctx);
                }
                None => self
                    .toasts
                    .error("The host or user name has characters ssh cannot take"),
            },
            RemoteCmd::CopyAddress => {
                self.ctx.copy_text(address(&host));
                self.toasts.info("Copied address");
            }
            RemoteCmd::Add => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{Jobs, Transfer};
    use crate::keys::Action;
    use keel_vfs::{Caps, Conflict, Entry, LocalProvider};
    use std::io::{Read, Write};

    fn host(id: &str, auth: RemoteAuth) -> RemoteHost {
        RemoteHost {
            id: id.into(),
            label: format!("Box {id}"),
            host: "example.invalid".into(),
            port: 2222,
            user: "alice".into(),
            auth,
            home: Some("/srv/data".into()),
            bookmarks: vec![("Logs".into(), "/var/log".into())],
        }
    }

    #[test]
    fn remotes_round_trip_through_config_without_secrets() {
        let mut editor = Editor {
            label: "Media box".into(),
            host: "example.invalid".into(),
            user: "alice".into(),
            auth: AuthKind::Password,
            secret: "hunter2-very-secret".into(),
            bookmarks: vec![("Films".into(), "/media/films".into())],
            ..Editor::default()
        };
        let (pw_host, secret) = editor.build(&[]).unwrap();
        assert_eq!(pw_host.id, "media-box");
        assert_eq!(secret, Some(("password", "hunter2-very-secret".into())));
        editor.auth = AuthKind::KeyFile;
        editor.passphrase_in_keyring = true;
        editor.label = "Media box".into();
        let (key_host, secret) = editor.build(std::slice::from_ref(&pw_host)).unwrap();
        assert_eq!(key_host.id, "media-box-2", "ids stay unique");
        assert_eq!(secret.unwrap().0, "passphrase");

        let settings = Settings {
            remotes: vec![pw_host, key_host, host("agent", RemoteAuth::Agent)],
            ..Settings::default()
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert!(text.contains("[[remotes]]"), "{text}");
        assert!(!text.contains("hunter2"), "no secret in config.toml");
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back, settings);
        assert!(!back.remote_thumbnails, "remote thumbnails default off");

        // Editing keeps the id; an empty secret keeps the stored one.
        let edit = Editor::edit(&settings.remotes[0]);
        assert_eq!(edit.build(&settings.remotes).unwrap().1, None);
        assert_eq!(
            edit.build(&settings.remotes).unwrap().0,
            settings.remotes[0]
        );
        // Shell characters never reach the terminal line.
        let bad = Editor {
            host: "x; rm -rf ~".into(),
            ..edit
        };
        assert!(bad.build(&[]).is_err());
    }

    #[test]
    fn sidebar_rows_follow_config_and_status() {
        let hosts = vec![
            host("nas", RemoteAuth::Agent),
            RemoteHost {
                home: None,
                bookmarks: vec![],
                ..host("pi", RemoteAuth::PasswordInKeyring)
            },
        ];
        let mut status = HashMap::new();
        status.insert(
            "nas".to_owned(),
            (ConnStatus::Connected, "connected".into()),
        );
        let rows = crate::sidebar_remotes::rows(&hosts, &status);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "Box nas");
        assert_eq!(rows[0].status, ConnStatus::Connected);
        assert_eq!(rows[0].home, VPath::parse("sftp://nas/srv/data").unwrap());
        assert_eq!(
            rows[0].bookmarks,
            [(
                "Logs".to_owned(),
                VPath::parse("sftp://nas/var/log").unwrap()
            )]
        );
        assert_eq!(
            rows[1].home,
            VPath::parse("sftp://pi/").unwrap(),
            "no home = /"
        );
        assert_eq!(rows[1].status, ConnStatus::Disconnected);
        assert_eq!(address(&hosts[0]), "sftp://alice@example.invalid:2222");
        assert_eq!(
            ssh_command(&hosts[0]).as_deref(),
            Some("ssh -p 2222 alice@example.invalid")
        );
    }

    #[test]
    fn host_key_prompt_waits_for_the_answer() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        for (trust, expect) in [(true, true), (false, false)] {
            let (reply, answer) = crossbeam_channel::bounded(1);
            state.apply(Msg::Remote(RemoteEvent::HostKeyPrompt {
                host_id: "nas".into(),
                fingerprint: "SHA256:abc".into(),
                reply,
            }));
            assert_eq!(state.remotes.prompts.len(), 1);
            assert_eq!(state.remotes.prompts[0].fingerprint, "SHA256:abc");
            assert!(
                answer.try_recv().is_err(),
                "nothing sent before the user answers"
            );
            state.remotes.answer(trust);
            assert_eq!(answer.try_recv(), Ok(expect));
            assert!(state.remotes.prompts.is_empty());
        }
        // The provider's events reach the app through its channel too.
        state.settings.remotes = vec![host("nas", RemoteAuth::Agent)];
        state.remote_tick();
        assert_eq!(state.remotes.state("nas"), Some(ConnStatus::Disconnected));
        state
            .remotes
            .events
            .send(RemoteEvent::Status {
                host_id: "nas".into(),
                status: ConnStatus::Failed,
                detail: "TCP connect failed".into(),
            })
            .unwrap();
        let msg = state.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        state.apply(msg);
        assert_eq!(state.remotes.state("nas"), Some(ConnStatus::Failed));
        assert!(state
            .toasts
            .list
            .iter()
            .any(|t| t.text == "Box nas: TCP connect failed"));
        let tab = Tab::new(VPath::parse("sftp://nas/srv").unwrap());
        assert_eq!(
            state.remotes.banner(&tab).as_deref(),
            Some("Reconnecting to Box nas… (showing the last listing)")
        );
    }

    #[test]
    fn remote_delete_asks_with_the_host_label() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        state.settings.remotes = vec![host("nas", RemoteAuth::Agent)];
        state.remote_tick();
        let remote = VPath::parse("sftp://nas/srv").unwrap();
        state.tab_mut(0).dir = remote.clone();
        state.tab_mut(0).set_listing(crate::tab::Listing::new(vec![
            crate::tab::test_entry(&remote, "a.txt", Kind::File, 1),
            crate::tab::test_entry(&remote, "b.txt", Kind::File, 2),
        ]));
        state.run(0, Action::SelectAll);
        state.run(0, Action::Delete);
        match &state.dialog {
            Some(crate::dialogs::Dialog::Confirm { text, on_yes }) => {
                assert_eq!(text, "Delete 2 items on Box nas? This cannot be undone.");
                assert!(matches!(on_yes, Action::DeleteRemote(p) if p.len() == 2));
            }
            _ => panic!("no confirm dialog"),
        }
    }

    /// Serves `sftp://<id>/...` from a temp folder: the local provider standing in for a
    /// remote host, so jobs take `ops::transfer`'s streaming path.
    struct Mirror {
        root: PathBuf,
    }
    impl Mirror {
        fn local(&self, p: &VPath) -> VPath {
            VPath::local(self.root.join(p.path.trim_start_matches('/')))
        }
    }
    impl Provider for Mirror {
        fn scheme(&self) -> &'static str {
            "sftp"
        }
        fn caps(&self) -> Caps {
            LocalProvider.caps()
        }
        fn list(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
            Ok(LocalProvider
                .list(&self.local(dir))?
                .into_iter()
                .map(|e| Entry {
                    path: dir.join(&e.name),
                    ..e
                })
                .collect())
        }
        fn stat(&self, p: &VPath) -> anyhow::Result<Entry> {
            Ok(Entry {
                path: p.clone(),
                ..LocalProvider.stat(&self.local(p))?
            })
        }
        fn read(&self, p: &VPath) -> anyhow::Result<Box<dyn Read + Send>> {
            LocalProvider.read(&self.local(p))
        }
        fn write(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
            LocalProvider.write(&self.local(p))
        }
        fn create_new(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
            LocalProvider.create_new(&self.local(p))
        }
        fn mkdir(&self, p: &VPath) -> anyhow::Result<()> {
            LocalProvider.mkdir(&self.local(p))
        }
        fn rename(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
            LocalProvider.rename(&self.local(from), &self.local(to))
        }
        fn rename_noreplace(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
            LocalProvider.rename_noreplace(&self.local(from), &self.local(to))
        }
        fn rename_replace(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
            LocalProvider.rename_replace(&self.local(from), &self.local(to))
        }
        fn remove_empty_dir(&self, p: &VPath) -> anyhow::Result<()> {
            LocalProvider.remove_empty_dir(&self.local(p))
        }
        fn remove(&self, p: &VPath) -> anyhow::Result<()> {
            let local = self.local(p).to_local_path().unwrap();
            if local.is_dir() {
                std::fs::remove_dir_all(local)?;
            } else {
                std::fs::remove_file(local)?;
            }
            Ok(())
        }
        fn local_copy(&self, p: &VPath) -> anyhow::Result<PathBuf> {
            Ok(self.local(p).to_local_path().unwrap())
        }
    }

    fn run_job(jobs: &mut Jobs, rx: &Receiver<Msg>, id: u64) -> anyhow::Result<()> {
        loop {
            match rx
                .recv_timeout(Duration::from_secs(10))
                .expect("job answers")
            {
                Msg::JobProgress { id: got, p } => jobs.progress(got, p),
                Msg::JobDone { id: got, result } if got == id => return result,
                _ => {}
            }
        }
    }

    #[test]
    fn transfer_jobs_go_local_to_remote_and_back() {
        let root = std::env::temp_dir().join(format!("keel-remote-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["local/src/sub", "remote", "back"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("local/src/a.txt"), "alpha").unwrap();
        std::fs::write(root.join("local/src/sub/b.txt"), "beta").unwrap();
        let router = Router::new();
        router.register_remote_provider(
            "fake".into(),
            Arc::new(Mirror {
                root: root.join("remote"),
            }),
        );
        let router = Arc::new(router);
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut jobs = Jobs::new(egui::Context::default());
        let remote = remote_path("fake", "/");
        let up = Transfer {
            src: vec![VPath::local(root.join("local/src"))],
            dst: remote.clone(),
            mv: false,
            extract: None,
        };
        let id = jobs.start(up, Conflict::Skip, router.clone(), tx.clone());
        run_job(&mut jobs, &rx, id).expect("upload");
        assert_eq!(
            std::fs::read_to_string(root.join("remote/src/sub/b.txt")).unwrap(),
            "beta"
        );
        // Back down, as a move: the remote copy goes once the local one is verified.
        let down = Transfer {
            src: vec![remote.join("src")],
            dst: VPath::local(root.join("back")),
            mv: true,
            extract: None,
        };
        let id = jobs.start(down, Conflict::Skip, router.clone(), tx.clone());
        run_job(&mut jobs, &rx, id).expect("download");
        assert_eq!(
            std::fs::read_to_string(root.join("back/src/a.txt")).unwrap(),
            "alpha"
        );
        assert!(!root.join("remote/src").exists(), "moved off the remote");
        // A remote delete removes for real (no trash on a host).
        std::fs::write(root.join("remote/gone.txt"), "x").unwrap();
        let id = jobs.delete(vec![remote.join("gone.txt")], router, tx);
        run_job(&mut jobs, &rx, id).expect("delete");
        assert!(!root.join("remote/gone.txt").exists());
        assert_eq!(jobs.list.last().unwrap().title, "Deleting 1 item");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_search_walks_names_with_a_bound() {
        let root = std::env::temp_dir().join(format!("keel-remote-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/deep")).unwrap();
        for f in [
            "Report.txt",
            "a/report-2.md",
            "a/deep/REPORT.csv",
            "a/other.txt",
        ] {
            std::fs::write(root.join(f), "").unwrap();
        }
        let mirror = Mirror { root: root.clone() };
        let top = remote_path("fake", "/");
        let hits = search_names(&mirror, &top, "report", 100, 100, &|| false).unwrap();
        assert_eq!(hits.len(), 3);
        let bounded = search_names(&mirror, &top, "report", 2, 100, &|| false).unwrap();
        assert!(bounded.len() <= 2, "stops after the entry limit");
        assert!(search_names(&mirror, &top, "x", 100, 100, &|| true)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
