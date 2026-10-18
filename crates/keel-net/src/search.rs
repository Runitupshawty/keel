//! Network indexes (Everything's Network Indexes and Everything Server): a paired device
//! searches this device's system index under the **Search my files** grant
//! (`Node::set_search_grant`: off by default, per device, revocable).
//!
//! A host answers `Request::Search` only for a paired device it gave the grant, checked on
//! every request like any share. The query is bounded (at most `SEARCH_TEXT_MAX` bytes of
//! text, `SEARCH_MAX` hits), each device runs one search at a time and at most
//! `NodeOptions::search_rate` a minute, and a search that takes longer than
//! `NodeOptions::search_timeout` is answered with an error (its slot is freed only when
//! the search really ends, so a slow index cannot pile searches up). A reply carries
//! paths, sizes, dates and whether each hit is a folder, never file contents, and stays
//! well under the header limit; a hit opens on the asking device only through a share
//! (`SearchHit::open`, kept only where a read grant covers it). Revoking the grant (or
//! forgetting the device) closes the device's sessions, so a search under way is dropped.
//! Searches are not relayed: a host searches its own index only.

use crate::*;
use anyhow::{bail, Result};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

/// Searches one device may run per minute, by default.
pub const SEARCH_RATE: usize = 120;
/// How long a search may take on the host, by default.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Path bytes in one reply: well under the 1 MiB header limit.
const REPLY_BYTES: usize = 512 * 1024;
const MINUTE: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct SearchState {
    /// Devices with a search running on this host.
    busy: HashSet<PeerId>,
    /// Per device: when its minute started and the searches run in it.
    budget: HashMap<PeerId, (Instant, usize)>,
}

/// A device's search slot: freed when dropped (when the search really ended).
struct Slot {
    state: Arc<Mutex<SearchState>>,
    peer: PeerId,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.state.lock().busy.remove(&self.peer);
    }
}

/// Why a query is refused before it runs.
fn refused(q: &SearchQuery) -> Option<&'static str> {
    if q.text.len() > SEARCH_TEXT_MAX {
        Some("query too long")
    } else if q.text.trim().is_empty() {
        Some("empty query")
    } else if q.text.chars().any(char::is_control) {
        Some("invalid query")
    } else {
        None
    }
}

/// Keeps at most `max` hits and `REPLY_BYTES` of paths, leaves out hits with an invalid
/// or overlong path, and keeps `open` only where a read grant of `peer` covers it. True
/// in the second value when hits were left out for the bounds.
pub(crate) fn bound(
    hits: Vec<SearchHit>,
    max: u32,
    grants: &[Grant],
    peer: PeerId,
) -> (Vec<SearchHit>, bool) {
    let mut more = hits.len() > max as usize;
    let mut bytes = 0;
    let mut out = Vec::new();
    for mut hit in hits.into_iter().take(max as usize) {
        if hit.path.is_empty() || hit.path.len() > SEARCH_PATH_MAX || hit.path.contains('\0') {
            continue;
        }
        hit.open = hit.open.filter(|(source, path)| {
            !source.is_empty()
                && scope::valid_path(path)
                && grants.iter().any(|g| {
                    g.peer == peer && g.source == *source && scope::contains(&g.subtree, path)
                })
        });
        bytes += hit.path.len() + hit.open.as_ref().map_or(0, |(s, p)| s.len() + p.len());
        if bytes > REPLY_BYTES {
            more = true;
            break;
        }
        out.push(hit);
    }
    (out, more)
}

impl SearchHit {
    /// Where the hit opens on the asking device: `node://<host>/<source>/<path>` through a
    /// share, else `node://<host>/~/<host path>`, which shows the hit but cannot be opened
    /// (the host refuses `~`, which no share can be).
    pub fn vpath(&self, host: &PeerId) -> keel_vfs::VPath {
        let path = match &self.open {
            Some((source, path)) => format!("/{source}/{path}"),
            None => format!("/{}/{}", NOT_SHARED, self.path.trim_start_matches('/')),
        };
        keel_vfs::VPath {
            scheme: "node".into(),
            authority: host.0.to_string(),
            path,
        }
    }
}

