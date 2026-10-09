//! Cloud storage as folders: Google Drive, Dropbox and S3-compatible buckets over `opendal`,
//! addressed as `cloud://<account id>/path`. Synchronous like the SFTP provider: call on a
//! worker thread. Tokens and keys live in a `SecretStore` (the OS keychain in the app),
//! never in config, logs or error messages: errors carry only the opendal error kind and
//! the HTTP status, never server text or URLs.
pub mod oauth;
pub mod secrets;
use crate::{Caps, ConnStatus, Entry, Kind, Progress, Provider, RemoteEvent, VPath};
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
pub use oauth::{oauth_authorize, OAuthClient, OAuthTokens, AUTH_TIMEOUT};
use opendal::{blocking, options, Buffer, ErrorKind};
use parking_lot::{Mutex, RwLock};
pub use secrets::{forget_account, KeyringStore, MemoryStore, SecretStore, KEYRING_SERVICE};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read, Write},
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Non-secret account metadata (config `[[clouds]]`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CloudAccount {
    /// Stable slug (`[A-Za-z0-9_-]+`): the VPath authority and the keychain prefix.
    pub id: String,
    pub label: String,
    pub kind: CloudKind,
    /// Folder (Drive/Dropbox path) or key prefix (S3) shown as the account's root.
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub client_id_override: Option<String>,
    /// Required for `CloudKind::S3`; its keys are always in the secret store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Config>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CloudKind {
    GoogleDrive,
    Dropbox,
    S3,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    /// e.g. `https://s3.us-west-002.backblazeb2.com`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
}

/// What `Provider::remove` does on each service (for the delete confirmation wording).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveKind {
    /// Google Drive: moved to the Drive trash.
    Trash,
    /// Dropbox: deleted, restorable from dropbox.com for about 30 days.
    RecoverableDelete,
    /// S3: gone (unless the bucket keeps versions).
    Permanent,
}

/// Dropbox's single-request upload limit (opendal has no upload sessions yet).
const DROPBOX_UPLOAD_LIMIT: u64 = 150 * 1024 * 1024;

impl CloudKind {
    pub fn remove_kind(self) -> RemoveKind {
        match self {
            CloudKind::GoogleDrive => RemoveKind::Trash,
            CloudKind::Dropbox => RemoveKind::RecoverableDelete,
            CloudKind::S3 => RemoveKind::Permanent,
        }
    }
    /// The app registration shipped in `assets/cloud-clients.toml`, unless it is still a
    /// placeholder (users bring their own client id; see that file).
    pub fn default_client(self) -> Option<OAuthClient> {
        #[derive(serde::Deserialize)]
        struct Entry {
            client_id: String,
            #[serde(default)]
            client_secret: Option<String>,
        }
        let table: HashMap<String, Entry> =
            toml::from_str(include_str!("../../../../assets/cloud-clients.toml")).ok()?;
        let entry = table.get(match self {
            CloudKind::GoogleDrive => "google_drive",
            CloudKind::Dropbox => "dropbox",
            CloudKind::S3 => return None,
        })?;
        let placeholder = |s: &str| s.trim().is_empty() || s.contains("YOUR_");
        (!placeholder(&entry.client_id)).then(|| OAuthClient {
            id: entry.client_id.clone(),
            secret: entry.client_secret.clone().filter(|s| !placeholder(s)),
        })
    }
}

/// The client used for an account: its override (with a `<id>/client_secret` from the
/// store, if any) or the shipped default.
pub fn resolve_client(account: &CloudAccount, secrets: &dyn SecretStore) -> Result<OAuthClient> {
    let default = account.kind.default_client();
    let id = account
        .client_id_override
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or(default.as_ref().map(|c| c.id.clone()))
        .context("no OAuth client id configured: add your own app's client id in Settings")?;
    let secret = secrets
        .get(&format!("{}/client_secret", account.id))?
        .or(default.and_then(|c| c.secret));
    Ok(OAuthClient { id, secret })
}

