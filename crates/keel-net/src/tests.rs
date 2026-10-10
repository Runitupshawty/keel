use super::*;
use anyhow::{bail, Result};
use keel_vfs::cloud::MemoryStore;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Files(parking_lot::Mutex<HashMap<String, Vec<u8>>>);
impl Files {
    fn fixture() -> Arc<Self> {
        Arc::new(Self(parking_lot::Mutex::new(HashMap::from([(
            "shared/file".into(),
            b"0123456789".to_vec(),
        )]))))
    }
}
#[async_trait::async_trait]
impl Handler for Files {
    async fn sources(&self, _: &RequestCtx) -> Vec<SourceInfo> {
        ["docs", "private"]
            .into_iter()
            .map(|id| SourceInfo {
                id: id.into(),
                label: id.into(),
                kind: "memory".into(),
            })
            .collect()
    }
    async fn list(&self, ctx: &RequestCtx, source: &str, path: &str) -> Result<Vec<EntryInfo>> {
        assert_eq!((source, path), ("docs", "shared"));
        Ok(vec![self.stat(ctx, source, "shared/file").await?])
    }
    async fn stat(&self, _: &RequestCtx, source: &str, path: &str) -> Result<EntryInfo> {
        assert_eq!(source, "docs");
        let size = if path == "shared/slow" {
            100_000
        } else {
            self.0
                .lock()
                .get(path)
                .ok_or_else(|| anyhow::anyhow!("missing"))?
                .len() as u64
        };
        Ok(EntryInfo {
            name: path.rsplit('/').next().unwrap().into(),
            is_dir: false,
            size,
            modified: Some(1),
            content_id: None,
        })
    }
    async fn read(
        &self,
        _: &RequestCtx,
        source: &str,
        path: &str,
        range: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        assert_eq!(source, "docs");
        if path == "shared/slow" {
            let (mut tx, rx) = tokio::io::duplex(16);
            tokio::spawn(async move {
                let _ = tx.write_all(b"start").await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
            return Ok(Box::new(rx));
        }
        let data = self.0.lock().get(path).unwrap().clone();
        let (offset, len) = range.unwrap_or((0, data.len() as u64));
        Ok(Box::new(std::io::Cursor::new(
            data[offset as usize..(offset + len).min(data.len() as u64) as usize].to_vec(),
        )))
    }
    async fn write(
        &self,
        _: &RequestCtx,
        source: &str,
        path: &str,
        mut body: Box<dyn AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> Result<()> {
        assert_eq!(source, "docs");
        assert_eq!((at.offset, at.final_), (0, true));
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes).await?;
        anyhow::ensure!(bytes.len() as u64 == at.size, "wrong body size");
        self.0.lock().insert(path.into(), bytes);
        Ok(())
    }
    async fn stat_partial(&self, _: &RequestCtx, _: &str, _: &str) -> Result<u64> {
        Ok(0)
    }
    async fn mkdir(&self, _: &RequestCtx, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn rename(&self, _: &RequestCtx, _: &str, from: &str, to: &str) -> Result<()> {
        let mut files = self.0.lock();
        let Some(bytes) = files.remove(from) else {
            bail!("missing")
        };
        files.insert(to.into(), bytes);
        Ok(())
    }
    async fn remove(&self, _: &RequestCtx, _: &str, path: &str) -> Result<()> {
        self.0.lock().remove(path);
        Ok(())
    }
    async fn storage(&self, _: &RequestCtx) -> Option<Storage> {
        Some(Storage {
            used: 10,
            total: 100,
        })
    }
}

fn whole(size: u64) -> WriteAt {
    WriteAt {
        offset: 0,
        size,
        final_: true,
        expect: None,
    }
}

pub(super) async fn open(dir: &tempfile::TempDir, secrets: Arc<MemoryStore>) -> Arc<Node> {
    Node::open_with_options(
        secrets,
        dir.path(),
        Files::fixture(),
        NodeOptions::offline(),
    )
    .await
    .unwrap()
}
async fn pair(a: &Node, b: &Node) -> (PeerId, PeerId) {
    let code = a.pair_code().await.unwrap();
    let ticket = code.ticket().parse().unwrap();
    let peer = b.pair_with(&ticket).await.unwrap();
    assert_eq!(peer.id, PeerId(a.id()));
    (PeerId(a.id()), PeerId(b.id()))
}
fn allow(node: &Node, peer: PeerId, access: Access) {
    node.grant(Grant {
        peer,
        source: "docs".into(),
        subtree: "shared".into(),
        access,
        created: 1,
    })
    .unwrap();
}

#[tokio::test]
async fn paired_loopback_requests_filter_sources_and_stream_ranges() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    a.set_label("First");
    b.set_label("Second");
    let ea = a.events();
    let eb = b.events();
    let (aid, bid) = pair(&a, &b).await;
    assert!(ea
        .try_iter()
        .any(|e| matches!(e, NetEvent::Paired(p) if p.id == bid)));
    assert!(eb
        .try_iter()
        .any(|e| matches!(e, NetEvent::Paired(p) if p.id == aid)));
    assert!(
        matches!(b.request(&aid, Request::Ping).await.unwrap(), Response::Pong { label, .. } if label == "First")
    );
    assert!(
        matches!(a.request(&bid, Request::Ping).await.unwrap(), Response::Pong { label, .. } if label == "Second")
    );
    assert_eq!(
        b.request(&aid, Request::ListSources).await.unwrap(),
        Response::Sources(vec![])
    );
    allow(&a, bid, Access::Read);
    assert!(
        matches!(b.request(&aid, Request::ListSources).await.unwrap(), Response::Sources(v) if v.len() == 1 && v[0].id == "docs")
    );
    assert!(
        matches!(b.request(&aid, Request::List { source: "docs".into(), path: "shared".into(), after: None, limit: 10 }).await.unwrap(), Response::Entries { entries: v, more: false } if v[0].name == "file")
    );
    assert!(
        matches!(b.request(&aid, Request::Stat { source: "docs".into(), path: "shared/file".into() }).await.unwrap(), Response::Entry(e) if e.size == 10)
    );
    let mut stream = b
        .read_stream(&aid, "docs", "shared/file", Some((3, 4)))
        .await
        .unwrap();
    let mut body = Vec::new();
    stream.read_to_end(&mut body).await.unwrap();
    assert_eq!(body, b"3456");
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn grants_revoke_inflight_and_forget_refuses_connections() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, bid) = pair(&a, &b).await;
    let stat = Request::Stat {
        source: "docs".into(),
        path: "shared/file".into(),
    };
    assert!(matches!(
        b.request(&aid, stat.clone()).await.unwrap(),
        Response::Denied(_)
    ));
    allow(&a, bid, Access::Read);
    assert!(matches!(
        b.request(
            &aid,
            Request::Write {
                source: "docs".into(),
                path: "shared/new".into(),
                offset: 0,
                size: 0,
                final_: true,
                expect: None,
            }
        )
        .await
        .unwrap(),
        Response::Denied(_)
    ));
    let mut stream = b
        .read_stream(&aid, "docs", "shared/slow", None)
        .await
        .unwrap();
    let mut first = [0; 5];
    stream.read_exact(&mut first).await.unwrap();
    a.revoke(&bid, "docs", "shared").unwrap();
    let mut rest = Vec::new();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut rest))
            .await
            .unwrap()
            .is_err()
    );
    assert!(matches!(
        b.request(&aid, stat).await.unwrap(),
        Response::Denied(_)
    ));
    a.forget_peer(&bid).unwrap();
    assert!(b.request(&aid, Request::Ping).await.is_err());
    assert!(a.peers().is_empty());
    assert!(a.grants().is_empty());
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn persistent_identity_peers_grants_and_label_reload() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let secrets = Arc::new(MemoryStore::default());
    let a = open(&da, secrets.clone()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, bid) = pair(&a, &b).await;
    a.set_label("Persisted");
    allow(&a, bid, Access::Read);
    a.close().await;
    drop(a);
    let a = open(&da, secrets.clone()).await;
    assert_eq!(a.id(), aid.0);
    assert_eq!(a.label(), "Persisted");
    assert_eq!(a.peers()[0].id, bid);
    assert_eq!(a.grants()[0].peer, bid);
    assert_eq!(secrets.keys(), ["net/node-secret"]);
    assert!(matches!(
        a.request(&bid, Request::Ping).await.unwrap(),
        Response::Pong { .. }
    ));
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn immediate_request_after_idle_session_revocation_is_denied() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, bid) = pair(&a, &b).await;
    allow(&a, bid, Access::ReadWrite);
    let req = Request::Stat {
        source: "docs".into(),
        path: "shared/file".into(),
    };
    assert!(matches!(
        b.request(&aid, req.clone()).await.unwrap(),
        Response::Entry(_)
    ));
    a.revoke(&bid, "docs", "shared").unwrap();
    assert!(matches!(
        b.request(&aid, req).await.unwrap(),
        Response::Denied(_)
    ));
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn streaming_writes_are_exact_and_mutations_obey_subtree() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, bid) = pair(&a, &b).await;
    allow(&a, bid, Access::ReadWrite);
    assert_eq!(
        b.write_stream(
            &aid,
            "docs",
            "shared/new",
            Box::new(std::io::Cursor::new(b"new body".to_vec())),
            whole(8),
        )
        .await
        .unwrap(),
        Response::Ok
    );
    let mut read = b
        .read_stream(&aid, "docs", "shared/new", None)
        .await
        .unwrap();
    let mut body = Vec::new();
    read.read_to_end(&mut body).await.unwrap();
    assert_eq!(body, b"new body");
    assert!(b
        .write_stream(
            &aid,
            "docs",
            "shared/truncated",
            Box::new(std::io::Cursor::new(b"short".to_vec())),
            whole(8),
        )
        .await
        .is_err());
    assert!(matches!(
        b.request(
            &aid,
            Request::Stat {
                source: "docs".into(),
                path: "shared/truncated".into()
            }
        )
        .await
        .unwrap(),
        Response::Error(_)
    ));
    assert!(matches!(
        b.request(
            &aid,
            Request::Rename {
                source: "docs".into(),
                from: "shared/new".into(),
                to: "private/new".into()
            }
        )
        .await
        .unwrap(),
        Response::Denied(_)
    ));
    assert_eq!(
        b.request(
            &aid,
            Request::Rename {
                source: "docs".into(),
                from: "shared/new".into(),
                to: "shared/renamed".into()
            }
        )
        .await
        .unwrap(),
        Response::Ok
    );
    assert_eq!(
        b.request(
            &aid,
            Request::Mkdir {
                source: "docs".into(),
                path: "shared/folder".into()
            }
        )
        .await
        .unwrap(),
        Response::Ok
    );
    assert_eq!(
        b.request(
            &aid,
            Request::Remove {
                source: "docs".into(),
                path: "shared/renamed".into()
            }
        )
        .await
        .unwrap(),
        Response::Ok
    );
    assert!(
        matches!(b.request(&aid,Request::Grants).await.unwrap(),Response::Grants(g) if g.len()==1 && g[0].peer == bid)
    );
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn wrong_replayed_and_expired_pair_codes_fail() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let code = a.pair_code().await.unwrap();
    let mut ticket = code.decode().unwrap();
    ticket.secret[0] ^= 1;
    assert!(b
        .pair_with(&PairCode::encode(&ticket).unwrap())
        .await
        .is_err());
    let peer = b.pair_with(&code).await.unwrap();
    assert_eq!(peer.id, PeerId(a.id()));
    assert!(b.pair_with(&code).await.is_err());
    let code = a.pair_code().await.unwrap();
    let mut ticket = code.decode().unwrap();
    ticket.expires = 1;
    assert!(b
        .pair_with(&PairCode::encode(&ticket).unwrap())
        .await
        .is_err());
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn concurrent_pairing_consumes_invitation_once() {
    let (da, db, dc) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let c = open(&dc, Arc::default()).await;
    let code = a.pair_code().await.unwrap();
    let (first, second) = tokio::join!(b.pair_with(&code), c.pair_with(&code));
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(a.peers().len(), 1);
    assert_eq!(b.peers().len() + c.peers().len(), 1);
    a.close().await;
    b.close().await;
    c.close().await;
}

