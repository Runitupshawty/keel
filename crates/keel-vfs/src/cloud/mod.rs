//! Cloud storage as folders: Google Drive, Dropbox, S3-compatible buckets and WebDAV over `opendal`,
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
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Directives to keep in the app's `tracing` filter: the S3 signing library logs its
/// credential providers at debug level (Keel's prints no key, but a debug filter should
/// not depend on that).
pub const LOG_FILTER_HINT: &str = "reqsign_core=warn,reqsign_aws_v4=warn";

/// Non-secret account metadata (config `[[clouds]]`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CloudAccount {
    /// Stable lowercase slug (`[a-z0-9_-]+`): the VPath authority and the keychain prefix
    /// (lowercase because Windows Credential Manager ignores case).
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
    /// Required for `CloudKind::WebDav`; its password is the secret `<id>/password`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webdav: Option<WebDavConfig>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CloudKind {
    GoogleDrive,
    Dropbox,
    S3,
    /// Nextcloud, ownCloud, Synology, Apache mod_dav: any WebDAV collection URL.
    WebDav,
}
/// A WebDAV account (basic auth; the password or app password is in the secret store as
/// `<id>/password`). The account's `root` is a folder below `url`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WebDavConfig {
    /// e.g. `https://host/remote.php/dav/files/<user>/` (see `normalize_url`).
    pub url: String,
    pub username: String,
    /// The user accepted plain `http://` (files and password travel unencrypted).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
}
impl WebDavConfig {
    /// The collection URL with a trailing slash. `https://` only, or `http://` with
    /// `insecure`; other schemes (`webdav://`, ...) are refused, as are credentials,
    /// queries and fragments in the URL (the password has its own field).
    pub fn normalize_url(url: &str, insecure: bool) -> Result<String> {
        let mut u = url::Url::parse(url.trim())
            .map_err(|_| anyhow::anyhow!("not a valid address; use https://host/path/"))?;
        match u.scheme() {
            "https" => {}
            "http" if insecure => {}
            "http" => anyhow::bail!("http:// sends the password unencrypted; use https://"),
            other => anyhow::bail!("unsupported scheme {other}://; use https://"),
        }
        anyhow::ensure!(u.host_str().is_some(), "the address has no host");
        anyhow::ensure!(
            u.username().is_empty() && u.password().is_none(),
            "leave the user name and password out of the address"
        );
        anyhow::ensure!(
            u.query().is_none() && u.fragment().is_none(),
            "the address must not have a query or fragment"
        );
        if !u.path().ends_with('/') {
            let path = format!("{}/", u.path());
            u.set_path(&path);
        }
        Ok(u.into())
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.username.trim().is_empty(), "enter the user name");
        Self::normalize_url(&self.url, self.insecure).map(|_| ())
    }
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    /// e.g. `https://s3.us-west-002.backblazeb2.com`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
}

pub use crate::provider::RemoveKind;

/// Cloud failures a caller may want to tell apart (`anyhow::Error::downcast_ref`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CloudError {
    /// The TLS crypto needs CPU features this machine lacks (named in the field).
    #[error(
        "cloud accounts need a CPU with AES, AVX2, ADX and BMI2 (most x86-64 CPUs since \
         2014) or a 64-bit ARM CPU with AES and SHA-2; this one lacks {0}"
    )]
    UnsupportedCpu(String),
    /// HTTPS could not be set up.
    #[error("cloud access unavailable: {0}")]
    Unavailable(String),
}

/// Dropbox's single-request upload limit (opendal has no upload sessions yet).
const DROPBOX_UPLOAD_LIMIT: u64 = 150 * 1024 * 1024;
/// Drive uploads are one request with the whole file in memory (opendal has no resumable
/// Drive upload yet), so they are capped.
const DRIVE_UPLOAD_LIMIT: u64 = 256 * 1024 * 1024;
/// Entries shown per folder. ponytail: a bigger folder is cut off here (and a warning
/// logged); page it in the UI if anyone keeps that many files in one folder.
/// WebDAV uploads are one request with the whole file in memory (opendal's WebDAV writer
/// does not stream), so they are capped. ponytail: chunked upload is server-specific.
const WEBDAV_UPLOAD_LIMIT: u64 = 1024 * 1024 * 1024;
const LIST_CAP: usize = 50_000;

