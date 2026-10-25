//! SFTP, FTP and FTPS remotes in the app: hosts from `[[remotes]]` in config.toml
//! registered with the Router, connection status, host-key and certificate prompts from the
//! providers, the host editor (Settings → Remotes), and the remote-only actions (connect,
//! disconnect, ssh in the terminal, name search, download progress). No IO here runs on the
//! UI thread.

use crate::settings::Settings;
use crate::state::{AppState, Msg};
use crate::tab::Tab;
use crate::worker::{send, spawn};
use crossbeam_channel::{Receiver, Sender};
use egui::{Id, Modal};
use keel_vfs::{ConnStatus, Kind, Provider, RemoteAuth, RemoteEvent, RemoteHost, Router};
use keel_vfs::{FtpProvider, RemoteProtocol, SftpProvider, VPath};
use parking_lot::RwLock;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// A host's live provider, with the sidebar's connect / disconnect.
#[derive(Clone)]
pub enum HostConn {
    Sftp(Arc<SftpProvider>),
    Ftp(Arc<FtpProvider>),
}
impl HostConn {
    fn new(h: &RemoteHost, events: Sender<RemoteEvent>) -> Self {
        match h.protocol {
            RemoteProtocol::Sftp => Self::Sftp(Arc::new(SftpProvider::new(h.clone(), events))),
            RemoteProtocol::Ftp | RemoteProtocol::Ftps => {
                Self::Ftp(Arc::new(FtpProvider::new(h.clone(), events)))
            }
        }
    }
    fn provider(&self) -> Arc<dyn Provider> {
        match self {
            Self::Sftp(p) => p.clone(),
            Self::Ftp(p) => p.clone(),
        }
    }
    fn connect(&self) {
        let _ = match self {
            Self::Sftp(p) => p.connect(),
            Self::Ftp(p) => p.connect(),
        };
    }
    fn disconnect(&self) {
        match self {
            Self::Sftp(p) => p.disconnect(),
            Self::Ftp(p) => p.disconnect(),
        }
    }
}

/// Live providers by host id (connect / disconnect from the sidebar).
pub type HostMap = Arc<RwLock<HashMap<String, HostConn>>>;

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
    /// The resolved `host:port` (`jump host ` first for a ProxyJump hop).
    pub endpoint: String,
    pub fingerprint: String,
    /// An FTPS certificate that failed verification: why (None: an SSH host key).
    pub problem: Option<String>,
    reply: Sender<bool>,
    since: Instant,
}

