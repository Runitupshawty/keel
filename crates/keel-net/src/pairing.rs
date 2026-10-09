use crate::{
    node::{now, random},
    wire, Node, Peer, ALPN,
};
use anyhow::{ensure, Context, Result};
use hmac::{Hmac, Mac};
use iroh::{endpoint::Connection, Endpoint, EndpointAddr, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const LIFETIME: Duration = Duration::from_secs(600);
const SHORT: &str = "keel1-";
const TICKET: &str = "keel-ticket1-";

/// A bearer invitation. Display shows the 128-bit short form; Debug is redacted.
/// Generated invitations retain a full ticket for offline/QR use. A parsed short
/// form only knows the rendezvous id and needs public address lookup; its `ticket`
/// method therefore returns that same short form.
#[derive(Clone)]
pub struct PairCode(String);
impl fmt::Debug for PairCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairCode([redacted])")
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Ticket {
    pub secret: [u8; 16],
    pub addr: EndpointAddr,
    pub expires: i64,
}
fn key(secret: &[u8; 16]) -> SecretKey {
    let mut hash = Sha256::new();
    hash.update(b"keel/net/1 rendezvous");
    hash.update(secret);
    SecretKey::from_bytes(&hash.finalize().into())
}
impl PairCode {
    /// Full generated ticket, suitable for QR. Treat this and Display as secrets.
    pub fn ticket(&self) -> String {
        self.0.clone()
    }
    pub(crate) fn encode(ticket: &Ticket) -> Result<Self> {
        Ok(Self(format!(
            "{TICKET}{}",
            data_encoding::BASE32_NOPAD
                .encode(&wire::encode(ticket)?)
                .to_ascii_lowercase()
        )))
    }
    pub(crate) fn decode(&self) -> Result<Ticket> {
        if let Some(encoded) = self.0.strip_prefix(TICKET) {
            let ticket: Ticket = wire::decode(
                &data_encoding::BASE32_NOPAD.decode(encoded.to_ascii_uppercase().as_bytes())?,
            )?;
            ensure!(
                ticket.addr.id == key(&ticket.secret).public(),
                "invalid invitation"
            );
            ensure!(ticket.addr.addrs.len() <= 32, "invalid invitation");
            Ok(ticket)
        } else {
            let encoded = self
                .0
                .strip_prefix(SHORT)
                .context("invalid invitation")?
                .replace('-', "");
            let secret: [u8; 16] = data_encoding::BASE32_NOPAD
                .decode(encoded.to_ascii_uppercase().as_bytes())?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid invitation"))?;
            Ok(Ticket {
                addr: EndpointAddr::new(key(&secret).public()),
                secret,
                expires: 0,
            })
        }
    }
}
impl fmt::Display for PairCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ticket = self.decode().map_err(|_| fmt::Error)?;
        let text = data_encoding::BASE32_NOPAD
            .encode(&ticket.secret)
            .to_ascii_lowercase();
        write!(
            f,
            "{SHORT}{}-{}-{}-{}",
            &text[..7],
            &text[7..14],
            &text[14..20],
            &text[20..]
        )
    }
}
impl FromStr for PairCode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        ensure!(s.len() <= 16 * 1024, "invalid invitation");
        let code = Self(s.trim().to_ascii_lowercase());
        code.decode()
            .map_err(|_| anyhow::anyhow!("invalid invitation"))?;
        Ok(code)
    }
}

pub(crate) struct Invitation {
    pub endpoint: Endpoint,
    pub stop: CancellationToken,
}
#[derive(Clone, Serialize, Deserialize)]
struct Identity {
    addr: EndpointAddr,
    label: String,
    nonce: [u8; 32],
}
#[derive(Serialize, Deserialize)]
struct Challenge {
    host: Identity,
    expires: i64,
    proof: [u8; 32],
}
#[derive(Serialize)]
struct Transcript<'a> {
    joiner: &'a Identity,
    host: &'a Identity,
    expires: i64,
}

fn proof(secret: &[u8; 16], role: &[u8], transcript: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"keel/net/1 pair");
    mac.update(role);
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}
fn verify(secret: &[u8; 16], role: &[u8], transcript: &[u8], tag: &[u8; 32]) -> Result<()> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"keel/net/1 pair");
    mac.update(role);
    mac.update(transcript);
    mac.verify_slice(tag)
        .map_err(|_| anyhow::anyhow!("pairing authentication failed"))
}
impl Node {
    fn identity(&self) -> Result<Identity> {
        Ok(Identity {
            addr: self.endpoint.addr(),
            label: self.label(),
            nonce: random()?,
        })
    }

