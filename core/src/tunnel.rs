//! Tunnel capability primitive — see `ffi_spec/Aster-tunneling.md`.
//!
//! An RPC handler that has validated a peer's request to open a tunnel
//! calls [`TunnelRegistry::authorize`] to mint a 32-byte capability
//! token. The peer presents that token on a fresh bidi stream marked
//! with `FLAG_TUNNEL`; the reactor pops the entry from the registry
//! and splices QUIC ↔ backend socket.
//!
//! Properties:
//! - Tickets are CSPRNG (`getrandom`) — unforgeable.
//! - Registries are per-`CoreConnection` — a ticket leaked to another
//!   peer is unredeemable since that peer's connection has a different
//!   registry.
//! - One-shot: redeem pops the entry, replay is impossible.
//! - TTL'd: lazy expiry on `redeem`/`authorize`.
//! - Bounded: per-connection cap (default 64) blunts abuse.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{CoreRecvStream, CoreSendStream};

/// Opaque 32-byte capability token. Returned to the peer in an RPC
/// response; presented back on a fresh bidi stream with `FLAG_TUNNEL`
/// to redeem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TunnelTicket(pub [u8; 32]);

impl TunnelTicket {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// What a tunnel connects to on the server side. Held in core's
/// per-connection registry; never sent over the wire.
#[derive(Clone, Debug)]
pub enum TunnelTarget {
    /// Raw TCP splice. The acceptor opens a TCP connection to `addr`
    /// and pipes bytes in both directions until either side closes.
    Tcp { addr: SocketAddr },
    // Future variants (see Aster-tunneling.md §11):
    //   Udp { addr: SocketAddr },
    //   HttpProxy { addr, host_header, origin_header },
}

/// Per-node defaults for tunnel issuance. Plumbed through `NodeConfig`.
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    pub max_ticket_ttl: Duration,
    pub default_ticket_ttl: Duration,
    pub max_outstanding_per_connection: usize,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            max_ticket_ttl: Duration::from_secs(120),
            default_ticket_ttl: Duration::from_secs(30),
            max_outstanding_per_connection: 64,
        }
    }
}

#[derive(Debug)]
pub enum AuthorizeTunnelError {
    TooManyOutstanding { cap: usize },
    TtlOutOfRange { max_ms: u128 },
    EmptyTargets,
    Rng(String),
}

impl std::fmt::Display for AuthorizeTunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyOutstanding { cap } => write!(
                f,
                "too many outstanding tickets on this connection (cap = {cap})"
            ),
            Self::TtlOutOfRange { max_ms } => {
                write!(f, "ttl out of range (max = {max_ms} ms)")
            }
            Self::EmptyTargets => write!(f, "authorize_tunnel: targets list is empty"),
            Self::Rng(e) => write!(f, "rng failure: {e}"),
        }
    }
}

impl std::error::Error for AuthorizeTunnelError {}

struct RegistryEntry {
    /// Ordered preference list. Redeem tries each in order and splices
    /// the first one that connects.
    targets: Vec<TunnelTarget>,
    expires_at: Instant,
}

/// Per-connection ticket registry. Owned by `CoreConnection`. Drops
/// when the connection drops, mass-revoking any unredeemed tickets.
pub struct TunnelRegistry {
    config: TunnelConfig,
    inner: Mutex<HashMap<TunnelTicket, RegistryEntry>>,
}

impl TunnelRegistry {
    pub fn new(config: TunnelConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Mint a fresh ticket for one or more targets, valid for `ttl`.
    /// `targets` is an ordered preference list — at redeem the
    /// acceptor tries each in order, splicing the first one that
    /// connects. A `ttl` of zero uses `config.default_ticket_ttl`.
    /// The cap counts each ticket as one entry, regardless of how
    /// many targets it carries.
    pub fn authorize(
        &self,
        targets: Vec<TunnelTarget>,
        ttl: Duration,
    ) -> Result<TunnelTicket, AuthorizeTunnelError> {
        if targets.is_empty() {
            return Err(AuthorizeTunnelError::EmptyTargets);
        }

        let effective_ttl = if ttl.is_zero() {
            self.config.default_ticket_ttl
        } else {
            ttl
        };
        if effective_ttl > self.config.max_ticket_ttl {
            return Err(AuthorizeTunnelError::TtlOutOfRange {
                max_ms: self.config.max_ticket_ttl.as_millis(),
            });
        }

        let mut map = self.inner.lock().expect("tunnel registry mutex poisoned");
        let now = Instant::now();
        map.retain(|_, e| e.expires_at > now);

        if map.len() >= self.config.max_outstanding_per_connection {
            return Err(AuthorizeTunnelError::TooManyOutstanding {
                cap: self.config.max_outstanding_per_connection,
            });
        }

        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| AuthorizeTunnelError::Rng(e.to_string()))?;
        let ticket = TunnelTicket(bytes);
        map.insert(
            ticket,
            RegistryEntry {
                targets,
                expires_at: now + effective_ttl,
            },
        );
        Ok(ticket)
    }

