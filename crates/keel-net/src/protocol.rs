use crate::{wire, *};
use anyhow::{bail, ensure, Result};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::io::AsyncRead;
use tokio_util::sync::CancellationToken;

// A request owns its connection through completion (or the body reader's Drop).
// A fresh connection avoids racing a locally cached session against a revoke's
// remote CONNECTION_CLOSE. Mutations are never automatically retried.
struct RequestConnection(Connection);
impl Drop for RequestConnection {
    fn drop(&mut self) {
        self.0.close(0u8.into(), b"request finished");
    }
}
struct BodyReader {
    reader: wire::ExactReader<RecvStream>,
    _connection: RequestConnection,
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
        result = &mut upload => {
            result?;
            Ok(tokio::time::timeout(timeout, &mut response).await??)
        }
    }
}
impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.reader).poll_read(cx, buf)
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
    match req {
        Request::Ping
        | Request::ListSources
        | Request::Grants
        | Request::DropOffer { .. }
        | Request::DropCancel { .. } => true,
        Request::List { source, path, .. }
        | Request::Stat { source, path }
        | Request::Read { source, path, .. } => check(source, path, false),
        Request::Write { source, path, .. }
        | Request::StatPartial { source, path }
        | Request::Mkdir { source, path }
        | Request::Remove { source, path } => check(source, path, true),
        Request::Rename { source, from, to } => {
            check(source, from, true) && check(source, to, true)
        }
    }
}

impl Node {
    pub(crate) async fn serve_connection(&self, conn: Connection, cancel: CancellationToken) {
        loop {
            let (mut send, mut recv) = tokio::select! { biased;
                _ = cancel.cancelled() => break,
                streams = conn.accept_bi() => match streams { Ok(s) => s, Err(_) => break }
            };
            let weak = self.weak.clone();
            let conn = conn.clone();
            let cancel = cancel.clone();
            self.tasks.spawn(async move {
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
                        node.emit(NetEvent::Request { peer, what: req.name().into() });
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
                    tokio::io::copy(&mut body, send).await?;
                    return Ok(Response::Read { size });
                }
                Request::DropOffer { id, files } => self.drop_offer(&ctx, &id, files).await?,
                Request::DropCancel { id } => {
                    self.drop_cancel(peer, &id);
                    Response::Ok
                }
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
                    let body = Box::new(wire::ExactReader::new(recv, size));
                    match source.strip_prefix(crate::spacedrop::SOURCE) {
                        Some(id) => self.drop_piece(peer, id, &path, body, at).await?,
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
    async fn start_request(
        &self,
        peer: &PeerId,
        req: &Request,
    ) -> Result<(RequestConnection, SendStream, RecvStream)> {
        let conn = RequestConnection(self.connect(peer).await?);
        let (mut send, recv) = conn.0.open_bi().await?;
        wire::send(&mut send, req).await?;
        Ok((conn, send, recv))
    }
    /// Sends a header-only request. Use `read_stream` to consume a Read body and
    /// `write_stream` for a nonempty Write body. Denials remain `Response::Denied`.
    pub async fn request(&self, peer: &PeerId, req: Request) -> Result<Response> {
        tokio::time::timeout(self.options.request_timeout, async {
            let (_conn, mut send, mut recv) = self.start_request(peer, &req).await?;
            send.finish()?;
            let response: Response = wire::recv(&mut recv).await?;
            if let Response::Pong { label, storage } = &response {
                let mut state = self.state.lock();
                if let Some(r) = state.data.peers.iter_mut().find(|r| &r.peer.id == peer) {
                    r.peer.label = label.clone();
                    r.peer.storage = storage.clone();
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
        tokio::time::timeout(self.options.request_timeout, async {
            let (conn, mut send, mut recv) = self
                .start_request(
                    peer,
                    &Request::Read {
                        source: source.into(),
                        path: path.into(),
                        range,
                    },
                )
                .await?;
            send.finish()?;
            match wire::recv(&mut recv).await? {
                Response::Read { size } => Ok(BodyReader {
                    reader: wire::ExactReader::new(recv, size),
                    _connection: conn,
                }),
                Response::Denied(_) => bail!("access denied"),
                _ => bail!("read failed"),
            }
        })
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
            tokio::time::timeout(self.options.request_timeout, self.start_request(peer, req))
                .await??;
        let mut body = wire::ExactReader::new(body, size);
        let upload = async {
            tokio::io::copy(&mut body, &mut send).await?;
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
