use super::{auth, hostkeys, ConnStatus, RemoteEvent, RemoteHost};
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use russh::client;
use russh_sftp::client::RawSftpSession;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// The tokio runtime shared by the SFTP and cloud providers.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("SFTP runtime")
    })
}

/// How long a first-connect "trust this host key?" prompt may stay open.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Fail-fast marker: the pool is waiting out its reconnect backoff.
#[derive(Debug, thiserror::Error)]
#[error("reconnecting in {0} s")]
struct Backoff(u64);

pub(super) struct Client {
    host: RemoteHost,
    events: Sender<RemoteEvent>,
    problem: Arc<parking_lot::Mutex<Option<String>>>,
    prompting: Arc<AtomicBool>,
    listener: Arc<AtomicBool>,
}
impl client::Handler for Client {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, key: &russh::keys::ssh_key::PublicKey) -> Result<bool> {
        let key = ssh_key::PublicKey::from_openssh(&key.to_openssh()?)?;
        let host = self.host.host.clone();
        let port = self.host.port;
        let candidate = key.clone();
        let verdict = tokio::task::spawn_blocking(move || {
            hostkeys::known_hosts_check(&host, port, &candidate)
        })
        .await?;
        match verdict {
            hostkeys::HostKeyVerdict::Known => Ok(true),
            hostkeys::HostKeyVerdict::Mismatch => {
                *self.problem.lock() = Some(format!(
                    "host key for {} does not match ~/.ssh/known_hosts (changed or revoked); \
                     refusing to connect. If the host was reinstalled, remove its old entry.",
                    self.host.host
                ));
                Ok(false)
            }
            // Nobody would answer a prompt (no window drains the events): fail at once
            // instead of pinning a worker for PROMPT_TIMEOUT.
            hostkeys::HostKeyVerdict::Unknown(_) if !self.listener.load(Ordering::SeqCst) => {
                *self.problem.lock() = Some(format!(
                    "host key for {} is not known yet and no Keel window is open to confirm \
                     it; connect from the Keel window first",
                    self.host.host
                ));
                Ok(false)
            }
            hostkeys::HostKeyVerdict::Unknown(fingerprint) => {
                let (reply, receiver) = crossbeam_channel::bounded(1);
                self.events
                    .try_send(RemoteEvent::HostKeyPrompt {
                        host_id: self.host.id.clone(),
                        fingerprint,
                        reply,
                    })
                    .context("host key approval unavailable")?;
                self.prompting.store(true, Ordering::SeqCst);
                let trusted = tokio::task::spawn_blocking(move || {
                    receiver.recv_timeout(PROMPT_TIMEOUT).unwrap_or(false)
                })
                .await
                .unwrap_or(false);
                self.prompting.store(false, Ordering::SeqCst);
                if !trusted {
                    *self.problem.lock() = Some("host key was not trusted".into());
                    return Ok(false);
                }
                let host = self.host.host.clone();
                tokio::task::spawn_blocking(move || hostkeys::add_known_host(&host, port, &key))
                    .await??;
                Ok(true)
            }
        }
    }
}

pub(super) struct Session {
    pub raw: RawSftpSession,
    pub ssh: client::Handle<Client>,
    pub posix_rename: bool,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.raw.close_session();
    }
}
struct State {
    session: Option<Arc<Session>>,
    failures: u32,
    retry_at: Instant,
}

