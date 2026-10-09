//! Cloud accounts in the app: `[[clouds]]` from config.toml registered with the Router,
//! connection status from the providers, the account wizard (Settings → Cloud: kind →
//! fields → browser sign-in or S3 keys), re-sign-in from a toast, and removal (revoke +
//! keychain cleanup). Tokens and keys only ever go to the `SecretStore` (OS keychain), on
//! workers; no IO here runs on the UI thread.

use crate::keys::Action;
use crate::settings::Settings;
use crate::state::{AppState, Msg};
use crate::worker::{send, spawn};
use crossbeam_channel::{Receiver, Sender};
use egui::{Id, Modal};
use keel_vfs::cloud::{self, oauth_authorize, resolve_client, store_tokens, AUTH_TIMEOUT};
use keel_vfs::{
    Caps, CloudAccount, CloudKind, CloudProvider, ConnStatus, Entry, Progress, Provider,
    RemoteEvent, RemoveKind, Router, S3Config, SecretStore, VPath,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// README section on registering your own OAuth app.
pub const CLIENT_ID_HELP: &str =
    "https://github.com/Runitupshawty/keel#cloud-accounts-bring-your-own-client-id";

/// Sidebar right-click and toast commands on an account (`Add` opens the wizard).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudCmd {
    Reconnect,
    Edit,
    /// Asks first.
    Remove,
    /// Confirmed: sign out and forget the account.
    RemoveNow,
    /// Run the browser sign-in again (revoked or expired grant).
    SignIn,
    Add,
}

pub fn kind_name(kind: CloudKind) -> &'static str {
    match kind {
        CloudKind::GoogleDrive => "Google Drive",
        CloudKind::Dropbox => "Dropbox",
        CloudKind::S3 => "S3",
    }
}

/// `cloud://<id>/`: the account's root (its `root` folder is applied by the provider).
pub fn root_of(id: &str) -> VPath {
    VPath {
        scheme: "cloud".into(),
        authority: id.into(),
        path: "/".into(),
    }
}

/// The delete confirmation: what `remove` really does on that service.
pub fn delete_text(n: usize, kind: RemoveKind) -> String {
    let items = crate::jobs::items(n);
    match kind {
        RemoveKind::Trash => format!("Move {items} to the Drive trash?"),
        RemoveKind::RecoverableDelete => {
            format!("Delete {items} on Dropbox? (recoverable for 30 days)")
        }
        RemoveKind::Permanent => format!("Delete {items} from the bucket permanently?"),
    }
}

/// The provider's revoked / missing grant errors ("…; sign in again").
pub fn needs_sign_in(detail: &str) -> bool {
    detail.contains("sign in again")
}

/// `cloud://<id>/` before its first use: reads the keychain and builds the `CloudProvider`
/// on the worker that first calls it, so session restore stays lazy and the UI thread never
/// waits on the keychain. Listings double as the connection check (status events).
struct LazyCloud {
    account: CloudAccount,
    secrets: Arc<dyn SecretStore>,
    events: Sender<RemoteEvent>,
    inner: Mutex<Option<Arc<CloudProvider>>>,
    last: Mutex<Option<(ConnStatus, String)>>,
}

impl LazyCloud {
    fn get(&self) -> anyhow::Result<Arc<CloudProvider>> {
        let mut inner = self.inner.lock();
        if let Some(p) = &*inner {
            return Ok(p.clone());
        }
        let p = CloudProvider::connect(&self.account, self.secrets.clone(), self.events.clone())
            .inspect_err(|e| self.status(ConnStatus::Failed, format!("{e:#}")))?;
        let p = Arc::new(p);
        *inner = Some(p.clone());
        Ok(p)
    }
    /// Sends a status change (repeats are dropped).
    fn status(&self, status: ConnStatus, detail: String) {
        let mut last = self.last.lock();
        if last.as_ref() == Some(&(status, detail.clone())) {
            return;
        }
        *last = Some((status, detail.clone()));
        let _ = self.events.send(RemoteEvent::Status {
            host_id: format!("cloud:{}", self.account.id),
            status,
            detail,
        });
    }
}

fn not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

