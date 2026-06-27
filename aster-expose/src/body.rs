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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

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
