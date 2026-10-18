//! Network indexes in the search tab (Everything's Network Indexes): with Settings →
//! Search → **Include paired devices** on, a system search also asks every paired device
//! that is online, over keel-net, and its hits come back as `node://<device>/...` paths
//! (the Host column names the device). A device answers only when it gave this one
//! **Search my files** (the shares dialog); the others are skipped quietly.
//!
//! `network-index-host:<name>` keeps a search to the devices of that name (or id): this
//! device's own index is not searched then, and with devices left out nothing is found.
//!
//! The other way round, `host_search` is what this device runs when a paired device
//! searches it: the app's own searcher, local hits only (never another device's).

use anyhow::Context as _;
use keel_net::{Node, PeerId, SearchHit, SearchQuery, SEARCH_MAX};
use keel_search::{Hit, Query, SearchState, Searcher};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The function name, as Everything spells it.
const HOST_TERM: &str = "network-index-host:";

/// The `network-index-host:` values of `text` and the text without them (as typed when
/// there are none). A value may be quoted (`network-index-host:"Living room"`).
pub fn split_hosts(text: &str) -> (Vec<String>, String) {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    let mut hosts = Vec::new();
    let mut rest = Vec::new();
    for t in tokens {
        let head = t.get(..HOST_TERM.len());
        match head.filter(|h| h.eq_ignore_ascii_case(HOST_TERM)) {
            Some(_) => {
                let value = t[HOST_TERM.len()..].trim_matches('"').trim();
                if !value.is_empty() {
                    hosts.push(value.to_owned());
                }
            }
            None => rest.push(t),
        }
    }
    match hosts.is_empty() && !text.to_ascii_lowercase().contains(HOST_TERM) {
        true => (hosts, text.to_owned()),
        false => (hosts, rest.join(" ")),
    }
}

/// Whether `host` (a `network-index-host:` value) names the device: its name, without
/// case, or the start of its id (at least four characters).
fn names(host: &str, id: &PeerId, label: &str) -> bool {
    host.eq_ignore_ascii_case(label.trim())
        || (host.len() >= 4 && id.0.to_string().starts_with(&host.to_ascii_lowercase()))
}

/// What a device is asked for `q`.
pub fn remote_query(q: &Query) -> SearchQuery {
    SearchQuery {
        text: q.text.clone(),
        max: q.max.min(SEARCH_MAX),
        match_case: q.match_case,
        whole_word: q.whole_word,
        match_path: q.match_path,
        match_diacritics: q.match_diacritics,
        regex: q.regex,
        folders_only: q.folders_only,
    }
}

/// A device's hit as a search tab row: its `node://` path, size and date.
pub fn hit_of(host: &PeerId, h: &SearchHit) -> Hit {
    Hit {
        path: h.vpath(host),
        is_dir: h.is_dir,
        size: h.size,
        modified: (h.modified)
            .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s.max(0) as u64))),
        snippet: None,
    }
}

/// The devices a search asks: the node, its runtime, and the paired devices online
/// (id, name).
pub struct Net {
    pub node: Arc<Node>,
    pub rt: tokio::runtime::Handle,
    pub peers: Vec<(PeerId, String)>,
}

/// The system searcher with the paired devices' indexes (`net`; None: this device only,
/// as when Include paired devices is off).
pub struct NetSearcher {
    local: Arc<dyn Searcher>,
    net: Option<Net>,
}

impl NetSearcher {
    pub fn new(local: Arc<dyn Searcher>, net: Option<Net>) -> Self {
        Self { local, net }
    }
}

impl Searcher for NetSearcher {
    fn query(&self, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let (hosts, text) = split_hosts(&q.text);
        let q = Query { text, ..q.clone() };
        let asked: Vec<(PeerId, tokio::task::JoinHandle<anyhow::Result<keel_net::SearchResult>>)> =
            match &self.net {
                // A pane's search keeps to its folder here.
                Some(net) if q.within.is_none() && !q.text.trim().is_empty() => (net.peers.iter())
                    .filter(|(id, label)| hosts.is_empty() || hosts.iter().any(|h| names(h, id, label)))
                    .map(|(id, _)| {
                        let (node, peer, query) = (net.node.clone(), *id, remote_query(&q));
                        (*id, net.rt.spawn(async move { node.search(&peer, query).await }))
                    })
                    .collect(),
                _ => Vec::new(),
            };
        let local = match hosts.is_empty() && !q.text.trim().is_empty() {
            true => Some(self.local.query(&q)),
            false => None,
        };
        let mut found = Vec::new();
        for (peer, task) in asked {
            let rt = &self.net.as_ref().expect("asked devices have a node").rt;
            match rt.block_on(task) {
                Ok(Ok(result)) => found.extend(result.hits.iter().map(|h| hit_of(&peer, h))),
                // Not granted, offline, too slow: that device is left out.
                Ok(Err(e)) => tracing::debug!(peer = %peer.0, "network index: {e:#}"),
                Err(e) => tracing::debug!(peer = %peer.0, "network index: {e}"),
            }
        }
        match local {
            Some(Ok(mut hits)) => {
                hits.append(&mut found);
                Ok(hits)
            }
            Some(Err(e)) if found.is_empty() => Err(e),
            _ => Ok(found),
        }
    }
    fn available(&self) -> bool {
        self.local.available()
    }
    fn probe(&self) -> bool {
        self.local.probe()
    }
    fn status(&self) -> Option<String> {
        self.local.status()
    }
    fn name(&self) -> &'static str {
        self.local.name()
    }
    fn state(&self) -> SearchState {
        self.local.state()
    }
    fn covers(&self, dir: &std::path::Path) -> bool {
        self.local.covers(dir)
    }
}