impl Provider for LazyCloud {
    fn scheme(&self) -> &'static str {
        "cloud"
    }
    fn caps(&self) -> Caps {
        Caps {
            write: true,
            rename: true,
            delete: true,
            watch: false,
        }
    }
    fn list(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
        let connected = matches!(&*self.last.lock(), Some((ConnStatus::Connected, _)));
        if !connected {
            self.status(ConnStatus::Connecting, "connecting".into());
        }
        let result = self.get()?.list(dir);
        match &result {
            Ok(_) => self.status(ConnStatus::Connected, "connected".into()),
            Err(e) if !not_found(e) => self.status(ConnStatus::Failed, format!("{e:#}")),
            Err(_) => {}
        }
        result
    }
    fn stat(&self, p: &VPath) -> anyhow::Result<Entry> {
        self.get()?.stat(p)
    }
    fn read(&self, p: &VPath) -> anyhow::Result<Box<dyn Read + Send>> {
        self.get()?.read(p)
    }
    fn write(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
        self.get()?.write(p)
    }
    fn create_new(&self, p: &VPath) -> anyhow::Result<Box<dyn Write + Send>> {
        self.get()?.create_new(p)
    }
    fn mkdir(&self, p: &VPath) -> anyhow::Result<()> {
        self.get()?.mkdir(p)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
        self.get()?.rename(from, to)
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
        self.get()?.rename_noreplace(from, to)
    }
    fn rename_replace(&self, from: &VPath, to: &VPath) -> anyhow::Result<()> {
        self.get()?.rename_replace(from, to)
    }
    fn remove_empty_dir(&self, p: &VPath) -> anyhow::Result<()> {
        self.get()?.remove_empty_dir(p)
    }
    fn remove(&self, p: &VPath) -> anyhow::Result<()> {
        self.get()?.remove(p)
    }
    fn local_copy(&self, p: &VPath) -> anyhow::Result<PathBuf> {
        self.get()?.local_copy(p)
    }
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> anyhow::Result<PathBuf> {
        self.get()?.local_copy_cancellable(p, progress, cancel)
    }
}

pub struct Clouds {
    /// The accounts the providers were built from (the last synced `settings.clouds`).
    accounts: Vec<CloudAccount>,
    pub status: HashMap<String, (ConnStatus, String)>,
    /// The app's own channel for provider and sign-in events (forwarded as `Msg::Remote`).
    pub events: Sender<RemoteEvent>,
    /// The OS keychain (memory in tests).
    pub secrets: Arc<dyn SecretStore>,
    pub wizard: Option<Wizard>,
    confirm_remove: Option<String>,
}

impl Clouds {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, secrets: Arc<dyn SecretStore>) -> Self {
        let (events, rx) = crossbeam_channel::unbounded::<RemoteEvent>();
        spawn("keel-cloud-events", move || {
            for event in rx {
                send(&tx, &ctx, Msg::Remote(event));
            }
        });
        Self {
            accounts: Vec::new(),
            status: HashMap::new(),
            events,
            secrets,
            wizard: None,
            confirm_remove: None,
        }
    }

    /// Registers new or changed accounts with the router and drops removed ones. Cheap when
    /// nothing changed (every frame); registering does no IO (`LazyCloud`).
    pub fn sync(&mut self, router: &Router, accounts: &[CloudAccount]) {
        if self.accounts == accounts {
            return;
        }
        for a in accounts {
            if !self.accounts.contains(a) {
                self.register(router, a);
            }
        }
        for old in &self.accounts {
            if !accounts.iter().any(|a| a.id == old.id) {
                router.unregister_cloud(&old.id);
            }
        }
        self.status
            .retain(|id, _| accounts.iter().any(|a| a.id == *id));
        self.accounts = accounts.to_vec();
    }

    fn register(&mut self, router: &Router, a: &CloudAccount) {
        let provider = LazyCloud {
            account: a.clone(),
            secrets: self.secrets.clone(),
            events: self.events.clone(),
            inner: Mutex::default(),
            last: Mutex::default(),
        };
        router.register_cloud_provider(a.id.clone(), Arc::new(provider));
        self.status
            .insert(a.id.clone(), (ConnStatus::Disconnected, String::new()));
    }

    /// A fresh provider that reads the keychain again on next use (new tokens or keys).
    pub fn reconnect(&mut self, router: &Router, id: &str) {
        if let Some(a) = self.account(id).cloned() {
            self.register(router, &a);
        }
    }

    pub fn account(&self, id: &str) -> Option<&CloudAccount> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Settings → Cloud: the account list with Add / Edit / Remove.
    pub fn settings_page(&mut self, ui: &mut egui::Ui, s: &mut Settings, tx: &Sender<Msg>) {
        ui.checkbox(
            &mut s.remote_thumbnails,
            "Thumbnails for remote and cloud files",
        )
        .on_hover_text("Grid view then downloads every image and video it shows");
        ui.add_space(6.0);
        if s.clouds.is_empty() {
            ui.weak("No cloud accounts yet.");
        }
        let mut remove = None;
        egui::Grid::new("settings-clouds")
            .num_columns(4)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for a in &s.clouds {
                    ui.strong(&a.label);
                    ui.label(kind_name(a.kind));
                    ui.weak(match &a.s3 {
                        Some(s3) => format!("{} / {}", s3.endpoint, s3.bucket),
                        None => a.root.clone().unwrap_or_else(|| "/".into()),
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Edit…").clicked() {
                            self.wizard = Some(Wizard::edit(a));
                        }
                        let confirming = self.confirm_remove.as_deref() == Some(a.id.as_str());
                        let text = if confirming {
                            "Really remove?"
                        } else {
                            "Remove"
                        };
                        if ui.button(text).clicked() {
                            if confirming {
                                remove = Some(a.clone());
                            } else {
                                self.confirm_remove = Some(a.id.clone());
                            }
                        }
                    });
                    ui.end_row();
                }
            });
        if let Some(a) = remove {
            self.confirm_remove = None;
            s.clouds.retain(|c| c.id != a.id);
            sign_out(a, self.secrets.clone(), tx.clone(), ui.ctx().clone());
        }
        ui.add_space(6.0);
        if ui.button("Add account…").clicked() {
            self.wizard = Some(Wizard::default());
        }
    }
}

