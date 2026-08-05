//! Async reader that emits a fixed byte prefix, then delegates all further
//! reads to a wrapped reader. Used to prepend a hand-rolled AIFF header
//! (that ffmpeg can't emit over an unseekable pipe) to raw PCM bytes
//! ffmpeg produces on its stdout.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

pub struct PrefixReader<R: AsyncRead + Unpin> {
    prefix: Vec<u8>,
    prefix_offset: usize,
    inner: R,
}

impl<R: AsyncRead + Unpin> PrefixReader<R> {
    pub fn new(prefix: Vec<u8>, inner: R) -> Self {
        Self {
            prefix,
            prefix_offset: 0,
            inner,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for PrefixReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Drain the prefix first.
        if self.prefix_offset < self.prefix.len() {
            let remaining = &self.prefix[self.prefix_offset..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.prefix_offset += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn drains_prefix_then_inner() {
        let inner: &[u8] = b"world";
        let mut r = PrefixReader::new(b"hello ".to_vec(), inner);
        let mut out = String::new();
        r.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn works_with_empty_prefix() {
        let inner: &[u8] = b"data";
        let mut r = PrefixReader::new(Vec::new(), inner);
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await.unwrap();
        assert_eq!(&buf, b"data");
    }

    #[tokio::test]
    async fn works_with_empty_inner() {
        let inner: &[u8] = b"";
        let mut r = PrefixReader::new(b"only prefix".to_vec(), inner);
        let mut out = String::new();
        r.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "only prefix");
    }
}
