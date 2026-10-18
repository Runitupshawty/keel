//! Network indexes between two in-process nodes: the **Search my files** grant, its
//! revocation, the bounds, and unpaired devices.
use super::*;
use anyhow::{bail, Result};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::AsyncRead;

/// A host whose system index holds `hits` files (the first one inside the share
/// `docs/shared`, the second outside it) and takes `delay` per search.
struct Index {
    hits: usize,
    delay: Duration,
    searches: AtomicUsize,
}

impl Index {
    fn new(hits: usize, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            hits,
            delay,
            searches: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl Handler for Index {
    async fn sources(&self, _: &RequestCtx) -> Vec<SourceInfo> {
        Vec::new()
    }
    async fn list(&self, _: &RequestCtx, _: &str, _: &str) -> Result<Vec<EntryInfo>> {
        bail!("unused")
    }
    async fn stat(&self, _: &RequestCtx, _: &str, _: &str) -> Result<EntryInfo> {
        bail!("unused")
    }
    async fn read(
        &self,
        _: &RequestCtx,
        _: &str,
        _: &str,
        _: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        bail!("unused")
    }
    async fn write(
        &self,
        _: &RequestCtx,
        _: &str,
        _: &str,
        _: Box<dyn AsyncRead + Send + Unpin>,
        _: WriteAt,
    ) -> Result<()> {
        bail!("unused")
    }
    async fn stat_partial(&self, _: &RequestCtx, _: &str, _: &str) -> Result<u64> {
        bail!("unused")
    }
    async fn mkdir(&self, _: &RequestCtx, _: &str, _: &str) -> Result<()> {
        bail!("unused")
    }
    async fn rename(&self, _: &RequestCtx, _: &str, _: &str, _: &str) -> Result<()> {
        bail!("unused")
    }
    async fn remove(&self, _: &RequestCtx, _: &str, _: &str) -> Result<()> {
        bail!("unused")
    }
    async fn storage(&self, _: &RequestCtx) -> Option<Storage> {
        None
    }
    fn search(&self, _: &RequestCtx, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        self.searches.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Ok((0..self.hits)
            .map(|i| SearchHit {
                path: format!("/data/{}-{i}.txt", query.text),
                is_dir: false,
                size: i as u64,
                modified: Some(1),
                open: match i {
                    0 => Some(("docs".into(), format!("shared/{}-0.txt", query.text))),
                    1 => Some(("docs".into(), "private/x.txt".into())),
                    _ => None,
                },
            })
            .collect())
    }
}

struct Two {
    host: Arc<Node>,
    asker: Arc<Node>,
    /// The asking device, as the host knows it.
    asker_id: PeerId,
    /// The host, as the asking device knows it.
    host_id: PeerId,
    index: Arc<Index>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn two(index: Arc<Index>, options: NodeOptions) -> Two {
    let (dh, da) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let secrets = || Arc::new(keel_vfs::cloud::MemoryStore::default());
    let host = Node::open_with_options(secrets(), dh.path(), index.clone(), options)
        .await
        .unwrap();
    let asker = Node::open_with_options(
        secrets(),
        da.path(),
        Index::new(0, Duration::ZERO),
        NodeOptions::offline(),
    )
    .await
    .unwrap();
    let code = host.pair_code().await.unwrap();
    let ticket = code.ticket().parse().unwrap();
    asker.pair_with(&ticket).await.unwrap();
    Two {
        asker_id: PeerId(asker.id()),
        host_id: PeerId(host.id()),
        host,
        asker,
        index,
        _dirs: (dh, da),
    }
}

impl Two {
    async fn ask(&self, text: &str, max: u32) -> Response {
        let query = SearchQuery {
            text: text.into(),
            max,
            ..SearchQuery::default()
        };
        self.asker
            .request(&self.host_id, Request::Search { query })
            .await
            .unwrap()
    }
    async fn close(self) {
        self.host.close().await;
        self.asker.close().await;
    }
}

fn denied(r: &Response) -> &str {
    match r {
        Response::Denied(why) => why,
        other => panic!("not denied: {other:?}"),
    }
}

#[tokio::test]
async fn search_needs_the_grant_and_revoking_it_takes_effect_at_once() {
    let t = two(Index::new(3, Duration::ZERO), NodeOptions::offline()).await;
    // Off by default: refused, and the host's index is never asked.
    assert_eq!(denied(&t.ask("report", 10).await), "grant required");
    assert!(t
        .asker
        .search(&t.host_id, SearchQuery::default())
        .await
        .is_err());
    assert_eq!(t.index.searches.load(Ordering::SeqCst), 0);
    assert!(t.host.search_grants().is_empty());

    t.host.set_search_grant(&t.asker_id, true).unwrap();
    assert_eq!(t.host.search_grants(), vec![t.asker_id]);
    let found = t
        .asker
        .search(
            &t.host_id,
            SearchQuery {
                text: "report".into(),
                max: 10,
                ..SearchQuery::default()
            },
        )
        .await
        .unwrap();
    assert!(!found.more);
    let paths: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/data/report-0.txt",
            "/data/report-1.txt",
            "/data/report-2.txt"
        ]
    );
    // No share yet: no hit opens.
    assert!(found.hits.iter().all(|h| h.open.is_none()));
    assert_eq!(
        found.hits[2].vpath(&t.host_id).display(),
        format!("node://{}/~/data/report-2.txt", t.host_id.0)
    );

    // A read share of docs/shared: the hit inside it opens there, the other stays shut.
    t.host
        .grant(Grant {
            peer: t.asker_id,
            source: "docs".into(),
            subtree: "shared".into(),
            access: Access::Read,
            created: 1,
        })
        .unwrap();
    let Response::Hits { hits, .. } = t.ask("report", 10).await else {
        panic!("no hits")
    };
    assert_eq!(
        hits[0].open,
        Some(("docs".into(), "shared/report-0.txt".into()))
    );
    assert_eq!(
        hits[0].vpath(&t.host_id).display(),
        format!("node://{}/docs/shared/report-0.txt", t.host_id.0)
    );
    assert_eq!(hits[1].open, None, "outside the share");

    // Revoked: the next search is refused (the share stays).
    t.host.set_search_grant(&t.asker_id, false).unwrap();
    assert_eq!(denied(&t.ask("report", 10).await), "grant required");
    assert!(t.host.search_grants().is_empty());
    assert_eq!(t.host.grants().len(), 1);
    t.close().await;
}

#[tokio::test]
async fn revoking_drops_a_search_under_way() {
    let mut options = NodeOptions::offline();
    options.search_timeout = Duration::from_secs(30);
    let t = two(Index::new(1, Duration::from_millis(800)), options).await;
    t.host.set_search_grant(&t.asker_id, true).unwrap();
    let (asker, host_id) = (t.asker.clone(), t.host_id);
    let running = tokio::spawn(async move {
        let query = SearchQuery {
            text: "slow".into(),
            max: 10,
            ..SearchQuery::default()
        };
        asker.search(&host_id, query).await
    });
    while t.index.searches.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    t.host.set_search_grant(&t.asker_id, false).unwrap();
    assert!(running.await.unwrap().is_err(), "no hits after the revoke");
    t.close().await;
}

#[tokio::test]
async fn queries_and_replies_are_bounded() {
    let mut options = NodeOptions::offline();
    options.search_rate = 4;
    let t = two(Index::new(1_500, Duration::ZERO), options).await;
    t.host.set_search_grant(&t.asker_id, true).unwrap();
    // At most SEARCH_MAX hits, whatever is asked; `more` says there were more.
    let Response::Hits { hits, more } = t.ask("a", 1_000_000).await else {
        panic!("no hits")
    };
    assert_eq!((hits.len(), more), (SEARCH_MAX as usize, true));
    let Response::Hits { hits, more } = t.ask("a", 5).await else {
        panic!("no hits")
    };
    assert_eq!((hits.len(), more), (5, true));
    // Text: too long, empty, control characters.
    let long = "x".repeat(SEARCH_TEXT_MAX + 1);
    assert_eq!(denied(&t.ask(&long, 10).await), "query too long");
    assert_eq!(denied(&t.ask("  ", 10).await), "empty query");
    assert_eq!(denied(&t.ask("a\u{1}b", 10).await), "invalid query");
    // The rate: four searches ran this minute (refused queries do not count).
    assert!(matches!(t.ask("b", 1).await, Response::Hits { .. }));
    assert!(matches!(t.ask("c", 1).await, Response::Hits { .. }));
    assert_eq!(denied(&t.ask("d", 1).await), "too many searches");
    assert_eq!(t.index.searches.load(Ordering::SeqCst), 4);
    t.close().await;
}

#[tokio::test]
async fn a_slow_search_times_out_and_holds_its_slot_until_it_ends() {
    let mut options = NodeOptions::offline();
    options.search_timeout = Duration::from_millis(200);
    let t = two(Index::new(1, Duration::from_millis(1_200)), options).await;
    t.host.set_search_grant(&t.asker_id, true).unwrap();
    assert_eq!(
        t.ask("slow", 10).await,
        Response::Error("search timed out".into())
    );
    // Still running on the host: the next one waits its turn.
    assert_eq!(denied(&t.ask("again", 10).await), "a search is under way");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        t.ask("later", 10).await,
        Response::Error("search timed out".into()),
        "the slot came back"
    );
    t.close().await;
}