/// Revokes the grant where the service allows it and deletes the keychain entries, on a
/// worker (both block).
fn sign_out(
    account: CloudAccount,
    secrets: Arc<dyn SecretStore>,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    spawn("keel-cloud-sign-out", move || {
        let label = &account.label;
        let msg = match cloud::sign_out(&account, &*secrets) {
            Ok(()) => Msg::Info(format!(
                "Removed {label}; its sign-in is deleted from this PC"
            )),
            Err(e) => Msg::Toast(format!(
                "Removed {label} and deleted its sign-in from this PC, but the service did not \
                 confirm the revoke: {e:#}"
            )),
        };
        send(&tx, &ctx, msg);
    });
}

/// Where the wizard is.
pub enum Step {
    /// Pick Google Drive, Dropbox or S3 (new accounts only).
    Kind,
    Fields,
    /// The browser sign-in and / or the keychain writes, on a worker.
    Pending(Box<Pending>),
}

pub struct Pending {
    pub account: CloudAccount,
    pub sign_in: bool,
    started: Instant,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<(), String>>,
}

pub enum Outcome {
    Open,
    Cancelled,
    /// Save `account` (its secrets are already in the keychain).
    Done(CloudAccount),
}

/// Add / Edit account dialog, also used alone for "sign in again". Secrets typed here go
/// to the keychain only.
pub struct Wizard {
    pub step: Step,
    /// Some: editing this account (its id stays).
    pub id: Option<String>,
    pub kind: CloudKind,
    pub label: String,
    /// Folder (Drive / Dropbox) or key prefix (S3); empty = the whole drive or bucket.
    pub root: String,
    pub client_id: String,
    /// Google "Desktop app" secret (optional, keychain).
    pub client_secret: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub error: Option<String>,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            step: Step::Kind,
            id: None,
            kind: CloudKind::GoogleDrive,
            label: String::new(),
            root: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            endpoint: String::new(),
            region: String::new(),
            bucket: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            error: None,
        }
    }
}

/// The secrets to store: (`<id>/<field>`, value).
pub type Secrets = Vec<(String, String)>;

impl Wizard {
    pub fn edit(a: &CloudAccount) -> Self {
        let s3 = a.s3.clone().unwrap_or(S3Config {
            endpoint: String::new(),
            region: String::new(),
            bucket: String::new(),
        });
        Self {
            step: Step::Fields,
            id: Some(a.id.clone()),
            kind: a.kind,
            label: a.label.clone(),
            root: a.root.clone().unwrap_or_default(),
            client_id: a.client_id_override.clone().unwrap_or_default(),
            endpoint: s3.endpoint,
            region: s3.region,
            bucket: s3.bucket,
            ..Self::default()
        }
    }

    pub fn choose(&mut self, kind: CloudKind) {
        self.kind = kind;
        if self.label.is_empty() {
            self.label = kind_name(kind).into();
        }
        self.error = None;
        self.step = Step::Fields;
    }

