//! FTPS: rustls (graviola crypto) behind suppaftp's `TlsConnector`, the server certificate
//! checked against the OS trust store, and certificates the user trusted pinned by their
//! SHA-256 fingerprint (`ftps_known_hosts` in Keel's configuration folder).

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use suppaftp::{FtpError, FtpResult};

/// A certificate that failed verification: its fingerprint and why it failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub fingerprint: String,
    pub reason: String,
}

/// `AB:CD:...`, the SHA-256 of the certificate's DER bytes.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[derive(Debug)]
struct Verifier {
    inner: Arc<dyn ServerCertVerifier>,
    /// The fingerprint the user trusted for this host, if any.
    pin: Option<String>,
    rejected: Arc<parking_lot::Mutex<Option<Rejected>>>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = fingerprint(end_entity);
        if self.pin.as_deref() == Some(seen.as_str()) {
            return Ok(ServerCertVerified::assertion());
        }
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
            .inspect_err(|e| {
                *self.rejected.lock() = Some(Rejected {
                    fingerprint: seen,
                    reason: e.to_string(),
                })
            })
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// graviola, checked once: it asserts CPU features on first use (as for cloud accounts).
fn provider() -> Result<Arc<rustls::crypto::CryptoProvider>> {
    static PROVIDER: OnceLock<Option<Arc<rustls::crypto::CryptoProvider>>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| {
            crate::quiet_panic::quiet(|| {
                std::panic::catch_unwind(|| Arc::new(rustls_graviola::default_provider())).ok()
            })
        })
        .clone()
        .context("this processor lacks a feature FTPS' TLS crypto needs")
}

/// The OS trust store (tests: the given roots only).
fn system_verifier(
    provider: Arc<rustls::crypto::CryptoProvider>,
    #[cfg(test)] roots: &[CertificateDer<'static>],
) -> Result<Arc<dyn ServerCertVerifier>> {
    #[cfg(test)]
    if !roots.is_empty() {
        let mut store = rustls::RootCertStore::empty();
        for r in roots {
            store.add(r.clone())?;
        }
        return Ok(rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(store),
            provider,
        )
        .build()?);
    }
    Ok(Arc::new(rustls_platform_verifier::Verifier::new(provider)?))
}

/// Opens TLS on the control connection and on every data connection, with one client
/// configuration (so data connections can resume the control connection's session, which
/// many servers require).
pub struct Connector {
    config: Arc<ClientConfig>,
    pub rejected: Arc<parking_lot::Mutex<Option<Rejected>>>,
    timeout: Duration,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connector")
    }
}

impl Connector {
    /// `pin`: a fingerprint accepted even when verification fails; `timeout`: for the
    /// handshake and every read and write; `roots`: trust only these (tests), empty: the OS
    /// trust store.
    pub fn new(
        pin: Option<String>,
        timeout: Duration,
        roots: &[CertificateDer<'static>],
    ) -> Result<Self> {
        let provider = provider()?;
        #[cfg(test)]
        let inner = system_verifier(provider.clone(), roots)?;
        #[cfg(not(test))]
        let inner = {
            let _ = roots;
            system_verifier(provider.clone())?
        };
        let rejected = Arc::default();
        let verifier = Verifier {
            inner,
            pin,
            rejected: Arc::clone(&rejected),
        };
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
            rejected,
            timeout,
        })
    }
}

impl suppaftp::TlsConnector for Connector {
    type Stream = Tls;
    /// Completes the handshake here: a connection whose handshake failed is never used.
    fn connect(&self, domain: &str, mut stream: TcpStream) -> FtpResult<Tls> {
        let name = ServerName::try_from(domain.trim_start_matches('[').trim_end_matches(']'))
            .map_err(|e| FtpError::SecureError(e.to_string()))?
            .to_owned();
        let mut conn = ClientConnection::new(Arc::clone(&self.config), name)
            .map_err(|e| FtpError::SecureError(e.to_string()))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|()| stream.set_write_timeout(Some(self.timeout)))
            .map_err(FtpError::ConnectionError)?;
        while conn.is_handshaking() {
            conn.complete_io(&mut stream)
                .map_err(|e| FtpError::SecureError(e.to_string()))?;
        }
        Ok(Tls {
            stream: StreamOwned::new(conn, stream),
        })
    }
}

/// A TLS stream; dropping it sends `close_notify` (servers count a data connection without
/// one as a failed transfer).
#[derive(Debug)]
pub struct Tls {
    stream: StreamOwned<ClientConnection, TcpStream>,
}

