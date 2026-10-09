//! Cloud accounts in the app: `[[clouds]]` from config.toml registered with the Router,
//! connection status from the providers, the account wizard (Settings → Cloud: kind →
//! fields → browser sign-in or S3 keys), re-sign-in from a toast, and removal (revoke +
//! keychain cleanup, or keychain cleanup only). Tokens and keys only ever go to the
//! `SecretStore` (OS keychain), on workers, and only once the sign-in succeeded; no IO here
//! runs on the UI thread.

use crate::keys::Action;
use crate::settings::Settings;
use crate::state::{AppState, Msg};
use crate::worker::{send, spawn};
use crossbeam_channel::{Receiver, Sender};
use egui::{Id, Modal};
use keel_vfs::cloud::{
    self, forget_account, oauth_authorize, resolve_client, store_tokens, OAuthClient, OAuthTokens,
    AUTH_TIMEOUT,
};
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
use zeroize::{Zeroize, Zeroizing};

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
    /// Confirmed: sign out (revoke the grant) and forget the account.
    RemoveNow,
    /// Confirmed: forget the account and its keys on this PC only (no revoke).
    RemoveLocal,
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

/// Errors about one folder (missing, forbidden, not a folder): the service answered, so
/// they say nothing about the account's connection or sign-in.
fn folder_error(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{NotADirectory, NotFound, PermissionDenied};
    let detail = format!("{e:#}");
    !needs_sign_in(&detail)
        && !detail.contains("(HTTP 401)")
        && e.chain().any(|c| {
            c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| matches!(io.kind(), NotFound | PermissionDenied | NotADirectory))
        })
}

/// An `http://` S3 endpoint outside this PC and the LAN: files and signed requests cross
/// the internet unencrypted.
pub fn insecure_endpoint(endpoint: &str) -> bool {
    let Some(rest) = endpoint.trim().strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            !(ip.is_loopback() || ip.is_private() || ip.is_link_local())
        }
        // fc00::/7 unique local, fe80::/10 link local.
        Ok(std::net::IpAddr::V6(ip)) => {
            let seg = ip.segments()[0];
            !(ip.is_loopback() || seg & 0xfe00 == 0xfc00 || seg & 0xffc0 == 0xfe80)
        }
        // Single-label names (`minio`) and the usual LAN suffixes are local.
        Err(_) => {
            let host = host.to_ascii_lowercase();
            let lan = [".local", ".lan", ".home.arpa", ".internal", ".localhost"];
            !(host == "localhost" || !host.contains('.') || lan.iter().any(|s| host.ends_with(s)))
        }
    }
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
            Err(e) if !folder_error(e) => self.status(ConnStatus::Failed, format!("{e:#}")),
            _ => self.status(ConnStatus::Connected, "connected".into()),
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
    fn create_new_cancellable<'a>(
        &self,
        p: &VPath,
        cancel: &'a AtomicBool,
    ) -> anyhow::Result<Box<dyn Write + Send + 'a>> {
        self.get()?.create_new_cancellable(p, cancel)
    }
    fn uploads_on_flush(&self) -> Option<&'static str> {
        self.get().ok()?.uploads_on_flush()
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
    fn list_complete(&self, dir: &VPath) -> anyhow::Result<Vec<Entry>> {
        self.get()?.list_complete(dir)
    }
    /// Known from the account without connecting.
    fn remove_kind(&self) -> RemoveKind {
        self.account.kind.remove_kind()
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
    /// The account whose "Remove?" question is open.
    pub removing: Option<String>,
    /// `cloud::check_cpu`: Err(why) when this CPU cannot run the TLS crypto at all.
    pub cpu: Result<(), String>,
    /// How a sign-out revokes the grant (`cloud::revoke_tokens`; a recorder in tests).
    pub revoke: Revoke,
}