/// Saves tokens under `<id>/access_token`, `<id>/refresh_token` (kept if `None`) and
/// `<id>/expires_at` (Unix seconds).
pub fn store_tokens(secrets: &dyn SecretStore, id: &str, tokens: &OAuthTokens) -> Result<()> {
    secrets.set(&format!("{id}/access_token"), &tokens.access)?;
    if let Some(refresh) = &tokens.refresh {
        secrets.set(&format!("{id}/refresh_token"), refresh)?;
    }
    let secs = tokens
        .expires_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secrets.set(&format!("{id}/expires_at"), &secs.to_string())
}

/// Signs an account out: revokes its grant where the service allows it (Drive: the refresh
/// token; Dropbox: the access token), then deletes its keychain entries whatever the
/// service answered. S3 keys are only revocable in the provider's console. Blocks on the
/// network: workers only. The error is the revoke's (the entries are gone either way).
pub fn sign_out(account: &CloudAccount, secrets: &dyn SecretStore) -> Result<()> {
    let field = match account.kind {
        CloudKind::GoogleDrive => "refresh_token",
        _ => "access_token",
    };
    let revoked = match (
        oauth::revoke_url(account.kind),
        secrets.get(&format!("{}/{field}", account.id)),
    ) {
        (Some(url), Ok(Some(token))) => oauth::revoke(account.kind, url, &token),
        _ => Ok(()),
    };
    forget_account(secrets, &account.id);
    revoked
}

/// rustls' crypto (pure Rust, so cross-target builds need no C toolchain) and opendal's
/// HTTP transport, installed once per process.
fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err means another component installed a provider first: use that one.
        let _ =
            rustls::crypto::CryptoProvider::install_default(rustls_graviola::default_provider());
        opendal::install_default();
        // That installs no HTTP transport with the rustls-no-provider feature: every request
        // would fail as ConfigInvalid. Install reqwest here, after the crypto provider. A
        // connect timeout so a silent host fails in seconds; transfers have no total limit.
        match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
        {
            Ok(client) => opendal::HttpTransporter::install_default(
                opendal_http_transport_reqwest::ReqwestTransport::new(client),
            ),
            Err(e) => tracing::error!("cloud HTTP client unavailable: {e}"),
        }
    });
}
/// The HTTP client for OAuth token requests: no redirects (credentials never follow one).
pub(crate) fn http_client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    init();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .context("HTTP client unavailable")?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// Tries per operation (the first plus four retries) for 429 / 5xx / temporary errors.
const MAX_TRIES: u32 = 5;
/// Delay before retry `attempt` (0-based): exponential from 500 ms, capped at 8 s, with
/// "equal jitter" (half fixed, half random by `unit` in [0, 1)). `None`: give up.
pub(crate) fn backoff(attempt: u32, unit: f64) -> Option<Duration> {
    if attempt + 1 >= MAX_TRIES {
        return None;
    }
    let base = Duration::from_millis(500)
        .saturating_mul(1 << attempt.min(16))
        .min(Duration::from_secs(8));
    Some(base / 2 + base.mul_f64(unit.clamp(0.0, 1.0) / 2.0))
}
fn jitter() -> f64 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    f64::from(u32::from_le_bytes(b)) / (f64::from(u32::MAX) + 1.0)
}