    /// The account to save and the secrets for the keychain, or why the form is not complete.
    pub fn build(&self, existing: &[CloudAccount]) -> Result<(CloudAccount, Secrets), String> {
        let label = self.label.trim();
        if label.is_empty() {
            return Err("Enter a label".into());
        }
        let root = self.root.trim().trim_matches('/');
        let bad_segment = |s: &str| s.is_empty() || s == "." || s == "..";
        if root.contains('\\') || (!root.is_empty() && root.split('/').any(bad_segment)) {
            return Err("The root folder must be a plain path like Photos/2026".into());
        }
        let new = self.id.is_none();
        let taken: Vec<String> = existing.iter().map(|a| a.id.clone()).collect();
        let id = self
            .id
            .clone()
            .unwrap_or_else(|| crate::remotes::slug(label, &taken));
        let key = |field: &str| format!("{id}/{field}");
        let mut secrets = Secrets::new();
        let client_id = self.client_id.trim();
        let s3 = if self.kind == CloudKind::S3 {
            let (endpoint, region, bucket) =
                (self.endpoint.trim(), self.region.trim(), self.bucket.trim());
            if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
                return Err("The endpoint must start with https://".into());
            }
            if region.is_empty() {
                return Err("Enter the region (e.g. us-west-002 for Backblaze B2)".into());
            }
            if bucket.is_empty() || bucket.contains(['/', ' ']) {
                return Err("Enter the bucket name".into());
            }
            let (ak, sk) = (self.access_key.trim(), self.secret_key.trim());
            match (ak.is_empty(), sk.is_empty()) {
                (false, false) => {
                    secrets.push((key("access_key_id"), ak.to_owned()));
                    secrets.push((key("secret_access_key"), sk.to_owned()));
                }
                (true, true) if !new => {}
                _ => return Err("Enter both the access key id and the secret key".into()),
            }
            Some(S3Config {
                endpoint: endpoint.to_owned(),
                region: region.to_owned(),
                bucket: bucket.to_owned(),
            })
        } else {
            if client_id.is_empty() && self.kind.default_client().is_none() {
                return Err("Enter your app's client id (Keel ships none)".into());
            }
            if !self.client_secret.trim().is_empty() {
                secrets.push((key("client_secret"), self.client_secret.trim().to_owned()));
            }
            None
        };
        Ok((
            CloudAccount {
                id,
                label: label.to_owned(),
                kind: self.kind,
                root: (!root.is_empty()).then(|| format!("/{root}")),
                client_id_override: (self.kind != CloudKind::S3 && !client_id.is_empty())
                    .then(|| client_id.to_owned()),
                s3,
            },
            secrets,
        ))
    }

    /// Validates, then stores the secrets and (with `sign_in`) runs the browser sign-in on a
    /// worker; the result arrives through `poll`.
    pub fn submit(
        &mut self,
        existing: &[CloudAccount],
        sign_in: bool,
        store: Arc<dyn SecretStore>,
        events: Sender<RemoteEvent>,
        ctx: &egui::Context,
    ) {
        let (account, secrets) = match self.build(existing) {
            Ok(built) => built,
            Err(e) => return self.error = Some(e),
        };
        let sign_in = sign_in && account.kind != CloudKind::S3;
        let a = account.clone();
        self.start(account, sign_in, ctx, move |cancel| {
            for (k, v) in &secrets {
                store.set(k, v)?;
            }
            if sign_in {
                let client = resolve_client(&a, &*store)?;
                let tokens = oauth_authorize(a.kind, &client, &events, cancel)?;
                anyhow::ensure!(!cancel.load(Ordering::Relaxed), "sign-in cancelled");
                store_tokens(&*store, &a.id, &tokens)?;
            }
            Ok(())
        });
    }

    /// Runs `work` on a worker; `cancel` is set when the user gives up.
    pub fn start(
        &mut self,
        account: CloudAccount,
        sign_in: bool,
        ctx: &egui::Context,
        work: impl FnOnce(&AtomicBool) -> anyhow::Result<()> + Send + 'static,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let (stop, ctx2) = (cancel.clone(), ctx.clone());
        let started = spawn("keel-cloud-sign-in", move || {
            let _ = tx.send(work(&stop).map_err(|e| format!("{e:#}")));
            ctx2.request_repaint();
        });
        if !started {
            self.error = Some("Could not start a worker thread".into());
            return;
        }
        self.error = None;
        self.step = Step::Pending(Box::new(Pending {
            account,
            sign_in,
            started: Instant::now(),
            cancel,
            rx,
        }));
    }

    /// The worker's answer: done, or back to the fields with its error.
    pub fn poll(&mut self) -> Outcome {
        let Step::Pending(p) = &self.step else {
            return Outcome::Open;
        };
        let result = match p.rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return Outcome::Open,
            Err(_) => Err("the sign-in worker stopped".into()),
        };
        match result {
            Ok(()) => {
                let Step::Pending(p) = std::mem::replace(&mut self.step, Step::Fields) else {
                    unreachable!("checked above")
                };
                Outcome::Done(p.account)
            }
            Err(e) => {
                self.step = Step::Fields;
                self.error = Some(e);
                Outcome::Open
            }
        }
    }

    /// Gives up: a running sign-in stops listening (its tokens are never stored).
    pub fn cancel(&mut self) -> Outcome {
        if let Step::Pending(p) = &self.step {
            p.cancel.store(true, Ordering::Relaxed);
        }
        Outcome::Cancelled
    }

    /// Time left for the browser sign-in.
    pub fn left(&self) -> Option<Duration> {
        match &self.step {
            Step::Pending(p) if p.sign_in => Some(AUTH_TIMEOUT.saturating_sub(p.started.elapsed())),
            _ => None,
        }
    }

    fn ui(
        &mut self,
        ctx: &egui::Context,
        existing: &[CloudAccount],
        store: &Arc<dyn SecretStore>,
        events: &Sender<RemoteEvent>,
    ) -> Outcome {
        let polled = self.poll();
        if !matches!(polled, Outcome::Open) {
            return polled;
        }
        let mut outcome = Outcome::Open;
        let mut submit = None;
        let title = match (&self.id, &self.step) {
            (_, Step::Pending(p)) if p.sign_in => format!("Sign in to {}", kind_name(self.kind)),
            (Some(_), _) => format!("Edit {} account", kind_name(self.kind)),
            (None, _) => "Add a cloud account".into(),
        };
        let modal = Modal::new(Id::new("keel-cloud-wizard")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.strong(title);
            ui.add_space(6.0);
            match &self.step {
                Step::Kind => {
                    ui.label("Which service?");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        for kind in [CloudKind::GoogleDrive, CloudKind::Dropbox, CloudKind::S3] {
                            let text = match kind {
                                CloudKind::S3 => "S3-compatible (B2, AWS, MinIO)",
                                k => kind_name(k),
                            };
                            if ui.button(text).clicked() {
                                self.choose(kind);
                            }
                        }
                    });
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        outcome = Outcome::Cancelled;
                    }
                }
                Step::Fields => submit = self.fields_ui(ui, &mut outcome),
                Step::Pending(p) => {
                    match self.left() {
                        Some(left) => {
                            ui.label(format!(
                                "Finish signing in to {} in your browser.",
                                kind_name(self.kind)
                            ));
                            ui.horizontal(|ui| {
                                ui.spinner();
                                let s = left.as_secs();
                                ui.weak(format!("{}:{:02} left", s / 60, s % 60));
                            });
                            ctx.request_repaint_after(Duration::from_secs(1));
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(format!("Saving {} to the OS keychain…", p.account.label));
                            });
                        }
                    }
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        outcome = self.cancel();
                    }
                }
            }
        });
        if let Some(sign_in) = submit {
            self.submit(existing, sign_in, store.clone(), events.clone(), ctx);
        }
        if matches!(outcome, Outcome::Open) && modal.should_close() {
            outcome = self.cancel();
        }
        outcome
    }

    /// The form; Some(sign_in) when submitted.
    fn fields_ui(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) -> Option<bool> {
        let oauth = self.kind != CloudKind::S3;
        let editing = self.id.is_some();
        let mut submit = None;
        egui::Grid::new("cloud-wizard")
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
                let secret = |ui: &mut egui::Ui, name: &str, text: &mut String, hint: &str| {
                    ui.label(name);
                    ui.add(
                        egui::TextEdit::singleline(text)
                            .password(true)
                            .hint_text(hint)
                            .desired_width(300.0),
                    );
                    ui.end_row();
                };
                field(ui, "Label", &mut self.label, kind_name(self.kind));
                if oauth {
                    field(ui, "Root folder", &mut self.root, "/ (optional)");
                    let hint = if self.kind.default_client().is_some() {
                        "built-in (optional)"
                    } else {
                        "required: Keel ships none"
                    };
                    field(ui, "Client id", &mut self.client_id, hint);
                    if self.kind == CloudKind::GoogleDrive {
                        secret(
                            ui,
                            "Client secret",
                            &mut self.client_secret,
                            "optional; kept in the OS keychain",
                        );
                    }
                    ui.label("");
                    ui.hyperlink_to("Bring your own client id", CLIENT_ID_HELP);
                    ui.end_row();
                } else {
                    field(
                        ui,
                        "Endpoint",
                        &mut self.endpoint,
                        "https://s3.us-west-002.backblazeb2.com",
                    );
                    field(ui, "Region", &mut self.region, "us-west-002");
                    field(ui, "Bucket", &mut self.bucket, "");
                    field(ui, "Key prefix", &mut self.root, "(optional)");
                    let hint = if editing {
                        "leave empty to keep the stored one"
                    } else {
                        "kept in the OS keychain"
                    };
                    secret(ui, "Access key id", &mut self.access_key, hint);
                    secret(ui, "Secret key", &mut self.secret_key, hint);
                }
            });
        if let Some(e) = &self.error {
            ui.add_space(4.0);
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if oauth && !editing {
                if ui.button("Sign in…").clicked() {
                    submit = Some(true);
                }
            } else {
                if ui.button("Save").clicked() {
                    submit = Some(false);
                }
                if oauth && ui.button("Sign in again…").clicked() {
                    submit = Some(true);
                }
            }
            if !editing && ui.button("Back").clicked() {
                self.step = Step::Kind;
            }
            if ui.button("Cancel").clicked() {
                *outcome = Outcome::Cancelled;
            }
        });
        submit
    }
}