/// Revokes a grant from tokens in memory.
pub type Revoke = fn(CloudKind, &OAuthTokens, &OAuthClient) -> anyhow::Result<()>;

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
            removing: None,
            cpu: cloud::check_cpu().map_err(|e| e.to_string()),
            revoke: cloud::revoke_tokens,
        }
    }

    /// Registers new or changed accounts with the router and drops removed ones (their ids
    /// are returned). Cheap when nothing changed (every frame); registering does no IO
    /// (`LazyCloud`).
    pub fn sync(&mut self, router: &Router, accounts: &[CloudAccount]) -> Vec<String> {
        if self.accounts == accounts {
            return Vec::new();
        }
        for a in accounts {
            if !self.accounts.contains(a) {
                self.register(router, a);
            }
        }
        let mut removed = Vec::new();
        for old in &self.accounts {
            if !accounts.iter().any(|a| a.id == old.id) {
                router.unregister_cloud(&old.id);
                removed.push(old.id.clone());
            }
        }
        self.status
            .retain(|id, _| accounts.iter().any(|a| a.id == *id));
        self.accounts = accounts.to_vec();
        removed
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
    /// Remove asks in the same dialog as the sidebar's (`AppState::cloud_modals`).
    pub fn settings_page(&mut self, ui: &mut egui::Ui, s: &mut Settings, _tx: &Sender<Msg>) {
        ui.checkbox(
            &mut s.remote_thumbnails,
            "Thumbnails for remote and cloud files",
        )
        .on_hover_text("Grid view then downloads every image and video it shows");
        ui.add_space(6.0);
        if s.clouds.is_empty() {
            ui.weak("No cloud accounts yet.");
        }
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
                        if ui.button("Remove…").clicked() {
                            self.removing = Some(a.id.clone());
                        }
                    });
                    ui.end_row();
                }
            });
        ui.add_space(6.0);
        let add = ui.add_enabled(self.cpu.is_ok(), egui::Button::new("Add account…"));
        if add.clicked() {
            self.wizard = Some(Wizard::default());
        }
        if let Err(why) = &self.cpu {
            ui.colored_label(ui.visuals().error_fg_color, why);
        }
    }
}

/// Deletes the keychain entries and, with `revoke`, then revokes the grant where the service
/// allows it, on a worker (both block).
fn sign_out(
    account: CloudAccount,
    secrets: Arc<dyn SecretStore>,
    revoke: Option<Revoke>,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    spawn("keel-cloud-sign-out", move || {
        let label = &account.label;
        let Some(revoke) = revoke else {
            forget_account(&*secrets, &account.id);
            let text = format!("Removed {label}; its keys are deleted from this PC");
            return send(&tx, &ctx, Msg::Info(text));
        };
        let revoked = cloud::sign_out_with(&account, &*secrets, |t, c| revoke(account.kind, t, c));
        let msg = match revoked {
            Ok(()) => Msg::Info(format!(
                "Removed {label}; signed out and its keys are deleted from this PC"
            )),
            Err(e) => Msg::Toast(format!(
                "Removed {label} and deleted its keys from this PC, but the service did not \
                 confirm the sign-out: {e:#}"
            )),
        };
        send(&tx, &ctx, msg);
    });
}

/// The "Remove?" question: what each answer does on that service.
pub fn remove_text(a: &CloudAccount) -> String {
    let label = &a.label;
    match a.kind {
        CloudKind::S3 => format!(
            "Remove {label}? Keel deletes its keys from this PC (revoke them in the \
             provider's console); the files in the bucket stay."
        ),
        _ => format!(
            "Remove {label}?\n\nSign out and remove: signs this app out of {label} everywhere \
             it uses this client id and deletes its keys from this PC.\n\nRemove from this PC \
             only: deletes its keys here; other devices stay signed in.\n\nThe files in the \
             cloud stay."
        ),
    }
}

/// The "Remove?" dialog: Some(answer) once answered, `Some(None)` = cancelled.
fn remove_ui(ctx: &egui::Context, a: &CloudAccount) -> Option<Option<CloudCmd>> {
    let mut answer = None;
    let modal = Modal::new(Id::new("keel-cloud-remove")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.label(remove_text(a));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if a.kind == CloudKind::S3 {
                if ui.button("Remove").clicked() {
                    answer = Some(Some(CloudCmd::RemoveLocal));
                }
            } else {
                if ui.button("Sign out and remove").clicked() {
                    answer = Some(Some(CloudCmd::RemoveNow));
                }
                if ui.button("Remove from this PC only").clicked() {
                    answer = Some(Some(CloudCmd::RemoveLocal));
                }
            }
            if ui.button("Cancel").clicked() {
                answer = Some(None);
            }
        });
    });
    if answer.is_none() && modal.should_close() {
        answer = Some(None);
    }
    answer
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
/// to the keychain only; they leave the form when it is submitted and are wiped from
/// memory when dropped.
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
    pub client_secret: Zeroizing<String>,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: Zeroizing<String>,
    pub secret_key: Zeroizing<String>,
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
            client_secret: Zeroizing::default(),
            endpoint: String::new(),
            region: String::new(),
            bucket: String::new(),
            access_key: Zeroizing::default(),
            secret_key: Zeroizing::default(),
            error: None,
        }
    }
}