/// The HTTP status opendal recorded for a failed request, if any.
fn http_status(e: &opendal::Error) -> Option<u16> {
    // ponytail: opendal keeps the response only as a formatted context string; parse it
    // until it exposes the status.
    let text = e.to_string();
    let at = text.find("response: Parts { status: ")? + "response: Parts { status: ".len();
    text.get(at..at + 3)?.parse().ok()
}
fn retryable(e: &opendal::Error) -> bool {
    e.is_temporary()
        || e.kind() == ErrorKind::RateLimited
        || http_status(e).is_some_and(|s| s == 429 || (500..600).contains(&s))
}
/// Sanitised: the opendal kind and HTTP status only (no server text, URLs or tokens).
fn wire(e: &opendal::Error, p: &VPath) -> anyhow::Error {
    let kind = match e.kind() {
        ErrorKind::NotFound => io::ErrorKind::NotFound,
        ErrorKind::PermissionDenied => io::ErrorKind::PermissionDenied,
        ErrorKind::AlreadyExists | ErrorKind::ConditionNotMatch => io::ErrorKind::AlreadyExists,
        ErrorKind::IsADirectory => io::ErrorKind::IsADirectory,
        ErrorKind::NotADirectory => io::ErrorKind::NotADirectory,
        ErrorKind::Unsupported => io::ErrorKind::Unsupported,
        _ if http_status(e) == Some(401) => io::ErrorKind::PermissionDenied,
        _ => io::ErrorKind::Other,
    };
    let status = http_status(e)
        .map(|s| format!(" (HTTP {s})"))
        .unwrap_or_default();
    // No HTTP answer at all after the retries: the network or the endpoint is down.
    if kind == io::ErrorKind::Other && e.is_temporary() && status.is_empty() {
        return io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("{}: cannot reach the cloud service", p.display()),
        )
        .into();
    }
    io::Error::new(kind, format!("{}: cloud {}{status}", p.display(), e.kind())).into()
}
fn not_found(p: &VPath) -> anyhow::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: not found", p.display()),
    )
    .into()
}

/// OAuth state of a Drive / Dropbox account.
struct OAuth {
    client: OAuthClient,
    endpoints: oauth::Endpoints,
    expires_at: Mutex<SystemTime>,
    /// One refresh at a time.
    refreshing: Mutex<()>,
}

struct Core {
    account: CloudAccount,
    op: RwLock<blocking::Operator>,
    oauth: Option<OAuth>,
    secrets: Arc<dyn SecretStore>,
    events: Sender<RemoteEvent>,
    /// Directory path -> (listed at, entries).
    listings: Mutex<HashMap<String, (Instant, Vec<Entry>)>>,
    ttl: Duration,
}