#[test]
fn node_id_round_trips_base32_and_iroh_hex() {
    let key = iroh::SecretKey::from_bytes(&crate::node::random().unwrap()).public();
    let id = NodeId(*key.as_bytes());
    assert_eq!(id.to_string().len(), 52);
    assert_eq!(id.to_string().parse::<NodeId>().unwrap(), id);
    assert_eq!(key.to_string().parse::<NodeId>().unwrap(), id);
    assert!("invalid".parse::<NodeId>().is_err());
    assert!("invalid".parse::<PairCode>().is_err());
}

#[tokio::test]
async fn graviola_process_default_does_not_break_node_tls() {
    // Other tests never install a default. This must be the actual process default.
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        rustls_graviola::default_provider()
            .install_default()
            .expect("default unset")
    });
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, _) = pair(&a, &b).await;
    assert!(matches!(
        b.request(&aid, Request::Ping).await.unwrap(),
        Response::Pong { .. }
    ));
    a.close().await;
    b.close().await;
}

#[tokio::test]
#[ignore = "requires public relays and discovery; set KEEL_NET_RELAY_TEST=1"]
async fn relay_only_short_code_pairing_and_ping() {
    if std::env::var("KEEL_NET_RELAY_TEST").as_deref() != Ok("1") {
        return;
    }
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let options = NodeOptions {
        relay_only: true,
        ..NodeOptions::default()
    };
    let a = Node::open_with_options(
        Arc::new(MemoryStore::default()),
        da.path(),
        Files::fixture(),
        options.clone(),
    )
    .await
    .unwrap();
    let b = Node::open_with_options(
        Arc::new(MemoryStore::default()),
        db.path(),
        Files::fixture(),
        options,
    )
    .await
    .unwrap();
    let code = a.pair_code().await.unwrap();
    let short: PairCode = code.to_string().parse().unwrap();
    let peer = b.pair_with(&short).await.unwrap();
    let events = b.events();
    assert!(matches!(
        b.request(&peer.id, Request::Ping).await.unwrap(),
        Response::Pong { .. }
    ));
    assert!(events
        .try_iter()
        .any(|e| matches!(e, NetEvent::PeerOnline(_, Link::Relay))));
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn second_open_of_a_data_directory_fails_until_closed() {
    // Only one Node can own a directory, so a second process can never write a
    // stale copy of the grants (e.g. resurrect a revoked grant).
    let dir = tempfile::tempdir().unwrap();
    let a = open(&dir, Arc::default()).await;
    let second = Node::open_with_options(
        Arc::new(MemoryStore::default()),
        dir.path(),
        Files::fixture(),
        NodeOptions::offline(),
    )
    .await;
    assert!(second.err().unwrap().to_string().contains("already in use"));
    a.close().await;
    open(&dir, Arc::default()).await.close().await;
}

#[tokio::test]
async fn requests_share_one_connection_and_link_is_stable() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, bid) = pair(&a, &b).await;
    let events = b.events();
    for _ in 0..3 {
        assert!(matches!(
            b.request(&aid, Request::Ping).await.unwrap(),
            Response::Pong { .. }
        ));
    }
    let seen: Vec<_> = events.try_iter().collect();
    let online = seen
        .iter()
        .filter(|e| matches!(e, NetEvent::PeerOnline(p, _) if *p == aid))
        .count();
    assert_eq!(online, 1, "{seen:?}");
    assert!(!seen.iter().any(|e| matches!(e, NetEvent::PeerOffline(_))));
    assert_eq!(b.peers()[0].link, Link::Lan);
    assert_eq!(a.state.lock().sessions[&bid].len(), 1);
    // A's requests to B reuse B's connection rather than dialing back.
    assert!(matches!(
        a.request(&bid, Request::Ping).await.unwrap(),
        Response::Pong { .. }
    ));
    assert_eq!(a.state.lock().sessions[&bid].len(), 1);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn hostile_labels_are_never_stored() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    a.set_label("Host");
    let (aid, _) = pair(&a, &b).await;
    assert!(a.try_set_label("evil\u{202E}txt").is_err());
    // A peer that bypasses its own checks still cannot plant a label here.
    a.state.lock().data.label = format!("{}\u{7}", "x".repeat(300));
    assert!(matches!(
        b.request(&aid, Request::Ping).await.unwrap(),
        Response::Pong { .. }
    ));
    assert_eq!(b.peers()[0].label, "Host");
    a.state.lock().data.label = "Renamed".into();
    b.request(&aid, Request::Ping).await.unwrap();
    assert_eq!(b.peers()[0].label, "Renamed");
    let stranger = iroh::SecretKey::from_bytes(&crate::node::random().unwrap()).public();
    assert!(b
        .paired(iroh::EndpointAddr::new(stranger), "a\u{2067}b".into())
        .is_err());
    a.close().await;
    b.close().await;
    let b = open(&db, Arc::default()).await;
    assert_eq!(b.peers()[0].label, "Renamed");
    b.close().await;
}