/// The secrets to store: (`<id>/<field>`, value).
pub type Secrets = Vec<(String, Zeroizing<String>)>;

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
        if let Some(id) = &self.id {
            if !cloud::valid_id(id) {
                return Err(format!(
                    "The id \"{id}\" in config.toml must be lowercase letters, digits, - or _;                      remove this account and add it again"
                ));
            }
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
                    secrets.push((key("access_key_id"), ak.to_owned().into()));
                    secrets.push((key("secret_access_key"), sk.to_owned().into()));
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
                let secret = self.client_secret.trim().to_owned().into();
                secrets.push((key("client_secret"), secret));
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

    /// Validates, then (with `sign_in`) runs the browser sign-in and stores the secrets on a
    /// worker (`save_account`); the result arrives through `poll`. The typed secrets move
    /// out of the form.
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
        for typed in [
            &mut self.client_secret,
            &mut self.access_key,
            &mut self.secret_key,
        ] {
            typed.zeroize();
        }
        let sign_in = sign_in && account.kind != CloudKind::S3;
        let old = existing.iter().find(|a| a.id == account.id);
        let save = Save {
            new: old.is_none(),
            same_client: old.is_some_and(|o| o.client_id_override == account.client_id_override),
            sign_in,
            secrets,
        };
        let a = account.clone();
        self.start(account, sign_in, ctx, move |cancel| {
            save_account(&a, save, &*store, &events, cancel)
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

    /// Gives up: a running sign-in stops listening (its tokens are revoked, never stored).
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
        if !oauth && insecure_endpoint(&self.endpoint) {
            ui.add_space(4.0);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "This http:// endpoint is not on this PC or your network: files travel \
                 unencrypted. Use https:// if the service offers it.",
            );
        }
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

/// What the wizard's worker stores.
struct Save {
    /// Not in config.toml yet: a failed save leaves nothing behind in the keychain.
    new: bool,
    /// The stored client secret belongs to this client id.
    same_client: bool,
    sign_in: bool,
    secrets: Secrets,
}

/// The wizard's worker. With `sign_in`, the browser sign-in runs first, with the client
/// built from the form in memory: nothing is written before it succeeds, so a cancel or a
/// timeout leaves the keychain as it was (and never pairs a new client id with an old
/// secret). Then the form's secrets and the tokens are stored. A failed new account's
/// entries are all deleted.
fn save_account(
    a: &CloudAccount,
    save: Save,
    store: &dyn SecretStore,
    events: &Sender<RemoteEvent>,
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let typed_secret = save
        .secrets
        .iter()
        .find(|(k, _)| k.ends_with("/client_secret"))
        .map(|(_, v)| v.to_string());
    let result = (|| {
        let tokens = if save.sign_in {
            let mut client = resolve_client(a, store)?;
            if let Some(secret) = &typed_secret {
                client.secret = Some(secret.clone());
            } else if !save.same_client {
                client.secret = a
                    .client_id_override
                    .is_none()
                    .then(|| a.kind.default_client().and_then(|c| c.secret))
                    .flatten();
            }
            let tokens = oauth_authorize(a.kind, &client, events, cancel)?;
            if cancel.load(Ordering::Relaxed) {
                // The service minted a grant nobody will use: take it back.
                let _ = cloud::revoke_tokens(a.kind, &tokens, &client);
                anyhow::bail!("sign-in cancelled");
            }
            Some(tokens)
        } else {
            None
        };
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "cancelled");
        for (k, v) in &save.secrets {
            store.set(k, v)?;
        }
        if typed_secret.is_none() && !save.same_client {
            store.delete(&format!("{}/client_secret", a.id))?;
        }
        if let Some(tokens) = &tokens {
            store_tokens(store, &a.id, tokens)?;
        }
        // ponytail: an edit cancelled during these writes keeps them; only new accounts
        // are rolled back.
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "cancelled");
        Ok(())
    })();
    if result.is_err() && save.new {
        forget_account(store, &a.id);
    }
    result
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
            .insert(id.to_owned(), (status, detail.clone()));
        // Toasts only on a change of state (a lost sign-in counts as its own state).
        let state = |s: ConnStatus, d: &str| (s, s == ConnStatus::Failed && needs_sign_in(d));
        if before.is_some_and(|(s, d)| state(s, &d) == state(status, &detail)) {
            return;
        }
        let label = &account.label;
        match status {
            ConnStatus::Connected => self.toasts.info(format!("Connected to {label}")),
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

    /// Per frame: config → router, sidebar rows. Tabs on a removed account go home (a dead
    /// cloud tab is never saved in the session).
    pub fn cloud_tick(&mut self) {
        for id in self.clouds.sync(&self.router, &self.settings.clouds) {
            for p in 0..2 {
                for t in 0..self.panes[p].tabs.len() {
                    let dir = &self.panes[p].tabs[t].dir;
                    if dir.scheme == "cloud" && dir.authority == id {
                        self.panes[p].tabs[t] = crate::tab::Tab::new(self.home.clone());
                        self.list(p, t);
                    }
                }
            }
        }
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
            match &self.clouds.cpu {
                Ok(()) => self.clouds.wizard = Some(Wizard::default()),
                Err(why) => self.toasts.error(why.clone()),
            }
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
            CloudCmd::Remove => self.clouds.removing = Some(id),
            CloudCmd::RemoveNow | CloudCmd::RemoveLocal => {
                self.settings.clouds.retain(|a| a.id != id);
                self.cloud_tick();
                let revoke = (cmd == CloudCmd::RemoveNow).then_some(self.clouds.revoke);
                sign_out(
                    account,
                    self.clouds.secrets.clone(),
                    revoke,
                    self.tx.clone(),
                    self.ctx.clone(),
                );
            }
            CloudCmd::SignIn => {
                if self.clouds.wizard.is_some() {
                    return self
                        .toasts
                        .info("Finish or close the open cloud account dialog first");
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

    /// The wizard and the "Remove?" question, every frame. A finished account is saved to
    /// `settings.clouds` (written by the persistence worker) and reconnected so its new
    /// secrets are read.
    pub fn cloud_modals(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.clouds.removing.clone() {
            let Some(account) = self.clouds.account(&id).cloned() else {
                self.clouds.removing = None;
                return;
            };
            if let Some(answer) = remove_ui(ctx, &account) {
                self.clouds.removing = None;
                if let Some(cmd) = answer {
                    self.cloud_cmd(id, cmd);
                }
            }
            return;
        }
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
        w.access_key = Zeroizing::new("KEYID".into());
        w.secret_key = Zeroizing::new("SECRET-xyz".into());
        w.label = "B2 photos".into();
        w.submit(&[], false, store.clone(), events.clone(), &ctx);
        assert!(matches!(w.step, Step::Pending(_)));
        assert!(w.secret_key.is_empty(), "the typed key left the form");
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
        // Ids are lowercase (the keychain ignores case): generated so, refused otherwise.
        w.label = "My Drive".into();
        w.client_id = "123.apps.googleusercontent.com".into();
        assert_eq!(w.build(&[]).unwrap().0.id, "my-drive");
        let mut upper = Wizard::edit(&CloudAccount {
            id: "Drive".into(),
            ..account
        });
        upper.client_id = "123.apps.googleusercontent.com".into();
        assert!(upper.build(&[]).unwrap_err().contains("lowercase"));
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
        w.access_key = Zeroizing::new("AKIA-not-in-config".into());
        w.secret_key = Zeroizing::new("hunter2-very-secret".into());
        let (s3, secrets) = w.build(&[]).unwrap();
        assert_eq!(s3.root.as_deref(), Some("/nightly"));
        assert_eq!(secrets.len(), 2);
        let mut g = Wizard::default();
        g.choose(CloudKind::GoogleDrive);
        g.client_id = "123.apps.googleusercontent.com".into();
        g.client_secret = Zeroizing::new("GOCSPX-secret".into());
        let (drive, secrets) = g.build(std::slice::from_ref(&s3)).unwrap();
        assert_eq!(
            secrets,
            [(
                "google-drive/client_secret".to_owned(),
                Zeroizing::new("GOCSPX-secret".to_owned())
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
        // A cloud path whose account is gone is refused, never "Move to trash?".
        state.dialog = None;
        let ghost = root_of("ghost");
        state.tab_mut(0).dir = ghost.clone();
        state
            .tab_mut(0)
            .set_listing(crate::tab::Listing::new(vec![crate::tab::test_entry(
                &ghost,
                "a.txt",
                Kind::File,
                1,
            )]));
        state.run(0, Action::SelectAll);
        state.run(0, Action::Delete);
        assert!(state.dialog.is_none());
        assert!(state
            .toasts
            .list
            .iter()
            .any(|t| t.text == "cloud://ghost/a.txt: unknown cloud account"));
    }

    /// Nothing reaches the keychain before the sign-in (or the S3 save) went through; a new
    /// account that fails or is cancelled leaves no entries at all.
    #[test]
    fn cancelled_or_failed_adds_leave_no_keychain_entries() {
        let store = MemoryStore::default();
        let (events, _rx) = crossbeam_channel::unbounded();
        let s3 = account("b2", CloudKind::S3);
        let keys = |new| Save {
            new,
            same_client: !new,
            sign_in: false,
            secrets: vec![
                (
                    "b2/access_key_id".to_owned(),
                    Zeroizing::new("AK".to_owned()),
                ),
                (
                    "b2/secret_access_key".to_owned(),
                    Zeroizing::new("SK".to_owned()),
                ),
            ],
        };
        let (cancelled, go) = (AtomicBool::new(true), AtomicBool::new(false));
        // Leftovers of an earlier attempt go too.
        store.set("b2/tokens", "{}").unwrap();
        assert!(save_account(&s3, keys(true), &store, &events, &cancelled).is_err());
        assert!(store.keys().is_empty(), "{:?}", store.keys());
        // A cancelled edit keeps the stored keys.
        store.set("b2/access_key_id", "OLD").unwrap();
        assert!(save_account(&s3, keys(false), &store, &events, &cancelled).is_err());
        assert_eq!(store.keys(), ["b2/access_key_id"]);
        // A sign-in that fails (here: no client id, before any browser) does not store the
        // typed client secret either.
        let drive = account("drive", CloudKind::GoogleDrive);
        let sign_in = Save {
            new: true,
            same_client: false,
            sign_in: true,
            secrets: vec![(
                "drive/client_secret".to_owned(),
                Zeroizing::new("GOCSPX".to_owned()),
            )],
        };
        let err = save_account(&drive, sign_in, &store, &events, &go).unwrap_err();
        assert!(format!("{err:#}").contains("client id"), "{err:#}");
        assert_eq!(store.keys(), ["b2/access_key_id"]);
        // Not cancelled: stored.
        save_account(&s3, keys(false), &store, &events, &go).unwrap();
        assert_eq!(
            store.get("b2/access_key_id").unwrap().as_deref(),
            Some("AK")
        );
    }

    static REVOKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn record_revoke(_: CloudKind, t: &OAuthTokens, _: &OAuthClient) -> anyhow::Result<()> {
        assert_eq!(t.refresh.as_deref(), Some("rt"));
        REVOKED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[test]
    fn remove_signs_out_everywhere_or_on_this_pc_only() {
        let dir = VPath::local(std::env::temp_dir());
        let mut state = AppState::new(egui::Context::default(), Arc::new(Router::new()), dir);
        state.clouds.revoke = record_revoke;
        let store = state.clouds.secrets.clone();
        let drive = account("drive", CloudKind::GoogleDrive);
        assert!(remove_text(&drive)
            .contains("signs this app out of drive label everywhere it uses this client id"));
        let tokens = OAuthTokens {
            access: "at".into(),
            refresh: Some("rt".into()),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(3600),
        };
        for (cmd, revokes) in [(CloudCmd::RemoveLocal, 0), (CloudCmd::RemoveNow, 1)] {
            state.settings.clouds = vec![drive.clone()];
            state.cloud_tick();
            store_tokens(&*store, "drive", &tokens).unwrap();
            state.tab_mut(0).dir = root_of("drive");
            state.cloud_cmd("drive".into(), CloudCmd::Remove);
            assert_eq!(
                state.clouds.removing.as_deref(),
                Some("drive"),
                "asks first"
            );
            state.clouds.removing = None;
            state.cloud_cmd("drive".into(), cmd);
            assert!(state.settings.clouds.is_empty());
            assert_eq!(state.tab(0).dir, state.home, "its tab went home");
            loop {
                match state
                    .rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("an answer")
                {
                    Msg::Info(t) | Msg::Toast(t) if t.starts_with("Removed") => break,
                    _ => {}
                }
            }
            assert_eq!(store.get("drive/tokens").unwrap(), None);
            assert_eq!(REVOKED.load(Ordering::SeqCst), revokes, "{cmd:?}");
        }
    }

    #[test]
    fn only_account_errors_change_the_account_status() {
        let io = |kind, text: &str| anyhow::Error::from(std::io::Error::new(kind, text.to_owned()));
        use std::io::ErrorKind as K;
        assert!(folder_error(&io(K::NotFound, "cloud://d/x: not found")));
        assert!(folder_error(&io(
            K::PermissionDenied,
            "cloud://d/x: cloud PermissionDenied (HTTP 403)"
        )));
        assert!(!folder_error(&io(
            K::PermissionDenied,
            "the cloud sign-in expired or was revoked; sign in again"
        )));
        assert!(!folder_error(&io(
            K::PermissionDenied,
            "cloud://d/x: cloud Unexpected (HTTP 401)"
        )));
        assert!(!folder_error(&io(
            K::ConnectionRefused,
            "cannot reach the cloud service"
        )));
    }

    #[test]
    fn plain_http_endpoints_off_the_lan_are_flagged() {
        for bad in [
            "http://s3.example.com",
            "http://8.8.8.8:9000/b",
            "http://[2001:db8::1]:9000",
        ] {
            assert!(insecure_endpoint(bad), "{bad}");
        }
        for ok in [
            "https://s3.example.com",
            "http://localhost:9000",
            "http://127.0.0.1:9000",
            "http://192.168.1.81:9000",
            "http://10.0.0.5",
            "http://minio:9000",
            "http://nas.local:9000",
            "http://[::1]:9000",
            "http://[fd00::5]:9000",
        ] {
            assert!(!insecure_endpoint(ok), "{ok}");
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
        // The same failure again is no new toast.
        state.apply(Msg::Remote(RemoteEvent::Status {
            host_id: "cloud:drive".into(),
            status: ConnStatus::Failed,
            detail: "the cloud sign-in expired or was revoked; sign in again".into(),
        }));
        assert_eq!(
            state
                .toasts
                .list
                .iter()
                .filter(|t| t.action.is_some())
                .count(),
            1
        );
        // The button reruns the wizard's sign-in submit for this account. No client id is
        // configured here, so it stops at validation (no browser opens) and the form says why.
        state.run(0, action.clone());
        let w = state.clouds.wizard.as_mut().expect("sign-in dialog");
        assert!(matches!(w.step, Step::Pending(_)) || w.error.is_some());
        let outcome = settle(w);
        assert!(matches!(outcome, Outcome::Open));
        assert!(
            w.error.as_deref().unwrap().contains("client id"),
            "{:?}",
            w.error
        );
        // Clicked again while that dialog is open: it stays as it is, and a toast says why.
        state.run(0, action);
        assert!(state.clouds.wizard.as_ref().unwrap().error.is_some());
        assert!(state
            .toasts
            .list
            .iter()
            .any(|t| t.text.starts_with("Finish or close")));
        // A CPU the TLS crypto cannot run on: "Add" says why instead of opening the wizard.
        state.clouds.wizard = None;
        state.clouds.cpu = Err("cloud accounts need a CPU with AES".into());
        state.cloud_cmd(String::new(), CloudCmd::Add);
        assert!(state.clouds.wizard.is_none());
        assert!(state
            .toasts
            .list
            .iter()
            .any(|t| t.text.contains("need a CPU")));
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