/// One lazily connected SSH/SFTP session per host. Every call blocks the calling worker
/// thread (never the UI thread) for at most the operation timeout (default 20 s).
pub struct ConnPool {
    host: RemoteHost,
    events: Sender<RemoteEvent>,
    listener: Arc<AtomicBool>,
    state: Mutex<State>,
    status: parking_lot::Mutex<ConnStatus>,
    timeout: parking_lot::RwLock<Duration>,
}
impl ConnPool {
    /// Assumes something answers `HostKeyPrompt` events on `events`.
    pub fn new(host: RemoteHost, events: Sender<RemoteEvent>) -> Self {
        Self::with_prompt_listener(host, events, Arc::new(AtomicBool::new(true)))
    }
    /// `listener` false: an unknown host key fails at once instead of prompting.
    pub fn with_prompt_listener(
        host: RemoteHost,
        events: Sender<RemoteEvent>,
        listener: Arc<AtomicBool>,
    ) -> Self {
        Self {
            host,
            events,
            listener,
            state: Mutex::new(State {
                session: None,
                failures: 0,
                retry_at: Instant::now(),
            }),
            status: parking_lot::Mutex::new(ConnStatus::Disconnected),
            timeout: parking_lot::RwLock::new(Duration::from_secs(20)),
        }
    }
    pub fn set_timeout(&self, timeout: Duration) {
        *self.timeout.write() = timeout.max(Duration::from_millis(1));
    }
    pub fn status(&self) -> ConnStatus {
        *self.status.lock()
    }
    fn event(&self, status: ConnStatus, detail: &str) {
        *self.status.lock() = status;
        let _ = self.events.try_send(RemoteEvent::Status {
            host_id: self.host.id.clone(),
            status,
            detail: detail.into(),
        });
    }
    /// Opens the session now (the sidebar's "Connect"); operations otherwise connect lazily.
    pub fn connect(&self) -> Result<()> {
        self.session().map(drop)
    }
    pub fn disconnect(&self) {
        self.run(async {
            let mut state = self.state.lock().await;
            if let Some(session) = state.session.take() {
                let _ = session.raw.close_session();
                let _ = session
                    .ssh
                    .disconnect(russh::Disconnect::ByApplication, "", "")
                    .await;
            }
            state.failures = 0;
            state.retry_at = Instant::now();
            Ok(())
        })
        .ok();
        self.event(ConnStatus::Disconnected, "disconnected");
    }
    pub(super) fn run<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        run_for(*self.timeout.read(), future)
    }
    /// No overall deadline: for long multi-request operations (listing a huge folder)
    /// whose every SFTP request is still bounded by the session's request timeout.
    pub(super) fn run_per_request<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        runtime().block_on(future)
    }
    pub(super) fn session(&self) -> Result<Arc<Session>> {
        let timeout = *self.timeout.read();
        // Worst case, each bounded by `timeout`: the backoff wait, TCP connect, the
        // handshake up to the host key check, the handshake slice in which a prompt ends,
        // and auth + SFTP start; an answered host key prompt adds up to PROMPT_TIMEOUT.
        let prompt = if self.listener.load(Ordering::SeqCst) {
            PROMPT_TIMEOUT
        } else {
            Duration::ZERO
        };
        let result = run_for(timeout * 5 + prompt, async {
            let mut state = self.state.lock().await;
            if let Some(s) = &state.session {
                if !s.ssh.is_closed() {
                    return Ok((s.clone(), false));
                }
                state.session = None;
            }
            // Wait out a short backoff; fail fast on a long one so a dead host never
            // pins a worker and the first call after the window reconnects.
            let wait = state.retry_at.saturating_duration_since(Instant::now());
            if wait > timeout {
                return Err(Backoff(wait.as_secs().max(1)).into());
            }
            tokio::time::sleep(wait).await;
            self.event(ConnStatus::Connecting, "connecting");
            let session = self.open(timeout).await?;
            state.failures = 0;
            state.session = Some(session.clone());
            Ok((session, true))
        });
        match result {
            Ok((session, fresh)) => {
                if fresh {
                    self.event(ConnStatus::Connected, "connected");
                }
                Ok(session)
            }
            Err(e) => {
                if e.downcast_ref::<Backoff>().is_none() {
                    self.failed(&format!("{e:#}"));
                }
                Err(e)
            }
        }
    }
    async fn open(&self, timeout: Duration) -> Result<Arc<Session>> {
        let problem = Arc::new(parking_lot::Mutex::new(None));
        let prompting = Arc::new(AtomicBool::new(false));
        let handler = Client {
            host: self.host.clone(),
            events: self.events.clone(),
            problem: problem.clone(),
            prompting: prompting.clone(),
            listener: self.listener.clone(),
        };
        let mut config = client::Config {
            keepalive_interval: Some(Duration::from_secs(10)),
            keepalive_max: 2,
            ..Default::default()
        };
        // A host with known keys may only present one of those key types: offering others
        // would let an attacker with a different key type get a first-connect prompt.
        let (host, port) = (self.host.host.clone(), self.host.port);
        let known =
            tokio::task::spawn_blocking(move || hostkeys::known_key_types(&host, port)).await?;
        if !known.is_empty() {
            let keys: Vec<_> = config
                .preferred
                .key
                .iter()
                .filter(|a| known.iter().any(|k| k == key_type(a)))
                .cloned()
                .collect();
            anyhow::ensure!(!keys.is_empty(), algorithm_changed(&self.host.host, &known));
            config.preferred.key = keys.into();
        }
        let stream = tokio::time::timeout(
            timeout,
            tokio::net::TcpStream::connect((self.host.host.as_str(), self.host.port)),
        )
        .await
        .context("TCP connect timed out")?
        .context("TCP connect failed")?;
        let connecting = client::connect_stream(Arc::new(config), stream, handler);
        tokio::pin!(connecting);
        let connected = loop {
            match tokio::time::timeout(timeout, &mut connecting).await {
                Ok(r) => break r,
                Err(_) if prompting.load(Ordering::SeqCst) => continue,
                Err(_) => anyhow::bail!("SSH handshake timed out"),
            }
        };
        let mut ssh = connected.map_err(|e| match problem.lock().take() {
            Some(problem) => anyhow::anyhow!(problem),
            None if !known.is_empty()
                && matches!(
                    e.downcast_ref::<russh::Error>(),
                    Some(russh::Error::NoCommonAlgo {
                        kind: russh::AlgorithmKind::Key,
                        ..
                    })
                ) =>
            {
                anyhow::anyhow!(algorithm_changed(&self.host.host, &known))
            }
            None => e.context("SSH handshake failed"),
        })?;
        tokio::time::timeout(timeout, async {
            auth::authenticate(&mut ssh, &self.host).await?;
            let channel = ssh.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            let raw = RawSftpSession::new(channel.into_stream());
            raw.set_timeout(timeout.as_secs().max(1));
            let version = raw.init().await?;
            Ok(Arc::new(Session {
                raw,
                ssh,
                posix_rename: version.extensions.contains_key("posix-rename@openssh.com"),
            }))
        })
        .await
        .context("SSH authentication or SFTP start timed out")?
    }
    /// Drops the session and schedules the next reconnect after the backoff.
    pub(super) fn failed(&self, detail: &str) {
        if let Ok(mut state) = self.state.try_lock() {
            if let Some(session) = state.session.take() {
                let _ = session.raw.close_session();
            }
            state.retry_at = Instant::now() + backoff(state.failures);
            state.failures = state.failures.saturating_add(1);
        }
        self.event(
            ConnStatus::Failed,
            &format!("{detail}; reconnecting on next operation"),
        );
    }
}
fn run_for<T>(timeout: Duration, future: impl Future<Output = Result<T>>) -> Result<T> {
    runtime().block_on(async {
        tokio::time::timeout(timeout, future)
            .await
            .context("SFTP operation timed out")?
    })
}
fn backoff(failures: u32) -> Duration {
    Duration::from_secs((1u64 << failures.min(5)).min(30))
}
/// The known_hosts key type a negotiated host key algorithm uses.
fn key_type(algorithm: &russh::keys::Algorithm) -> &str {
    match algorithm {
        russh::keys::Algorithm::Rsa { .. } => "ssh-rsa",
        other => other.as_str(),
    }
}
fn algorithm_changed(host: &str, known: &[String]) -> String {
    format!(
        "host key algorithm changed for {host}: ~/.ssh/known_hosts trusts {} but the server \
         offers none of them; refusing to connect. If the host was reinstalled, remove its old \
         entry.",
        known.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_timeout_and_backoff() {
        let (tx, _) = crossbeam_channel::unbounded();
        let host = RemoteHost {
            id: "test".into(),
            label: String::new(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: super::super::RemoteAuth::Agent,
            home: None,
            bookmarks: vec![],
        };
        let pool = ConnPool::new(host, tx);
        pool.set_timeout(Duration::from_millis(20));
        let start = Instant::now();
        assert!(pool.run::<()>(std::future::pending()).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            (0..7).map(|n| backoff(n).as_secs()).collect::<Vec<_>>(),
            [1, 2, 4, 8, 16, 30, 30]
        );
    }

    /// Review focus 2: a host that accepts TCP but never speaks SSH fails within the
    /// timeout, reports Failed, and the next call fails fast during the backoff window.
    #[test]
    fn hung_host_times_out_then_backs_off() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let host = RemoteHost {
            id: "hung".into(),
            label: String::new(),
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            user: "nobody".into(),
            auth: super::super::RemoteAuth::Agent,
            home: None,
            bookmarks: vec![],
        };
        let pool = ConnPool::new(host, tx);
        pool.set_timeout(Duration::from_millis(300));
        let start = Instant::now();
        let err = pool.connect().unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(pool.status(), ConnStatus::Failed);
        let start = Instant::now();
        let err = pool.connect().unwrap_err();
        assert!(err.downcast_ref::<Backoff>().is_some(), "{err:#}");
        assert!(start.elapsed() < Duration::from_millis(100));
        let statuses: Vec<_> = rx
            .try_iter()
            .filter_map(|e| match e {
                RemoteEvent::Status { status, .. } => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, [ConnStatus::Connecting, ConnStatus::Failed]);
        pool.disconnect();
        assert_eq!(pool.status(), ConnStatus::Disconnected);
    }

    struct Nobody;
    impl russh::server::Handler for Nobody {
        type Error = russh::Error;
    }
    /// An in-process SSH server with a fresh `algorithm` host key that rejects all logins.
    fn fake_server(algorithm: russh::keys::Algorithm) -> (u16, russh::keys::PublicKey) {
        use russh::keys::ssh_key::rand_core::OsRng;
        let key = russh::keys::PrivateKey::random(&mut OsRng, algorithm).unwrap();
        let public = key.public_key().clone();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            ..Default::default()
        });
        let listener = runtime()
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        runtime().spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, socket, Nobody).await {
                        let _ = session.await;
                    }
                });
            }
        });
        (port, public)
    }
    fn pool_for(port: u16, listener: bool) -> (ConnPool, crossbeam_channel::Receiver<RemoteEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let host = RemoteHost {
            id: "fake".into(),
            label: String::new(),
            host: "127.0.0.1".into(),
            port,
            user: "nobody".into(),
            auth: super::super::RemoteAuth::PasswordInKeyring,
            home: None,
            bookmarks: vec![],
        };
        let pool = ConnPool::with_prompt_listener(host, tx, Arc::new(AtomicBool::new(listener)));
        pool.set_timeout(Duration::from_secs(5));
        (pool, rx)
    }

    /// m29: no prompt listener -> an unknown key fails fast. M12: known_hosts trusts only
    /// another key type -> hard failure, never a prompt. A matching key passes the check.
    #[test]
    fn host_key_algorithm_rules_against_a_local_server() {
        let _guard = hostkeys::TEST_LOCK.lock();
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("known_hosts");
        std::fs::write(&file, "").unwrap();
        *hostkeys::TEST_FILE.lock() = Some(file.clone());
        let (port, server_key) = fake_server(russh::keys::Algorithm::Ed25519);

        let (pool, rx) = pool_for(port, false);
        let start = Instant::now();
        let err = format!("{:#}", pool.connect().unwrap_err());
        assert!(err.contains("no Keel window"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!rx
            .try_iter()
            .any(|e| matches!(e, RemoteEvent::HostKeyPrompt { .. })));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");

        let (_, other) = fake_server(russh::keys::Algorithm::Ecdsa {
            curve: russh::keys::EcdsaCurve::NistP256,
        });
        std::fs::write(
            &file,
            format!("[127.0.0.1]:{port} {}\n", other.to_openssh().unwrap()),
        )
        .unwrap();
        let (pool, rx) = pool_for(port, true);
        let err = format!("{:#}", pool.connect().unwrap_err());
        assert!(err.contains("host key algorithm changed"), "{err}");
        assert!(!rx
            .try_iter()
            .any(|e| matches!(e, RemoteEvent::HostKeyPrompt { .. })));

        std::fs::write(
            &file,
            format!(
                "[127.0.0.1]:{port} {}\n[127.0.0.1]:{port} {}\n",
                other.to_openssh().unwrap(),
                server_key.to_openssh().unwrap()
            ),
        )
        .unwrap();
        let (pool, _) = pool_for(port, false);
        let err = format!("{:#}", pool.connect().unwrap_err());
        *hostkeys::TEST_FILE.lock() = None;
        // Past the host key check: the login itself fails (no stored password).
        assert!(err.contains("keychain"), "{err}");
    }
}