#[tokio::test]
async fn unpaired_devices_are_refused_and_forgetting_drops_the_grant() {
    let t = two(Index::new(2, Duration::ZERO), NodeOptions::offline()).await;
    t.host.set_search_grant(&t.asker_id, true).unwrap();
    assert!(matches!(t.ask("x", 10).await, Response::Hits { .. }));
    t.host.forget_peer(&t.asker_id).unwrap();
    assert!(t.host.search_grants().is_empty());
    let query = SearchQuery {
        text: "x".into(),
        max: 10,
        ..SearchQuery::default()
    };
    assert!(t.asker.search(&t.host_id, query).await.is_err());
    assert!(
        t.host.set_search_grant(&t.asker_id, true).is_err(),
        "only paired devices"
    );
    t.close().await;
}

#[tokio::test]
async fn the_grant_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(keel_vfs::cloud::MemoryStore::default());
    let other = tempfile::tempdir().unwrap();
    let a = crate::tests::open(&dir, secrets.clone()).await;
    let b = crate::tests::open(&other, Arc::default()).await;
    let code = a.pair_code().await.unwrap();
    b.pair_with(&code.ticket().parse().unwrap()).await.unwrap();
    let bid = PeerId(b.id());
    a.set_search_grant(&bid, true).unwrap();
    a.close().await;
    let a = crate::tests::open(&dir, secrets).await;
    assert_eq!(a.search_grants(), vec![bid]);
    a.close().await;
    b.close().await;
}

