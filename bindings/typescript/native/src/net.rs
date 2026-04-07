//! Network module — wraps CoreConnection, streams.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::{CoreConnection, CoreRecvStream, CoreSendStream};

use crate::error::to_napi_err;

// ============================================================================
// IrohConnection
// ============================================================================

#[napi]
pub struct IrohConnection {
    pub(crate) inner: CoreConnection,
}

impl From<CoreConnection> for IrohConnection {
    fn from(inner: CoreConnection) -> Self {
        Self { inner }
    }
}

#[napi]
impl IrohConnection {
    /// Open a bidirectional QUIC stream. Returns [sendStream, recvStream].
    #[napi]
    pub async fn open_bi(&self) -> Result<IrohBiStream> {
        let (send, recv) = self.inner.clone().open_bi().await.map_err(to_napi_err)?;
        Ok(IrohBiStream {
            send: Some(IrohSendStream { inner: send }),
            recv: Some(IrohRecvStream { inner: recv }),
        })
    }

    /// Accept an incoming bidirectional stream.
    #[napi]
    pub async fn accept_bi(&self) -> Result<IrohBiStream> {
        let (send, recv) = self.inner.clone().accept_bi().await.map_err(to_napi_err)?;
        Ok(IrohBiStream {
            send: Some(IrohSendStream { inner: send }),
            recv: Some(IrohRecvStream { inner: recv }),
        })
    }

    /// Get remote node ID as hex string.
    #[napi]
    pub fn remote_node_id(&self) -> String {
        self.inner.remote_id()
    }

    /// Send a datagram (unreliable).
    #[napi]
    pub fn send_datagram(&self, data: Buffer) -> Result<()> {
        self.inner.send_datagram(data.to_vec()).map_err(to_napi_err)
    }

    /// Read the next datagram.
    #[napi]
    pub async fn read_datagram(&self) -> Result<Buffer> {
        let data = self
            .inner
            .clone()
            .read_datagram()
            .await
            .map_err(to_napi_err)?;
        Ok(Buffer::from(data))
    }

    /// Open a unidirectional send stream.
    #[napi]
    pub async fn open_uni(&self) -> Result<IrohSendStream> {
        let send = self.inner.clone().open_uni().await.map_err(to_napi_err)?;
        Ok(IrohSendStream { inner: send })
    }

    /// Accept a unidirectional receive stream.
    #[napi]
    pub async fn accept_uni(&self) -> Result<IrohRecvStream> {
        let recv = self.inner.clone().accept_uni().await.map_err(to_napi_err)?;
        Ok(IrohRecvStream { inner: recv })
    }

    /// Maximum datagram size, or null if not supported.
    #[napi]
    pub fn max_datagram_size(&self) -> Option<u32> {
        self.inner.max_datagram_size().map(|s| s as u32)
    }

    /// Get connection info (debug format).
    #[napi]
    pub fn connection_info(&self) -> String {
        format!("{:?}", self.inner.connection_info())
    }

    /// Close the connection.
    #[napi]
    pub fn close(&self, error_code: u32, reason: String) -> Result<()> {
        self.inner
            .close(error_code as u64, reason.into_bytes())
            .map_err(to_napi_err)
    }
}

// ============================================================================
// BiStream (send + recv pair)
// ============================================================================

#[napi]
pub struct IrohBiStream {
    send: Option<IrohSendStream>,
    recv: Option<IrohRecvStream>,
}

#[napi]
impl IrohBiStream {
    /// Take the send stream (can only be called once).
    #[napi]
    pub fn take_send(&mut self) -> Result<IrohSendStream> {
        self.send
            .take()
            .ok_or_else(|| napi::Error::from_reason("send stream already taken".to_string()))
    }

    /// Take the recv stream (can only be called once).
    #[napi]
    pub fn take_recv(&mut self) -> Result<IrohRecvStream> {
        self.recv
            .take()
            .ok_or_else(|| napi::Error::from_reason("recv stream already taken".to_string()))
    }
}

// ============================================================================
// IrohSendStream
// ============================================================================

#[napi]
pub struct IrohSendStream {
    pub(crate) inner: CoreSendStream,
}

#[napi]
impl IrohSendStream {
    /// Write all bytes to the stream.
    #[napi]
    pub async fn write_all(&self, data: Buffer) -> Result<()> {
        self.inner
            .clone()
            .write_all(data.to_vec())
            .await
            .map_err(to_napi_err)
    }

    /// Signal that no more data will be sent.
    #[napi]
    pub async fn finish(&self) -> Result<()> {
        self.inner.clone().finish().await.map_err(to_napi_err)
    }
}

// ============================================================================
// IrohRecvStream
// ============================================================================

#[napi]
pub struct IrohRecvStream {
    pub(crate) inner: CoreRecvStream,
}

#[napi]
impl IrohRecvStream {
    /// Read exactly n bytes.
    #[napi]
    pub async fn read_exact(&self, n: u32) -> Result<Buffer> {
        let data = self
            .inner
            .clone()
            .read_exact(n as usize)
            .await
            .map_err(to_napi_err)?;
        Ok(Buffer::from(data))
    }

    /// Read all remaining bytes up to a maximum.
    #[napi]
    pub async fn read_to_end(&self, max_len: u32) -> Result<Buffer> {
        let data = self
            .inner
            .clone()
            .read_to_end(max_len as usize)
            .await
            .map_err(to_napi_err)?;
        Ok(Buffer::from(data))
    }
}
