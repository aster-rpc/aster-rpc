//! Custom-ALPN connections and streams.
//!
//! For protocols layered directly on a node (admission handshakes, bespoke
//! request/response, streaming) rather than the built-in blobs/docs/gossip.
//! Register the ALPNs at startup with
//! [`Node::start_with_alpns`](crate::Node::start_with_alpns), then
//! [`Node::accept`](crate::Node::accept) inbound or
//! [`Node::connect`](crate::Node::connect) outbound.

use crate::error::Result;
use crate::id::NodeId;
use aster_transport_core::{CoreConnection, CoreRecvStream, CoreSendStream};

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

/// The send half of a bidirectional stream.
pub struct SendStream {
    inner: CoreSendStream,
}

impl SendStream {
    fn new(inner: CoreSendStream) -> Self {
        Self { inner }
    }

    /// Write all of `data` to the stream.
    pub async fn write_all(&self, data: impl Into<Vec<u8>>) -> Result<()> {
        Ok(self.inner.write_all(data.into()).await?)
    }

    /// Finish the stream (signal end-of-data to the peer).
    pub async fn finish(&self) -> Result<()> {
        Ok(self.inner.finish().await?)
    }
}

/// The receive half of a bidirectional stream.
pub struct RecvStream {
    inner: CoreRecvStream,
}

impl RecvStream {
    fn new(inner: CoreRecvStream) -> Self {
        Self { inner }
    }

    /// Read up to `max_len` bytes; `None` at end of stream.
    pub async fn read(&self, max_len: usize) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.read(max_len).await?)
    }

    /// Read exactly `n` bytes.
    pub async fn read_exact(&self, n: usize) -> Result<Vec<u8>> {
        Ok(self.inner.read_exact(n).await?)
    }

    /// Read the whole stream to end, up to `max_size` bytes.
    pub async fn read_to_end(&self, max_size: usize) -> Result<Vec<u8>> {
        Ok(self.inner.read_to_end(max_size).await?)
    }

    /// Tell the peer to stop sending (abort the stream) with `code`.
    pub fn stop(&self, code: u64) -> Result<()> {
        Ok(self.inner.stop(code)?)
    }
}