/// One cloud account. Cheap to share: the router holds it as `Arc<dyn Provider>`.
pub struct CloudProvider {
    core: Arc<Core>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
/// opendal wants a runtime handle for its blocking API: the shared SFTP runtime.
fn blocking_op(op: opendal::Operator) -> Result<blocking::Operator> {
    let _guard = crate::sftp::conn::runtime().enter();
    Ok(blocking::Operator::new(op)?)
}
fn oauth_op(kind: CloudKind, root: Option<&str>, access: &str) -> Result<opendal::Operator> {
    let root = root.unwrap_or("/");
    Ok(match kind {
        CloudKind::GoogleDrive => opendal::Operator::new(
            opendal::services::Gdrive::default()
                .root(root)
                .access_token(access),
        )?,
        CloudKind::Dropbox => opendal::Operator::new(
            opendal::services::Dropbox::default()
                .root(root)
                .access_token(access),
        )?,
        CloudKind::S3 => anyhow::bail!("S3 does not use OAuth"),
    })
}

impl CloudProvider {
    /// Builds the account's operator from the secret store. Reads the keychain but makes no
    /// network request: an expired access token is refreshed by the first operation.
    pub fn connect(
        account: &CloudAccount,
        secrets: Arc<dyn SecretStore>,
        events: Sender<RemoteEvent>,
    ) -> Result<Self> {
        anyhow::ensure!(valid_id(&account.id), "cloud id must be a stable slug");
        init();
        let key = |field: &str| format!("{}/{field}", account.id);
        let (op, oauth) = match account.kind {
            CloudKind::S3 => {
                let cfg = account
                    .s3
                    .as_ref()
                    .context("S3 account without endpoint, region and bucket")?;
                let missing = || anyhow::anyhow!("S3 keys missing from the keychain");
                let id = secrets.get(&key("access_key_id"))?.ok_or_else(missing)?;
                let secret = secrets
                    .get(&key("secret_access_key"))?
                    .ok_or_else(missing)?;
                let builder = opendal::services::S3::default()
                    .endpoint(&cfg.endpoint)
                    .region(&cfg.region)
                    .bucket(&cfg.bucket)
                    .root(account.root.as_deref().unwrap_or("/"))
                    .access_key_id(&id)
                    .secret_access_key(&secret)
                    // Never pick up ~/.aws or instance credentials behind the user's back.
                    .disable_config_load()
                    .disable_ec2_metadata();
                (opendal::Operator::new(builder)?, None)
            }
            kind => {
                let client = resolve_client(account, &*secrets)?;
                let access = secrets.get(&key("access_token"))?;
                let expires_at = secrets
                    .get(&key("expires_at"))?
                    .and_then(|s| s.parse().ok())
                    .map_or(UNIX_EPOCH, |s| UNIX_EPOCH + Duration::from_secs(s));
                // No access token yet: build with a dummy one, already expired, so the first
                // call refreshes.
                let (access, expires_at) = match access {
                    Some(a) if !a.is_empty() => (a, expires_at),
                    _ => ("expired".into(), UNIX_EPOCH),
                };
                let op = oauth_op(kind, account.root.as_deref(), &access)?;
                let oauth = OAuth {
                    client,
                    endpoints: oauth::Endpoints::for_kind(kind)?,
                    expires_at: Mutex::new(expires_at),
                    refreshing: Mutex::new(()),
                };
                (op, Some(oauth))
            }
        };
        Self::build(account.clone(), op, oauth, secrets, events)
    }
    /// A provider over any operator (tests use opendal's memory service as a stand-in).
    pub fn with_operator(
        account: CloudAccount,
        op: opendal::Operator,
        events: Sender<RemoteEvent>,
    ) -> Result<Self> {
        anyhow::ensure!(valid_id(&account.id), "cloud id must be a stable slug");
        init();
        Self::build(account, op, None, Arc::new(MemoryStore::default()), events)
    }
    fn build(
        account: CloudAccount,
        op: opendal::Operator,
        oauth: Option<OAuth>,
        secrets: Arc<dyn SecretStore>,
        events: Sender<RemoteEvent>,
    ) -> Result<Self> {
        Ok(Self {
            core: Arc::new(Core {
                account,
                op: RwLock::new(blocking_op(op)?),
                oauth,
                secrets,
                events,
                listings: Mutex::default(),
                ttl: Duration::from_secs(60),
            }),
        })
    }
    pub fn account(&self) -> &CloudAccount {
        &self.core.account
    }
    /// What `remove` does here (trash, recoverable delete or permanent).
    pub fn remove_kind(&self) -> RemoveKind {
        self.core.account.kind.remove_kind()
    }
    /// Drops cached listings (e.g. a "Refresh" in the UI).
    pub fn invalidate_all(&self) {
        self.core.listings.lock().clear();
    }
    #[cfg(test)]
    fn set_ttl(&mut self, ttl: Duration) {
        Arc::get_mut(&mut self.core).expect("unshared").ttl = ttl;
    }
}

/// `/a/b` -> `a/b` (files) or `a/b/` (folders); the root is `/`.
fn key(p: &VPath, dir: bool) -> String {
    let path = p.path.trim_matches('/');
    match (path.is_empty(), dir) {
        (true, _) => "/".into(),
        (false, true) => format!("{path}/"),
        (false, false) => path.into(),
    }
}
fn parent_path(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((dir, _)) => dir.into(),
    }
}