impl CloudKind {
    pub fn remove_kind(self) -> RemoveKind {
        match self {
            CloudKind::GoogleDrive => RemoveKind::Trash,
            CloudKind::Dropbox => RemoveKind::RecoverableDelete,
            CloudKind::S3 | CloudKind::WebDav => RemoveKind::Permanent,
        }
    }
    fn name(self) -> &'static str {
        match self {
            CloudKind::GoogleDrive => "Google Drive",
            CloudKind::Dropbox => "Dropbox",
            CloudKind::S3 => "S3",
            CloudKind::WebDav => "WebDAV",
        }
    }
    /// Largest file one upload request may carry.
    fn upload_limit(self) -> Option<u64> {
        match self {
            CloudKind::GoogleDrive => Some(DRIVE_UPLOAD_LIMIT),
            CloudKind::Dropbox => Some(DROPBOX_UPLOAD_LIMIT),
            CloudKind::S3 => None,
            CloudKind::WebDav => Some(WEBDAV_UPLOAD_LIMIT),
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
            CloudKind::S3 | CloudKind::WebDav => return None,
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

/// All of an account's tokens in one keychain entry (`<id>/tokens`, JSON), so a refresh
/// is saved atomically.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredTokens {
    #[serde(default)]
    access: String,
    #[serde(default)]
    refresh: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    expires_at: u64,
}
/// Per-field entries written by earlier builds: read as a fallback, removed on save.
const LEGACY_TOKEN_FIELDS: [&str; 3] = ["access_token", "refresh_token", "expires_at"];
/// Windows Credential Manager keeps at most 1280 UTF-16 units per secret.
const KEYCHAIN_MAX_CHARS: usize = 1200;

/// Saves tokens as the one `<id>/tokens` entry. A `None` refresh token keeps the stored
/// one. An access token too long for the keychain is not kept (the next start refreshes).
pub fn store_tokens(secrets: &dyn SecretStore, id: &str, tokens: &OAuthTokens) -> Result<()> {
    let refresh = match &tokens.refresh {
        Some(r) => Some(r.clone()),
        None => load_tokens(secrets, id)
            .ok()
            .flatten()
            .and_then(|t| t.refresh),
    };
    let mut stored = StoredTokens {
        access: tokens.access.clone(),
        refresh,
        expires_at: tokens
            .expires_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let mut json = serde_json::to_string(&stored)?;
    if json.chars().count() > KEYCHAIN_MAX_CHARS {
        stored.access.clear();
        stored.expires_at = 0;
        json = serde_json::to_string(&stored)?;
    }
    secrets.set(&format!("{id}/tokens"), &json)?;
    for field in LEGACY_TOKEN_FIELDS {
        let _ = secrets.delete(&format!("{id}/{field}"));
    }
    Ok(())
}
/// The account's tokens: the JSON entry, else the per-field entries of earlier builds.
fn load_tokens(secrets: &dyn SecretStore, id: &str) -> Result<Option<OAuthTokens>> {
    let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
    if let Some(json) = secrets.get(&format!("{id}/tokens"))? {
        let t: StoredTokens = serde_json::from_str(&json)
            .context("the stored cloud sign-in is unreadable; sign in again")?;
        return Ok(Some(OAuthTokens {
            access: t.access,
            refresh: t.refresh,
            expires_at: at(t.expires_at),
        }));
    }
    let field = |f: &str| secrets.get(&format!("{id}/{f}"));
    let (access, refresh) = (field("access_token")?, field("refresh_token")?);
    if access.is_none() && refresh.is_none() {
        return Ok(None);
    }
    Ok(Some(OAuthTokens {
        access: access.unwrap_or_default(),
        refresh,
        expires_at: field("expires_at")?
            .and_then(|s| s.parse().ok())
            .map_or(UNIX_EPOCH, at),
    }))
}

/// Signs an account out: deletes its keychain entries first (so a slow or offline revoke
/// can never delete the secrets of a new account that reuses the id), then revokes the
/// grant with the tokens read before (`revoke_tokens`). S3 keys are only revocable in the
/// provider's console. Blocks on the network: workers only. The error is the revoke's (the
/// entries are gone either way).
pub fn sign_out(account: &CloudAccount, secrets: &dyn SecretStore) -> Result<()> {
    sign_out_with(account, secrets, |tokens, client| {
        revoke_tokens(account.kind, tokens, client)
    })
}
/// `sign_out` with its own revoke (called with the tokens and client read before the
/// entries were deleted).
pub fn sign_out_with(
    account: &CloudAccount,
    secrets: &dyn SecretStore,
    revoke: impl FnOnce(&OAuthTokens, &OAuthClient) -> Result<()>,
) -> Result<()> {
    let tokens = load_tokens(secrets, &account.id).ok().flatten();
    // Only a Dropbox refresh needs the client; Drive revokes without one.
    let client = resolve_client(account, secrets).unwrap_or(OAuthClient {
        id: String::new(),
        secret: None,
    });
    forget_account(secrets, &account.id);
    match tokens {
        Some(tokens) if !matches!(account.kind, CloudKind::S3 | CloudKind::WebDav) => {
            revoke(&tokens, &client)
        }
        _ => Ok(()),
    }
}

/// Revokes a grant from tokens in memory (no keychain access): Drive's refresh token ends
/// the whole grant; Dropbox revokes through an access token, refreshed first when expired.
/// For a sign-out, and for a sign-in cancelled after the tokens arrived. Blocks on the
/// network: workers only.
pub fn revoke_tokens(kind: CloudKind, tokens: &OAuthTokens, client: &OAuthClient) -> Result<()> {
    match (oauth::revoke_url(kind), oauth::Endpoints::for_kind(kind)) {
        (Some(url), Ok(endpoints)) => revoke_at(kind, tokens, client, &endpoints, url),
        _ => Ok(()),
    }
}
fn revoke_at(
    kind: CloudKind,
    tokens: &OAuthTokens,
    client: &OAuthClient,
    endpoints: &oauth::Endpoints,
    url: &str,
) -> Result<()> {
    let fresh = !tokens.access.is_empty()
        && tokens.expires_at > SystemTime::now() + Duration::from_secs(60);
    let token = match (kind, &tokens.refresh) {
        (CloudKind::GoogleDrive, refresh) => refresh.clone(),
        _ if fresh => Some(tokens.access.clone()),
        (_, Some(refresh)) => match oauth::refresh(endpoints, client, refresh) {
            Ok(t) => Some(t.access),
            // The grant is already gone: nothing left to revoke.
            Err(e) if io_kind(&e) == Some(io::ErrorKind::PermissionDenied) => None,
            Err(e) => return Err(e),
        },
        (_, None) => Some(tokens.access.clone()).filter(|a| !a.is_empty()),
    };
    match token {
        Some(token) => oauth::revoke(kind, url, &token),
        None => Ok(()),
    }
}

/// CPU features graviola (rustls' crypto here) asserts on first use; see its README.
#[cfg(target_arch = "x86_64")]
const CPU_FEATURES: &[&str] = &[
    "aes",
    "pclmulqdq",
    "ssse3",
    "bmi1",
    "bmi2",
    "adx",
    "avx",
    "avx2",
];
#[cfg(target_arch = "aarch64")]
const CPU_FEATURES: &[&str] = &["neon", "aes", "pmull", "sha2"];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const CPU_FEATURES: &[&str] = &[];
#[cfg(target_arch = "x86_64")]
fn cpu_has(feature: &str) -> bool {
    use std::arch::is_x86_feature_detected as has;
    match feature {
        "aes" => has!("aes"),
        "pclmulqdq" => has!("pclmulqdq"),
        "ssse3" => has!("ssse3"),
        "bmi1" => has!("bmi1"),
        "bmi2" => has!("bmi2"),
        "adx" => has!("adx"),
        "avx" => has!("avx"),
        "avx2" => has!("avx2"),
        _ => false,
    }
}
#[cfg(target_arch = "aarch64")]
fn cpu_has(feature: &str) -> bool {
    use std::arch::is_aarch64_feature_detected as has;
    match feature {
        "neon" => has!("neon"),
        "aes" => has!("aes"),
        "pmull" => has!("pmull"),
        "sha2" => has!("sha2"),
        _ => false,
    }
}
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn cpu_has(_: &str) -> bool {
    false
}
fn cpu_error(has: impl Fn(&str) -> bool) -> Result<(), CloudError> {
    let missing: Vec<_> = CPU_FEATURES.iter().copied().filter(|f| !has(f)).collect();
    match missing.is_empty() {
        true => Ok(()),
        false => Err(CloudError::UnsupportedCpu(
            missing.join(", ").to_uppercase(),
        )),
    }
}
/// Whether this machine can use cloud accounts at all (the UI may grey out "Add account").
pub fn check_cpu() -> Result<(), CloudError> {
    cpu_error(cpu_has)
}

/// rustls' crypto (graviola: pure Rust, so cross-target builds need no C toolchain) and
/// opendal's HTTP transport, installed once per process. Every entry point calls this.
fn init() -> Result<(), CloudError> {
    static INIT: OnceLock<Result<(), CloudError>> = OnceLock::new();
    INIT.get_or_init(|| {
        check_cpu()?;
        // graviola asserts its CPU features itself: should the check above miss one, the
        // panic must not take the app down.
        std::panic::catch_unwind(|| {
            // Err: another component installed a provider first; use that one.
            let _ = rustls::crypto::CryptoProvider::install_default(
                rustls_graviola::default_provider(),
            );
        })
        .map_err(|_| CloudError::UnsupportedCpu("a feature its TLS crypto needs".into()))?;
        // reqwest's rustls checks certificates with the platform verifier (OS store).
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| CloudError::Unavailable("the HTTPS client could not be set up".into()))?;
        opendal::HttpTransporter::install_default(
            opendal_http_transport_reqwest::ReqwestTransport::new(client),
        );
        Ok(())
    })
    .clone()
}
/// Downloads `url` (HTTPS only; redirects followed) into memory, at most `max` bytes,
/// calling `progress(done, total)` as it goes. Blocking: call it on a worker thread.
pub fn https_get(
    url: &str,
    max: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Vec<u8>> {
    use std::io::Read;
    anyhow::ensure!(url.starts_with("https://"), "not an HTTPS address: {url}");
    init()?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .https_only(true)
        .user_agent(concat!("Keel/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("HTTP client unavailable")?;
    let mut resp = client.get(url).send()?.error_for_status()?;
    let total = resp.content_length();
    let too_big = || anyhow::anyhow!("the download is larger than {} MB", max >> 20);
    if total.is_some_and(|t| t > max) {
        return Err(too_big());
    }
    let mut out = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() as u64 > max {
            return Err(too_big());
        }
        progress(out.len() as u64, total);
    }
}

/// The HTTP client for OAuth token requests: no redirects (credentials never follow one).
pub(crate) fn http_client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    init()?;
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
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
/// For calls nobody can cancel.
static NEVER: AtomicBool = AtomicBool::new(false);
/// Waits `delay` in slices of at most 100 ms; ends early, with an error, on `cancel`.
fn sleep_unless_cancelled(delay: Duration, cancel: &AtomicBool) -> Result<()> {
    let end = Instant::now() + delay;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled").into());
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
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
    let status = http_status(e);
    e.is_temporary()
        || e.kind() == ErrorKind::RateLimited
        || status.is_some_and(|s| s == 429 || (500..600).contains(&s))
        // Drive answers rate limits with 403 rateLimitExceeded / userRateLimitExceeded.
        || status == Some(403) && {
            let text = e.to_string().to_ascii_lowercase();
            text.contains("ratelimitexceeded") || text.contains("rate limit exceeded")
        }
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
fn sign_in_again() -> anyhow::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "the cloud sign-in expired or was revoked; sign in again",
    )
    .into()
}
fn io_kind(e: &anyhow::Error) -> Option<io::ErrorKind> {
    e.downcast_ref::<io::Error>().map(io::Error::kind)
}

/// Builds the account's operator for an access token.
type MakeOp = Box<dyn Fn(&str) -> Result<opendal::Operator> + Send + Sync>;

/// OAuth state of a Drive / Dropbox account.
struct OAuth {
    client: OAuthClient,
    endpoints: oauth::Endpoints,
    expires_at: Mutex<SystemTime>,
    /// One refresh at a time.
    refreshing: Mutex<()>,
    /// The refresh token the service refused (empty: there was none). Until the store
    /// holds another one (the user signed in again) every call fails at once, without a
    /// request.
    revoked: Mutex<Option<String>>,
    make_op: MakeOp,
}
impl OAuth {
    fn new(
        client: OAuthClient,
        endpoints: oauth::Endpoints,
        expires_at: SystemTime,
        make_op: MakeOp,
    ) -> Self {
        Self {
            client,
            endpoints,
            expires_at: Mutex::new(expires_at),
            refreshing: Mutex::new(()),
            revoked: Mutex::new(None),
            make_op,
        }
    }
}

struct Core {
    account: CloudAccount,
    op: RwLock<blocking::Operator>,
    /// Bumped (under the `op` write lock) whenever the operator is replaced: a 401 from an
    /// older operator only retries, so concurrent 401s refresh the token once.
    generation: AtomicU64,
    oauth: Option<OAuth>,
    secrets: Arc<dyn SecretStore>,
    events: Sender<RemoteEvent>,
    /// Directory path -> (listed at, entries).
    listings: Mutex<HashMap<String, (Instant, Vec<Entry>)>>,
    ttl: Duration,
    list_cap: usize,
    /// For services that take a file in one request (it is held in memory until then).
    upload_limit: Option<u64>,
}

/// One cloud account. Cheap to share: the router holds it as `Arc<dyn Provider>`.
pub struct CloudProvider {
    core: Arc<Core>,
}

/// A usable account id: a lowercase slug (`[a-z0-9_-]+`), see `CloudAccount::id`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
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
        CloudKind::S3 | CloudKind::WebDav => anyhow::bail!("this account kind does not use OAuth"),
    })
}

