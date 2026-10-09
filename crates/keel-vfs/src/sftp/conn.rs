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

pub(super) fn runtime() -> &'static tokio::runtime::Runtime {
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
    state: Mutex<State>,
    status: parking_lot::Mutex<ConnStatus>,
    timeout: parking_lot::RwLock<Duration>,
}
impl ConnPool {
    pub fn new(host: RemoteHost, events: Sender<RemoteEvent>) -> Self {
        Self {
            host,
            events,
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
    pub(super) fn session(&self) -> Result<Arc<Session>> {
        let timeout = *self.timeout.read();
        // TCP connect, handshake and auth each get `timeout`; a pending host key prompt
        // extends the handshake up to PROMPT_TIMEOUT.
        let result = run_for(timeout * 3 + PROMPT_TIMEOUT, async {
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
        };
        let config = client::Config {
            keepalive_interval: Some(Duration::from_secs(10)),
            keepalive_max: 2,
            ..Default::default()
        };
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
            None => e.context("SSH handshake failed"),
        })?;
        tokio::time::timeout(timeout, async {
            auth::authenticate(&mut ssh, &self.host).await?;
            let channel = ssh.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            let raw = RawSftpSession::new(channel.into_stream());
            raw.set_timeout(timeout.as_secs().max(1));
            raw.init().await?;
            Ok(Arc::new(Session { raw, ssh }))
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
}