impl Core {
    fn validate(&self, p: &VPath) -> Result<()> {
        anyhow::ensure!(
            p.scheme == "cloud"
                && p.authority == self.account.id
                && p.path.starts_with('/')
                && !p.path.contains(['\0', '\\'])
                && p.path
                    .split('/')
                    .skip(1)
                    .all(|s| s != "." && s != ".." && !(s.is_empty() && p.path != "/")),
            "invalid cloud path: {}",
            p.display()
        );
        Ok(())
    }
    fn status(&self, status: ConnStatus, detail: &str) {
        let _ = self.events.send(RemoteEvent::Status {
            host_id: format!("cloud:{}", self.account.id),
            status,
            detail: detail.into(),
        });
    }
    /// New access token from the stored refresh token; the operator is rebuilt with it.
    /// `force` false: only if still expiring once this thread holds the refresh lock (another
    /// thread may have just refreshed).
    fn refresh(&self, force: bool) -> Result<()> {
        let oauth = self.oauth.as_ref().context("no OAuth for this account")?;
        let _one = oauth.refreshing.lock();
        if !force && !Self::expiring(oauth) {
            return Ok(());
        }
        let id = &self.account.id;
        let result = (|| {
            let refresh = self
                .secrets
                .get(&format!("{id}/refresh_token"))?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "not signed in; sign in again",
                    )
                })?;
            let tokens = oauth::refresh(&oauth.endpoints, &oauth.client, &refresh)?;
            store_tokens(&*self.secrets, id, &tokens)?;
            *self.op.write() = blocking_op(oauth_op(
                self.account.kind,
                self.account.root.as_deref(),
                &tokens.access,
            )?)?;
            *oauth.expires_at.lock() = tokens.expires_at;
            anyhow::Ok(())
        })();
        match &result {
            Ok(()) => self.status(ConnStatus::Connected, "signed in"),
            Err(e) => self.status(ConnStatus::Failed, &format!("{e:#}")),
        }
        result
    }
    /// One operation: refreshes an expiring token first, refreshes once on HTTP 401, and
    /// retries 429 / 5xx / temporary errors with `backoff`.
    fn call<T>(
        &self,
        p: &VPath,
        f: impl Fn(&blocking::Operator) -> opendal::Result<T>,
    ) -> Result<T> {
        if self.oauth.as_ref().is_some_and(Self::expiring) {
            self.refresh(false)?;
        }
        let (mut attempt, mut refreshed) = (0, false);
        loop {
            let op = self.op.read().clone();
            let e = match f(&op) {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            if http_status(&e) == Some(401) && self.oauth.is_some() && !refreshed {
                refreshed = true;
                self.refresh(true)?;
                continue;
            }
            match backoff(attempt, jitter()).filter(|_| retryable(&e)) {
                Some(delay) => {
                    tracing::debug!(attempt, ?delay, "cloud request retry");
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                None => return Err(wire(&e, p)),
            }
        }
    }
    fn expiring(oauth: &OAuth) -> bool {
        *oauth.expires_at.lock() <= SystemTime::now() + Duration::from_secs(60)
    }
    fn caps(&self) -> opendal::Capability {
        self.op.read().info().capability()
    }

    fn entry(&self, path: VPath, meta: &opendal::Metadata) -> Entry {
        let name = path.name().to_owned();
        let dir = meta.is_dir();
        let ext = name
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty() && !dir)
            .map(|(_, ext)| ext.to_lowercase())
            .unwrap_or_default();
        Entry {
            kind: if dir { Kind::Dir } else { Kind::File },
            size: if dir { 0 } else { meta.content_length() },
            modified: meta.last_modified().map(SystemTime::from),
            hidden: name.starts_with('.'),
            ext,
            is_link: false,
            encrypted: false,
            name,
            path,
        }
    }
    fn root_entry(&self, p: &VPath) -> Entry {
        Entry {
            path: p.clone(),
            name: String::new(),
            kind: Kind::Dir,
            size: 0,
            modified: None,
            hidden: false,
            is_link: false,
            encrypted: false,
            ext: String::new(),
        }
    }
    fn cached(&self, dir: &str) -> Option<Vec<Entry>> {
        let mut listings = self.listings.lock();
        match listings.get(dir) {
            Some((at, entries)) if at.elapsed() < self.ttl => Some(entries.clone()),
            Some(_) => {
                listings.remove(dir);
                None
            }
            None => None,
        }
    }
    /// After a change at `path`: its parent's listing and anything under it are stale.
    fn invalidate(&self, path: &str) {
        let parent = parent_path(path);
        let path = path.trim_end_matches('/');
        let under = format!("{path}/");
        self.listings
            .lock()
            .retain(|dir, _| *dir != parent && dir != path && !dir.starts_with(&under));
    }

    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.validate(dir)?;
        if let Some(hit) = self.cached(&dir.path) {
            return Ok(hit);
        }
        let own = key(dir, true);
        let raw = self.call(dir, |op| op.list(&own))?;
        let mut names = HashSet::new();
        let mut entries = Vec::with_capacity(raw.len());
        for item in raw {
            if item.path() == own || item.path() == own.trim_start_matches('/') {
                continue;
            }
            let name = item.name().trim_end_matches('/');
            // ponytail: Drive allows two items with one name; the first wins here (the
            // path is ambiguous to opendal too). A rename on the web makes both visible.
            if name.is_empty() || name.contains(['/', '\\']) || !names.insert(name.to_owned()) {
                continue;
            }
            entries.push(self.entry(dir.join(name), item.metadata()));
        }
        self.listings
            .lock()
            .insert(dir.path.clone(), (Instant::now(), entries.clone()));
        Ok(entries)
    }
    /// From a fresh cached listing of the parent when there is one.
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.validate(p)?;
        let Some(parent) = p.parent() else {
            return Ok(self.root_entry(p));
        };
        if let Some(listing) = self.cached(&parent.path) {
            return listing
                .into_iter()
                .find(|e| e.name == p.name())
                .ok_or_else(|| not_found(p));
        }
        self.stat_remote(p)
    }
    fn stat_remote(&self, p: &VPath) -> Result<Entry> {
        if p.parent().is_none() {
            return Ok(self.root_entry(p));
        }
        let file = key(p, false);
        match self.call(p, |op| op.stat(&file)) {
            Ok(meta) => Ok(self.entry(p.clone(), &meta)),
            Err(e) if is_not_found(&e) => {
                let dir = key(p, true);
                let meta = self.call(p, |op| op.stat(&dir))?;
                Ok(self.entry(p.clone(), &meta))
            }
            Err(e) => Err(e),
        }
    }
    fn maybe_stat(&self, p: &VPath) -> Result<Option<Entry>> {
        match self.stat_remote(p) {
            Ok(e) => Ok(Some(e)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Moves `from` to `to`. Folders move file by file (opendal renames files only);
    /// files use the service's rename, else a server-side copy, else a stream.
    fn rename(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.validate(from)?;
        self.validate(to)?;
        anyhow::ensure!(
            from.parent().is_some() && to.parent().is_some(),
            "cannot rename the root"
        );
        let source = self.stat_remote(from)?;
        if let Some(existing) = self.maybe_stat(to)? {
            anyhow::ensure!(
                replace,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{}: already exists", to.display())
                )
            );
            anyhow::ensure!(
                source.kind == Kind::File && existing.kind == Kind::File,
                "only a file can replace a file: {}",
                to.display()
            );
            // Drive's rename replaces (trashing the old file); S3 copies over it; Dropbox
            // refuses, so the old file goes first (restorable there for 30 days).
            if self.account.kind == CloudKind::Dropbox {
                let k = key(to, false);
                self.call(to, |op| op.delete(&k))?;
            }
        }
        // ponytail: check-then-rename; a file created at `to` in between is replaced on
        // Drive/S3 (Dropbox refuses). Services offer no atomic no-replace move here.
        let result = self.move_entry(from, to, source.kind == Kind::Dir, 0);
        self.invalidate(&from.path);
        self.invalidate(&to.path);
        result
    }
    fn move_entry(&self, from: &VPath, to: &VPath, dir: bool, depth: usize) -> Result<()> {
        anyhow::ensure!(depth < 256, "directory nesting limit: {}", from.display());
        if dir {
            let (src, dst) = (key(from, true), key(to, true));
            let children: Vec<_> = self
                .call(from, |op| op.list(&src))?
                .into_iter()
                .filter(|c| c.path() != src)
                .collect();
            // Drive allows one name twice; moving both would replace one with the other.
            let mut seen = HashSet::new();
            if let Some(twice) = children
                .iter()
                .map(|c| c.name().trim_end_matches('/'))
                .find(|n| !seen.insert(*n))
            {
                anyhow::bail!(
                    "{} holds two items named {twice:?}; rename one of them first",
                    from.display()
                );
            }
            self.call(to, |op| op.create_dir(&dst))?;
            for child in children {
                let name = child.name().trim_end_matches('/');
                self.move_entry(
                    &from.join(name),
                    &to.join(name),
                    child.metadata().is_dir(),
                    depth + 1,
                )?;
            }
            return self.call(from, |op| op.delete(&src));
        }
        let (src, dst) = (key(from, false), key(to, false));
        let caps = self.caps();
        if caps.rename {
            return self.call(from, |op| op.rename(&src, &dst));
        }
        if caps.copy {
            self.call(from, |op| op.copy(&src, &dst))?;
        } else {
            // ponytail: whole file in memory; only backends without rename and copy (the
            // test memory service) get here.
            let data = self.call(from, |op| op.read(&src))?;
            self.call(to, |op| op.write(&dst, data.clone()))?;
        }
        self.call(from, |op| op.delete(&src))
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.validate(p)?;
        anyhow::ensure!(p.parent().is_some(), "cannot delete the account root");
        let entry = self.stat_remote(p)?;
        let k = key(p, entry.kind == Kind::Dir);
        // Drive trashes and Dropbox deletes a folder with its contents in one request;
        // S3 has no folders, so every key under the prefix goes.
        let result = if entry.kind == Kind::Dir && self.account.kind == CloudKind::S3 {
            self.call(p, |op| {
                op.delete_options(
                    &k,
                    options::DeleteOptions {
                        recursive: true,
                        ..Default::default()
                    },
                )
            })
        } else {
            self.call(p, |op| op.delete(&k))
        };
        self.invalidate(&p.path);
        result
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        self.validate(p)?;
        anyhow::ensure!(p.parent().is_some(), "cannot delete the account root");
        let k = key(p, true);
        let children = self.call(p, |op| op.list(&k))?;
        anyhow::ensure!(
            children.iter().all(|c| c.path() == k),
            "directory not empty: {}",
            p.display()
        );
        let result = self.call(p, |op| op.delete(&k));
        self.invalidate(&p.path);
        result
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.validate(p)?;
        if let Some(e) = self.maybe_stat(p)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: already exists ({:?})", p.display(), e.kind),
            )
            .into());
        }
        let k = key(p, true);
        let result = self.call(p, |op| op.create_dir(&k));
        self.invalidate(&p.path);
        result
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
}

