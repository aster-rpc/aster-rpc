//! Network module — wraps CoreConnection, streams.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::tunnel::{TunnelTarget, TunnelTicket};
use aster_transport_core::{CoreConnection, CoreRecvStream, CoreSendStream, CoreTransportSnapshot};

use crate::error::to_napi_err;

/// JS-side tunnel-target payload. Mirrors the `TunnelTarget` discriminated
/// union exposed by the high-level binding (see `aster.tunnel`). Only
/// `kind = "tcp"` is supported by core in v1.
#[napi(object)]
pub struct TunnelTargetJs {
    pub kind: String,
    pub host: String,
    pub port: u32,
}

/// Selected QUIC path snapshot exposed to JS. `peerAddr` and `relayUrl`
/// are mutually exclusive — exactly one is populated when a path is
/// selected. The peer's IP is not visible through a relay path.
#[napi(object)]
pub struct TransportSnapshot {
    /// Peer's UDP address `{ host, port }` when the selected path is
    /// direct. `null` when relayed.
    pub peer_addr: Option<PeerAddr>,
    /// Relay server URL when the selected path goes through a relay.
    /// `null` for direct paths.
    pub relay_url: Option<String>,
    /// RTT for the selected path in microseconds, or `null` if not yet
    /// measured.
    pub rtt_micros: Option<f64>,
}

#[napi(object)]
pub struct PeerAddr {
    pub host: String,
    pub port: u32,
}

impl From<CoreTransportSnapshot> for TransportSnapshot {
    fn from(snap: CoreTransportSnapshot) -> Self {
        Self {
            peer_addr: snap.peer_addr.map(|s| PeerAddr {
                host: s.ip().to_string(),
                port: s.port() as u32,
            }),
            relay_url: snap.relay_url,
            rtt_micros: snap.rtt_micros.map(|v| v as f64),
        }
    }
}

// ============================================================================
// IrohConnection
// ============================================================================

#[napi]
pub struct IrohConnection {
    pub(crate) inner: CoreConnection,
    pub(crate) alpn_tag: Option<String>,
}

impl From<CoreConnection> for IrohConnection {
    fn from(inner: CoreConnection) -> Self {
        Self {
            inner,
            alpn_tag: None,
        }
    }
}

impl IrohConnection {
    /// Borrow a clone of the underlying `CoreConnection` for sibling
    /// modules (reactor, call) that need direct access to the core pool
    /// surface without going through the `napi`-decorated methods.
    pub(crate) fn core_clone(&self) -> CoreConnection {
        self.inner.clone()
    }
}

#[napi]
impl IrohConnection {
    /// The ALPN this connection was accepted on (set by acceptAster).
    #[napi]
    pub fn alpn(&self) -> Option<String> {
        self.alpn_tag.clone()
    }

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

    /// Snapshot of the currently selected network path. Cheap; safe to
    /// call once per RPC dispatch.
    #[napi]
    pub fn transport_snapshot(&self) -> TransportSnapshot {
        TransportSnapshot::from(self.inner.transport_snapshot())
    }

    /// Close the connection.
    #[napi]
    pub fn close(&self, error_code: u32, reason: String) -> Result<()> {
        self.inner
            .close(error_code as u64, reason.into_bytes())
            .map_err(to_napi_err)
    }

    // ── Per-connection metrics (for routing / HA) ──

    /// Current round-trip time in milliseconds for the selected path.
    #[napi]
    pub fn rtt_ms(&self) -> f64 {
        self.inner.rtt_ms()
    }

    /// Total bytes sent on the selected path (UDP layer).
    #[napi]
    pub fn bytes_sent(&self) -> f64 {
        self.inner.bytes_sent() as f64
    }

    /// Total bytes received on the selected path (UDP layer).
    #[napi]
    pub fn bytes_recv(&self) -> f64 {
        self.inner.bytes_recv() as f64
    }

    /// Current congestion window size in bytes.
    #[napi]
    pub fn congestion_window(&self) -> f64 {
        self.inner.congestion_window() as f64
    }

    /// Number of lost packets on the selected path.
    #[napi]
    pub fn lost_packets(&self) -> f64 {
        self.inner.lost_packets() as f64
    }

    /// Number of congestion events on the selected path.
    #[napi]
    pub fn congestion_events(&self) -> f64 {
        self.inner.congestion_events() as f64
    }

    /// Current path MTU in bytes.
    #[napi]
    pub fn current_mtu(&self) -> u32 {
        self.inner.current_mtu() as u32
    }

    // ── Tunneling — see ffi_spec/Aster-tunneling.md ──

    /// Authorize a tunnel covering one or more targets. `targets` is an
    /// ordered preference list `[{ kind, host, port }, ...]` where
    /// `kind` is `"tcp"` for now (UDP / HttpProxy land in §11). At
    /// redeem the acceptor tries each in order and splices the first
    /// one that connects; if all fail the stream closes silently.
    /// Returns the 32-byte opaque capability for the handler's RPC
    /// response. `ttlSecs == 0` uses the node default (30s).
    #[napi]
    pub fn authorize_tunnel(&self, targets: Vec<TunnelTargetJs>, ttl_secs: u32) -> Result<Buffer> {
        let mut core_targets = Vec::with_capacity(targets.len());
        for t in &targets {
            match t.kind.as_str() {
                "tcp" => {
                    let port = u16::try_from(t.port).map_err(|_| {
                        napi::Error::from_reason(format!("port out of range: {}", t.port))
                    })?;
                    core_targets.push(TunnelTarget::Tcp {
                        addr: parse_socket_addr(&t.host, port)?,
                    });
                }
                other => {
                    return Err(napi::Error::from_reason(format!(
                        "tunnel kind {other:?} not supported in v1 (TCP only)"
                    )))
                }
            }
        }
        let ticket = self
            .inner
            .authorize_tunnel(
                core_targets,
                std::time::Duration::from_secs(ttl_secs as u64),
            )
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        Ok(Buffer::from(ticket.as_bytes().to_vec()))
    }

    /// Open a tunnel by redeeming a 32-byte ticket previously returned
    /// by the peer. Bytes after this call are raw application data.
    #[napi]
    pub async fn open_tunnel(&self, ticket: Buffer) -> Result<IrohBiStream> {
        let bytes = ticket.to_vec();
        if bytes.len() != 32 {
            return Err(napi::Error::from_reason(format!(
                "ticket must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes);
        let ticket = TunnelTicket(buf);
        let (send, recv) = self
            .inner
            .clone()
            .open_tunnel(ticket)
            .await
            .map_err(to_napi_err)?;
        Ok(IrohBiStream {
            send: Some(IrohSendStream { inner: send }),
            recv: Some(IrohRecvStream { inner: recv }),
        })
    }
}

fn parse_socket_addr(host: &str, port: u16) -> Result<std::net::SocketAddr> {
    let addr_str = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    addr_str
        .parse()
        .map_err(|e| napi::Error::from_reason(format!("bad address: {e}")))
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
