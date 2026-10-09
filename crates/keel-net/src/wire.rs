use anyhow::{ensure, Result};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const MAX_HEADER: usize = 1024 * 1024;

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes)?;
    ensure!(bytes.len() <= MAX_HEADER, "header exceeds limit");
    Ok(bytes)
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(bytes.len() <= MAX_HEADER, "header exceeds limit");
    let mut cursor = std::io::Cursor::new(bytes);
    let value = ciborium::from_reader(&mut cursor)?;
    ensure!(
        cursor.position() == bytes.len() as u64,
        "trailing header data"
    );
    Ok(value)
}

pub(crate) async fn send<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    let bytes = encode(value)?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

pub(crate) async fn recv<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let len = stream.read_u32().await? as usize;
    ensure!(len <= MAX_HEADER, "header exceeds limit");
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    decode(&bytes)
}

/// Reports truncated and overlong bodies as errors, including a FIN before the
/// advertised size. EOF is not reported until the underlying stream's FIN arrives.
pub(crate) struct ExactReader<R> {
    inner: R,
    remaining: u64,
}
impl<R> ExactReader<R> {
    pub(crate) fn new(inner: R, size: u64) -> Self {
        Self {
            inner,
            remaining: size,
        }
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for ExactReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::{io, pin::Pin, task::Poll};
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.remaining == 0 {
            let mut extra = [0u8; 1];
            let mut probe = tokio::io::ReadBuf::new(&mut extra);
            return match Pin::new(&mut self.inner).poll_read(cx, &mut probe) {
                Poll::Ready(Ok(())) if !probe.filled().is_empty() => Poll::Ready(Err(
                    io::Error::new(io::ErrorKind::InvalidData, "body exceeds stated size"),
                )),
                other => other,
            };
        }
        let limit = self.remaining.min(buf.remaining() as u64) as usize;
        let mut partial = tokio::io::ReadBuf::new(&mut buf.initialize_unfilled()[..limit]);
        match Pin::new(&mut self.inner).poll_read(cx, &mut partial) {
            Poll::Ready(Ok(())) => {
                let len = partial.filled().len();
                if len == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated body",
                    )));
                }
                buf.advance(len);
                self.remaining -= len as u64;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rejects_oversize_truncated_and_trailing_headers() {
        let mut oversized = std::io::Cursor::new(((MAX_HEADER + 1) as u32).to_be_bytes());
        assert!(recv::<String>(&mut oversized).await.is_err());
        let mut truncated = std::io::Cursor::new(vec![0, 0, 0, 4, 0]);
        assert!(recv::<String>(&mut truncated).await.is_err());
        assert!(decode::<u8>(&[1, 2]).is_err());
    }
    #[tokio::test]
    async fn exact_body_checks_both_ends() {
        for (bytes, size, good) in [
            (b"abc".to_vec(), 3, true),
            (b"abc".to_vec(), 4, false),
            (b"abc".to_vec(), 2, false),
        ] {
            let mut reader = ExactReader::new(std::io::Cursor::new(bytes), size);
            assert_eq!(reader.read_to_end(&mut Vec::new()).await.is_ok(), good);
        }
    }
}
