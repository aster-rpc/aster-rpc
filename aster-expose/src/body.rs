//! Streaming bodies for the relay hop (design doc §7, cut 1).
//!
//! The inbound body ([`RelayBody`]) streams chunks off the Aster recv side
//! until the peer finishes its send half (clean EOF); the outbound body is
//! pumped frame-by-frame onto the send side by [`crate::relay::serve_request`]
//! / the client. Nothing is buffered, so SSE, chunked transfer, and long-lived
//! HTTP/2 responses flow through.
//!
//! gRPC trailers are **V2** (see design doc §9): `[body…until finish]` cannot
//! carry a trailers frame, so `grpc-status` would be lost. A later body-framing
//! bump adds a trailer frame after the body.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame};
use tokio::io::{AsyncRead, ReadBuf};

/// Boxed error carried by relay bodies.
pub type RelayError = Box<dyn std::error::Error + Send + Sync>;

/// The body handlers return: a `Send` (not `Sync`) boxed [`Body`]. Boxed so the
/// handler can return any body shape — `Full`, a stream, or a relayed
/// [`RelayBody`] — behind one type.
pub type ResponseBody = http_body_util::combinators::UnsyncBoxBody<Bytes, RelayError>;

/// Read chunk size when pulling body bytes off the stream.
const CHUNK: usize = 16 * 1024;

/// An inbound HTTP body that streams chunks off an `AsyncRead` (the Aster recv
/// side) until EOF. Used for both the server-side request body and the
/// client-side response body — the wire is symmetric (`[body…until finish]`).
pub struct RelayBody {
    reader: Option<Pin<Box<dyn AsyncRead + Send>>>,
    chunk: usize,
}

impl RelayBody {
    pub fn new(reader: Pin<Box<dyn AsyncRead + Send>>) -> Self {
        Self {
            reader: Some(reader),
            chunk: CHUNK,
        }
    }
}

impl Body for RelayBody {
    type Data = Bytes;
    type Error = RelayError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, RelayError>>> {
        let this = self.get_mut(); // RelayBody: Unpin
        let chunk = this.chunk;
        let Some(reader) = this.reader.as_mut() else {
            return Poll::Ready(None);
        };
        let mut buf = vec![0u8; chunk];
        let mut rb = ReadBuf::new(&mut buf);
        match reader.as_mut().poll_read(cx, &mut rb) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                this.reader = None;
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Ready(Ok(())) => {
                let n = rb.filled().len();
                if n == 0 {
                    this.reader = None; // clean EOF — peer finished its send side
                    return Poll::Ready(None);
                }
                buf.truncate(n);
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(buf)))))
            }
        }
    }
}

// ── channel body ────────────────────────────────────────────────────────────

/// Shared end-of-body state: an abort error wins over everything queued;
/// trailers follow the last data chunk.
#[derive(Default)]
struct ChannelEnd {
    abort: Option<String>,
    trailers: Option<http::HeaderMap>,
}

/// A body fed chunk by chunk through a [`BodySender`] — the plain, non-generic
/// way to produce a streaming body from code that is not itself a [`Body`]
/// (a language binding writing a response, or a consumer streaming an upload
/// into [`relay_request_streaming`](crate::relay::relay_request_streaming)).
/// Bounded: at most `capacity` chunks wait, so a fast writer is held back
/// (awaits) until the reader catches up.
pub struct ChannelBody {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    end: std::sync::Arc<std::sync::Mutex<ChannelEnd>>,
    done: bool,
}

/// The write half of a [`body_channel`]. Clone-cheap; the body ends cleanly
/// once every clone is dropped or [`finish`](Self::finish)ed.
#[derive(Clone)]
pub struct BodySender {
    tx: tokio::sync::mpsc::Sender<Bytes>,
    end: std::sync::Arc<std::sync::Mutex<ChannelEnd>>,
}

/// The reading side went away (the peer is gone or the body was dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyClosed;

impl std::fmt::Display for BodyClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("body reader is gone")
    }
}

impl std::error::Error for BodyClosed {}

/// A bounded streaming body and its writer. `capacity` (>= 1) chunks may be
/// queued before [`BodySender::send`] waits.
pub fn body_channel(capacity: usize) -> (BodySender, ChannelBody) {
    let (tx, rx) = tokio::sync::mpsc::channel(capacity.max(1));
    let end = std::sync::Arc::new(std::sync::Mutex::new(ChannelEnd::default()));
    (
        BodySender {
            tx,
            end: end.clone(),
        },
        ChannelBody {
            rx,
            end,
            done: false,
        },
    )
}

impl BodySender {
    /// Queue one chunk, waiting while `capacity` chunks are already queued
    /// (backpressure). Empty chunks are skipped.
    pub async fn send(&self, chunk: Bytes) -> Result<(), BodyClosed> {
        if chunk.is_empty() {
            return if self.tx.is_closed() {
                Err(BodyClosed)
            } else {
                Ok(())
            };
        }
        self.tx.send(chunk).await.map_err(|_| BodyClosed)
    }