pub struct Remotes {
    /// The hosts the providers were built from (the last synced `settings.remotes`).
    hosts: Vec<RemoteHost>,
    pub conns: HostMap,
    pub status: HashMap<String, (ConnStatus, String)>,
    events: Sender<RemoteEvent>,
    pub prompts: VecDeque<HostKeyPrompt>,
    pub editor: Option<Editor>,
    /// The Settings window's page.
    pub page: crate::settings::Page,
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
            conns: Arc::default(),
            status: HashMap::new(),
            events,
            prompts: VecDeque::new(),
            editor: None,
            page: crate::settings::Page::General,
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
        let mut map = self.conns.write();
        for h in hosts {
            if self.hosts.contains(h) && map.contains_key(&h.id) {
                continue;
            }
            let provider = HostConn::new(h, self.events.clone());
            router.register_remote_provider(h.id.clone(), provider.provider());
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
        if !keel_vfs::is_remote_scheme(&tab.dir.scheme) {
            return None;
        }
        let id = tab.dir.authority.as_str();
        let label = self.label(id);
        if tab.is_search() {
            return Some(
                crate::i18n::tr("remotes.remote_search")
                    .replace("{path}", &tab.dir.path)
                    .replace("{label}", label)
                    .replace("{n}", &SEARCH_LIMIT.to_string()),
            );
        }
        let connect = || crate::i18n::tr("remotes.connecting").replace("{label}", label);
        Some(match self.state(id)? {
            ConnStatus::Connected => return None,
            ConnStatus::Connecting => connect(),
            ConnStatus::Failed => crate::i18n::tr("remotes.reconnecting").replace("{label}", label),
            ConnStatus::Disconnected if tab.loading => connect(),
            ConnStatus::Disconnected => {
                crate::i18n::tr("remotes.disconnected").replace("{label}", label)
            }
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
            match &p.problem {
                None => {
                    ui.strong(
                        crate::i18n::tr("remotes.first_connection").replace("{label}", &label),
                    );
                    ui.label(&p.endpoint);
                    ui.add_space(4.0);
                    ui.label(crate::i18n::tr("remotes.first_connection_note"));
                }
                Some(problem) => {
                    ui.strong(
                        crate::i18n::tr("remotes.unverified_certificate")
                            .replace("{label}", &label),
                    );
                    ui.label(&p.endpoint);
                    ui.add_space(4.0);
                    ui.label(
                        crate::i18n::tr("remotes.certificate_note").replace("{problem}", problem),
                    );
                }
            }
            ui.add_space(4.0);
            ui.monospace(&p.fingerprint);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(crate::i18n::tr("remotes.trust_and_connect"))
                    .clicked()
                {
                    answer = Some(true);
                }
                if ui.button(crate::i18n::tr("remotes.cancel")).clicked() {
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
        ui.checkbox(
            &mut s.remote_thumbnails,
            crate::i18n::tr("remotes.thumbnails"),
        )
        .on_hover_text(crate::i18n::tr("remotes.thumbnails_tip"));
        ui.add_space(6.0);
        if s.remotes.is_empty() {
            ui.weak(crate::i18n::tr("remotes.no_hosts"));
        }
        let mut remove = None;
        egui::Grid::new("settings-remotes")
            .num_columns(4)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for h in &s.remotes {
                    ui.strong(&h.label);
                    let r = ui.label(format!(
                        "{} {}@{}:{}",
                        h.protocol.name(),
                        h.user,
                        h.host,
                        h.port
                    ));
                    if h.protocol == RemoteProtocol::Ftp {
                        r.on_hover_text(keel_vfs::FTP_CLEAR_TEXT);
                    }
                    ui.weak(auth_name(h));
                    ui.horizontal(|ui| {
                        if ui.button(crate::i18n::tr("remotes.edit")).clicked() {
                            self.editor = Some(Editor::edit(h));
                        }
                        let confirming = self.confirm_remove.as_deref() == Some(h.id.as_str());
                        let text = if confirming {
                            crate::i18n::tr("remotes.really_remove")
                        } else {
                            crate::i18n::tr("remotes.remove")
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
                // Tests never touch the keychain.
                #[cfg(not(test))]
                keel_vfs::sftp::auth::forget_secrets(&id);
                #[cfg(test)]
                let _ = id;
                send(&tx, &ctx, Msg::Info("Remote host removed".into()));
            });
        }
        ui.add_space(6.0);
        if ui.button(crate::i18n::tr("remotes.add_host")).clicked() {
            self.editor = Some(Editor::default());
        }
    }
}

fn auth_name(h: &RemoteHost) -> &'static str {
    if h.protocol != RemoteProtocol::Sftp {
        return match is_anonymous(&h.user) {
            true => crate::i18n::tr("remotes.anonymous"),
            false => crate::i18n::tr("remotes.password_keychain"),
        };
    }
    match &h.auth {
        RemoteAuth::Agent => crate::i18n::tr("remotes.ssh_agent"),
        RemoteAuth::KeyFile { .. } => crate::i18n::tr("remotes.key_file"),
        RemoteAuth::PasswordInKeyring => crate::i18n::tr("remotes.password_keychain"),
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
        // Tests never touch the keychain.
        #[cfg(test)]
        let stored = {
            let _ = (&host_id, &value);
            Ok::<(), anyhow::Error>(())
        };
        #[cfg(not(test))]
        let stored = keel_vfs::sftp::auth::store_secret(&host_id, kind, &value);
        let msg = match stored {
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
    pub protocol: RemoteProtocol,
    /// FTP: sign in as `anonymous` (no password).
    pub anonymous: bool,
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
    pub use_ssh_config: bool,
    pub error: Option<String>,
    picker: Option<Receiver<Option<PathBuf>>>,
    /// The resolved-connection line under the host field, and the fields it was made from.
    summary: String,
    summary_of: Option<RemoteHost>,
    summary_rx: Option<Receiver<String>>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            id: None,
            label: String::new(),
            protocol: RemoteProtocol::Sftp,
            anonymous: false,
            host: String::new(),
            port: "22".into(),
            user: String::new(),
            auth: AuthKind::KeyFile,
            key_path: String::new(),
            passphrase_in_keyring: false,
            secret: String::new(),
            home: String::new(),
            bookmarks: Vec::new(),
            use_ssh_config: true,
            error: None,
            picker: None,
            summary: String::new(),
            summary_of: None,
            summary_rx: None,
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

/// FTP user names are often email addresses; no spaces or control characters.
fn valid_ftp_user(u: &str) -> bool {
    !u.is_empty() && !u.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// FTP's anonymous sign-in (RFC 1635); an empty user means the same.
pub fn is_anonymous(user: &str) -> bool {
    user.is_empty() || user.eq_ignore_ascii_case("anonymous")
}

/// A stable id slug from the label, unique among the `taken` ids.
pub fn slug(label: &str, taken: &[String]) -> String {
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
    let free = |id: &str| taken.iter().all(|t| t != id);
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
        let ftp = h.protocol != RemoteProtocol::Sftp;
        Self {
            id: Some(h.id.clone()),
            label: h.label.clone(),
            protocol: h.protocol,
            anonymous: ftp && is_anonymous(&h.user),
            host: h.host.clone(),
            port: h.port.to_string(),
            user: h.user.clone(),
            auth,
            key_path,
            passphrase_in_keyring,
            home: h.home.clone().unwrap_or_default(),
            bookmarks: h.bookmarks.clone(),
            use_ssh_config: h.use_ssh_config,
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
            return Err(crate::i18n::tr("remotes.err_label").to_owned());
        }
        if !valid_host(host) {
            return Err(crate::i18n::tr("remotes.err_host").to_owned());
        }
        let port: u16 = match self.port.trim().parse() {
            Ok(p) if p > 0 => p,
            _ => return Err(crate::i18n::tr("remotes.err_port").to_owned()),
        };
        let ftp = self.protocol != RemoteProtocol::Sftp;
        let user = match ftp && self.anonymous {
            true => "anonymous",
            false => user,
        };
        if ftp {
            if !valid_ftp_user(user) {
                return Err(crate::i18n::tr("remotes.err_user_ftp").to_owned());
            }
        // With ~/.ssh/config an empty user comes from the config (or the local account).
        } else if !(valid_user(user) || user.is_empty() && self.use_ssh_config) {
            return Err(crate::i18n::tr("remotes.err_user_sftp").to_owned());
        }
        let home = self.home.trim();
        if !home.is_empty() && !home.starts_with('/') {
            return Err(crate::i18n::tr("remotes.err_home").to_owned());
        }
        let mut bookmarks = Vec::new();
        for (name, path) in &self.bookmarks {
            let (name, path) = (name.trim(), path.trim());
            if name.is_empty() && path.is_empty() {
                continue;
            }
            if !path.starts_with('/') {
                return Err(crate::i18n::tr("remotes.err_bookmark_path").replace("{name}", name));
            }
            let name = if name.is_empty() { path } else { name };
            bookmarks.push((name.to_owned(), path.to_owned()));
        }
        let new = self.id.is_none();
        let secret = (!self.secret.is_empty()).then(|| self.secret.clone());
        let auth = match ftp {
            true if self.anonymous => None,
            true => Some(AuthKind::Password),
            false => Some(self.auth),
        };
        let (auth, secret) = match auth {
            None => (RemoteAuth::PasswordInKeyring, None),
            Some(AuthKind::Agent) => (RemoteAuth::Agent, None),
            Some(AuthKind::KeyFile) => (
                RemoteAuth::KeyFile {
                    path: PathBuf::from(self.key_path.trim()),
                    passphrase_in_keyring: self.passphrase_in_keyring,
                },
                secret
                    .filter(|_| self.passphrase_in_keyring)
                    .map(|s| ("passphrase", s)),
            ),
            Some(AuthKind::Password) => {
                if new && secret.is_none() {
                    return Err(crate::i18n::tr("remotes.err_password").to_owned());
                }
                (
                    RemoteAuth::PasswordInKeyring,
                    secret.map(|s| ("password", s)),
                )
            }
        };
        if new
            && !ftp
            && self.auth == AuthKind::KeyFile
            && self.passphrase_in_keyring
            && secret.is_none()
        {
            return Err(crate::i18n::tr("remotes.err_key_passphrase").to_owned());
        }
        let taken: Vec<String> = existing.iter().map(|h| h.id.clone()).collect();
        let id = self.id.clone().unwrap_or_else(|| slug(label, &taken));
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
                use_ssh_config: self.use_ssh_config && !ftp,
                protocol: self.protocol,
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
        self.refresh_summary(ctx);
        let mut outcome = EditorOutcome::Open;
        let title = if self.id.is_some() {
            crate::i18n::tr("remotes.editor_edit_title")
        } else {
            crate::i18n::tr("remotes.editor_add_title")
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
                    field(
                        ui,
                        crate::i18n::tr("remotes.label"),
                        &mut self.label,
                        crate::i18n::tr("remotes.label_hint"),
                    );
                    ui.label(crate::i18n::tr("remotes.protocol"));
                    ui.vertical(|ui| {
                        let before = self.protocol;
                        ui.horizontal(|ui| {
                            for p in [
                                RemoteProtocol::Sftp,
                                RemoteProtocol::Ftps,
                                RemoteProtocol::Ftp,
                            ] {
                                ui.radio_value(&mut self.protocol, p, p.name());
                            }
                        });
                        // The port follows the protocol unless the user changed it.
                        if self.protocol != before
                            && self.port.trim() == before.default_port().to_string()
                        {
                            self.port = self.protocol.default_port().to_string();
                        }
                        match self.protocol {
                            RemoteProtocol::Ftp => {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    keel_vfs::FTP_CLEAR_TEXT,
                                );
                            }
                            RemoteProtocol::Ftps => {
                                ui.weak(crate::i18n::tr("remotes.ftps_note"));
                            }
                            RemoteProtocol::Sftp => {}
                        }
                    });
                    ui.end_row();
                    let ftp = self.protocol != RemoteProtocol::Sftp;
                    let hint = match ftp {
                        true => crate::i18n::tr("remotes.host_hint_ftp"),
                        false => crate::i18n::tr("remotes.host_hint_sftp"),
                    };
                    field(ui, crate::i18n::tr("remotes.host"), &mut self.host, hint);
                    if !ftp {
                        ui.label("");
                        ui.vertical(|ui| {
                            ui.checkbox(
                                &mut self.use_ssh_config,
                                crate::i18n::tr("remotes.use_ssh_config"),
                            );
                            if !self.summary.is_empty() {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(&self.summary).small())
                                        .wrap(),
                                );
                            }
                        });
                        ui.end_row();
                    }
                    let port_hint = self.protocol.default_port().to_string();
                    field(
                        ui,
                        crate::i18n::tr("remotes.port"),
                        &mut self.port,
                        &port_hint,
                    );
                    if ftp {
                        ui.label(crate::i18n::tr("remotes.sign_in"));
                        ui.checkbox(&mut self.anonymous, crate::i18n::tr("remotes.anonymously"));
                        ui.end_row();
                        if !self.anonymous {
                            field(ui, crate::i18n::tr("remotes.user"), &mut self.user, "");
                            ui.label(crate::i18n::tr("remotes.password"));
                            secret_field(ui, &mut self.secret, self.id.is_some());
                            ui.end_row();
                        }
                        field(
                            ui,
                            crate::i18n::tr("remotes.initial_folder"),
                            &mut self.home,
                            crate::i18n::tr("remotes.initial_folder_hint"),
                        );
                        return;
                    }
                    field(ui, crate::i18n::tr("remotes.user"), &mut self.user, "");
                    ui.label(crate::i18n::tr("remotes.sign_in_with"));
                    ui.horizontal(|ui| {
                        ui.radio_value(
                            &mut self.auth,
                            AuthKind::KeyFile,
                            crate::i18n::tr("remotes.key_file"),
                        );
                        ui.radio_value(
                            &mut self.auth,
                            AuthKind::Agent,
                            crate::i18n::tr("remotes.ssh_agent"),
                        );
                        ui.radio_value(
                            &mut self.auth,
                            AuthKind::Password,
                            crate::i18n::tr("remotes.password"),
                        );
                    });
                    ui.end_row();
                    match self.auth {
                        AuthKind::KeyFile => {
                            ui.label(crate::i18n::tr("remotes.key_file"));
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.key_path)
                                        .hint_text(crate::i18n::tr("remotes.key_file_hint"))
                                        .desired_width(220.0),
                                );
                                let busy = self.picker.is_some();
                                if ui
                                    .add_enabled(
                                        !busy,
                                        egui::Button::new(crate::i18n::tr("remotes.browse")),
                                    )
                                    .clicked()
                                {
                                    self.picker = Some(pick_key(ctx.clone()));
                                }
                            });
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(
                                &mut self.passphrase_in_keyring,
                                crate::i18n::tr("remotes.key_passphrase_note"),
                            );
                            ui.end_row();
                            if self.passphrase_in_keyring {
                                ui.label(crate::i18n::tr("remotes.passphrase"));
                                secret_field(ui, &mut self.secret, self.id.is_some());
                                ui.end_row();
                            }
                        }
                        AuthKind::Password => {
                            ui.label(crate::i18n::tr("remotes.password"));
                            secret_field(ui, &mut self.secret, self.id.is_some());
                            ui.end_row();
                        }
                        AuthKind::Agent => {}
                    }
                    field(
                        ui,
                        crate::i18n::tr("remotes.initial_folder"),
                        &mut self.home,
                        crate::i18n::tr("remotes.initial_folder_hint"),
                    );
                });
            ui.add_space(4.0);
            ui.label(crate::i18n::tr("remotes.bookmarks"));
            let mut drop_row = None;
            for (i, (name, path)) in self.bookmarks.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text(crate::i18n::tr("remotes.bookmark_label_hint"))
                            .desired_width(120.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(path)
                            .hint_text(crate::i18n::tr("remotes.bookmark_path_hint"))
                            .desired_width(250.0),
                    );
                    if ui.small_button("×").clicked() {
                        drop_row = Some(i);
                    }
                });
            }
            if let Some(i) = drop_row {
                self.bookmarks.remove(i);
            }
            if ui
                .small_button(crate::i18n::tr("remotes.add_bookmark"))
                .clicked()
            {
                self.bookmarks.push(Default::default());
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(crate::i18n::tr("remotes.save")).clicked() {
                    match self.build(existing) {
                        Ok((host, secret)) => outcome = EditorOutcome::Save(Box::new(host), secret),
                        Err(e) => self.error = Some(e),
                    }
                }
                if ui.button(crate::i18n::tr("remotes.cancel")).clicked() {
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

impl Editor {
    /// Re-resolves the summary on a worker whenever the fields it depends on change; only
    /// the newest answer is kept (an older worker's send goes to a dropped receiver).
    fn refresh_summary(&mut self, ctx: &egui::Context) {
        if let Some(Ok(text)) = self.summary_rx.as_ref().map(Receiver::try_recv) {
            self.summary = text;
            self.summary_rx = None;
        }
        let host = self.host.trim();
        if !valid_host(host) || self.protocol != RemoteProtocol::Sftp {
            self.summary.clear();
            self.summary_of = None;
            return;
        }
        let draft = RemoteHost {
            id: self.id.clone().unwrap_or_else(|| "draft".into()),
            label: String::new(),
            host: host.to_owned(),
            port: self.port.trim().parse().unwrap_or(22),
            user: self.user.trim().to_owned(),
            auth: RemoteAuth::Agent,
            home: None,
            bookmarks: Vec::new(),
            use_ssh_config: self.use_ssh_config,
            protocol: RemoteProtocol::Sftp,
        };
        if self.summary_of.as_ref() == Some(&draft) {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.summary_of = Some(draft.clone());
        self.summary_rx = Some(rx);
        let ctx = ctx.clone();
        spawn("keel-ssh-config", move || {
            if tx.send(keel_vfs::sftp::describe(&draft, None)).is_ok() {
                ctx.request_repaint();
            }
        });
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
        let picked = crate::platform::file_dialog("SSH private key", |mut dialog| {
            if let Some(ssh) = directories::BaseDirs::new().map(|b| b.home_dir().join(".ssh")) {
                dialog = dialog.set_directory(ssh);
            }
            dialog.pick_file()
        });
        let _ = tx.send(picked);
        ctx.request_repaint();
    });
    if !started {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = tx.send(None);
        return rx;
    }
    rx
}

/// `sftp://<id><home>` (`ftp://` for FTP and FTPS), `/` when the host has no initial folder.
pub fn home_of(h: &RemoteHost) -> VPath {
    remote_path(h.protocol, &h.id, h.home.as_deref().unwrap_or("/"))
}

pub fn remote_path(protocol: RemoteProtocol, id: &str, path: &str) -> VPath {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    VPath {
        scheme: protocol.scheme().into(),
        authority: id.into(),
        path,
    }
}

/// `sftp://user@host[:port]` (`ftp://`, `ftps://`) for "Copy address".
pub fn address(h: &RemoteHost) -> String {
    let host = if h.host.contains(':') && !h.host.starts_with('[') {
        format!("[{}]", h.host)
    } else {
        h.host.clone()
    };
    let login = if h.user.is_empty() {
        host
    } else {
        format!("{}@{host}", h.user)
    };
    let scheme = h.protocol.name().to_ascii_lowercase();
    match h.port {
        port if port == h.protocol.default_port() => format!("{scheme}://{login}"),
        port => format!("{scheme}://{login}:{port}"),
    }
}

/// The line typed into the terminal pane, or None when the config holds characters that
/// could do more than name a host (it is hand-editable).
pub fn ssh_command(h: &RemoteHost) -> Option<String> {
    if h.protocol != RemoteProtocol::Sftp {
        return None;
    }
    let host = h.host.trim_start_matches('[').trim_end_matches(']');
    // No user: `ssh` takes it from the same ~/.ssh/config (an alias resolves there too).
    let login = match h.user.as_str() {
        "" if h.use_ssh_config => host.to_owned(),
        user if valid_user(user) => format!("{user}@{host}"),
        _ => return None,
    };
    valid_host(host).then(|| match h.port {
        22 => format!("ssh {login}"),
        port => format!("ssh -p {port} {login}"),
    })
}

pub fn delete_text(n: usize, label: &str) -> String {
    format!(
        "Delete {} on {label}? This cannot be undone.",
        crate::jobs::items(n)
    )
}

/// Paths on a remote host or cloud account (no trash there, downloads for previews).
pub fn is_network(p: &VPath) -> bool {
    matches!(p.scheme.as_str(), "sftp" | "ftp" | "cloud")
}

/// Materialises `path` for preview / open-with. Remote and cloud files report progress as
/// `Msg::Download` when larger than `BIG_DOWNLOAD`. Blocks: workers only.
pub fn materialise(
    router: &Router,
    path: &VPath,
    tx: &Sender<Msg>,
    ctx: &egui::Context,
) -> anyhow::Result<PathBuf> {
    let provider = router
        .provider_for(path)
        .ok_or_else(|| anyhow::anyhow!("no provider for {}", path.display()))?;
    if !is_network(path) {
        return provider.local_copy(path);
    }
    let name = path.name().to_owned();
    let last = Cell::new(None::<Instant>);
    let progress = |p: keel_vfs::Progress| {
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
    };
    provider.local_copy_cancellable(path, &progress, &AtomicBool::new(false))
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
                    snippet: None,
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
    request: (String, crate::omissions::Filter),
    id: u64,
    newest: Arc<AtomicU64>,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    let (text, omissions) = request;
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
        let result = result.map(|hits| omissions.apply(hits));
        send(&tx, &ctx, Msg::Search { id, result });
    });
}

