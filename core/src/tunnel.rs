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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;

use crate::stream_io::BoxFut;
use crate::{CoreConnection, CoreRecvStream, CoreSendStream};

// ============================================================================
// FLAG_TUNNEL payload subtype (design doc §5.2)
// ============================================================================
//
// `FLAG_TUNNEL` is the last free flag bit, so the first payload byte
// discriminates what kind of tunnel stream this is. Prefixing the L4 ticket
// with subtype 0 is a flag-day wire change to the shipped path — both ends
// rebuild together; there is no negotiation.

/// L4 raw splice: `[subtype 0][32-byte ticket]`.
pub const TUNNEL_SUBTYPE_TICKET: u8 = 0;
/// L7 HTTP-object relay: `[subtype 1][u16 len][service_id utf8]`. The H3-object
/// head + body follow as subsequent frames on the same stream (read by the
/// registered [`LocalTunnelAcceptor`]).
pub const TUNNEL_SUBTYPE_HTTP_RELAY: u8 = 1;

/// Default per-(connection, service) concurrent request-stream cap (design
/// doc §5.2 / §6.1). Replaces the one-shot model's `max_outstanding`; raised
/// for edge-shaped peers, low for ordinary ones. The connection-wide QUIC
/// `max_concurrent_bidi_streams` is a separate backstop.
pub const DEFAULT_MAX_STREAMS_PER_SERVICE: usize = 256;

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

// ============================================================================
// L7 HTTP-object relay — service-keyed acceptor + session admission (§5.2)
// ============================================================================

/// A local target the node exposes over Aster. The companion crate
/// (`aster-expose`) implements this: `authorize` runs the user's policy once
/// per `(connection, service)`; `accept` is handed each admitted request-stream
/// and decodes the H3-object off `recv`, dispatches to the `tower::Service`,
/// and encodes the response onto `send`. Core stays hyper-free — it never sees
/// an HTTP type.
///
/// Async methods return [`BoxFut`] rather than using `async_trait` so core
/// takes no extra dependency.
pub trait LocalTunnelAcceptor: Send + Sync {
    /// Decide whether the peer (see [`PeerContext`]) may use this service.
    /// Runs exactly once per `(connection, service)` — see admission below.
    /// The context carries the verified peer id **and** an opaque metadata
    /// blob the consumer attached when opening the stream (a token, capability,
    /// app identity, …), so policy isn't limited to node id alone.
    fn authorize(&self, ctx: PeerContext) -> BoxFut<Result<()>>;

    /// Handle one admitted request-stream. Fire-and-forget; the stream is
    /// closed when this future completes. `conn` is the verified connection the
    /// stream arrived on — its `remote_id()` is the peer identity, and it can be
    /// used to open reverse streams back to the peer (the routing control plane
    /// relies on this).
    fn accept(
        &self,
        conn: CoreConnection,
        send: CoreSendStream,
        recv: CoreRecvStream,
    ) -> BoxFut<()>;
}

/// Identity + caller-supplied metadata presented at service admission. Passed
/// to [`LocalTunnelAcceptor::authorize`]. A struct (not a bare string) so new
/// fields can be added without breaking the seam.
#[derive(Clone, Debug, Default)]
pub struct PeerContext {
    /// Verified remote endpoint id (iroh node id).
    pub peer_id: String,
    /// Opaque metadata the consumer attached to the open frame. Operator-
    /// defined (token / signed capability / app identity / …); empty if none.
    pub metadata: Vec<u8>,
}

/// Node-shared map of `service_id -> acceptor`. Logically node-level (the same
/// service is exposed across every connection), shared into each
/// `CoreConnection` via `Arc`.
#[derive(Default)]
pub struct LocalTargetRegistry {
    inner: Mutex<HashMap<String, Arc<dyn LocalTunnelAcceptor>>>,
}

impl LocalTargetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, service_id: &str, acceptor: Arc<dyn LocalTunnelAcceptor>) {
        self.inner
            .lock()
            .expect("local target registry poisoned")
            .insert(service_id.to_string(), acceptor);
    }

    pub fn unregister(&self, service_id: &str) {
        self.inner
            .lock()
            .expect("local target registry poisoned")
            .remove(service_id);
    }

    pub fn get(&self, service_id: &str) -> Option<Arc<dyn LocalTunnelAcceptor>> {
        self.inner
            .lock()
            .expect("local target registry poisoned")
            .get(service_id)
            .cloned()
    }
}

/// Per-service admission slot: a single-flight authorize result + a concurrent
/// stream counter for the cap.
struct ServiceSlot {
    /// Set once `authorize` succeeds. `OnceCell` gives single-flight for free —
    /// the first stream runs `authorize`, concurrent first-streams await the
    /// same result. Only `Ok` is cached, so a denied attempt can be retried.
    admitted: OnceCell<()>,
    in_flight: AtomicUsize,
}