    /// Queue one chunk if there is room now; `Ok(false)` when full.
    pub fn try_send(&self, chunk: Bytes) -> Result<bool, BodyClosed> {
        use tokio::sync::mpsc::error::TrySendError;
        if chunk.is_empty() {
            return Ok(true);
        }
        match self.tx.try_send(chunk) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Closed(_)) => Err(BodyClosed),
        }
    }

    /// Whether the reading side is gone.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Resolves once a [`try_send`](Self::try_send) could queue a chunk
    /// (`Ok`), or with `Err` when the reading side is gone. Readiness only:
    /// another sender may take the room first.
    pub async fn writable(&self) -> Result<(), BodyClosed> {
        self.tx.reserve().await.map(drop).map_err(|_| BodyClosed)
    }

    /// Resolves when the reading side is gone.
    pub async fn closed(&self) {
        self.tx.closed().await
    }

    /// End the body cleanly (equivalent to dropping this sender).
    pub fn finish(self) {}

    /// End the body with trailers, delivered after the queued chunks. (The
    /// Aster relay hop does not carry trailers yet -- design V2 -- so over
    /// the relay they are dropped; an in-process reader sees them.)
    pub fn finish_with_trailers(self, trailers: http::HeaderMap) {
        self.end.lock().unwrap_or_else(|e| e.into_inner()).trailers = Some(trailers);
    }

    /// End the body with an error, discarding anything still queued: the
    /// reader sees a failed (truncated) body rather than a clean end, so the
    /// relay resets the stream instead of finishing it.
    pub fn abort(self, message: impl Into<String>) {
        self.end.lock().unwrap_or_else(|e| e.into_inner()).abort = Some(message.into());
    }
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = RelayError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, RelayError>>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let take_abort = |end: &std::sync::Mutex<ChannelEnd>| {
            end.lock().unwrap_or_else(|e| e.into_inner()).abort.take()
        };
        if let Some(msg) = take_abort(&this.end) {
            this.done = true;
            return Poll::Ready(Some(Err(msg.into())));
        }
        match this.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
            Poll::Ready(None) => {
                this.done = true;
                // Every sender is gone; an abort set just before the last
                // drop still wins.
                if let Some(msg) = take_abort(&this.end) {
                    return Poll::Ready(Some(Err(msg.into())));
                }
                let trailers = this
                    .end
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .trailers
                    .take();
                Poll::Ready(trailers.map(|t| Ok(Frame::trailers(t))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn channel_body_streams_then_trailers() {
        let (tx, body) = body_channel(2);
        let writer = tokio::spawn(async move {
            for i in 0..5u8 {
                tx.send(Bytes::from(vec![i; 3])).await.unwrap();
            }
            let mut t = http::HeaderMap::new();
            t.insert("grpc-status", "0".parse().unwrap());
            tx.finish_with_trailers(t);
        });
        let collected = body.collect().await.unwrap();
        let trailers = collected.trailers().cloned().unwrap();
        assert_eq!(trailers["grpc-status"], "0");
        assert_eq!(collected.to_bytes().len(), 15);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn channel_body_backpressure_and_abort() {
        let (tx, mut body) = body_channel(1);
        assert!(tx.try_send(Bytes::from_static(b"a")).unwrap());
        // Full: the second chunk waits for the reader.
        assert!(!tx.try_send(Bytes::from_static(b"b")).unwrap());
        tx.abort("handler failed");
        // The abort wins over the queued chunk.
        assert!(body.frame().await.unwrap().is_err());
        assert!(body.frame().await.is_none());
    }

    #[tokio::test]
    async fn channel_sender_sees_a_dropped_reader() {
        let (tx, body) = body_channel(1);
        drop(body);
        assert!(tx.is_closed());
        assert_eq!(tx.send(Bytes::from_static(b"x")).await, Err(BodyClosed));
    }

    #[tokio::test]
    async fn streams_until_eof_across_chunks() {
        // 40 KiB spans three 16 KiB reads; RelayBody must concatenate them all.
        let payload = vec![7u8; 40 * 1024];
        let (mut w, r) = tokio::io::duplex(64 * 1024);
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            w.write_all(&vec![7u8; 40 * 1024]).await.unwrap();
            w.shutdown().await.unwrap(); // EOF
        });

        let body = RelayBody::new(Box::pin(r));
        let collected = body.collect().await.unwrap().to_bytes();
        assert_eq!(collected.len(), payload.len());
        assert_eq!(&collected[..], &payload[..]);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn empty_stream_yields_no_frames() {
        let (w, r) = tokio::io::duplex(1024);
        drop(w); // immediate EOF
        let body = RelayBody::new(Box::pin(r));
        let collected = body.collect().await.unwrap().to_bytes();
        assert!(collected.is_empty());
    }
}