/// Static S3 keys for opendal's signer. reqsign logs credential providers with `Debug`
/// at debug level, so that prints no key.
struct S3Keys(reqsign_aws_v4::StaticCredentialProvider);
impl std::fmt::Debug for S3Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("S3Keys(redacted)")
    }
}
impl reqsign_core::ProvideCredential for S3Keys {
    type Credential = reqsign_aws_v4::Credential;
    async fn provide_credential(
        &self,
        ctx: &reqsign_core::Context,
    ) -> reqsign_core::Result<Option<Self::Credential>> {
        self.0.provide_credential(ctx).await
    }
}
fn s3_op(account: &CloudAccount, key_id: &str, secret: &str) -> Result<opendal::Operator> {
    let cfg = account
        .s3
        .as_ref()
        .context("S3 account without endpoint, region and bucket")?;
    let keys = S3Keys(reqsign_aws_v4::StaticCredentialProvider::new(
        key_id, secret,
    ));
    let builder = opendal::services::S3::default()
        .endpoint(&cfg.endpoint)
        .region(&cfg.region)
        .bucket(&cfg.bucket)
        .root(account.root.as_deref().unwrap_or("/"))
        .credential_provider_chain(reqsign_core::ProvideCredentialChain::new().push(keys))
        // Never pick up ~/.aws or instance credentials behind the user's back.
        .disable_config_load()
        .disable_ec2_metadata();
    Ok(opendal::Operator::new(builder)?)
}