/// Cloud parts of `AppState` (kept here so state.rs only has small hooks).
impl AppState {
    /// A `cloud:<id>` status from a provider. `cloud-auth:*` progress is the wizard's.
    pub fn cloud_event(&mut self, host_id: &str, status: ConnStatus, detail: String) {
        let Some(id) = host_id.strip_prefix("cloud:") else {
            return;
        };
        let Some(account) = self.clouds.account(id).cloned() else {
            return;
        };
        let before = self
            .clouds
            .status
            .insert(id.to_owned(), (status, detail.clone()))
            .map(|s| s.0);
        let label = &account.label;
        match status {
            ConnStatus::Connected if before != Some(ConnStatus::Connected) => {
                self.toasts.info(format!("Connected to {label}"))
            }
            ConnStatus::Failed if account.kind != CloudKind::S3 && needs_sign_in(&detail) => {
                self.toasts.with_action(
                    format!("{label}: the sign-in expired or was revoked"),
                    "Sign in",
                    Action::Cloud {
                        id: id.to_owned(),
                        cmd: CloudCmd::SignIn,
                    },
                )
            }
            ConnStatus::Failed => self.toasts.error(format!("{label}: {detail}")),
            _ => {}
        }
    }

    /// Per frame: config → router, sidebar rows.
    pub fn cloud_tick(&mut self) {
        self.clouds.sync(&self.router, &self.settings.clouds);
        self.sidebar.clouds =
            crate::sidebar_remotes::cloud_rows(&self.settings.clouds, &self.clouds.status);
    }

