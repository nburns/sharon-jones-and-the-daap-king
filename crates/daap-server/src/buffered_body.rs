//! Decouples an ffmpeg (or any) AsyncRead producer from a slow HTTP
//! consumer by draining the producer into a bounded channel that sits
//! between them.
//!
//! Motivation. Hyper applies back-pressure to the response body — if the
//! client reads slowly, the body stream stops being polled and the
//! producer (ffmpeg stdout) blocks on write. When ffmpeg is stalled
//! while feeding on a network source with a request timeout, the
//! timeout fires and the whole pipeline unwinds mid-track. We used to
//! paper over the resulting short body by padding with zeros — which
//! decodes to silence in the client. That masked real failures as "the
//! last minute of the track went quiet."
//!
//! With source bytes pre-buffered into memory (see http.rs), the input
//! side of ffmpeg can't stall on a network timeout any more. What
//! remains is the output side: a fixed-size channel lets ffmpeg run
//! ahead of a slow client by up to `cap_bytes`. If the client is so
//! slow that even that buffer fills, ffmpeg blocks — but this only
//! bounds memory; ffmpeg's local pipe is fine to hold as long as we
//! don't drop it. Any producer error is delivered through the channel
//! as an `io::Error` — never swallowed, never masked with silence.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

/// One message from drainer to reader. `Bytes` on the happy path;
/// `Err` if the producer failed. Producer EOF is the channel close.
type Msg = io::Result<Bytes>;

/// Stream side of the buffered producer. Implements `AsyncRead` for
/// wiring into `tokio_util::io::ReaderStream` and thence
/// `axum::body::Body::from_stream`.
pub struct BufferedBody {
    rx: mpsc::Receiver<Msg>,
    /// Leftover from the previous chunk when the caller's buf was
    /// smaller than what we had.
    partial: Option<Bytes>,
    /// Once we've seen an error we return it, then stay done. This
    /// isn't strictly necessary (drainer drops rx on error) but keeps
    /// the state machine explicit.
    fused: bool,
}

impl BufferedBody {
    /// Spawn a task that drains `reader` into a bounded channel and
    /// return the reader end. `cap_bytes` bounds how far the drainer
    /// runs ahead of the consumer (approximate — measured in chunks
    /// of up to 16 KiB, so peak in-flight is `cap_bytes + one chunk`).
    ///
    /// The spawned task owns `reader`, so ffmpeg-like handles (which
    /// reap the child on drop) live as long as the drainer.
    pub fn spawn<R>(reader: R, cap_bytes: usize) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        // 16 KiB chunks. Channel capacity in messages, not bytes:
        // ceil(cap_bytes / chunk_size), min 4 so tests with tiny caps
        // still make progress.
        const CHUNK: usize = 16 * 1024;
        let chan_cap = (cap_bytes / CHUNK).max(4);
        let (tx, rx) = mpsc::channel::<Msg>(chan_cap);
        tokio::spawn(drain_loop(reader, tx, CHUNK));
        Self {
            rx,
            partial: None,
            fused: false,
        }
    }
}

async fn drain_loop<R>(mut reader: R, tx: mpsc::Sender<Msg>, chunk_size: usize)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buf = vec![0u8; chunk_size];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                // Producer EOF. Drop tx to close the channel; consumer
                // sees `None` and returns EOF too.
                return;
            }
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buf[..n]);
                if tx.send(Ok(chunk)).await.is_err() {
                    // Receiver went away (client disconnected).
                    return;
                }
            }
            Err(err) => {
                tracing::error!(?err, "buffered producer errored - propagating to reader");
                // Ignore send failure: nobody to tell.
                let _ = tx.send(Err(err)).await;
                return;
            }
        }
    }
}

impl AsyncRead for BufferedBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.fused {
            return Poll::Ready(Ok(()));
        }
        // Serve from a partial-consumed chunk first, if any.
        if let Some(chunk) = self.partial.take() {
            let n = chunk.len().min(buf.remaining());
            buf.put_slice(&chunk[..n]);
            if n < chunk.len() {
                self.partial = Some(chunk.slice(n..));
            }
            return Poll::Ready(Ok(()));
        }
        // Pull the next chunk.
        match self.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                self.fused = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(Err(err))) => {
                self.fused = true;
                Poll::Ready(Err(err))
            }
            Poll::Ready(Some(Ok(chunk))) => {
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.partial = Some(chunk.slice(n..));
                }
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn drains_fully_into_body() {
        let src: &[u8] = b"hello world, this is a small payload";
        let mut bb = BufferedBody::spawn(src, 128 * 1024);
        let mut out = Vec::new();
        bb.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, src);
    }

    #[tokio::test]
    async fn slow_reader_still_gets_full_content() {
        let src: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
        let mut bb = BufferedBody::spawn(std::io::Cursor::new(src.clone()), 128 * 1024);
        let mut got = Vec::new();
        let mut one = [0u8; 1];
        loop {
            let n = bb.read(&mut one).await.unwrap();
            if n == 0 {
                break;
            }
            got.push(one[0]);
        }
        assert_eq!(got, src);
    }

    #[tokio::test]
    async fn producer_error_propagates_no_silence_padding() {
        struct FailAfter {
            emitted: usize,
        }
        impl AsyncRead for FailAfter {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                if self.emitted < 100 {
                    let n = buf.remaining().min(100 - self.emitted);
                    let chunk = vec![0xAA; n];
                    buf.put_slice(&chunk);
                    self.emitted += n;
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(io::Error::other("simulated producer failure")))
                }
            }
        }
        let mut bb = BufferedBody::spawn(FailAfter { emitted: 0 }, 128 * 1024);
        let mut out = Vec::new();
        let err = bb.read_to_end(&mut out).await.expect_err("must error");
        assert!(err.to_string().contains("simulated"));
        // Real bytes delivered; tail is NOT zero-padded to hide the
        // failure.
        assert_eq!(out.len(), 100);
        assert!(out.iter().all(|&b| b == 0xAA));
    }

    #[tokio::test]
    async fn cap_bounds_drainer_ahead_of_reader() {
        // A producer that counts how many chunks it has emitted, so we
        // can observe the drainer being held back by the channel.
        struct Counter {
            emitted: Arc<AtomicUsize>,
            limit: usize,
        }
        impl AsyncRead for Counter {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                let done = self.emitted.load(Ordering::Acquire);
                if done >= self.limit {
                    return Poll::Ready(Ok(()));
                }
                let n = buf.remaining().min(self.limit - done);
                let chunk = vec![(done % 251) as u8; n];
                buf.put_slice(&chunk);
                self.emitted.fetch_add(n, Ordering::AcqRel);
                Poll::Ready(Ok(()))
            }
        }
        let emitted = Arc::new(AtomicUsize::new(0));
        let source = Counter {
            emitted: Arc::clone(&emitted),
            limit: 512 * 1024,
        };
        // 4 KiB cap → chan_cap floor'd to 4 messages × 16 KiB = 64 KiB
        // ceiling on the drainer's run-ahead. Producer wants to emit
        // 512 KiB.
        let mut bb = BufferedBody::spawn(source, 4 * 1024);
        // Give the drainer a chance to fill the channel.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        let ahead = emitted.load(Ordering::Acquire);
        assert!(
            ahead <= 5 * 16 * 1024,
            "drainer should be capped near chan_cap; got {ahead}"
        );
        // Now drain everything; must deliver the full 512 KiB.
        let mut got = Vec::new();
        bb.read_to_end(&mut got).await.unwrap();
        assert_eq!(got.len(), 512 * 1024);
    }
}
