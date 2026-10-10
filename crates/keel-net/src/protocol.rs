use crate::{wire, *};
use anyhow::{bail, ensure, Result};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio_util::sync::CancellationToken;

/// Concurrent requests served per connection (a node accepts at most
/// `MAX_CONNECTIONS_PER_PEER` connections per peer). Further streams wait in
/// QUIC flow control without allocating anything on the host.
pub(crate) const MAX_STREAMS_PER_CONNECTION: usize = 32;

// Requests share the peer's held connection, one stream each. A request that
// fails because that connection closed under it (for example a revoke's remote
// CONNECTION_CLOSE racing the send) is retried once on a fresh connection when
// it is a read; mutations are retried only if the stream never opened.
fn idempotent(req: &Request) -> bool {
    matches!(
        req,
        Request::Ping
            | Request::ListSources
            | Request::List { .. }
            | Request::Stat { .. }
            | Request::Read { .. }
            | Request::Grants
            | Request::DropStatus { .. }
            | Request::SyncPull { .. }
    )
}

async fn upload_response(
    recv: &mut (impl AsyncRead + Unpin),
    upload: impl std::future::Future<Output = Result<()>>,
    timeout: std::time::Duration,
) -> Result<Response> {
    tokio::pin!(upload);
    let response = wire::recv(recv);
    tokio::pin!(response);
    tokio::select! {
        result = &mut response => result,
        result = &mut upload => match result {
            Ok(()) => Ok(tokio::time::timeout(timeout, &mut response).await??),
            // A host that refuses the upload stops reading; its answer (usually
            // Denied) is still on the way and is more useful than the send error.
            Err(error) => match tokio::time::timeout(timeout, &mut response).await {
                Ok(Ok(response)) => Ok(response),
                _ => Err(error),
            },
        }
    }
}

fn permitted(grants: &[Grant], peer: PeerId, req: &Request) -> bool {
    let check = |source: &str, path: &str, write: bool| {
        grants.iter().any(|g| {
            g.peer == peer
                && g.source == source
                && scope::contains(&g.subtree, path)
                && (!write || g.access == Access::ReadWrite)
        })
    };
    let inside = |source: &str, path: &str| {
        grants.iter().any(|g| {
            g.peer == peer
                && g.source == source
                && scope::inside(&g.subtree, path)
                && g.access == Access::ReadWrite
        })
    };
    match req {
        Request::Ping
        | Request::ListSources
        | Request::Grants
        | Request::DropOffer { .. }
        | Request::DropStatus { .. }
        | Request::DropCancel { .. }
        // Answered only for devices this one syncs with (`Node::answer`).
        | Request::SyncPull { .. } => true,
        Request::List { source, path, .. }
        | Request::Stat { source, path }
        | Request::Read { source, path, .. } => check(source, path, false),
        Request::Mkdir { source, path } => check(source, path, true),
        // The granted root itself cannot be replaced, removed or renamed (and so has
        // no resumable partial write either).
        Request::Write { source, path, .. }
        | Request::StatPartial { source, path }
        | Request::Remove { source, path } => inside(source, path),
        Request::Rename { source, from, to } => inside(source, from) && inside(source, to),
    }
}

