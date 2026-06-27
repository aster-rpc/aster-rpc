//! Custom-ALPN connections and streams.
//!
//! For protocols layered directly on a node (admission handshakes, bespoke
//! request/response, streaming) rather than the built-in blobs/docs/gossip.
//! Register the ALPNs at startup with
//! [`Node::start_with_alpns`](crate::Node::start_with_alpns), then
//! [`Node::accept`](crate::Node::accept) inbound or
//! [`Node::connect`](crate::Node::connect) outbound.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::Result;
use crate::id::NodeId;
use aster_transport_core::stream_io::{AsterRead, AsterWrite};
use aster_transport_core::{CoreConnection, CoreRecvStream, CoreSendStream};

/// One network path of a connection and where it leads. Re-exported from
/// the transport core — Aster is a thin layer over iroh, not a wall around
/// it. See [`Connection::paths`].
pub use aster_transport_core::{CorePathInfo as PathInfo, CorePathRemote as PathRemote};

/// A QUIC connection to a peer on a custom ALPN. Cheap to clone.
#[derive(Clone)]
pub struct Connection {
    inner: CoreConnection,
}

impl Connection {
    pub(crate) fn new(inner: CoreConnection) -> Self {
        Self { inner }
    }

    /// The remote peer's id.
    pub fn peer(&self) -> NodeId {
        NodeId::from_hex(self.inner.remote_id())
    }

    /// The negotiated ALPN for this connection.
    pub fn alpn(&self) -> Vec<u8> {
        self.inner.connection_info().alpn
    }

    /// Snapshot of every currently-open network path. A live connection
    /// can hold several at once — typically a relay path plus a direct
    /// path once holepunching succeeds. The returned `Vec` is owned and
    /// safe to move across tasks. Use [`selected_path`](Self::selected_path)
    /// for just the path carrying application data right now.
    pub fn paths(&self) -> Vec<PathInfo> {
        self.inner.paths()
    }

    /// The network path currently carrying application data, if any.
    /// `None` if the connection has been dropped or no path is selected
    /// yet. This is where you read the live remote address / relay URL
    /// and RTT (via [`PathRemote`] and `rtt_micros`).
    pub fn selected_path(&self) -> Option<PathInfo> {
        self.inner.paths().into_iter().find(|p| p.is_selected)
    }

    /// Open a new bidirectional stream (initiator side).
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream)> {
        let (s, r) = self.inner.open_bi().await?;
        Ok((SendStream::new(s), RecvStream::new(r)))
    }

    /// Accept the next inbound bidirectional stream from the peer.
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream)> {
        let (s, r) = self.inner.accept_bi().await?;
        Ok((SendStream::new(s), RecvStream::new(r)))
    }

    /// Open a new unidirectional stream (initiator side). The returned
    /// [`SendStream`] is `AsyncWrite`, so it drops into any byte-stream
    /// consumer (a media lane, `tokio::io::copy`, a framed codec).
    pub async fn open_uni(&self) -> Result<SendStream> {
        Ok(SendStream::new(self.inner.open_uni().await?))
    }

    /// Accept the next inbound unidirectional stream. The returned
    /// [`RecvStream`] is `AsyncRead`.
    pub async fn accept_uni(&self) -> Result<RecvStream> {
        Ok(RecvStream::new(self.inner.accept_uni().await?))
    }

    /// Send one unreliable QUIC datagram. Datagrams are congestion-controlled
    /// but **not** retransmitted or flow-controlled — drop is silent if the
    /// payload exceeds [`max_datagram_size`](Self::max_datagram_size) or the
    /// send buffer is full. Use for latency-sensitive, loss-tolerant traffic
    /// (timing probes, real-time input). Returns an error only on a hard
    /// transport failure (e.g. datagrams unsupported / connection closed).
    pub fn send_datagram(&self, data: impl Into<Vec<u8>>) -> Result<()> {
        Ok(self.inner.send_datagram(data.into())?)
    }

    /// Await the next inbound unreliable datagram.
    pub async fn read_datagram(&self) -> Result<Vec<u8>> {
        Ok(self.inner.read_datagram().await?)
    }

    /// The largest datagram the current path will carry, if datagrams are
    /// supported. **Varies with the path** (relay vs. holepunched, MTU), so
    /// size payloads conservatively and re-query after a path change rather
    /// than caching across one.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.inner.max_datagram_size()
    }

    /// Close the connection with an application code and reason.
    pub fn close(&self, code: u64, reason: impl Into<Vec<u8>>) -> Result<()> {
        Ok(self.inner.close(code, reason.into())?)
    }

    /// Wait until the connection is closed by either side. A responder should
    /// `await` this after `finish()`ing its reply, so the connection isn't
    /// dropped (which would abort delivery) before the peer has read it.
    pub async fn closed(&self) {
        let _ = self.inner.closed().await;
    }
}