    /// Pop and return the target preference list for a ticket, if it
    /// exists and is not expired. Always removes the entry on lookup —
    /// replays against a fresh redeem are not possible.
    pub fn redeem(&self, ticket: &TunnelTicket) -> Option<Vec<TunnelTarget>> {
        let mut map = self.inner.lock().expect("tunnel registry mutex poisoned");
        let entry = map.remove(ticket)?;
        if entry.expires_at <= Instant::now() {
            return None;
        }
        Some(entry.targets)
    }

    /// Test-only inspection helpers.
    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        self.inner
            .lock()
            .expect("tunnel registry mutex poisoned")
            .len()
    }
}

/// Resolve a 32-byte ticket against the per-connection registry and,
/// on success, splice the QUIC bidi stream to the first reachable
/// backend target. Targets are tried in the order they were passed
/// to `authorize`; on TCP `connect()` failure the next target is
/// tried, and so on. If every target fails the stream is dropped
/// silently.
///
/// Called by `CoreConnection::accept_aster_bi` once it has detected
/// the `FLAG_TUNNEL` first frame and consumed its payload.
///
/// Failure modes (unknown ticket, expired, wrong length, all targets
/// unreachable) all close the QUIC stream silently — tunnels aren't
/// RPC, so there is no error frame contract. The peer learns failure
/// by seeing the stream close before any bytes flow.
pub async fn handle_tunnel_redeem(
    send: CoreSendStream,
    recv: CoreRecvStream,
    ticket_bytes: Vec<u8>,
    registry: Arc<TunnelRegistry>,
) -> Result<()> {
    if ticket_bytes.len() != 32 {
        return Err(anyhow!(
            "tunnel handshake: expected 32-byte ticket, got {}",
            ticket_bytes.len()
        ));
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&ticket_bytes);
    let ticket = TunnelTicket(buf);

    let targets = registry
        .redeem(&ticket)
        .ok_or_else(|| anyhow!("tunnel handshake: ticket unknown or expired"))?;

    splice_with_failover(send, recv, targets).await
}

/// Try each target in `targets` in order; splice the first one that
/// connects. If every target's TCP connect fails, return Ok — the
/// stream is dropped silently per the spec's "no error reply" rule.
pub async fn splice_with_failover(
    quic_send: CoreSendStream,
    quic_recv: CoreRecvStream,
    targets: Vec<TunnelTarget>,
) -> Result<()> {
    for target in targets {
        let connected = match target {
            TunnelTarget::Tcp { addr } => match TcpStream::connect(addr).await {
                Ok(tcp) => Some(tcp),
                Err(e) => {
                    tracing::debug!("tunnel splice: connect to {addr} failed: {e}");
                    None
                }
            },
        };
        if let Some(tcp) = connected {
            return splice_tcp_stream(quic_send, quic_recv, tcp).await;
        }
    }
    tracing::debug!("tunnel splice: no reachable targets");
    Ok(())
}

/// Splice a redeemed bidi QUIC stream to an already-connected TCP
/// socket. Returns when either direction EOFs or errors.
async fn splice_tcp_stream(
    quic_send: CoreSendStream,
    quic_recv: CoreRecvStream,
    tcp: TcpStream,
) -> Result<()> {
    let _ = tcp.set_nodelay(true);
    let (tcp_read, tcp_write) = tcp.into_split();

    let q_to_t = tokio::spawn(pump_recv_to_writer(quic_recv, tcp_write));
    let t_to_q = tokio::spawn(pump_reader_to_send(tcp_read, quic_send));

    let _ = tokio::join!(q_to_t, t_to_q);
    Ok(())
}

const TUNNEL_PUMP_BUF: usize = 16 * 1024;