impl suppaftp::TlsStream for Tls {
    type InnerStream = StreamOwned<ClientConnection, TcpStream>;
    fn tcp_stream(self) -> FtpResult<TcpStream> {
        self.stream
            .sock
            .try_clone()
            .map_err(FtpError::ConnectionError)
    }
    fn get_ref(&self) -> &TcpStream {
        self.stream.get_ref()
    }
    fn mut_ref(&mut self) -> &mut Self::InnerStream {
        &mut self.stream
    }
}

impl Drop for Tls {
    /// `close_notify`, then a short drain of what the server sent unasked (TLS 1.3 session
    /// tickets): a socket closed with unread data is reset, and a server can lose the end of
    /// an upload to the reset.
    fn drop(&mut self) {
        self.stream.conn.send_close_notify();
        while self.stream.conn.wants_write() {
            if self.stream.conn.write_tls(&mut self.stream.sock).is_err() {
                return;
            }
        }
        let sock = &mut self.stream.sock;
        let _ = sock.shutdown(std::net::Shutdown::Write);
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        let (mut buf, mut left) = ([0; 16 * 1024], 1 << 20);
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while left > 0 && std::time::Instant::now() < until {
            match std::io::Read::read(sock, &mut buf) {
                Ok(n) if n > 0 => left -= n.min(left),
                _ => break,
            }
        }
    }
}

/// Tests point the pins at a temp file (hold `TEST_LOCK` while it is set).
#[cfg(test)]
pub(crate) static TEST_FILE: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// `<KEEL_CONFIG_DIR or the per-user Keel configuration folder>/ftps_known_hosts`.
fn pins_file() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_FILE.lock().clone() {
        return Ok(path);
    }
    if let Some(dir) = std::env::var_os("KEEL_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("ftps_known_hosts"));
    }
    let app = if cfg!(target_os = "linux") {
        "keel"
    } else {
        "Keel"
    };
    Ok(directories::BaseDirs::new()
        .context("no configuration folder")?
        .config_dir()
        .join(app)
        .join("ftps_known_hosts"))
}

fn key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}

/// The fingerprint the user trusted for `host:port`, if any. An unreadable file fails
/// closed (an error, never "no pin").
pub fn pinned(host: &str, port: u16) -> Result<Option<String>> {
    pinned_at(&pins_file()?, host, port)
}

fn pinned_at(path: &Path, host: &str, port: u16) -> Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("cannot read the FTPS certificate pins"),
    };
    let key = key(host, port);
    Ok(text
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(k, _)| *k == key)
        .map(|(_, fp)| fp.trim().to_owned())
        .next_back())
}

/// Trusts `fingerprint` for `host:port` from now on (replacing an earlier pin).
pub fn pin(host: &str, port: u16, fingerprint: &str) -> Result<()> {
    pin_at(&pins_file()?, host, port, fingerprint)
}

fn pin_at(path: &Path, host: &str, port: u16, fingerprint: &str) -> Result<()> {
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _guard = LOCK.lock();
    anyhow::ensure!(
        !host.is_empty() && !host.contains(char::is_whitespace),
        "invalid host"
    );
    let key = key(host, port);
    let mut text: String = match fs::read_to_string(path) {
        Ok(t) => t
            .lines()
            .filter(|l| l.split_once(' ').is_none_or(|(k, _)| k != key))
            .map(|l| format!("{l}\n"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    text.push_str(&format!("{key} {fingerprint}\n"));
    fs::create_dir_all(path.parent().context("no folder for the pins")?)?;
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    temp.write_all(text.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pins_round_trip_per_host_and_port() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cfg/ftps_known_hosts");
        assert_eq!(pinned_at(&file, "Box", 21).unwrap(), None);
        pin_at(&file, "Box", 21, "AA:BB").unwrap();
        pin_at(&file, "box", 990, "CC").unwrap();
        assert_eq!(
            pinned_at(&file, "box", 21).unwrap().as_deref(),
            Some("AA:BB")
        );
        assert_eq!(pinned_at(&file, "BOX", 990).unwrap().as_deref(), Some("CC"));
        pin_at(&file, "box", 21, "DD").unwrap();
        assert_eq!(pinned_at(&file, "box", 21).unwrap().as_deref(), Some("DD"));
        assert_eq!(fs::read_to_string(&file).unwrap().lines().count(), 2);
        assert!(pin_at(&file, "a b", 21, "x").is_err());
        assert_eq!(fingerprint(b"").len(), 32 * 3 - 1);
    }
}