    /// Lists every tab on account `id` again.
    fn relist_cloud(&mut self, id: &str) {
        for p in 0..2 {
            for t in 0..self.panes[p].tabs.len() {
                let dir = &self.panes[p].tabs[t].dir;
                if dir.scheme == "cloud" && dir.authority == id {
                    self.list(p, t);
                }
            }
        }
    }

    pub fn cloud_cmd(&mut self, id: String, cmd: CloudCmd) {
        if cmd == CloudCmd::Add {
            self.clouds.wizard = Some(Wizard::default());
            return;
        }
        let Some(account) = self.clouds.account(&id).cloned() else {
            return self
                .toasts
                .error("That cloud account is no longer configured");
        };
        match cmd {
            CloudCmd::Reconnect => {
                self.clouds.reconnect(&self.router, &id);
                self.relist_cloud(&id);
            }
            CloudCmd::Edit => self.clouds.wizard = Some(Wizard::edit(&account)),
            CloudCmd::Remove => {
                self.dialog = Some(crate::dialogs::Dialog::Confirm {
                    text: format!(
                        "Remove {}? Keel signs out and deletes its keys from this PC; the \
                         files in the cloud stay.",
                        account.label
                    ),
                    on_yes: Action::Cloud {
                        id,
                        cmd: CloudCmd::RemoveNow,
                    },
                });
            }
            CloudCmd::RemoveNow => {
                self.settings.clouds.retain(|a| a.id != id);
                sign_out(
                    account,
                    self.clouds.secrets.clone(),
                    self.tx.clone(),
                    self.ctx.clone(),
                );
            }
            CloudCmd::SignIn => {
                if let Some(old) = &mut self.clouds.wizard {
                    old.cancel();
                }
                let mut w = Wizard::edit(&account);
                w.submit(
                    &self.settings.clouds,
                    true,
                    self.clouds.secrets.clone(),
                    self.clouds.events.clone(),
                    &self.ctx,
                );
                self.clouds.wizard = Some(w);
            }
            CloudCmd::Add => {}
        }
    }

    /// The wizard, every frame. A finished account is saved to `settings.clouds` (written by
    /// the persistence worker) and reconnected so its new secrets are read.
    pub fn cloud_modals(&mut self, ctx: &egui::Context) {
        let Some(w) = &mut self.clouds.wizard else {
            return;
        };
        let outcome = w.ui(
            ctx,
            &self.settings.clouds,
            &self.clouds.secrets,
            &self.clouds.events,
        );
        self.cloud_outcome(outcome);
    }

    pub fn cloud_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Open => {}
            Outcome::Cancelled => self.clouds.wizard = None,
            Outcome::Done(account) => {
                self.clouds.wizard = None;
                let id = account.id.clone();
                let text = format!("Saved {}", account.label);
                match self.settings.clouds.iter_mut().find(|a| a.id == id) {
                    Some(slot) => *slot = account,
                    None => self.settings.clouds.push(account),
                }
                self.cloud_tick();
                self.clouds.reconnect(&self.router, &id);
                self.relist_cloud(&id);
                self.toasts.info(text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{Jobs, Transfer};
    use crate::keys::Action;
    use keel_vfs::cloud::MemoryStore;
    use keel_vfs::{Conflict, Kind};

    fn account(id: &str, kind: CloudKind) -> CloudAccount {
        CloudAccount {
            id: id.into(),
            label: format!("{id} label"),
            kind,
            root: None,
            client_id_override: None,
            s3: None,
        }
    }

    fn settle(w: &mut Wizard) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match w.poll() {
                Outcome::Open if matches!(w.step, Step::Pending(_)) => {
                    assert!(Instant::now() < deadline, "worker never answered");
                    std::thread::sleep(Duration::from_millis(10));
                }
                o => return o,
            }
        }
    }