/// A download stream; errors are sanitised like `wire`.
struct CloudReader {
    inner: blocking::StdReader,
    path: VPath,
}
impl Read for CloudReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{}: cloud download interrupted", self.path.display()),
            )
        })
    }
}

/// One upload. Where the service can rename (Drive, Dropbox) it goes to a staging name
/// beside the target and is renamed into place on `flush()`; S3 writes the key directly
/// (an object only appears once complete). Dropped without `flush()`: discarded and
/// logged, never placed. Drive and Dropbox upload in one request at commit (opendal
/// buffers the file in memory until then).
struct CloudUpload {
    core: Arc<Core>,
    target: VPath,
    staged: Option<VPath>,
    writer: Option<blocking::Writer>,
    exclusive: bool,
    written: u64,
    done: bool,
    failed: bool,
}
impl CloudUpload {
    fn start(core: Arc<Core>, target: &VPath, exclusive: bool) -> Result<Self> {
        core.validate(target)?;
        anyhow::ensure!(target.parent().is_some(), "cannot write the account root");
        if exclusive && core.maybe_stat(target)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: destination exists", target.display()),
            )
            .into());
        }
        let caps = core.caps();
        let staged = caps.rename.then(|| VPath {
            path: crate::sftp::partial_path(&target.path),
            ..target.clone()
        });
        let at = staged.as_ref().unwrap_or(target);
        let k = key(at, false);
        let if_not_exists = exclusive && staged.is_none() && caps.write_with_if_not_exists;
        let writer = core.call(at, |op| {
            op.writer_options(
                &k,
                options::WriteOptions {
                    if_not_exists,
                    ..Default::default()
                },
            )
        })?;
        Ok(Self {
            core,
            target: target.clone(),
            staged,
            writer: Some(writer),
            exclusive,
            written: 0,
            done: false,
            failed: false,
        })
    }
    fn commit(&mut self) -> Result<()> {
        if self.done {
            return Ok(());
        }
        anyhow::ensure!(!self.failed, "upload failed: {}", self.target.display());
        let mut writer = self.writer.take().context("upload already closed")?;
        let result = writer
            .close()
            .map_err(|e| wire(&e, &self.target))
            .and_then(|_| match &self.staged {
                Some(staged) => self.core.rename(staged, &self.target, !self.exclusive),
                None => Ok(()),
            });
        self.core.invalidate(&self.target.path);
        match result {
            Ok(()) => {
                self.done = true;
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }
}
impl Write for CloudUpload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.done || self.failed {
            return Err(io::Error::other("upload finished or failed"));
        }
        if self.core.account.kind == CloudKind::Dropbox
            && self.written + bytes.len() as u64 > DROPBOX_UPLOAD_LIMIT
        {
            self.failed = true;
            return Err(io::Error::other(format!(
                "{}: files over 150 MB cannot be uploaded to Dropbox yet",
                self.target.display()
            )));
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("upload closed"))?;
        if let Err(e) = writer.write(Buffer::from(bytes.to_vec())) {
            self.failed = true;
            return Err(io::Error::other(format!("{:#}", wire(&e, &self.target))));
        }
        self.written += bytes.len() as u64;
        Ok(bytes.len())
    }
    /// Commits the upload (see `Provider::write`).
    fn flush(&mut self) -> io::Result<()> {
        self.commit().map_err(|e| {
            let kind = e
                .downcast_ref::<io::Error>()
                .map_or(io::ErrorKind::Other, io::Error::kind);
            io::Error::new(kind, format!("{e:#}"))
        })
    }
}
impl Drop for CloudUpload {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if !self.failed {
            tracing::error!(
                target = %self.target.display(),
                "cloud upload dropped without flush(); discarding it"
            );
        }
        // ponytail: dropping the writer leaves an unfinished S3 multipart upload to the
        // bucket's lifecycle rules; opendal's blocking writer has no abort.
        self.writer = None;
        if let Some(staged) = &self.staged {
            let k = key(staged, false);
            let _ = self.core.op.read().delete(&k);
        }
    }
}

