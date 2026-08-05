//! Async reader that guarantees exactly `expected` bytes are emitted,
//! regardless of what the wrapped reader actually produces.
//!
//! Used to reconcile a duration-based Content-Length header (what the
//! DAAP client trusts for framing / "truncated body" detection) with the
//! actual PCM byte count coming out of ffmpeg. `duration_ms` on the
//! source track is usually accurate to the second but not the sample —
//! a ClassicAiff response can end up ~10-1000 bytes short or long. With
//! a fixed Content-Length that mismatch causes hyper to close the
//! connection early, which the classic Mac client sees as
//! `MacTCP recv → -23005 (peer closed)`.
//!
//! Behaviour:
//! - Inner reader produces fewer bytes than `expected` → pad tail with
//!   `pad_byte` until the count matches. For signed 8-bit PCM, `0x00`
//!   is silence, so the pad is inaudible.
//! - Inner reader produces more bytes than `expected` → drop the extras
//!   and stop reading (drops the wrapper, which via `kill_on_drop`
//!   reaps ffmpeg).

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

pub struct ExactLengthReader<R: AsyncRead + Unpin> {
    inner: R,
    /// Total bytes we've promised to emit.
    expected: u64,
    /// Bytes emitted so far (from `inner` or from padding).
    emitted: u64,
    /// True once `inner` has hit EOF - stops polling it.
    inner_done: bool,
    /// Byte used to pad the tail if `inner` finishes early.
    pad_byte: u8,
}

impl<R: AsyncRead + Unpin> ExactLengthReader<R> {
    pub fn new(inner: R, expected: u64, pad_byte: u8) -> Self {
        Self {
            inner,
            expected,
            emitted: 0,
            inner_done: false,
            pad_byte,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ExactLengthReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.emitted >= self.expected {
            // Fully satisfied - signal EOF regardless of inner state.
            return Poll::Ready(Ok(()));
        }
        let remaining_budget = self.expected - self.emitted;

        if !self.inner_done {
            // Cap the read so inner can't overflow the budget in one call.
            let cap = remaining_budget.min(buf.remaining() as u64) as usize;
            let start = buf.filled().len();
            let mut sub = buf.take(cap);
            match Pin::new(&mut self.inner).poll_read(cx, &mut sub) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(())) => {
                    let n = sub.filled().len();
                    // `sub` writes into the same underlying buffer via `take`,
                    // but the outer `buf`'s filled-cursor doesn't advance
                    // automatically - do it explicitly.
                    unsafe { buf.assume_init(start + n) };
                    buf.set_filled(start + n);
                    if n == 0 {
                        self.inner_done = true;
                        // Fall through to padding on the next poll rather
                        // than doing both in one call - keeps the state
                        // transitions simple and easy to reason about.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        self.emitted += n as u64;
                        Poll::Ready(Ok(()))
                    }
                }
                Poll::Ready(Err(err)) => {
                    // Treat inner errors as premature EOF so downstream
                    // clients still see a well-framed body (padded with
                    // silence) instead of a torn connection.
                    tracing::warn!(?err, emitted = self.emitted, expected = self.expected, "inner stream errored - padding tail with silence");
                    self.inner_done = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        } else {
            let pad_len = remaining_budget.min(buf.remaining() as u64) as usize;
            let pad = vec![self.pad_byte; pad_len];
            buf.put_slice(&pad);
            self.emitted += pad_len as u64;
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn passthrough_when_inner_matches_expected() {
        let inner: &[u8] = b"exact-ten-";
        let mut r = ExactLengthReader::new(inner, 10, 0);
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(&out, b"exact-ten-");
        assert_eq!(out.len(), 10);
    }

    #[tokio::test]
    async fn pads_when_inner_is_short() {
        let inner: &[u8] = b"abc"; // only 3 bytes
        let mut r = ExactLengthReader::new(inner, 8, 0);
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(&out, b"abc\0\0\0\0\0");
        assert_eq!(out.len(), 8);
    }

    #[tokio::test]
    async fn truncates_when_inner_is_long() {
        let inner: &[u8] = b"this-is-way-too-long-for-the-budget";
        let mut r = ExactLengthReader::new(inner, 7, 0);
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(&out, b"this-is");
        assert_eq!(out.len(), 7);
    }

    #[tokio::test]
    async fn zero_expected_emits_nothing() {
        let inner: &[u8] = b"ignored";
        let mut r = ExactLengthReader::new(inner, 0, 0);
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn pad_byte_is_configurable() {
        let inner: &[u8] = b"";
        let mut r = ExactLengthReader::new(inner, 4, 0xAB);
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(&out, &[0xAB, 0xAB, 0xAB, 0xAB]);
    }

    #[tokio::test]
    async fn works_with_tiny_buffer() {
        // Force many small reads to exercise the buffer-cap logic.
        let inner: &[u8] = b"hi";
        let mut r = ExactLengthReader::new(inner, 5, b'.');
        let mut buf = [0u8; 1];
        let mut got = Vec::new();
        loop {
            let n = r.read(&mut buf).await.unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got, b"hi...");
    }
}
