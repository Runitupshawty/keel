use crate::{wire, *};
use anyhow::{bail, ensure, Result};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::io::AsyncRead;
use tokio_util::sync::CancellationToken;

// Requests share the peer's held connection, one stream each. A request that
// fails because that connection closed under it (for example a revoke's remote
// CONNECTION_CLOSE racing the send) is retried once on a fresh connection when
// it is a read; mutations are retried only if the stream never opened.
struct BodyReader {
    reader: wire::ExactReader<RecvStream>,
}
fn idempotent(req: &Request) -> bool {
    matches!(
        req,
        Request::Ping
            | Request::ListSources
            | Request::List { .. }
            | Request::Stat { .. }
            | Request::Read { .. }
            | Request::Grants
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
        Request::Ping | Request::ListSources | Request::Grants => true,
        Request::List { source, path }
        | Request::Stat { source, path }
        | Request::Read { source, path, .. } => check(source, path, false),
        Request::Write { source, path, .. }
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
                                && permitted(&state.data.grants,peer,&req)
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
        let response: Result<Response> = async {
            Ok(match req {
                Request::Ping => Response::Pong {
                    label: self.label(),
                    storage: self.handler.storage().await,
                },
                Request::ListSources => {
                    let mut sources = self.handler.sources().await;
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
                Request::List { source, path } => {
                    Response::Entries(self.handler.list(&source, &path).await?)
                }
                Request::Stat { source, path } => {
                    Response::Entry(self.handler.stat(&source, &path).await?)
                }
                Request::Read {
                    source,
                    path,
                    range,
                } => {
                    let info = self.handler.stat(&source, &path).await?;
                    ensure!(!info.is_dir, "cannot read directory");
                    let (offset, length) = range.unwrap_or((0, info.size));
                    ensure!(
                        offset <= info.size && offset.checked_add(length).is_some(),
                        "invalid range"
                    );
                    let size = length.min(info.size - offset);
                    let body = self
                        .handler
                        .read(&source, &path, Some((offset, size)))
                        .await?;
                    wire::send(send, &Response::Read { size }).await?;
                    body_started = true;
                    let mut body = wire::ExactReader::new(body, size);
                    // An error after this header must reset the stream, never append
                    // an error header to what the reader believes is raw file data.
                    tokio::io::copy(&mut body, send).await?;
                    return Ok(Response::Read { size });
                }
                Request::Write { source, path, size } => {
                    self.handler
                        .write(
                            &source,
                            &path,
                            Box::new(wire::ExactReader::new(recv, size)),
                            size,
                        )
                        .await?;
                    Response::Ok
                }
                Request::Mkdir { source, path } => {
                    self.handler.mkdir(&source, &path).await?;
                    Response::Ok
                }
                Request::Rename { source, from, to } => {
                    self.handler.rename(&source, &from, &to).await?;
                    Response::Ok
                }
                Request::Remove { source, path } => {
                    self.handler.remove(&source, &path).await?;
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
        let req = Request::Read {
            source: source.into(),
            path: path.into(),
            range,
        };
        tokio::time::timeout(
            self.options.request_timeout,
            self.with_retry(peer, &req, |mut send, mut recv| async move {
                send.finish()?;
                match wire::recv(&mut recv).await? {
                    Response::Read { size } => Ok(BodyReader {
                        reader: wire::ExactReader::new(recv, size),
                    }),
                    Response::Denied(_) => bail!("access denied"),
                    _ => bail!("read failed"),
                }
            }),
        )
        .await?
    }
    /// Streams exactly `size` bytes. The body must then reach EOF. A denied request
    /// stops uploading immediately. Hosts must stage writes (see `Handler`).
    pub async fn write_stream(
        &self,
        peer: &PeerId,
        source: &str,
        path: &str,
        body: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
    ) -> Result<Response> {
        let req = Request::Write {
            source: source.into(),
            path: path.into(),
            size,
        };
        let (_conn, mut send, mut recv) =
            tokio::time::timeout(self.options.request_timeout, async {
                let (conn, mut send, recv) = self.open_stream(peer).await?;
                wire::send(&mut send, &req).await?;
                Ok::<_, anyhow::Error>((conn, send, recv))
            })
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