#[tokio::test]
async fn denied_large_upload_reports_denied() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, bid) = pair(&a, &b).await;
    allow(&a, bid, Access::Read);
    for _ in 0..3 {
        let body = vec![7u8; 8 << 20];
        let size = body.len() as u64;
        assert!(matches!(
            b.write_stream(
                &aid,
                "docs",
                "shared/big",
                Box::new(std::io::Cursor::new(body)),
                whole(size)
            )
            .await
            .unwrap(),
            Response::Denied(_)
        ));
    }
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn revoking_nothing_is_an_error() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (_, bid) = pair(&a, &b).await;
    allow(&a, bid, Access::Read);
    assert!(a.revoke(&bid, "docs", "shared/").is_err());
    assert_eq!(a.grants().len(), 1);
    a.revoke(&bid, "docs", "shared").unwrap();
    assert!(a.revoke(&bid, "docs", "shared").is_err());
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn per_peer_connection_and_stream_caps() {
    use crate::{node::MAX_CONNECTIONS_PER_PEER, protocol::MAX_STREAMS_PER_CONNECTION};
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let (aid, bid) = pair(&a, &b).await;
    allow(&a, bid, Access::Read);
    // Streams: every slot busy with a stalled read, so a Ping must wait.
    let mut readers = Vec::new();
    for _ in 0..MAX_STREAMS_PER_CONNECTION {
        readers.push(
            b.read_stream(&aid, "docs", "shared/slow", None)
                .await
                .unwrap(),
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(500), b.request(&aid, Request::Ping))
            .await
            .is_err()
    );
    drop(readers);
    // Slots free up once the host's idle timeout ends the stalled bodies.
    let mut pong = false;
    for _ in 0..4 {
        if let Ok(Response::Pong { .. }) = b.request(&aid, Request::Ping).await {
            pong = true;
            break;
        }
    }
    assert!(pong);
    // Connections: B's held one plus raw extra dials, up to the cap.
    let mut extra = Vec::new();
    for _ in 1..MAX_CONNECTIONS_PER_PEER {
        extra.push(b.endpoint.connect(a.endpoint.addr(), ALPN).await.unwrap());
    }
    for _ in 0..40 {
        if a.state.lock().sessions[&bid].len() == MAX_CONNECTIONS_PER_PEER {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        a.state.lock().sessions[&bid].len(),
        MAX_CONNECTIONS_PER_PEER
    );
    let over = b.endpoint.connect(a.endpoint.addr(), ALPN).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), over.closed())
        .await
        .unwrap();
    assert!(extra.iter().all(|c| c.close_reason().is_none()));
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn corrupt_store_is_moved_aside() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("net")).unwrap();
    std::fs::write(dir.path().join("net/net.sqlite3"), b"not a database at all").unwrap();
    let a = open(&dir, Arc::default()).await;
    assert!(a.peers().is_empty());
    assert_eq!(a.label(), "Keel");
    let aside = std::fs::read_dir(dir.path().join("net"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("net.sqlite3.corrupt-")
        })
        .count();
    assert_eq!(aside, 1);
    a.close().await;
}