/// The send half of a stream (one direction of a bidi stream, or a uni stream).
///
/// Two ways to write: the chunk-oriented async methods ([`write_all`](Self::write_all)
/// / [`finish`](Self::finish)), or the [`AsyncWrite`] impl for byte-stream
/// consumers (`tokio::io::copy`, framed codecs, a media lane). Pick one mode per
/// stream — don't interleave a `write_all` with `AsyncWrite` polls. `Unpin`.
pub struct SendStream {
    io: AsterWrite<CoreSendStream>,
}

impl SendStream {
    fn new(inner: CoreSendStream) -> Self {
        Self {
            io: AsterWrite::new(inner),
        }
    }

    /// Write all of `data` to the stream.
    pub async fn write_all(&self, data: impl Into<Vec<u8>>) -> Result<()> {
        Ok(self.io.get_ref().write_all(data.into()).await?)
    }

    /// Finish the stream (signal end-of-data to the peer).
    pub async fn finish(&self) -> Result<()> {
        Ok(self.io.get_ref().finish().await?)
    }

    /// Set this stream's send priority (higher = scheduled first; default `0`).
    ///
    /// When several send streams share one connection — e.g. a control or audio
    /// lane alongside bulk/video on your own ALPN — the transport sends
    /// higher-priority streams' bytes first. This is the native-stream peer of
    /// the per-call priority the WebTransport transport applies. Errors only if
    /// the stream is already closed.
    pub async fn set_priority(&self, priority: i32) -> Result<()> {
        Ok(self.io.get_ref().set_priority(priority).await?)
    }

    /// This stream's current send priority.
    pub async fn priority(&self) -> Result<i32> {
        Ok(self.io.get_ref().priority().await?)
    }
}

impl AsyncWrite for SendStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// The receive half of a stream (one direction of a bidi stream, or a uni
/// stream).
///
/// Two ways to read: the async methods ([`read`](Self::read) / [`read_exact`](Self::read_exact)
/// / [`read_to_end`](Self::read_to_end)), or the [`AsyncRead`] impl for
/// byte-stream consumers. Pick one mode per stream. `Unpin`.
pub struct RecvStream {
    io: AsterRead<CoreRecvStream>,
}

impl RecvStream {
    fn new(inner: CoreRecvStream) -> Self {
        Self {
            io: AsterRead::new(inner),
        }
    }

    /// Read up to `max_len` bytes; `None` at end of stream.
    pub async fn read(&self, max_len: usize) -> Result<Option<Vec<u8>>> {
        Ok(self.io.get_ref().read(max_len).await?)
    }

    /// Read exactly `n` bytes.
    pub async fn read_exact(&self, n: usize) -> Result<Vec<u8>> {
        Ok(self.io.get_ref().read_exact(n).await?)
    }

    /// Read the whole stream to end, up to `max_size` bytes.
    pub async fn read_to_end(&self, max_size: usize) -> Result<Vec<u8>> {
        Ok(self.io.get_ref().read_to_end(max_size).await?)
    }

    /// Tell the peer to stop sending (abort the stream) with `code`.
    pub fn stop(&self, code: u64) -> Result<()> {
        Ok(self.io.get_ref().stop(code)?)
    }
}

impl AsyncRead for RecvStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}