/// Remote parts of `AppState` (kept here so state.rs only has small hooks).
impl AppState {
    pub fn remote_event(&mut self, event: RemoteEvent) {
        match event {
            // Cloud ids hold a ':' (SFTP host slugs never do).
            RemoteEvent::Status {
                host_id,
                status,
                detail,
            } if host_id.starts_with("cloud:") || host_id.starts_with("cloud-auth:") => {
                self.cloud_event(&host_id, status, detail)
            }
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
                                if keel_vfs::is_remote_scheme(&tab.dir.scheme)
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
                endpoint,
                fingerprint,
                reply,
            } => self.remotes.prompts.push_back(HostKeyPrompt {
                host_id,
                endpoint,
                fingerprint,
                problem: None,
                reply,
                since: Instant::now(),
            }),
            RemoteEvent::CertificatePrompt {
                host_id,
                endpoint,
                fingerprint,
                problem,
                reply,
            } => self.remotes.prompts.push_back(HostKeyPrompt {
                host_id,
                endpoint,
                fingerprint,
                problem: Some(problem),
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
            if !keel_vfs::is_remote_scheme(&tab.dir.scheme)
                || tab.is_search()
                || tab.loading
                || tab.error.is_none()
            {
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
        let provider = self.remotes.conns.read().get(&id).cloned();
        match cmd {
            RemoteCmd::Connect | RemoteCmd::Disconnect => {
                let Some(provider) = provider else { return };
                // Both block on the network (connect may wait on a host-key prompt).
                spawn("keel-connect", move || {
                    if cmd == RemoteCmd::Connect {
                        provider.connect();
                    } else {
                        provider.disconnect();
                    }
                });
            }
            RemoteCmd::Terminal if host.protocol != RemoteProtocol::Sftp => self
                .toasts
                .error("FTP remotes have no terminal (ssh needs SFTP)"),
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
            use_ssh_config: false,
            protocol: Default::default(),
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
    fn remote_editor_accepts_config_defaults() {
        let editor = Editor {
            label: "Alias".into(),
            host: "alias".into(),
            ..Editor::default()
        };
        let (host, _) = editor.build(&[]).expect("empty user inherits SSH config");
        assert!(host.use_ssh_config && host.user.is_empty());
        assert_eq!(address(&host), "sftp://alias");
        assert_eq!(ssh_command(&host).as_deref(), Some("ssh alias"));
        let off = Editor {
            use_ssh_config: false,
            ..editor
        };
        assert!(
            off.build(&[]).is_err(),
            "without the config a user is needed"
        );
    }

    #[test]
    fn remote_validation_errors_in_spanish() {
        crate::i18n::set("es");
        let mut editor = Editor::default();
        assert_eq!(editor.build(&[]).unwrap_err(), "Introduce una etiqueta");
        editor.label = "Media box".into();
        editor.host = "example.invalid".into();
        editor.port = "0".into();
        assert_eq!(
            editor.build(&[]).unwrap_err(),
            "El puerto debe estar entre 1 y 65535"
        );
        editor.port = "2222".into();
        editor.user = "alice".into();
        editor.bookmarks = vec![("Logs".into(), "var/log".into())];
        assert_eq!(
            editor.build(&[]).unwrap_err(),
            "Marcador \"Logs\": la ruta debe empezar por /"
        );
    }

    #[test]
    fn ftp_hosts_in_the_editor_and_the_sidebar() {
        let editor = || Editor {
            label: "Cam".into(),
            protocol: RemoteProtocol::Ftp,
            host: "cam.example.invalid".into(),
            port: "21".into(),
            user: "me".into(),
            secret: "pw".into(),
            ..Editor::default()
        };
        let (ftp, secret) = editor().build(&[]).unwrap();
        assert_eq!(
            (
                ftp.protocol,
                ftp.port,
                ftp.user.as_str(),
                ftp.use_ssh_config
            ),
            (RemoteProtocol::Ftp, 21, "me", false)
        );
        assert_eq!(ftp.auth, RemoteAuth::PasswordInKeyring);
        assert_eq!(secret, Some(("password", "pw".into())));
        let no_password = Editor {
            secret: String::new(),
            ..editor()
        };
        assert!(
            no_password.build(&[]).is_err(),
            "a new FTP host needs its password"
        );
        let anonymous = Editor {
            anonymous: true,
            user: String::new(),
            secret: String::new(),
            protocol: RemoteProtocol::Ftps,
            ..editor()
        };
        let (ftps, secret) = anonymous.build(&[]).unwrap();
        assert_eq!((ftps.user.as_str(), secret), ("anonymous", None));
        assert_eq!(auth_name(&ftps), "Anonymous");
        // Edit and save again: unchanged.
        let again = Editor::edit(&ftp)
            .build(std::slice::from_ref(&ftp))
            .unwrap();
        assert_eq!(again, (ftp.clone(), None));
        assert!(Editor::edit(&ftps).anonymous);

        assert_eq!(address(&ftp), "ftp://me@cam.example.invalid");
        assert_eq!(address(&ftps), "ftps://anonymous@cam.example.invalid");
        assert_eq!(ssh_command(&ftp), None, "no ssh for FTP");
        assert_eq!(home_of(&ftp), VPath::parse("ftp://cam/").unwrap());
        assert!(is_network(&home_of(&ftp)));
        let rows = crate::sidebar_remotes::rows(&[ftp, ftps], &HashMap::new());
        assert_eq!(rows[0].warning, Some(keel_vfs::FTP_CLEAR_TEXT));
        assert_eq!(rows[1].warning, None, "FTPS is encrypted");
        assert!(!rows[0].ssh && !rows[1].ssh);
        assert_eq!(rows[1].home.scheme, "ftp");
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
        assert!(rows[0].ssh && rows[0].warning.is_none());
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
                endpoint: "nas.example.invalid:22".into(),
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
        // An FTPS certificate waits the same way and carries why it was not verified.
        let (reply, answer) = crossbeam_channel::bounded(1);
        state.apply(Msg::Remote(RemoteEvent::CertificatePrompt {
            host_id: "cam".into(),
            endpoint: "cam.example.invalid:21".into(),
            fingerprint: "AB:CD".into(),
            problem: "invalid peer certificate: UnknownIssuer".into(),
            reply,
        }));
        assert_eq!(
            state.remotes.prompts[0].problem.as_deref(),
            Some("invalid peer certificate: UnknownIssuer")
        );
        state.remotes.answer(true);
        assert_eq!(answer.try_recv(), Ok(true));
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
        // A real connect attempt started by `remote_tick` may race its own status events in
        // ahead of the fake one; apply everything until the fake event has been seen.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let msg = state
                .rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("fake status event never arrived");
            let done = matches!(
                &msg,
                Msg::Remote(RemoteEvent::Status { detail, .. }) if detail == "TCP connect failed"
            );
            state.apply(msg);
            if done {
                break;
            }
        }
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
            Some(crate::dialogs::Dialog::Confirm { text, on_yes, .. }) => {
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
        fn list_complete(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
            self.list(dir)
        }
        fn remove_kind(&self) -> keel_vfs::RemoveKind {
            keel_vfs::RemoveKind::Permanent
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
        let remote = remote_path(RemoteProtocol::Sftp, "fake", "/");
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
        let top = remote_path(RemoteProtocol::Sftp, "fake", "/");
        let hits = search_names(&mirror, &top, "report", 100, 100, &|| false).unwrap();
        assert_eq!(hits.len(), 3);
        let bounded = search_names(&mirror, &top, "report", 2, 100, &|| false).unwrap();
        assert!(bounded.len() <= 2, "stops after the entry limit");
        assert!(search_names(&mirror, &top, "x", 100, 100, &|| true)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_status_in_spanish() {
        crate::i18n::set("es");
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let mut remotes = Remotes::new(tx, egui::Context::default());
        remotes.hosts.push(host("box", RemoteAuth::Agent));
        remotes
            .status
            .insert("box".into(), (ConnStatus::Connecting, String::new()));
        let tab = Tab::new(remote_path(RemoteProtocol::Sftp, "box", "/Files"));
        let banner = remotes.banner(&tab).expect("connecting banner");
        assert_ne!(banner, "Connecting to Box box…", "banner is not Spanish");
        assert!(banner.contains("Box box"), "{banner}");
        crate::i18n::set("en");
    }
}