async fn pump_recv_to_writer<W>(recv: CoreRecvStream, mut writer: W) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    loop {
        match recv.read(TUNNEL_PUMP_BUF).await? {
            Some(chunk) if !chunk.is_empty() => {
                if writer.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = writer.shutdown().await;
    Ok(())
}

async fn pump_reader_to_send<R>(mut reader: R, send: CoreSendStream) -> Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; TUNNEL_PUMP_BUF];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if send.write_all(buf[..n].to_vec()).await.is_err() {
            break;
        }
    }
    let _ = send.finish().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TunnelConfig {
        TunnelConfig::default()
    }

    fn target() -> TunnelTarget {
        TunnelTarget::Tcp {
            addr: "127.0.0.1:9".parse().unwrap(),
        }
    }

    fn one(t: TunnelTarget) -> Vec<TunnelTarget> {
        vec![t]
    }

    #[test]
    fn authorize_then_redeem_works() {
        let reg = TunnelRegistry::new(cfg());
        let ticket = reg
            .authorize(one(target()), Duration::from_secs(10))
            .unwrap();
        let targets = reg.redeem(&ticket).unwrap();
        assert_eq!(targets.len(), 1);
        assert!(matches!(targets[0], TunnelTarget::Tcp { .. }));
    }

    #[test]
    fn authorize_preserves_target_order() {
        let reg = TunnelRegistry::new(cfg());
        let a = TunnelTarget::Tcp {
            addr: "127.0.0.1:1".parse().unwrap(),
        };
        let b = TunnelTarget::Tcp {
            addr: "127.0.0.1:2".parse().unwrap(),
        };
        let c = TunnelTarget::Tcp {
            addr: "127.0.0.1:3".parse().unwrap(),
        };
        let ticket = reg
            .authorize(vec![a, b, c], Duration::from_secs(10))
            .unwrap();
        let got = reg.redeem(&ticket).unwrap();
        let ports: Vec<u16> = got
            .iter()
            .map(|t| match t {
                TunnelTarget::Tcp { addr } => addr.port(),
            })
            .collect();
        assert_eq!(ports, vec![1, 2, 3]);
    }

    #[test]
    fn authorize_rejects_empty_targets() {
        let reg = TunnelRegistry::new(cfg());
        let err = reg
            .authorize(Vec::new(), Duration::from_secs(10))
            .unwrap_err();
        assert!(matches!(err, AuthorizeTunnelError::EmptyTargets));
        assert_eq!(reg.outstanding(), 0);
    }

    #[test]
    fn redeem_is_one_shot() {
        let reg = TunnelRegistry::new(cfg());
        let ticket = reg
            .authorize(one(target()), Duration::from_secs(10))
            .unwrap();
        assert!(reg.redeem(&ticket).is_some());
        assert!(reg.redeem(&ticket).is_none());
    }

    #[test]
    fn unknown_ticket_is_none() {
        let reg = TunnelRegistry::new(cfg());
        assert!(reg.redeem(&TunnelTicket([0u8; 32])).is_none());
    }

    #[test]
    fn ttl_zero_uses_default() {
        let reg = TunnelRegistry::new(cfg());
        let ticket = reg.authorize(one(target()), Duration::ZERO).unwrap();
        assert!(reg.redeem(&ticket).is_some());
    }

    #[test]
    fn ttl_above_cap_rejected() {
        let reg = TunnelRegistry::new(cfg());
        let err = reg
            .authorize(one(target()), Duration::from_secs(3600))
            .unwrap_err();
        assert!(matches!(err, AuthorizeTunnelError::TtlOutOfRange { .. }));
    }

    #[test]
    fn capacity_bound_enforced_per_ticket_not_per_target() {
        // Cap is per-ticket; multi-target tickets only consume one slot.
        let mut c = cfg();
        c.max_outstanding_per_connection = 2;
        let reg = TunnelRegistry::new(c);
        let _ = reg
            .authorize(
                vec![target(), target(), target(), target()],
                Duration::from_secs(10),
            )
            .unwrap();
        let _ = reg
            .authorize(one(target()), Duration::from_secs(10))
            .unwrap();
        let err = reg
            .authorize(one(target()), Duration::from_secs(10))
            .unwrap_err();
        assert!(matches!(
            err,
            AuthorizeTunnelError::TooManyOutstanding { .. }
        ));
    }

    #[test]
    fn expired_entries_are_swept_on_authorize() {
        let mut c = cfg();
        c.max_outstanding_per_connection = 2;
        let reg = TunnelRegistry::new(c);
        // Manually insert two already-expired entries.
        {
            let mut m = reg.inner.lock().unwrap();
            m.insert(
                TunnelTicket([1u8; 32]),
                RegistryEntry {
                    targets: vec![target()],
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
            m.insert(
                TunnelTicket([2u8; 32]),
                RegistryEntry {
                    targets: vec![target()],
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
        }
        // Fills cap, but authorize should sweep first and succeed.
        let ok = reg.authorize(one(target()), Duration::from_secs(10));
        assert!(ok.is_ok());
        // Expired entries are gone; only the new one remains.
        assert_eq!(reg.outstanding(), 1);
    }

    #[test]
    fn redeem_treats_expired_as_missing() {
        let reg = TunnelRegistry::new(cfg());
        let ticket = TunnelTicket([7u8; 32]);
        {
            let mut m = reg.inner.lock().unwrap();
            m.insert(
                ticket,
                RegistryEntry {
                    targets: vec![target()],
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
        }
        assert!(reg.redeem(&ticket).is_none());
    }

    #[test]
    fn tickets_are_distinct() {
        let reg = TunnelRegistry::new(cfg());
        let a = reg
            .authorize(one(target()), Duration::from_secs(10))
            .unwrap();
        let b = reg
            .authorize(one(target()), Duration::from_secs(10))
            .unwrap();
        assert_ne!(a, b);
    }
}