    #[test]
    fn wizard_walks_kind_fields_pending_done_or_cancelled() {
        let ctx = egui::Context::default();
        let mut w = Wizard::default();
        assert!(matches!(w.step, Step::Kind));
        w.choose(CloudKind::S3);
        assert!(matches!(w.step, Step::Fields));
        assert_eq!(w.label, "S3");
        // Incomplete S3 fields stay on the form.
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
        let (events, _rx) = crossbeam_channel::unbounded();
        w.submit(&[], false, store.clone(), events.clone(), &ctx);
        assert!(matches!(w.step, Step::Fields));
        assert!(w.error.as_deref().unwrap().contains("endpoint"));
        w.endpoint = "https://s3.example.invalid".into();
        w.region = "us-west-002".into();
        w.bucket = "photos".into();
        w.access_key = "KEYID".into();
        w.secret_key = "SECRET-xyz".into();
        w.label = "B2 photos".into();
        w.submit(&[], false, store.clone(), events.clone(), &ctx);
        assert!(matches!(w.step, Step::Pending(_)));
        assert_eq!(w.left(), None, "no browser countdown for S3");
        let Outcome::Done(a) = settle(&mut w) else {
            panic!("S3 save did not finish")
        };
        assert_eq!(a.id, "b2-photos");
        assert_eq!(
            store.get("b2-photos/secret_access_key").unwrap().as_deref(),
            Some("SECRET-xyz")
        );

        // OAuth: pending with a countdown until the worker answers; cancel ends it.
        let mut w = Wizard::default();
        w.choose(CloudKind::Dropbox);
        w.client_id = "my-app-key".into();
        let (account, secrets) = w.build(&[]).unwrap();
        assert!(secrets.is_empty());
        assert_eq!(account.client_id_override.as_deref(), Some("my-app-key"));
        let (seen_tx, seen) = crossbeam_channel::bounded(1);
        w.start(account.clone(), true, &ctx, move |cancel| {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = seen_tx.send(());
            anyhow::bail!("sign-in cancelled")
        });
        assert!(w.left().unwrap() > Duration::from_secs(100));
        assert!(matches!(w.poll(), Outcome::Open));
        assert!(matches!(w.cancel(), Outcome::Cancelled));
        seen.recv_timeout(Duration::from_secs(5))
            .expect("the worker saw the cancel");
        // A failed sign-in returns to the form with the error; a good one is Done.
        let mut w = Wizard::edit(&account);
        w.start(account.clone(), true, &ctx, |_| {
            anyhow::bail!("browser said no")
        });
        assert!(matches!(settle(&mut w), Outcome::Open));
        assert!(matches!(w.step, Step::Fields));
        assert_eq!(w.error.as_deref(), Some("browser said no"));
        w.start(account.clone(), true, &ctx, |_| Ok(()));
        assert!(matches!(settle(&mut w), Outcome::Done(a) if a == account));
        // No client id anywhere: refused before any browser opens.
        let mut w = Wizard::default();
        w.choose(CloudKind::GoogleDrive);
        assert!(w.build(&[]).unwrap_err().contains("client id"));
    }

    #[test]
    fn clouds_round_trip_through_config_without_secrets() {
        let mut w = Wizard::default();
        w.choose(CloudKind::S3);
        w.label = "Backup".into();
        w.root = "/nightly/".into();
        w.endpoint = "https://s3.example.invalid".into();
        w.region = "eu-central-1".into();
        w.bucket = "bkp".into();
        w.access_key = "AKIA-not-in-config".into();
        w.secret_key = "hunter2-very-secret".into();
        let (s3, secrets) = w.build(&[]).unwrap();
        assert_eq!(s3.root.as_deref(), Some("/nightly"));
        assert_eq!(secrets.len(), 2);
        let mut g = Wizard::default();
        g.choose(CloudKind::GoogleDrive);
        g.client_id = "123.apps.googleusercontent.com".into();
        g.client_secret = "GOCSPX-secret".into();
        let (drive, secrets) = g.build(std::slice::from_ref(&s3)).unwrap();
        assert_eq!(
            secrets,
            [(
                "google-drive/client_secret".to_owned(),
                "GOCSPX-secret".to_owned()
            )]
        );
        let settings = Settings {
            clouds: vec![s3, drive],
            ..Settings::default()
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert!(text.contains("[[clouds]]"), "{text}");
        for secret in ["hunter2", "AKIA", "GOCSPX"] {
            assert!(!text.contains(secret), "{secret} leaked into config.toml");
        }
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back, settings);
        // Editing keeps the id and the stored keys (empty fields).
        let edit = Wizard::edit(&settings.clouds[0]);
        let (again, secrets) = edit.build(&settings.clouds).unwrap();
        assert_eq!(again, settings.clouds[0]);
        assert!(secrets.is_empty());
    }