/// The searcher a paired device's search runs on (set once the app's searcher loaded).
pub type Slot = Arc<RwLock<Option<Arc<dyn Searcher>>>>;

/// This device's answer to a paired device's search (`LibraryHandler::set_search`): the
/// app's system searcher, local hits only, never another device's (no relaying).
pub fn host_search(slot: Slot) -> impl Fn(&SearchQuery) -> anyhow::Result<Vec<SearchHit>> {
    move |q: &SearchQuery| {
        let searcher = slot.read().clone().context("search is still starting")?;
        if !split_hosts(&q.text).0.is_empty() {
            return Ok(Vec::new());
        }
        let query = Query {
            text: q.text.clone(),
            max: q.max.min(SEARCH_MAX),
            match_case: q.match_case,
            whole_word: q.whole_word,
            match_path: q.match_path,
            match_diacritics: q.match_diacritics,
            regex: q.regex,
            folders_only: q.folders_only,
            ..Query::default()
        };
        let secs = |t: SystemTime| {
            t.duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        };
        Ok((searcher.query(&query)?.into_iter())
            .filter(|h| h.path.to_local_path().is_some())
            .map(|h| SearchHit {
                path: h.path.path.clone(),
                is_dir: h.is_dir,
                size: h.size,
                modified: h.modified.and_then(secs),
                open: None,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_vfs::VPath;

    #[test]
    fn host_terms_come_out_of_the_query() {
        let (hosts, rest) = split_hosts("report  *.pdf");
        assert!(hosts.is_empty());
        assert_eq!(rest, "report  *.pdf", "untouched without the term");
        let (hosts, rest) = split_hosts("network-index-host:Desk report \"a b\"");
        assert_eq!((hosts, rest.as_str()), (vec!["Desk".into()], "report \"a b\""));
        let (hosts, rest) =
            split_hosts("Network-Index-Host:\"Living room\" x network-index-host:laptop");
        assert_eq!(hosts, ["Living room", "laptop"]);
        assert_eq!(rest, "x");
        // An empty value names no device, but the term still goes.
        assert_eq!(split_hosts("network-index-host: x"), (vec![], "x".into()));
    }

    fn peer(n: u8) -> PeerId {
        // Any 32 bytes make a NodeId for display and comparison.
        PeerId(keel_net::NodeId([n; 32]))
    }

    #[test]
    fn a_host_is_named_by_label_or_id() {
        let p = peer(1);
        let id = p.0.to_string();
        assert!(names("desk", &p, "Desk"));
        assert!(names(&id[..6], &p, "Desk"));
        assert!(!names(&id[..3], &p, "Desk"), "too short for an id");
        assert!(!names("lap", &p, "Desk"));
    }

    struct Fixed(Vec<Hit>);
    impl Searcher for Fixed {
        fn query(&self, _: &Query) -> anyhow::Result<Vec<Hit>> {
            Ok(self.0.clone())
        }
        fn available(&self) -> bool {
            true
        }
    }

    fn hit(path: VPath) -> Hit {
        Hit {
            path,
            is_dir: false,
            size: 3,
            modified: Some(UNIX_EPOCH + Duration::from_secs(5)),
            snippet: None,
        }
    }

    #[test]
    fn a_host_term_without_devices_finds_nothing_here() {
        let local = hit(VPath::local(std::env::temp_dir().join("a.txt")));
        let s = NetSearcher::new(Arc::new(Fixed(vec![local.clone()])), None);
        let q = |text: &str| Query {
            text: text.into(),
            ..Query::default()
        };
        assert_eq!(s.query(&q("a")).unwrap().len(), 1);
        assert!(s.query(&q("network-index-host:desk a")).unwrap().is_empty());
    }

    #[test]
    fn this_device_answers_with_local_hits_only() {
        let local = VPath::local(std::env::temp_dir().join("a.txt"));
        let remote = VPath::parse("sftp://host/a.txt").unwrap();
        let slot: Slot = Arc::default();
        let search = host_search(slot.clone());
        let q = SearchQuery {
            text: "a".into(),
            max: 10,
            ..SearchQuery::default()
        };
        assert!(search(&q).is_err(), "not loaded yet");
        *slot.write() = Some(Arc::new(Fixed(vec![hit(local.clone()), hit(remote)])));
        let hits = search(&q).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, local.path);
        assert_eq!((hits[0].size, hits[0].modified), (3, Some(5)));
        let relayed = SearchQuery {
            text: "network-index-host:x a".into(),
            ..q
        };
        assert!(search(&relayed).unwrap().is_empty(), "never relayed");
    }

    #[test]
    fn a_device_hit_becomes_a_node_row() {
        let p = peer(2);
        let h = SearchHit {
            path: "C:/data/a.txt".into(),
            is_dir: false,
            size: 9,
            modified: Some(60),
            open: Some(("docs".into(), "a.txt".into())),
        };
        let row = hit_of(&p, &h);
        assert_eq!(row.path.scheme, "node");
        assert_eq!(row.path.authority, p.0.to_string());
        assert_eq!(row.path.path, "/docs/a.txt");
        assert_eq!(row.modified, Some(UNIX_EPOCH + Duration::from_secs(60)));
        let q = Query {
            text: "x".into(),
            max: 50_000,
            regex: true,
            ..Query::default()
        };
        let asked = remote_query(&q);
        assert_eq!((asked.max, asked.regex), (SEARCH_MAX, true));
    }

    /// Two in-process devices: the host lets the asker search it, then shares a folder,
    /// and the asker's search tab searcher gets the host's hits as `node://` rows.
    #[test]
    fn a_paired_device_searches_this_one_through_its_shares() {
        use keel_core::{Library, SourceDef, SourceKind};
        use keel_net::{Access, Grant, LibraryHandler, NodeOptions};
        let (data_a, data_b, files) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let lib = Arc::new(Library::open(data_a.path(), "main").unwrap());
        let source = lib
            .add_source(SourceDef {
                label: "Docs".into(),
                root: VPath::local(files.path()),
                kind: SourceKind::Folder,
                include_hidden: false,
                ignore: Vec::new(),
                poll_secs: None,
                hash_shares: false,
            })
            .unwrap();
        let inside = VPath::local(files.path().join("report.txt"));
        let outside = VPath::local(data_a.path().join("report-elsewhere.txt"));
        let slot: Slot = Arc::default();
        *slot.write() = Some(Arc::new(Fixed(vec![hit(inside), hit(outside)])));
        let handler = Arc::new(LibraryHandler::new(lib.clone()));
        handler.set_search(host_search(slot));
        let other = Arc::new(Library::open(data_b.path(), "main").unwrap());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let secrets = || Arc::new(keel_vfs::cloud::MemoryStore::default());
        let open = |dir: &std::path::Path, h: Arc<LibraryHandler>| {
            rt.block_on(Node::open_with_options(
                secrets(),
                dir,
                h,
                NodeOptions::offline(),
            ))
            .unwrap()
        };
        let host = open(data_a.path(), handler);
        let asker = open(data_b.path(), Arc::new(LibraryHandler::new(other)));
        rt.block_on(async {
            let code = host.pair_code().await.unwrap();
            asker
                .pair_with(&code.ticket().parse().unwrap())
                .await
                .unwrap();
        });
        let (host_id, asker_id) = (PeerId(host.id()), PeerId(asker.id()));
        let s = NetSearcher::new(
            Arc::new(Fixed(Vec::new())),
            Some(Net {
                node: asker.clone(),
                rt: rt.handle().clone(),
                peers: vec![(host_id, "Desk".into())],
            }),
        );
        let q = |text: &str| Query {
            text: text.into(),
            ..Query::default()
        };
        assert!(s.query(&q("report")).unwrap().is_empty(), "not allowed yet");

        host.set_search_grant(&asker_id, true).unwrap();
        let hits = s.query(&q("report")).unwrap();
        assert_eq!(hits.len(), 2);
        let not_shared = format!("/{}/", keel_net::NOT_SHARED);
        assert!(hits.iter().all(|h| h.path.scheme == "node"
            && h.path.authority == host_id.0.to_string()
            && h.path.path.starts_with(&not_shared)));

        host.grant(Grant {
            peer: asker_id,
            source: source.0.clone(),
            subtree: String::new(),
            access: Access::Read,
            created: 1,
        })
        .unwrap();
        let hits = s.query(&q("network-index-host:desk report")).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.path.as_str()).collect();
        assert_eq!(paths[0], format!("/{}/report.txt", source.0));
        assert!(paths[1].starts_with(&not_shared), "outside the share");
        assert!(s.query(&q("network-index-host:laptop report")).unwrap().is_empty());

        host.set_search_grant(&asker_id, false).unwrap();
        assert!(s.query(&q("report")).unwrap().is_empty(), "revoked");
        rt.block_on(async {
            host.close().await;
            asker.close().await;
        });
    }
}