impl Node {
    pub(crate) async fn serve_connection(&self, conn: Connection, cancel: CancellationToken) {
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS_PER_CONNECTION));
        loop {
            let permit = tokio::select! { biased;
                _ = cancel.cancelled() => break,
                permit = permits.clone().acquire_owned() => match permit { Ok(p) => p, Err(_) => break }
            };
            let (mut send, mut recv) = tokio::select! { biased;
                _ = cancel.cancelled() => break,
                streams = conn.accept_bi() => match streams { Ok(s) => s, Err(_) => break }
            };
            let weak = self.weak.clone();
            let conn = conn.clone();
            let cancel = cancel.clone();
            self.tasks.spawn(async move {
                let _permit = permit;
                let Some(node) = weak.upgrade() else { return };
                let peer = PeerId(NodeId(*conn.remote_id().as_bytes()));
                let result = tokio::select! { biased;
                    _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
                    result = async {
                        let req: Request = tokio::time::timeout(node.options.request_timeout, wire::recv(&mut recv)).await??;
                        let allowed = {
                            let state = node.state.lock();
                            !state.closed && !cancel.is_cancelled() && state.data.peers.iter().any(|r| r.peer.id == peer)
                                && (permitted(&state.data.grants,peer,&req) || node.drop_permits(peer, &req))
                        };
                        tracing::debug!(peer = %peer.0, what = req.name(), allowed, "net request");
                        node.emit_request(peer, req.name());
                        if !allowed { wire::send(&mut send, &Response::Denied("grant required".into())).await?; return Ok(()); }
                        node.answer(peer,req,&mut send,recv).await
                    } => result,
                };
                if result.is_ok() { let _ = send.finish(); } else { let _ = send.reset(1u8.into()); }
            });
        }
    }
    async fn answer(
        &self,
        peer: PeerId,
        req: Request,
        send: &mut SendStream,
        recv: RecvStream,
    ) -> Result<()> {
        let mut body_started = false;
        let ctx = RequestCtx {
            peer,
            label: self
                .state
                .lock()
                .data
                .peers
                .iter()
                .find(|r| r.peer.id == peer)
                .map(|r| r.peer.label.clone())
                .unwrap_or_default(),
        };
        let h = &self.handler;
        let response: Result<Response> = async {
            Ok(match req {
                Request::Ping => Response::Pong {
                    label: self.label(),
                    storage: h.storage(&ctx).await,
                },
                Request::ListSources => {
                    let mut sources = h.sources(&ctx).await;
                    let state = self.state.lock();
                    sources.retain(|s| {
                        state
                            .data
                            .grants
                            .iter()
                            .any(|g| g.peer == peer && g.source == s.id)
                    });
                    Response::Sources(sources)
                }
                Request::Grants => Response::Grants(
                    self.grants()
                        .into_iter()
                        .filter(|g| g.peer == peer)
                        .collect(),
                ),
                Request::List {
                    source,
                    path,
                    after,
                    limit,
                } => {
                    // ponytail: the Handler lists the whole folder for every page; a
                    // Handler-side cursor if huge folders over slow links matter.
                    let mut entries = h.list(&ctx, &source, &path).await?;
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    if let Some(after) = after {
                        entries.retain(|e| e.name > after);
                    }
                    let limit = limit.clamp(1, PAGE_LIMIT) as usize;
                    let more = entries.len() > limit;
                    entries.truncate(limit);
                    Response::Entries { entries, more }
                }
                Request::Stat { source, path } => {
                    Response::Entry(h.stat(&ctx, &source, &path).await?)
                }
                Request::Read {
                    source,
                    path,
                    range,
                } => {
                    let info = h.stat(&ctx, &source, &path).await?;
                    ensure!(!info.is_dir, "cannot read directory");
                    let (offset, length) = range.unwrap_or((0, info.size));
                    ensure!(
                        offset <= info.size && offset.checked_add(length).is_some(),
                        "invalid range"
                    );
                    let size = length.min(info.size - offset);
                    let body = h.read(&ctx, &source, &path, Some((offset, size))).await?;
                    wire::send(send, &Response::Read { size }).await?;
                    body_started = true;
                    let mut body = wire::ExactReader::new(body, size);
                    // An error after this header must reset the stream, never append
                    // an error header to what the reader believes is raw file data.
                    wire::copy_idle(&mut body, send, self.options.request_timeout).await?;
                    return Ok(Response::Read { size });
                }
                Request::DropOffer { id, files } => self.drop_offer(&ctx, &id, files).await?,
                Request::DropStatus { id } => {
                    // A short wait, well inside the sender's request timeout.
                    let wait = (self.options.request_timeout / 4).min(Duration::from_secs(5));
                    self.drop_status(&ctx, &id, wait).await?
                }
                Request::DropCancel { id } => {
                    self.drop_cancel(&ctx, &id).await;
                    Response::Ok
                }
                Request::SyncPull { since } => self.serve_sync(peer, since).await?,
                Request::Write {
                    source,
                    path,
                    offset,
                    size,
                    final_,
                    expect,
                } => {
                    ensure!(offset.checked_add(size).is_some(), "invalid range");
                    let at = WriteAt {
                        offset,
                        size,
                        final_,
                        expect,
                    };
                    let body = Box::new(wire::IdleReader::new(
                        wire::ExactReader::new(recv, size),
                        self.options.request_timeout,
                    ));
                    match source.strip_prefix(crate::spacedrop::SOURCE) {
                        Some(id) => self.drop_piece(&ctx, id, &path, body, at).await?,
                        None => h.write(&ctx, &source, &path, body, at).await?,
                    }
                    Response::Ok
                }
                Request::StatPartial { source, path } => {
                    match source.strip_prefix(crate::spacedrop::SOURCE) {
                        Some(id) => self.drop_partial(peer, id, &path)?,
                        None => Response::Partial {
                            len: h.stat_partial(&ctx, &source, &path).await?,
                            complete: false,
                        },
                    }
                }
                Request::Mkdir { source, path } => {
                    h.mkdir(&ctx, &source, &path).await?;
                    Response::Ok
                }
                Request::Rename { source, from, to } => {
                    h.rename(&ctx, &source, &from, &to).await?;
                    Response::Ok
                }
                Request::Remove { source, path } => {
                    h.remove(&ctx, &source, &path).await?;
                    Response::Ok
                }
            })
        }
        .await;
        match response {
            Ok(Response::Read { .. }) => Ok(()),
            Ok(response) => wire::send(send, &response).await,
            Err(error) if body_started => Err(error),
            Err(_) => wire::send(send, &Response::Error("host operation failed".into())).await,
        }
    }
    /// Opens a stream on the held connection, dialing if needed. A connection
    /// found closed before the stream opened (nothing sent yet) is replaced.
    async fn open_stream(&self, peer: &PeerId) -> Result<(Connection, SendStream, RecvStream)> {
        let conn = self.connect(peer).await?;
        if let Ok((send, recv)) = conn.open_bi().await {
            return Ok((conn, send, recv));
        }
        let conn = self.connect(peer).await?;
        let (send, recv) = conn.open_bi().await?;
        Ok((conn, send, recv))
    }
    /// Sends `req` and runs `exchange`, once more on a fresh connection if that
    /// failed because the connection closed and `req` is a read.
    async fn with_retry<T, F: std::future::Future<Output = Result<T>>>(
        &self,
        peer: &PeerId,
        req: &Request,
        exchange: impl Fn(SendStream, RecvStream) -> F,
    ) -> Result<T> {
        let attempt = || async {
            let (conn, mut send, recv) = self.open_stream(peer).await?;
            let result = async {
                wire::send(&mut send, req).await?;
                exchange(send, recv).await
            }
            .await;
            Ok::<_, anyhow::Error>((conn, result))
        };
        match attempt().await? {
            (conn, Err(_)) if idempotent(req) && conn.close_reason().is_some() => {
                attempt().await?.1
            }
            (_, result) => result,
        }
    }
    /// Sends a header-only request. Use `read_stream` to consume a Read body and
    /// `write_stream` for a nonempty Write body. Denials remain `Response::Denied`.
    pub async fn request(&self, peer: &PeerId, req: Request) -> Result<Response> {
        tokio::time::timeout(self.options.request_timeout, async {
            let response = self
                .with_retry(peer, &req, |mut send, mut recv| async move {
                    send.finish()?;
                    wire::recv::<Response>(&mut recv).await
                })
                .await?;
            if let Response::Pong { label, storage } = &response {
                let renamed = {
                    let mut state = self.state.lock();
                    state
                        .data
                        .peers
                        .iter_mut()
                        .find(|r| &r.peer.id == peer)
                        .is_some_and(|r| {
                            r.peer.storage = storage.clone();
                            scope::valid_label(label) && r.peer.label != *label
                        })
                };
                // Invalid labels are ignored; valid changes are persisted.
                if renamed
                    && self
                        .update(|d| {
                            if let Some(r) = d.peers.iter_mut().find(|r| &r.peer.id == peer) {
                                r.peer.label = label.clone();
                            }
                            Ok(())
                        })
                        .is_err()
                {
                    tracing::warn!("could not save peer label");
                }
            }
            Ok(response)
        })
        .await?
    }
    pub async fn read_stream(
        &self,
        peer: &PeerId,
        source: &str,
        path: &str,
        range: Option<(u64, u64)>,
    ) -> Result<impl AsyncRead + Send + Unpin> {
        let req = Request::Read {
            source: source.into(),
            path: path.into(),
            range,
        };
        let idle = self.options.request_timeout;
        tokio::time::timeout(
            idle,
            self.with_retry(peer, &req, |mut send, mut recv| async move {
                send.finish()?;
                match wire::recv(&mut recv).await? {
                    Response::Read { size } => Ok(wire::IdleReader::new(
                        wire::ExactReader::new(recv, size),
                        idle,
                    )),
                    Response::Denied(_) => bail!("access denied"),
                    _ => bail!("read failed"),
                }
            }),
        )
        .await?
    }
    /// Pushes one piece of a file (see `WriteAt`): streams exactly `at.size` bytes of
    /// `body`, which must then reach EOF. A denied request stops uploading immediately.
    /// A whole file in one go is `WriteAt { offset: 0, size, final_: true, expect }`.
    pub async fn write_stream(
        &self,
        peer: &PeerId,
        source: &str,
        path: &str,
        body: Box<dyn AsyncRead + Send + Unpin>,
        at: WriteAt,
    ) -> Result<Response> {
        let req = Request::Write {
            source: source.into(),
            path: path.into(),
            offset: at.offset,
            size: at.size,
            final_: at.final_,
            expect: at.expect,
        };
        self.body_request(peer, &req, body, at.size).await
    }
    /// Sends `req` followed by exactly `size` bytes of `body`.
    pub(crate) async fn body_request(
        &self,
        peer: &PeerId,
        req: &Request,
        body: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
    ) -> Result<Response> {
        let (_conn, mut send, mut recv) =
            tokio::time::timeout(self.options.request_timeout, async {
                let (conn, mut send, recv) = self.open_stream(peer).await?;
                wire::send(&mut send, req).await?;
                Ok::<_, anyhow::Error>((conn, send, recv))
            })
            .await??;
        let mut body = wire::ExactReader::new(body, size);
        let upload = async {
            wire::copy_idle(&mut body, &mut send, self.options.request_timeout).await?;
            send.finish()?;
            Ok::<(), anyhow::Error>(())
        };
        upload_response(&mut recv, upload, self.options.request_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fragmented_response_survives_upload_completion() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let (mut send, mut recv) = tokio::io::duplex(64);
        let (done, upload_done) = tokio::sync::oneshot::channel();
        let sender = tokio::spawn(async move {
            let bytes = wire::encode(&Response::Denied("scope".into())).unwrap();
            send.write_u32(bytes.len() as u32).await.unwrap();
            // Force the receive future to consume the prefix and suspend on CBOR.
            tokio::time::sleep(Duration::from_millis(20)).await;
            done.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            send.write_all(&bytes).await.unwrap();
        });
        let result = upload_response(
            &mut recv,
            async {
                upload_done.await?;
                Ok(())
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(result, Response::Denied("scope".into()));
        sender.await.unwrap();
    }
    #[test]
    fn mutations_must_stay_strictly_inside_the_subtree() {
        let peer = PeerId(NodeId(
            *iroh::SecretKey::from_bytes(&crate::node::random().unwrap())
                .public()
                .as_bytes(),
        ));
        let grant = |subtree: &str| Grant {
            peer,
            source: "docs".into(),
            subtree: subtree.into(),
            access: Access::ReadWrite,
            created: 0,
        };
        let remove = |path: &str| Request::Remove {
            source: "docs".into(),
            path: path.into(),
        };
        let write = |path: &str| Request::Write {
            source: "docs".into(),
            path: path.into(),
            offset: 0,
            size: 0,
            final_: true,
            expect: None,
        };
        let partial = |path: &str| Request::StatPartial {
            source: "docs".into(),
            path: path.into(),
        };
        let rename = |from: &str, to: &str| Request::Rename {
            source: "docs".into(),
            from: from.into(),
            to: to.into(),
        };
        let shared = [grant("shared")];
        assert!(!permitted(&shared, peer, &remove("shared")));
        assert!(!permitted(&shared, peer, &write("shared")));
        assert!(!permitted(&shared, peer, &partial("shared")));
        assert!(!permitted(&shared, peer, &rename("shared", "shared/x")));
        assert!(!permitted(&shared, peer, &rename("shared/x", "shared")));
        assert!(permitted(&shared, peer, &remove("shared/x")));
        assert!(permitted(&shared, peer, &write("shared/x")));
        assert!(permitted(&shared, peer, &partial("shared/x")));
        assert!(permitted(&shared, peer, &rename("shared/x", "shared/y")));
        let whole = [grant("")];
        assert!(!permitted(&whole, peer, &remove("")));
        assert!(permitted(&whole, peer, &remove("shared")));
    }
    #[test]
    fn rename_needs_both_scopes_and_write_permission() {
        let peer = PeerId(NodeId(
            *iroh::SecretKey::from_bytes(&crate::node::random().unwrap())
                .public()
                .as_bytes(),
        ));
        let mut grants = vec![Grant {
            peer,
            source: "docs".into(),
            subtree: "shared".into(),
            access: Access::Read,
            created: 0,
        }];
        let rename = Request::Rename {
            source: "docs".into(),
            from: "shared/a".into(),
            to: "shared/b".into(),
        };
        assert!(!permitted(&grants, peer, &rename));
        grants[0].access = Access::ReadWrite;
        assert!(permitted(&grants, peer, &rename));
        assert!(!permitted(
            &grants,
            peer,
            &Request::Rename {
                source: "docs".into(),
                from: "shared/a".into(),
                to: "private/b".into()
            }
        ));
    }
}