impl ServiceSlot {
    fn new() -> Self {
        Self {
            admitted: OnceCell::new(),
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Reserve one stream slot if under `cap`. Returns false at the cap.
    fn try_acquire(&self, cap: usize) -> bool {
        let mut cur = self.in_flight.load(Ordering::Relaxed);
        loop {
            if cur >= cap {
                return false;
            }
            match self.in_flight.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    fn release(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Per-connection admission state: which `(service)` this connection is
/// admitted for, and how many request-streams it currently has in flight.
/// Dropped with the connection (mass-revoke), exactly like [`TunnelRegistry`].
pub struct AdmissionState {
    services: Mutex<HashMap<String, Arc<ServiceSlot>>>,
    cap: usize,
}

impl AdmissionState {
    pub fn new(cap: usize) -> Self {
        Self {
            services: Mutex::new(HashMap::new()),
            cap,
        }
    }

    fn slot(&self, service_id: &str) -> Arc<ServiceSlot> {
        self.services
            .lock()
            .expect("admission state poisoned")
            .entry(service_id.to_string())
            .or_insert_with(|| Arc::new(ServiceSlot::new()))
            .clone()
    }
}

/// Decrements a service's in-flight counter on drop, so the cap is released
/// even if the acceptor future panics or the stream is dropped early.
struct InFlightGuard(Arc<ServiceSlot>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Parse `[u16 len][service_id utf8]([u16 mlen][metadata])?` from the first
/// frame's payload (the bytes after the subtype byte). The metadata block is
/// optional — an older/simpler opener may omit it, in which case metadata is
/// empty.
fn parse_open_frame(buf: &[u8]) -> Result<(String, Vec<u8>)> {
    if buf.len() < 2 {
        bail!("http-relay first frame too short for service_id length");
    }
    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let id = buf
        .get(2..2 + len)
        .context("http-relay service_id length exceeds frame")?;
    let service_id = std::str::from_utf8(id)
        .context("http-relay service_id not utf-8")?
        .to_string();

    let rest = &buf[2 + len..];
    let metadata = if rest.len() < 2 {
        Vec::new()
    } else {
        let mlen = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        rest.get(2..2 + mlen)
            .context("http-relay metadata length exceeds frame")?
            .to_vec()
    };
    Ok((service_id, metadata))
}

/// Why an http-relay stream was not admitted. All variants drop the stream
/// silently — http-relay streams aren't RPC and carry no error-frame contract.
#[derive(Debug)]
enum AdmitReject {
    UnknownService,
    Denied(String),
    CapReached(usize),
}

/// Resolve the acceptor, admit the `(connection, service)` once (single-flight),
/// and reserve a per-service stream slot. On success returns the acceptor plus
/// an [`InFlightGuard`] that releases the slot when dropped. Stream-free, so the
/// admission policy is unit-testable without a live connection.
async fn admit(
    service_id: &str,
    ctx: &PeerContext,
    registry: &LocalTargetRegistry,
    admissions: &AdmissionState,
) -> std::result::Result<(Arc<dyn LocalTunnelAcceptor>, InFlightGuard), AdmitReject> {
    let acceptor = registry
        .get(service_id)
        .ok_or(AdmitReject::UnknownService)?;
    let slot = admissions.slot(service_id);

    // Single-flight: only the first stream for an unadmitted (connection,
    // service) runs `authorize`; concurrent first-streams await the same
    // result rather than each firing a policy/JWT round-trip.
    slot.admitted
        .get_or_try_init(|| {
            let acceptor = acceptor.clone();
            let ctx = ctx.clone();
            async move { acceptor.authorize(ctx).await.map_err(|e| e.to_string()) }
        })
        .await
        .map_err(AdmitReject::Denied)?;

    // Per-service concurrent-stream cap — the abuse bound that replaces the
    // one-shot model's `max_outstanding`.
    if !slot.try_acquire(admissions.cap) {
        return Err(AdmitReject::CapReached(admissions.cap));
    }
    Ok((acceptor, InFlightGuard(slot)))
}

/// Dispatch a `subtype 1` http-relay stream: admit it (above), then hand the
/// raw stream to the registered acceptor, which decodes the H3-object and
/// dispatches to the local `tower::Service`. The in-flight guard is held for
/// the acceptor's lifetime so the cap reflects truly-active streams.
pub async fn handle_http_relay(
    conn: CoreConnection,
    send: CoreSendStream,
    recv: CoreRecvStream,
    first_frame_rest: Vec<u8>,
    peer: String,
    registry: Arc<LocalTargetRegistry>,
    admissions: Arc<AdmissionState>,
) -> Result<()> {
    let (service_id, metadata) = parse_open_frame(&first_frame_rest)?;
    let ctx = PeerContext {
        peer_id: peer,
        metadata,
    };
    match admit(&service_id, &ctx, &registry, &admissions).await {
        Ok((acceptor, _guard)) => acceptor.accept(conn, send, recv).await,
        Err(AdmitReject::UnknownService) => {
            tracing::debug!("http-relay: unknown service '{service_id}', dropping stream");
        }
        Err(AdmitReject::Denied(msg)) => {
            tracing::debug!("http-relay: authorize denied for '{service_id}': {msg}");
        }
        Err(AdmitReject::CapReached(cap)) => {
            tracing::debug!(
                "http-relay: per-service stream cap {cap} reached for '{service_id}', dropping"
            );
        }
    }
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

#[cfg(test)]
mod admission_tests {
    use super::*;

    /// A `LocalTunnelAcceptor` that counts `authorize` calls and can allow,
    /// deny, or delay. `accept` is a no-op (admission tests never reach it, so
    /// they never need to construct real streams).
    struct MockAcceptor {
        authorize_calls: Arc<AtomicUsize>,
        allow: bool,
        delay_ms: u64,
    }

    impl LocalTunnelAcceptor for MockAcceptor {
        fn authorize(&self, _ctx: PeerContext) -> BoxFut<Result<()>> {
            let calls = self.authorize_calls.clone();
            let allow = self.allow;
            let delay = self.delay_ms;
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                if allow {
                    Ok(())
                } else {
                    Err(anyhow!("denied by mock"))
                }
            })
        }

        fn accept(
            &self,
            _conn: CoreConnection,
            _send: CoreSendStream,
            _recv: CoreRecvStream,
        ) -> BoxFut<()> {
            Box::pin(async {})
        }
    }

    fn registry_with(service: &str, acc: MockAcceptor) -> Arc<LocalTargetRegistry> {
        let reg = Arc::new(LocalTargetRegistry::new());
        reg.register(service, Arc::new(acc));
        reg
    }

    /// A `PeerContext` carrying just a peer id (no metadata) for admit tests.
    fn ctx(peer: &str) -> PeerContext {
        PeerContext {
            peer_id: peer.to_string(),
            metadata: Vec::new(),
        }
    }

    fn mock(calls: &Arc<AtomicUsize>, allow: bool, delay_ms: u64) -> MockAcceptor {
        MockAcceptor {
            authorize_calls: calls.clone(),
            allow,
            delay_ms,
        }
    }

    #[tokio::test]
    async fn unknown_service_rejected() {
        let reg = Arc::new(LocalTargetRegistry::new());
        let adm = AdmissionState::new(8);
        let r = admit("nope", &ctx("peerA"), &reg, &adm).await;
        assert!(matches!(r, Err(AdmitReject::UnknownService)));
    }

    #[tokio::test]
    async fn admitted_once_skips_reauthorize() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reg = registry_with("svc", mock(&calls, true, 0));
        let adm = AdmissionState::new(8);
        let _g1 = admit("svc", &ctx("peerA"), &reg, &adm)
            .await
            .expect("first admit ok");
        let _g2 = admit("svc", &ctx("peerA"), &reg, &adm)
            .await
            .expect("second admit ok");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "authorize must run once per (connection, service)"
        );
    }

    #[tokio::test]
    async fn concurrent_first_streams_single_flight() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reg = registry_with("svc", mock(&calls, true, 50));
        let adm = Arc::new(AdmissionState::new(64));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let reg = reg.clone();
            let adm = adm.clone();
            handles.push(tokio::spawn(async move {
                admit("svc", &ctx("peerA"), &reg, &adm).await.is_ok()
            }));
        }
        let mut oks = 0;
        for h in handles {
            if h.await.unwrap() {
                oks += 1;
            }
        }
        assert_eq!(oks, 8, "all 8 concurrent first-streams admitted");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "authorize single-flighted to exactly one call"
        );
    }

    #[tokio::test]
    async fn denied_authorize_rejected() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reg = registry_with("svc", mock(&calls, false, 0));
        let adm = AdmissionState::new(8);
        match admit("svc", &ctx("peerA"), &reg, &adm).await {
            Err(AdmitReject::Denied(msg)) => assert!(msg.contains("denied")),
            Err(other) => panic!("expected Denied, got {other:?}"),
            Ok(_) => panic!("expected Denied, got Ok(admitted)"),
        }
    }