impl Provider for CloudProvider {
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
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.core.list(dir)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.core.stat(p)
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        let entry = self.core.stat(p)?;
        anyhow::ensure!(entry.kind == Kind::File, "not a file: {}", p.display());
        let k = key(p, false);
        let reader = self.core.call(p, |op| op.reader(&k)?.into_std_read(..))?;
        Ok(Box::new(CloudReader {
            inner: reader,
            path: p.clone(),
        }))
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(CloudUpload::start(self.core.clone(), p, false)?))
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(CloudUpload::start(self.core.clone(), p, true)?))
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.core.mkdir(p)
    }
    /// Never replaces an existing target.
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.core.rename(from, to, false)
    }
    fn rename_noreplace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.core.rename(from, to, false)
    }
    fn rename_replace(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.core.rename(from, to, true)
    }
    fn remove_empty_dir(&self, p: &VPath) -> Result<()> {
        self.core.remove_empty_dir(p)
    }
    /// Drive: to the trash; Dropbox: recoverable delete; S3: permanent (`remove_kind`).
    fn remove(&self, p: &VPath) -> Result<()> {
        self.core.remove(p)
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        self.local_copy_cancellable(p, &|_| {}, &AtomicBool::new(false))
    }
    /// Through the same download cache as SFTP (`<cache>/remote/cloud-<id>/`).
    fn local_copy_cancellable(
        &self,
        p: &VPath,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        self.core.validate(p)?;
        let a = &self.core.account;
        let s3 =
            a.s3.as_ref()
                .map(|s| format!("{}|{}", s.endpoint, s.bucket))
                .unwrap_or_default();
        let kind = format!("{:?}", a.kind);
        crate::sftp::cached_download(
            self,
            p,
            &format!("cloud-{}", a.id),
            &[&kind, &a.id, a.root.as_deref().unwrap_or(""), &s3],
            progress,
            cancel,
        )
    }
}

#[cfg(test)]
mod tests;