    #[test]
    fn sidebar_rows_follow_config_and_status() {
        let accounts = vec![
            account("drive", CloudKind::GoogleDrive),
            account("b2", CloudKind::S3),
        ];
        let mut status = HashMap::new();
        status.insert("b2".to_owned(), (ConnStatus::Failed, "unreachable".into()));
        let rows = crate::sidebar_remotes::cloud_rows(&accounts, &status);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "drive label");
        assert_eq!(rows[0].status, ConnStatus::Disconnected);
        assert_eq!(rows[0].root, VPath::parse("cloud://drive/").unwrap());
        assert_eq!(rows[1].status, ConnStatus::Failed);
        assert_eq!(rows[1].detail, "unreachable");
    }

    #[test]
    fn delete_asks_with_the_service_wording() {
        assert_eq!(
            delete_text(2, RemoveKind::Trash),
            "Move 2 items to the Drive trash?"
        );
        assert_eq!(
            delete_text(1, RemoveKind::RecoverableDelete),
            "Delete 1 item on Dropbox? (recoverable for 30 days)"
        );
        assert_eq!(
            delete_text(3, RemoveKind::Permanent),
            "Delete 3 items from the bucket permanently?"
        );
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        state.settings.clouds = vec![account("dbx", CloudKind::Dropbox)];
        state.cloud_tick();
        let remote = root_of("dbx");
        state.tab_mut(0).dir = remote.clone();
        state
            .tab_mut(0)
            .set_listing(crate::tab::Listing::new(vec![crate::tab::test_entry(
                &remote,
                "a.txt",
                Kind::File,
                1,
            )]));
        state.run(0, Action::SelectAll);
        state.run(0, Action::Delete);
        match &state.dialog {
            Some(crate::dialogs::Dialog::Confirm { text, on_yes }) => {
                assert_eq!(text, "Delete 1 item on Dropbox? (recoverable for 30 days)");
                assert!(matches!(on_yes, Action::DeleteRemote(p) if p.len() == 1));
            }
            _ => panic!("no confirm dialog"),
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

    /// The real cloud provider over opendal's memory service, so jobs take the cloud path.
    #[test]
    fn transfer_jobs_go_local_to_cloud_and_back() {
        let root = std::env::temp_dir().join(format!("keel-cloud-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("local/src/sub")).unwrap();
        std::fs::create_dir_all(root.join("back")).unwrap();
        std::fs::write(root.join("local/src/a.txt"), "alpha").unwrap();
        std::fs::write(root.join("local/src/sub/b.txt"), "beta").unwrap();
        let op = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
        let mem = Arc::new(
            CloudProvider::with_operator(
                account("mem", CloudKind::S3),
                op,
                crossbeam_channel::unbounded().0,
            )
            .unwrap(),
        );
        let router = Router::new();
        router.register_cloud_provider("mem".into(), mem.clone());
        let router = Arc::new(router);
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut jobs = Jobs::new(egui::Context::default());
        let cloud = root_of("mem");
        let up = Transfer {
            src: vec![VPath::local(root.join("local/src"))],
            dst: cloud.clone(),
            mv: false,
            extract: None,
        };
        let id = jobs.start(up, Conflict::Skip, router.clone(), tx.clone());
        run_job(&mut jobs, &rx, id).expect("upload");
        assert!(jobs.is_remote(id), "cloud jobs toast their failures");
        let mut text = String::new();
        mem.read(&cloud.join("src").join("sub").join("b.txt"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "beta");
        let down = Transfer {
            src: vec![cloud.join("src")],
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
        assert!(
            mem.stat(&cloud.join("src")).is_err(),
            "moved out of the cloud"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn revoked_sign_in_toasts_a_sign_in_button_that_reruns_the_flow() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        state.settings.clouds = vec![account("drive", CloudKind::GoogleDrive)];
        state.cloud_tick();
        assert_eq!(state.sidebar.clouds[0].status, ConnStatus::Disconnected);
        state.apply(Msg::Remote(RemoteEvent::Status {
            host_id: "cloud:drive".into(),
            status: ConnStatus::Failed,
            detail: "the cloud sign-in expired or was revoked; sign in again".into(),
        }));
        state.cloud_tick();
        assert_eq!(state.sidebar.clouds[0].status, ConnStatus::Failed);
        let toast = state
            .toasts
            .list
            .iter()
            .find(|t| t.action.is_some())
            .expect("a toast with a button");
        assert_eq!(
            toast.text,
            "drive label: the sign-in expired or was revoked"
        );
        let (button, action) = toast.action.clone().unwrap();
        assert_eq!(button, "Sign in");
        assert_eq!(
            action,
            Action::Cloud {
                id: "drive".into(),
                cmd: CloudCmd::SignIn
            }
        );
        // The button reruns the wizard's sign-in submit for this account. No client id is
        // configured here, so it stops at validation (no browser opens) and the form says why.
        state.run(0, action);
        let w = state.clouds.wizard.as_mut().expect("sign-in dialog");
        assert!(matches!(w.step, Step::Pending(_)) || w.error.is_some());
        let outcome = settle(w);
        assert!(matches!(outcome, Outcome::Open));
        assert!(
            w.error.as_deref().unwrap().contains("client id"),
            "{:?}",
            w.error
        );
        // Other failures are plain error toasts; connection comes back with an info toast.
        state.apply(Msg::Remote(RemoteEvent::Status {
            host_id: "cloud:drive".into(),
            status: ConnStatus::Connected,
            detail: "connected".into(),
        }));
        assert!(state
            .toasts
            .list
            .iter()
            .any(|t| t.text == "Connected to drive label"));
    }
}