    #[tokio::test]
    async fn per_service_cap_enforced() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reg = registry_with("svc", mock(&calls, true, 0));
        let adm = AdmissionState::new(2);
        let g1 = admit("svc", &ctx("p"), &reg, &adm).await.expect("1st ok");
        let _g2 = admit("svc", &ctx("p"), &reg, &adm).await.expect("2nd ok");
        match admit("svc", &ctx("p"), &reg, &adm).await {
            Err(AdmitReject::CapReached(cap)) => assert_eq!(cap, 2),
            Err(other) => panic!("expected CapReached, got {other:?}"),
            Ok(_) => panic!("expected CapReached, got Ok(admitted)"),
        }
        drop(g1); // releases one in-flight slot
        let _g4 = admit("svc", &ctx("p"), &reg, &adm)
            .await
            .expect("admit ok after a slot is released");
    }

    #[test]
    fn parse_open_frame_round_trips() {
        let id = "db.prod";
        let mut frame = Vec::new();
        frame.extend_from_slice(&(id.len() as u16).to_le_bytes());
        frame.extend_from_slice(id.as_bytes());
        assert_eq!(
            parse_open_frame(&frame).unwrap(),
            (id.to_string(), Vec::new())
        );
        assert!(parse_open_frame(&[0x05]).is_err()); // too short
        assert!(parse_open_frame(&[0xff, 0xff, 0x01]).is_err()); // len exceeds buf
    }
}