#[test]
fn hits_inside_a_local_source_open_there() {
    let roots = [
        ("docs".to_owned(), "/srv/docs".to_owned()),
        ("root".to_owned(), "/".to_owned()),
    ];
    assert_eq!(
        crate::host::shared_at(&roots, "/srv/docs/a/b.txt"),
        Some(("docs".into(), "a/b.txt".into()))
    );
    // A sibling folder that starts with the same letters is not inside.
    assert_eq!(
        crate::host::shared_at(&roots[..1], "/srv/docsx/b.txt"),
        None
    );
    assert_eq!(crate::host::shared_at(&roots[..1], "/srv/docs"), None);
    assert_eq!(
        crate::host::shared_at(&roots[1..], "/etc/x"),
        Some(("root".into(), "etc/x".into()))
    );
}

#[test]
fn replies_keep_open_only_under_a_read_grant() {
    let peer = PeerId(NodeId(
        *iroh::SecretKey::from_bytes(&crate::node::random().unwrap())
            .public()
            .as_bytes(),
    ));
    let hit = |path: &str, open: Option<(&str, &str)>| SearchHit {
        path: path.into(),
        is_dir: false,
        size: 0,
        modified: None,
        open: open.map(|(s, p)| (s.into(), p.into())),
    };
    let grants = [Grant {
        peer,
        source: "docs".into(),
        subtree: "shared".into(),
        access: Access::Read,
        created: 0,
    }];
    let (hits, more) = crate::search::bound(
        vec![
            hit("/d/shared/a", Some(("docs", "shared/a"))),
            hit("/d/b", Some(("docs", "b"))),
            hit("/d/c", Some(("docs", "../c"))),
            hit("", None),
            hit(&"x".repeat(SEARCH_PATH_MAX + 1), None),
            hit("/d/e", None),
        ],
        10,
        &grants,
        peer,
    );
    let opens: Vec<_> = hits.iter().map(|h| h.open.is_some()).collect();
    assert_eq!(opens, [true, false, false, false]);
    assert!(!more);
}