#[tokio::test]
async fn invitation_endpoint_closes_once_used() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    pair(&a, &b).await;
    let mut closed = false;
    for _ in 0..40 {
        if a.pairing.lock().await.as_ref().unwrap().stop.is_cancelled() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(closed);
    a.close().await;
    b.close().await;
}

/// Short-code pairing with global discovery and relays off: the code's device is found
/// by mDNS on this machine's network. CI runners may not loop multicast back, so there
/// it runs only with `KEEL_NET_MDNS_TEST=1`.
#[tokio::test]
async fn short_code_pairs_on_the_local_network_without_internet() {
    if std::env::var_os("CI").is_some() && std::env::var("KEEL_NET_MDNS_TEST").as_deref() != Ok("1")
    {
        return;
    }
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let options = NodeOptions {
        local_discovery: true,
        ..NodeOptions::offline()
    };
    assert!(!options.discovery && matches!(options.relay_mode, iroh::RelayMode::Disabled));
    let mut nodes = Vec::new();
    for dir in [&da, &db] {
        let secrets = Arc::new(MemoryStore::default());
        let node = Node::open_with_options(secrets, dir.path(), Files::fixture(), options.clone());
        nodes.push(node.await.unwrap());
    }
    let (a, b) = (nodes[0].clone(), nodes[1].clone());
    let code = a.pair_code().await.unwrap();
    let short: PairCode = code.to_string().parse().unwrap();
    assert!(short.ticket().starts_with("keel1-"));
    let started = std::time::Instant::now();
    let peer = b.pair_with(&short).await.unwrap();
    eprintln!("short-code pairing over mDNS took {:?}", started.elapsed());
    assert_eq!(peer.id, PeerId(a.id()));
    assert!(matches!(
        b.request(&peer.id, Request::Ping).await.unwrap(),
        Response::Pong { .. }
    ));
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn node_works_when_local_discovery_cannot_start() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(&da, Arc::default()).await;
    let b = open(&db, Arc::default()).await;
    let blocked = || -> Result<iroh::address_lookup::MemoryLookup> { bail!("multicast blocked") };
    crate::node::add_local_discovery(&a.endpoint, blocked());
    assert!(crate::node::MDNS_WARNED.load(std::sync::atomic::Ordering::Relaxed));
    crate::node::add_local_discovery(&a.endpoint, blocked());
    let (aid, _) = pair(&a, &b).await;
    assert!(matches!(
        b.request(&aid, Request::Ping).await.unwrap(),
        Response::Pong { .. }
    ));
    a.close().await;
    b.close().await;
}