/// The WebDAV operator: basic auth, and its own HTTP client (30 s without a byte from the
/// server ends a request; the process-wide one allows 60).
fn webdav_op(account: &CloudAccount, password: &str) -> Result<opendal::Operator> {
    let cfg = account
        .webdav
        .as_ref()
        .context("WebDAV account without address and user name")?;
    cfg.validate()?;
    let url = WebDavConfig::normalize_url(&cfg.url, cfg.insecure)?;
    let builder = opendal::services::Webdav::default()
        // opendal appends rooted paths (`/a/b`) to the endpoint.
        .endpoint(url.trim_end_matches('/'))
        .username(cfg.username.trim())
        .password(password)
        .root(account.root.as_deref().unwrap_or("/"));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .context("HTTP client unavailable")?;
    let transport = opendal::HttpTransporter::new(
        opendal_http_transport_reqwest::ReqwestTransport::new(client),
    );
    Ok(opendal::Operator::new(builder)?
        .with_context(opendal::OperationContext::new().with_http_transport(transport)))
}

impl CloudProvider {
    /// Builds the account's operator from the secret store. Reads the keychain but makes no
    /// network request: an expired access token is refreshed by the first operation.
    /// Fails with `CloudError::UnsupportedCpu` on a CPU the TLS crypto cannot run on.
    pub fn connect(
        account: &CloudAccount,
        secrets: Arc<dyn SecretStore>,
        events: Sender<RemoteEvent>,
    ) -> Result<Self> {
        anyhow::ensure!(
            valid_id(&account.id),
            "cloud id must be a stable lowercase slug"
        );
        init()?;
        let key = |field: &str| format!("{}/{field}", account.id);
        let (op, oauth) = match account.kind {
            CloudKind::S3 => {
                account
                    .s3
                    .as_ref()
                    .context("S3 account without endpoint, region and bucket")?;
                let missing = || anyhow::anyhow!("S3 keys missing from the keychain");
                let id = secrets.get(&key("access_key_id"))?.ok_or_else(missing)?;
                let secret = secrets
                    .get(&key("secret_access_key"))?
                    .ok_or_else(missing)?;
                (s3_op(account, &id, &secret)?, None)
            }
            CloudKind::WebDav => {
                let password = secrets
                    .get(&key("password"))?
                    .context("WebDAV password missing from the keychain")?;
                (webdav_op(account, &password)?, None)
            }
            kind => {
                let client = resolve_client(account, &*secrets)?;
                let root = account.root.clone();
                let make_op: MakeOp =
                    Box::new(move |access| oauth_op(kind, root.as_deref(), access));
                // No access token yet: build with a dummy one, already expired, so the first
                // call refreshes.
                let (access, expires_at) = match load_tokens(&*secrets, &account.id)? {
                    Some(t) if !t.access.is_empty() => (t.access, t.expires_at),
                    _ => ("expired".into(), UNIX_EPOCH),
                };
                let op = make_op(&access)?;
                let endpoints = oauth::Endpoints::for_kind(kind)?;
                (op, Some(OAuth::new(client, endpoints, expires_at, make_op)))
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
        anyhow::ensure!(
            valid_id(&account.id),
            "cloud id must be a stable lowercase slug"
        );
        init()?;
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
                upload_limit: account.kind.upload_limit(),
                account,
                op: RwLock::new(blocking_op(op)?),
                generation: AtomicU64::new(0),
                oauth,
                secrets,
                events,
                listings: Mutex::default(),
                ttl: Duration::from_secs(60),
                list_cap: LIST_CAP,
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
    /// The service refused the stored sign-in: every operation fails at once until new
    /// tokens are stored (`oauth_authorize` + `store_tokens`; re-registering also works).
    pub fn needs_reauth(&self) -> bool {
        self.core
            .oauth
            .as_ref()
            .is_some_and(|o| o.revoked.lock().is_some())
    }
    /// Drops cached listings (e.g. a "Refresh" in the UI).
    pub fn invalidate_all(&self) {
        self.core.listings.lock().clear();
    }
    #[cfg(test)]
    fn tune(&mut self, f: impl FnOnce(&mut Core)) {
        f(Arc::get_mut(&mut self.core).expect("unshared"));
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
/// `inner` is `outer` or inside it.
fn within(inner: &VPath, outer: &VPath) -> bool {
    let outer = outer.path.trim_end_matches('/');
    inner.path == outer || inner.path.starts_with(&format!("{outer}/"))
}
/// Google Docs, Sheets, ... and Drive shortcuts: no bytes to download (they need an
/// export), so listings leave them out and a folder copy does not stop at them.
fn google_native(meta: &opendal::Metadata) -> bool {
    meta.content_type().is_some_and(|t| {
        t.starts_with("application/vnd.google-apps.") && t != "application/vnd.google-apps.folder"
    })
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
    /// Puts an operator for `tokens` in place.
    fn install(&self, oauth: &OAuth, tokens: &OAuthTokens) -> Result<()> {
        let (access, expires_at) = match tokens.access.as_str() {
            "" => ("expired", UNIX_EPOCH),
            access => (access, tokens.expires_at),
        };
        let op = blocking_op((oauth.make_op)(access)?)?;
        {
            let mut current = self.op.write();
            *current = op;
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        *oauth.expires_at.lock() = expires_at;
        Ok(())
    }
    /// Fails at once while the sign-in is revoked, unless new tokens were stored since.
    fn ensure_signed_in(&self, oauth: &OAuth) -> Result<()> {
        let mut revoked = oauth.revoked.lock();
        let Some(refused) = revoked.as_ref() else {
            return Ok(());
        };
        match load_tokens(&*self.secrets, &self.account.id)? {
            Some(t) if t.refresh.as_ref().is_some_and(|r| r != refused) => {
                self.install(oauth, &t)?;
                *revoked = None;
                Ok(())
            }
            _ => Err(sign_in_again()),
        }
    }
    /// New access token from the stored refresh token; the operator is rebuilt with it.
    /// `seen`: the operator generation that got a 401 (`None`: the token is expiring). If
    /// another thread replaced the operator meanwhile, nothing is requested.
    fn refresh(&self, seen: Option<u64>) -> Result<()> {
        let oauth = self.oauth.as_ref().context("no OAuth for this account")?;
        let _one = oauth.refreshing.lock();
        if oauth.revoked.lock().is_some() {
            return Err(sign_in_again());
        }
        let done = match seen {
            Some(generation) => generation != self.generation.load(Ordering::SeqCst),
            None => !Self::expiring(oauth),
        };
        if done {
            return Ok(());
        }
        let id = &self.account.id;
        let mut refused = None;
        let result = (|| {
            let Some(refresh) = load_tokens(&*self.secrets, id)?.and_then(|t| t.refresh) else {
                refused = Some(String::new());
                return Err(sign_in_again());
            };
            let tokens =
                oauth::refresh(&oauth.endpoints, &oauth.client, &refresh).inspect_err(|e| {
                    if io_kind(e) == Some(io::ErrorKind::PermissionDenied) {
                        refused = Some(refresh.clone());
                    }
                })?;
            store_tokens(&*self.secrets, id, &tokens)?;
            self.install(oauth, &tokens)
        })();
        if let Some(refused) = refused {
            *oauth.revoked.lock() = Some(refused);
        }
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
        self.call_cancellable(p, &NEVER, f)
    }
    /// `call` whose retry waits end on `cancel`.
    fn call_cancellable<T>(
        &self,
        p: &VPath,
        cancel: &AtomicBool,
        f: impl Fn(&blocking::Operator) -> opendal::Result<T>,
    ) -> Result<T> {
        if let Some(oauth) = &self.oauth {
            self.ensure_signed_in(oauth)?;
            if Self::expiring(oauth) {
                self.refresh(None)?;
            }
        }
        let (mut attempt, mut refreshed) = (0, false);
        loop {
            let (op, generation) = {
                let op = self.op.read();
                (op.clone(), self.generation.load(Ordering::SeqCst))
            };
            let e = match f(&op) {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            if http_status(&e) == Some(401) && self.oauth.is_some() && !refreshed {
                refreshed = true;
                self.refresh(Some(generation))?;
                continue;
            }
            match backoff(attempt, jitter()).filter(|_| retryable(&e)) {
                Some(delay) => {
                    tracing::debug!(attempt, ?delay, "cloud request retry");
                    sleep_unless_cancelled(delay, cancel)?;
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
    /// `f` over the folder's listing while it is fresh (under the lock: no copy per lookup).
    fn cached<R>(&self, dir: &str, f: impl FnOnce(&[Entry]) -> R) -> Option<R> {
        let mut listings = self.listings.lock();
        match listings.get(dir) {
            Some((at, entries)) if at.elapsed() < self.ttl => Some(f(entries)),
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
        self.list_with(dir, false)
    }

    /// `complete`: skip the cache and fail instead of cutting the listing off at the cap.
    fn list_with(&self, dir: &VPath, complete: bool) -> Result<Vec<Entry>> {
        self.validate(dir)?;
        if !complete {
            if let Some(hit) = self.cached(&dir.path, <[Entry]>::to_vec) {
                return Ok(hit);
            }
        }
        let own = key(dir, true);
        // Paged lazily: past the cap (plus the folder itself and one more, to notice) no
        // further page is requested.
        let take = self.list_cap + 2;
        let raw = self.call(dir, |op| {
            op.lister(&own)?
                .take(take)
                .collect::<opendal::Result<Vec<_>>>()
        })?;
        let mut names = HashSet::new();
        let mut entries = Vec::with_capacity(raw.len());
        for item in raw {
            if item.path() == own || item.path() == own.trim_start_matches('/') {
                continue;
            }
            let name = item.name().trim_end_matches('/');
            // ponytail: Drive allows two items with one name; the first wins here (the
            // path is ambiguous to opendal too). A rename on the web makes both visible.
            if name.is_empty()
                || name.contains(['/', '\\'])
                || google_native(item.metadata())
                || !names.insert(name.to_owned())
            {
                continue;
            }
            entries.push(self.entry(dir.join(name), item.metadata()));
        }
        if entries.len() > self.list_cap {
            anyhow::ensure!(
                !complete,
                "{} has more than {} entries: listing incomplete",
                dir.display(),
                self.list_cap
            );
            entries.truncate(self.list_cap);
            tracing::warn!(
                folder = %dir.display(),
                shown = self.list_cap,
                "cloud folder has more entries than are shown"
            );
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
        let name = p.name();
        match self.cached(&parent.path, |entries| {
            entries.iter().find(|e| e.name == name).cloned()
        }) {
            Some(hit) => hit.ok_or_else(|| not_found(p)),
            None => self.stat_remote(p, &NEVER),
        }
    }
    fn stat_remote(&self, p: &VPath, cancel: &AtomicBool) -> Result<Entry> {
        if p.parent().is_none() {
            return Ok(self.root_entry(p));
        }
        let file = key(p, false);
        match self.call_cancellable(p, cancel, |op| op.stat(&file)) {
            Ok(meta) => Ok(self.entry(p.clone(), &meta)),
            Err(e) if is_not_found(&e) => {
                let dir = key(p, true);
                let meta = self.call_cancellable(p, cancel, |op| op.stat(&dir))?;
                Ok(self.entry(p.clone(), &meta))
            }
            Err(e) => Err(e),
        }
    }
    fn maybe_stat(&self, p: &VPath) -> Result<Option<Entry>> {
        match self.stat_remote(p, &NEVER) {
            Ok(e) => Ok(Some(e)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn read(&self, p: &VPath, cancel: &AtomicBool) -> Result<Box<dyn Read + Send>> {
        self.read_from(p, 0, cancel)
    }
    /// The file from byte `offset` (a ranged request; the size is the service's, not the
    /// listing cache's). Read it lazily: bytes are fetched as they are read.
    fn read_from(
        &self,
        p: &VPath,
        offset: u64,
        cancel: &AtomicBool,
    ) -> Result<Box<dyn Read + Send>> {
        let entry = self.stat(p)?;
        anyhow::ensure!(entry.kind == Kind::File, "not a file: {}", p.display());
        let k = key(p, false);
        let reader =
            self.call_cancellable(p, cancel, |op| op.reader(&k)?.into_std_read(offset..))?;
        Ok(Box::new(CloudReader {
            inner: reader,
            path: p.clone(),
        }))
    }
    /// Names (and whether each is a folder) directly in `dir`, from the service.
    fn children(&self, dir: &VPath) -> Result<Vec<(String, bool)>> {
        let k = key(dir, true);
        Ok(self
            .call(dir, |op| op.list(&k))?
            .into_iter()
            .filter(|c| c.path() != k)
            .map(|c| {
                (
                    c.name().trim_end_matches('/').to_owned(),
                    c.metadata().is_dir(),
                )
            })
            .collect())
    }

    fn rename(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        self.validate(from)?;
        self.validate(to)?;
        anyhow::ensure!(
            from.parent().is_some() && to.parent().is_some(),
            "cannot rename the root"
        );
        anyhow::ensure!(
            !within(to, from),
            "cannot move {} into itself",
            from.display()
        );
        let source = self.stat_remote(from, &NEVER)?;
        let existing = self.maybe_stat(to)?;
        if let Some(existing) = &existing {
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
        }
        // ponytail: check-then-move; a file created at `to` in between is replaced on
        // Drive/S3 (Dropbox refuses). Services offer no atomic no-replace move here.
        let result = match existing {
            // Drive's rename replaces (trashing the old file) and S3 copies over it;
            // Dropbox refuses to.
            Some(_) if self.account.kind == CloudKind::Dropbox => self.replace_aside(from, to),
            _ => self.move_entry(from, to, source.kind == Kind::Dir, 0),
        };
        self.invalidate(&from.path);
        self.invalidate(&to.path);
        result
    }
    /// The old file steps aside before the new one moves in, and comes back if that fails:
    /// neither copy is deleted before the other is in place.
    fn replace_aside(&self, from: &VPath, to: &VPath) -> Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let aside = VPath {
            path: format!(
                "{}.keel-replaced-{}-{}",
                to.path,
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            ..to.clone()
        };
        self.move_entry(to, &aside, false, 0)
            .with_context(|| format!("could not replace {}", to.display()))?;
        if let Err(e) = self.move_entry(from, to, false, 0) {
            let old = match self.move_entry(&aside, to, false, 0) {
                Ok(()) => "the old file is unchanged".to_owned(),
                Err(_) => format!("the old file is now {}", aside.display()),
            };
            return Err(e.context(format!(
                "could not replace {}: {old}; the new copy is still at {}",
                to.display(),
                from.display()
            )));
        }
        let k = key(&aside, false);
        if let Err(e) = self.call(&aside, |op| op.delete(&k)) {
            tracing::warn!(file = %aside.display(), "replaced file left behind: {e:#}");
        }
        Ok(())
    }
    /// One request where the service can rename (Drive and Dropbox move a folder with its
    /// contents); else folders file by file and files by copy (or stream) and delete.
    fn move_entry(&self, from: &VPath, to: &VPath, dir: bool, depth: usize) -> Result<()> {
        let caps = self.caps();
        let (src, dst) = (key(from, false), key(to, false));
        if caps.rename {
            // File-form keys for folders too: opendal forwards only those, and both
            // services resolve them to the folder.
            return self.call(from, |op| op.rename(&src, &dst));
        }
        if dir {
            return self.move_folder_by_files(from, to, depth);
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
    /// The source folder goes only once empty: anything added to it meanwhile, or not
    /// moved because of an error, stays there and is named.
    fn move_folder_by_files(&self, from: &VPath, to: &VPath, depth: usize) -> Result<()> {
        anyhow::ensure!(depth < 256, "directory nesting limit: {}", from.display());
        let children = self.children(from)?;
        // Drive allows one name twice; moving both would replace one with the other.
        let mut seen = HashSet::new();
        if let Some((twice, _)) = children.iter().find(|(n, _)| !seen.insert(n)) {
            anyhow::bail!(
                "{} holds two items named {twice:?}; rename one of them first",
                from.display()
            );
        }
        let dst = key(to, true);
        self.call(to, |op| op.create_dir(&dst))?;
        let total = children.len();
        for (moved, (name, is_dir)) in children.iter().enumerate() {
            self.move_entry(&from.join(name), &to.join(name), *is_dir, depth + 1)
                .with_context(|| {
                    format!(
                        "moved {moved} of {total} items out of {}; the source still holds \
                         the rest",
                        from.display()
                    )
                })?;
        }
        let left = self.children(from)?;
        if !left.is_empty() {
            let mut names: Vec<_> = left.iter().take(10).map(|(n, _)| n.as_str()).collect();
            if left.len() > names.len() {
                names.push("...");
            }
            anyhow::bail!(
                "moved {total} of {} items out of {}; the source still holds the rest: {}",
                total + left.len(),
                from.display(),
                names.join(", ")
            );
        }
        let src = key(from, true);
        self.call(from, |op| op.delete(&src))
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.validate(p)?;
        anyhow::ensure!(p.parent().is_some(), "cannot delete the account root");
        let entry = self.stat_remote(p, &NEVER)?;
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
        anyhow::ensure!(
            self.children(p)?.is_empty(),
            "directory not empty: {}",
            p.display()
        );
        let k = key(p, true);
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
    io_kind(e) == Some(io::ErrorKind::NotFound)
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

/// One upload, written straight to the target: no staging name is needed because the
/// file (or object) only changes once the upload completes. Services that take a file in
/// one request (Drive, Dropbox) get it from memory on `flush()`, with the usual retries
/// and token refresh, whose waits end on `cancel`; S3 streams (multipart past the first
/// part). Dropped without `flush()`: nothing is written.
struct CloudUpload<'c> {
    core: Arc<Core>,
    target: VPath,
    exclusive: bool,
    cancel: &'c AtomicBool,
    sink: Sink,
    written: u64,
    done: bool,
    failed: bool,
}
enum Sink {
    Memory(Vec<u8>),
    Stream(Option<blocking::Writer>),
}
impl<'c> CloudUpload<'c> {
    fn start(
        core: Arc<Core>,
        target: &VPath,
        exclusive: bool,
        cancel: &'c AtomicBool,
    ) -> Result<Self> {
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
        let sink = if caps.write_can_multi {
            let k = key(target, false);
            let if_not_exists = exclusive && caps.write_with_if_not_exists;
            Sink::Stream(Some(core.call_cancellable(target, cancel, |op| {
                op.writer_options(
                    &k,
                    options::WriteOptions {
                        if_not_exists,
                        ..Default::default()
                    },
                )
            })?))
        } else {
            Sink::Memory(Vec::new())
        };
        Ok(Self {
            core,
            target: target.clone(),
            exclusive,
            cancel,
            sink,
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
        if self.cancel.load(Ordering::Relaxed) {
            self.failed = true;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled").into());
        }
        let result = match &mut self.sink {
            Sink::Memory(data) => {
                let data = Buffer::from(std::mem::take(data));
                let k = key(&self.target, false);
                let opts = options::WriteOptions {
                    if_not_exists: self.exclusive && self.core.caps().write_with_if_not_exists,
                    ..Default::default()
                };
                // ponytail: cancel ends the retry waits, not a request already in flight.
                self.core
                    .call_cancellable(&self.target, self.cancel, |op| {
                        op.write_options(&k, data.clone(), opts.clone())
                    })
                    .map(drop)
            }
            Sink::Stream(writer) => match writer.take() {
                Some(mut w) => w.close().map(drop).map_err(|e| wire(&e, &self.target)),
                None => Err(anyhow::anyhow!("upload already closed")),
            },
        };
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
impl Write for CloudUpload<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.done || self.failed {
            return Err(io::Error::other("upload finished or failed"));
        }
        let len = bytes.len() as u64;
        let result = match &mut self.sink {
            Sink::Memory(data) => match self.core.upload_limit {
                Some(limit) if self.written + len > limit => Err(io::Error::other(format!(
                    "{}: files over {} MB cannot be uploaded to {} yet",
                    self.target.display(),
                    limit >> 20,
                    self.core.account.kind.name()
                ))),
                _ => {
                    data.extend_from_slice(bytes);
                    Ok(())
                }
            },
            Sink::Stream(writer) => match writer.as_mut() {
                Some(w) => w
                    .write(Buffer::from(bytes.to_vec()))
                    .map_err(|e| io::Error::other(format!("{:#}", wire(&e, &self.target)))),
                None => Err(io::Error::other("upload closed")),
            },
        };
        match result {
            Ok(()) => {
                self.written += len;
                Ok(bytes.len())
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }
    /// Commits the upload (see `Provider::write`).
    fn flush(&mut self) -> io::Result<()> {
        self.commit().map_err(|e| {
            let kind = io_kind(&e).unwrap_or(io::ErrorKind::Other);
            io::Error::new(kind, format!("{e:#}"))
        })
    }
}
impl Drop for CloudUpload<'_> {
    fn drop(&mut self) {
        if !self.done && !self.failed {
            tracing::error!(
                target = %self.target.display(),
                "cloud upload dropped without flush(); discarding it"
            );
        }
        // ponytail: dropping an S3 writer leaves an unfinished multipart upload to the
        // bucket's lifecycle rules; opendal's blocking writer has no abort.
    }
}

/// What `cached_download` sees: stats straight from the service (so its "source changed"
/// check compares fresh metadata, not the 60 s listing cache) and retry waits that end on
/// `cancel`.
struct Fresh<'a> {
    cloud: &'a CloudProvider,
    cancel: &'a AtomicBool,
}
impl Provider for Fresh<'_> {
    fn scheme(&self) -> &'static str {
        self.cloud.scheme()
    }
    fn caps(&self) -> Caps {
        self.cloud.caps()
    }
    fn list(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.cloud.list(dir)
    }
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.cloud.core.list_with(dir, true)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.cloud.core.validate(p)?;
        self.cloud.core.stat_remote(p, self.cancel)
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        self.cloud.core.read(p, self.cancel)
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.cloud.write(p)
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.cloud.mkdir(p)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.cloud.rename(from, to)
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.cloud.remove(p)
    }
    fn remove_kind(&self) -> RemoveKind {
        self.cloud.core.account.kind.remove_kind()
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        self.cloud.local_copy(p)
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
    fn list_complete(&self, dir: &VPath) -> Result<Vec<Entry>> {
        self.core.list_with(dir, true)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.core.stat(p)
    }
    fn read(&self, p: &VPath) -> Result<Box<dyn Read + Send>> {
        self.core.read(p, &NEVER)
    }
    fn read_range(&self, p: &VPath, offset: u64, len: u64) -> Result<Option<Box<dyn Read + Send>>> {
        Ok(Some(Box::new(
            self.core.read_from(p, offset, &NEVER)?.take(len),
        )))
    }
    fn write(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(CloudUpload::start(
            self.core.clone(),
            p,
            false,
            &NEVER,
        )?))
    }
    fn create_new(&self, p: &VPath) -> Result<Box<dyn Write + Send>> {
        self.create_new_cancellable(p, &NEVER)
    }
    fn create_new_cancellable<'a>(
        &self,
        p: &VPath,
        cancel: &'a AtomicBool,
    ) -> Result<Box<dyn Write + Send + 'a>> {
        Ok(Box::new(CloudUpload::start(
            self.core.clone(),
            p,
            true,
            cancel,
        )?))
    }
    fn uploads_on_flush(&self) -> Option<&'static str> {
        (!self.core.caps().write_can_multi).then(|| self.core.account.kind.name())
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
    fn remove_kind(&self) -> RemoveKind {
        self.core.account.kind.remove_kind()
    }
    fn local_copy(&self, p: &VPath) -> Result<PathBuf> {
        self.local_copy_cancellable(p, &|_| {}, &AtomicBool::new(false))
    }
    /// Through the same download cache as SFTP (`<cache>/remote/cloud-<id>/`), with fresh
    /// stats (not the listing cache) for its "source changed" check.
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
        let dav = a
            .webdav
            .as_ref()
            .map(|w| format!("{}|{}", w.url, w.username))
            .unwrap_or_default();
        let kind = format!("{:?}", a.kind);
        crate::sftp::cached_download(
            &Fresh {
                cloud: self,
                cancel,
            },
            p,
            &format!("cloud-{}", a.id),
            &[&kind, &a.id, a.root.as_deref().unwrap_or(""), &s3, &dav],
            progress,
            cancel,
        )
    }
}

#[cfg(test)]
mod tests;