/// The first component of a remote hit's path outside every share (`SearchHit::vpath`).
pub const NOT_SHARED: &str = "~";

impl Node {
    /// **Search my files**: whether `peer` may search this device's system index (names,
    /// sizes and dates; never contents). Off by default; persisted with the grants.
    /// Turning it off closes the device's sessions, so a search under way is dropped and
    /// its next request is checked again.
    pub fn set_search_grant(&self, peer: &PeerId, on: bool) -> Result<()> {
        let (changed, mut state) = self.update(|d| {
            anyhow::ensure!(d.peers.iter().any(|r| &r.peer.id == peer), "unknown peer");
            let had = d.search.contains(peer);
            d.search.retain(|p| p != peer);
            if on {
                d.search.push(*peer);
            }
            Ok(had != on)
        })?;
        let offline = changed && !on && state.disconnect(peer);
        drop(state);
        if offline {
            self.emit(NetEvent::PeerOffline(*peer));
        }
        if changed {
            self.emit(NetEvent::GrantChanged);
        }
        Ok(())
    }

    /// The devices this one lets search its files.
    pub fn search_grants(&self) -> Vec<PeerId> {
        self.state.lock().data.search.clone()
    }

    /// Takes `peer`'s search slot, or says why not (a search under way, the rate).
    fn search_slot(&self, peer: PeerId) -> Result<Slot, &'static str> {
        let mut s = self.searches.lock();
        if s.busy.contains(&peer) {
            return Err("a search is under way");
        }
        let rate = self.options.search_rate;
        let (start, used) = s.budget.entry(peer).or_insert((Instant::now(), 0));
        if start.elapsed() >= MINUTE {
            *start = Instant::now();
            *used = 0;
        }
        if *used >= rate {
            return Err("too many searches");
        }
        *used += 1;
        s.busy.insert(peer);
        Ok(Slot {
            state: self.searches.clone(),
            peer,
        })
    }

    /// Answers a `Request::Search` from `ctx`'s device (its grant was checked).
    pub(crate) async fn serve_search(
        &self,
        ctx: &RequestCtx,
        mut query: SearchQuery,
    ) -> Result<Response> {
        if let Some(why) = refused(&query) {
            return Ok(Response::Denied(why.into()));
        }
        query.max = query.max.clamp(1, SEARCH_MAX);
        let slot = match self.search_slot(ctx.peer) {
            Ok(slot) => slot,
            Err(why) => return Ok(Response::Denied(why.into())),
        };
        let (handler, c, q) = (self.handler.clone(), ctx.clone(), query.clone());
        let run = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            handler.search(&c, &q)
        });
        let hits = match tokio::time::timeout(self.options.search_timeout, run).await {
            Err(_) => return Ok(Response::Error("search timed out".into())),
            Ok(joined) => joined.map_err(|e| anyhow::anyhow!("search task failed: {e}"))??,
        };
        let grants = self.grants();
        let (hits, more) = bound(hits, query.max, &grants, ctx.peer);
        Ok(Response::Hits { hits, more })
    }

    /// Searches `peer`'s system index (it must have given this device **Search my files**).
    /// Fails when the device refuses (no grant, too many searches, a search under way),
    /// cannot be reached, or does not answer within the request timeout.
    pub async fn search(&self, peer: &PeerId, query: SearchQuery) -> Result<SearchResult> {
        match self.request(peer, Request::Search { query }).await? {
            Response::Hits { mut hits, more } => {
                hits.truncate(SEARCH_MAX as usize);
                Ok(SearchResult { hits, more })
            }
            Response::Denied(why) => bail!("the device refused the search: {why}"),
            Response::Error(e) => bail!("the device could not search: {e}"),
            _ => bail!("unexpected answer to a search"),
        }
    }
}