    pub async fn pair_code(&self) -> Result<PairCode> {
        ensure!(!self.stop.is_cancelled(), "node closed");
        let mut active = self.pairing.lock().await;
        if let Some(old) = active.take() {
            old.stop.cancel();
            old.endpoint.close().await;
        }
        let secret = random()?;
        let endpoint = self.options.builder(key(&secret))?.bind().await?;
        if !matches!(self.options.relay_mode, iroh::RelayMode::Disabled)
            && tokio::time::timeout(self.options.request_timeout, endpoint.online())
                .await
                .is_err()
        {
            endpoint.close().await;
            anyhow::bail!("pairing relay unavailable");
        }
        let expires = now() + LIFETIME.as_secs() as i64;
        let deadline = tokio::time::Instant::now() + LIFETIME;
        let code = PairCode::encode(&Ticket {
            secret,
            addr: endpoint.addr(),
            expires,
        })?;
        let stop = self.stop.child_token();
        *active = Some(Invitation {
            endpoint: endpoint.clone(),
            stop: stop.clone(),
        });
        let weak = self.weak.clone();
        // The mutex serializes successful proofs/commit, not network handshakes.
        let consumed = Arc::new(tokio::sync::Mutex::new(false));
        let timeout = self.options.request_timeout;
        self.tasks.spawn(async move {
            loop {
                let incoming = tokio::select! { biased;
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep_until(deadline) => break,
                    incoming = endpoint.accept() => match incoming { Some(i) => i, None => break }
                };
                let Some(node) = weak.upgrade() else { break };
                let consumed = consumed.clone(); let stop = stop.clone(); let weak = weak.clone();
                node.tasks.spawn(async move {
                    let exchange = async {
                        let conn = incoming.await?;
                        let result = if let Some(node) = weak.upgrade() { node.accept_pair(&conn,secret,expires,deadline,consumed).await } else { anyhow::bail!("node closed") };
                        if result.is_err() { conn.close(1u8.into(),b"pairing failed"); }
                        result
                    };
                    tokio::select! { biased;
                        _ = stop.cancelled() => {},
                        _ = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now()+timeout),exchange) => {},
                    }
                });
            }
            stop.cancel(); endpoint.close().await;
        });
        Ok(code)
    }

    async fn accept_pair(
        &self,
        conn: &Connection,
        secret: [u8; 16],
        expires: i64,
        deadline: tokio::time::Instant,
        consumed: Arc<tokio::sync::Mutex<bool>>,
    ) -> Result<()> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let joiner: Identity = wire::recv(&mut recv).await?;
        ensure!(
            joiner.addr.id == conn.remote_id()
                && joiner.addr.addrs.len() <= 32
                && joiner.label.len() <= 256,
            "invalid peer identity"
        );
        ensure!(
            !*consumed.lock().await && tokio::time::Instant::now() < deadline,
            "invitation unavailable"
        );
        let host = self.identity()?;
        let transcript = wire::encode(&Transcript {
            joiner: &joiner,
            host: &host,
            expires,
        })?;
        wire::send(
            &mut send,
            &Challenge {
                host,
                expires,
                proof: proof(&secret, b"host", &transcript),
            },
        )
        .await?;
        let tag: [u8; 32] = wire::recv(&mut recv).await?;
        verify(&secret, b"joiner", &transcript, &tag)?;
        {
            let mut used = consumed.lock().await;
            ensure!(
                !*used && tokio::time::Instant::now() < deadline,
                "invitation unavailable"
            );
            self.paired(joiner.addr, joiner.label)?;
            *used = true;
        }
        wire::send(&mut send, &proof(&secret, b"committed", &transcript)).await?;
        send.finish()?;
        // Keep the QUIC handle alive until the joiner receives the final proof.
        let _ = send.stopped().await;
        Ok(())
    }

    pub async fn pair_with(&self, code: &PairCode) -> Result<Peer> {
        ensure!(!self.stop.is_cancelled(), "node closed");
        let ticket = code.decode()?;
        tokio::time::timeout(self.options.request_timeout, async {
            let conn = self.endpoint.connect(ticket.addr, ALPN).await?;
            let result = async {
                let (mut send, mut recv) = conn.open_bi().await?;
                let joiner = self.identity()?;
                wire::send(&mut send, &joiner).await?;
                let challenge: Challenge = wire::recv(&mut recv).await?;
                ensure!(
                    (ticket.expires == 0 || ticket.expires == challenge.expires)
                        && challenge.host.addr.addrs.len() <= 32
                        && challenge.host.label.len() <= 256,
                    "invalid invitation"
                );
                let transcript = wire::encode(&Transcript {
                    joiner: &joiner,
                    host: &challenge.host,
                    expires: challenge.expires,
                })?;
                verify(&ticket.secret, b"host", &transcript, &challenge.proof)?;
                wire::send(&mut send, &proof(&ticket.secret, b"joiner", &transcript)).await?;
                send.finish()?;
                let committed: [u8; 32] = wire::recv(&mut recv).await?;
                verify(&ticket.secret, b"committed", &transcript, &committed)?;
                self.paired(challenge.host.addr, challenge.host.label)
            }
            .await;
            conn.close(0u8.into(), b"pairing finished");
            result
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inviter_monotonic_deadline_controls_expiry_despite_clock_skew() {
        for skew in [-3600, 3600] {
            let da = tempfile::tempdir().unwrap();
            let db = tempfile::tempdir().unwrap();
            let a = crate::tests::open(&da, Arc::default()).await;
            let b = crate::tests::open(&db, Arc::default()).await;
            let secret = random().unwrap();
            let ep = a
                .options
                .builder(key(&secret))
                .unwrap()
                .bind()
                .await
                .unwrap();
            let expires = now() + 600 + skew;
            let code = PairCode::encode(&Ticket {
                secret,
                addr: ep.addr(),
                expires,
            })
            .unwrap();
            let parsed: PairCode = code.ticket().parse().unwrap();
            let serving = ep.clone();
            let host = a.clone();
            let server = tokio::spawn(async move {
                let conn = serving.accept().await.unwrap().await.unwrap();
                host.accept_pair(
                    &conn,
                    secret,
                    expires,
                    tokio::time::Instant::now() + LIFETIME,
                    Arc::new(tokio::sync::Mutex::new(false)),
                )
                .await
                .unwrap();
            });
            assert_eq!(b.pair_with(&parsed).await.unwrap().id.0, a.id());
            server.await.unwrap();
            ep.close().await;
            a.close().await;
            b.close().await;
        }
    }

    #[tokio::test]
    async fn host_rejects_past_monotonic_deadline() {
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let a = crate::tests::open(&da, Arc::default()).await;
        let b = crate::tests::open(&db, Arc::default()).await;
        let secret = random().unwrap();
        let ep = a
            .options
            .builder(key(&secret))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let expires = now() + 600;
        let code = PairCode::encode(&Ticket {
            secret,
            addr: ep.addr(),
            expires,
        })
        .unwrap();
        let serving = ep.clone();
        let host = a.clone();
        let server = tokio::spawn(async move {
            let conn = serving.accept().await.unwrap().await.unwrap();
            assert!(host
                .accept_pair(
                    &conn,
                    secret,
                    expires,
                    tokio::time::Instant::now(),
                    Arc::new(tokio::sync::Mutex::new(false))
                )
                .await
                .is_err());
            conn.close(1u8.into(), b"expired");
        });
        assert!(b.pair_with(&code).await.is_err());
        server.await.unwrap();
        ep.close().await;
        assert!(a.peers().is_empty());
        assert!(b.peers().is_empty());
        a.close().await;
        b.close().await;
    }

    #[test]
    fn proofs_reject_wrong_secret_role_and_transcript() {
        let secret = random().unwrap();
        let wrong = random().unwrap();
        let tag = proof(&secret, b"host", b"transcript");
        assert!(verify(&secret, b"host", b"transcript", &tag).is_ok());
        assert!(verify(&wrong, b"host", b"transcript", &tag).is_err());
        assert!(verify(&secret, b"joiner", b"transcript", &tag).is_err());
        assert!(verify(&secret, b"host", b"altered", &tag).is_err());
    }
}
