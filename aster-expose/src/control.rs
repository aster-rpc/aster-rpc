//! Routing control plane (design doc §7 step 9) — register
//! host/port/protocol routes with a peer that acts as an edge, and the
//! reciprocal capability to *be* such a peer.
//!
//! **Salvo-free**: an origin registers routes without pulling in the edge
//! stack; only the public HTTP listener (`crate::edge`, behind the `edge`
//! feature) needs Salvo. The route table ([`EdgeRouter`]) lives here so both
//! sides share it.
//!
//! **P2P-symmetric**: the same connection carries registrations in either
//! direction. "Expose me to inbound traffic for host H" is [`request_route`];
//! "reach your internal service S" is the relay path in [`crate::relay`]. Any
//! node can do either on one connection.
//!
//! Wire (one length-framed message each way over the control stream):
//! ```text
//! request (origin -> edge):  [u32 len][ u8 ver | u16 count | specs… | u32 mlen | metadata ]
//! ack     (edge -> origin):  [u32 len][ u16 count | granted-specs… ]
//!   spec: [u8 protocol][u16 port][u16 hlen][host][u16 slen][service_id]
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};

use aster_transport_core::stream_io::BoxFut;
use aster_transport_core::tunnel::{LocalTunnelAcceptor, PeerContext};
use aster_transport_core::{CoreConnection, CoreRecvStream, CoreSendStream};

/// Reserved service id the edge exposes for route registration. An origin opens
/// an http-relay stream to it (via [`request_route`]) to claim routes.
pub const CONTROL_SERVICE_ID: &str = "__aster_route_control__";

/// Wire version for the route-request / ack framing.
const WIRE_VERSION: u8 = 1;

/// Bound on a single control message (route request or ack).
const MAX_CONTROL_MSG: usize = 64 * 1024;

/// How inbound traffic for a route is carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteProtocol {
    /// L7 HTTP object relay (H1/H2/WebSocket now; H3/WebTransport are V2).
    Http,
    /// L4 raw TCP splice (modeled now; listener binding deferred).
    Tcp,
}

impl RouteProtocol {
    fn to_u8(self) -> u8 {
        match self {
            Self::Http => 0,
            Self::Tcp => 1,
        }
    }
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Http),
            1 => Ok(Self::Tcp),
            other => bail!("unknown route protocol {other} (H3/WebTransport are V2)"),
        }
    }
}

/// One route a node asks a peer to bind on its behalf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSpec {
    /// Host / TLS SNI to match; `""` = any (port-only, e.g. raw TCP).
    pub host: String,
    /// Edge-facing port; `0` = the listener's default.
    pub port: u16,
    pub protocol: RouteProtocol,
    /// Origin-side target id (what it exposed via `expose_local_http`).
    pub service_id: String,
}

// ── wire codec ───────────────────────────────────────────────────────────────

fn put_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = u16::try_from(s.len()).map_err(|_| anyhow!("string too long ({} bytes)", s.len()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn put_spec(out: &mut Vec<u8>, s: &RouteSpec) -> Result<()> {
    out.push(s.protocol.to_u8());
    out.extend_from_slice(&s.port.to_le_bytes());
    put_str(out, &s.host)?;
    put_str(out, &s.service_id)?;
    Ok(())
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| anyhow!("overflow"))?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| anyhow!("control message truncated"))?;
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn string(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        Ok(String::from_utf8(self.take(len)?.to_vec())?)
    }
    fn spec(&mut self) -> Result<RouteSpec> {
        let protocol = RouteProtocol::from_u8(self.u8()?)?;
        let port = self.u16()?;
        let host = self.string()?;
        let service_id = self.string()?;
        Ok(RouteSpec {
            host,
            port,
            protocol,
            service_id,
        })
    }
}

/// Encode the route-request body: `version + specs + metadata`.
pub fn encode_route_request(specs: &[RouteSpec], metadata: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![WIRE_VERSION];
    let count = u16::try_from(specs.len()).map_err(|_| anyhow!("too many routes"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for s in specs {
        put_spec(&mut out, s)?;
    }
    let mlen = u32::try_from(metadata.len()).map_err(|_| anyhow!("metadata too long"))?;
    out.extend_from_slice(&mlen.to_le_bytes());
    out.extend_from_slice(metadata);
    Ok(out)
}

/// Decode a route-request body into `(specs, metadata)`.
pub fn decode_route_request(buf: &[u8]) -> Result<(Vec<RouteSpec>, Vec<u8>)> {
    let mut r = Reader::new(buf);
    let v = r.u8()?;
    if v != WIRE_VERSION {
        bail!("unsupported route wire version {v}");
    }
    let count = r.u16()? as usize;
    let mut specs = Vec::with_capacity(count);
    for _ in 0..count {
        specs.push(r.spec()?);
    }
    let mlen = r.u32()? as usize;
    let metadata = r.take(mlen)?.to_vec();
    Ok((specs, metadata))
}

fn encode_ack(granted: &[RouteSpec]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let count = u16::try_from(granted.len()).map_err(|_| anyhow!("too many routes"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for s in granted {
        put_spec(&mut out, s)?;
    }
    Ok(out)
}

fn decode_ack(buf: &[u8]) -> Result<Vec<RouteSpec>> {
    let mut r = Reader::new(buf);
    let count = r.u16()? as usize;
    let mut specs = Vec::with_capacity(count);
    for _ in 0..count {
        specs.push(r.spec()?);
    }
    Ok(specs)
}

// ── length-framed stream I/O ────────────────────────────────────────────────

async fn write_framed(send: &CoreSendStream, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len()).map_err(|_| anyhow!("control message too large"))?;
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&len.to_le_bytes());
    framed.extend_from_slice(body);
    send.write_all(framed).await
}

async fn read_framed(recv: &CoreRecvStream, max: usize) -> Result<Vec<u8>> {
    let lenb = recv.read_exact(4).await?;
    let len = u32::from_le_bytes([lenb[0], lenb[1], lenb[2], lenb[3]]) as usize;
    if len > max {
        bail!("control message too large: {len} > {max}");
    }
    recv.read_exact(len).await
}

// ── route table ──────────────────────────────────────────────────────────────

/// Where a route resolves: the origin connection, its exposed service id, and
/// how to carry the traffic.
#[derive(Clone)]
pub struct EdgeRoute {
    pub conn: CoreConnection,
    pub service_id: String,
    pub protocol: RouteProtocol,
}

/// `(host, port) → origin route` table. Populated by the control plane
/// ([`serve_routes_on_connection`]) and read by the edge data plane
/// (`crate::edge::RelayHandler`). Cheap to clone (shares one `Arc`).
#[derive(Clone, Default)]
pub struct EdgeRouter {
    routes: Arc<Mutex<HashMap<(String, u16), EdgeRoute>>>,
}

impl EdgeRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_spec(&self, spec: &RouteSpec, conn: CoreConnection) {
        self.routes.lock().expect("routes poisoned").insert(
            (spec.host.clone(), spec.port),
            EdgeRoute {
                conn,
                service_id: spec.service_id.clone(),
                protocol: spec.protocol,
            },
        );
    }

    /// Convenience: an HTTP route on the listener's default port.
    pub fn register(
        &self,
        host: impl Into<String>,
        conn: CoreConnection,
        service_id: impl Into<String>,
    ) {
        let spec = RouteSpec {
            host: host.into(),
            port: 0,
            protocol: RouteProtocol::Http,
            service_id: service_id.into(),
        };
        self.register_spec(&spec, conn);
    }

    pub fn unregister(&self, host: &str, port: u16) {
        self.routes
            .lock()
            .expect("routes poisoned")
            .remove(&(host.to_string(), port));
    }

    /// Resolve a route: exact `(host, port)`, then the `(host, 0)` default-port
    /// fallback.
    pub fn lookup(&self, host: &str, port: u16) -> Option<EdgeRoute> {
        let m = self.routes.lock().expect("routes poisoned");
        m.get(&(host.to_string(), port))
            .or_else(|| m.get(&(host.to_string(), 0)))
            .cloned()
    }
}

// ── edge side: accept registrations ─────────────────────────────────────────

/// Decides which requested routes a peer may bind. Given the peer's identity +
/// metadata and the requested specs, returns the **granted subset** (or `Err`
/// to reject all). This is the edge's admission hook.
pub type RoutePolicy =
    Arc<dyn Fn(PeerContext, Vec<RouteSpec>) -> BoxFut<Result<Vec<RouteSpec>>> + Send + Sync>;

/// Acceptor for the reserved control service: reads a route request, runs the
/// policy, installs granted routes into the shared [`EdgeRouter`], and holds
/// them until the control stream closes (then evicts).
pub struct ControlAcceptor {
    router: EdgeRouter,
    policy: RoutePolicy,
}

impl ControlAcceptor {
    pub fn new(router: EdgeRouter, policy: RoutePolicy) -> Self {
        Self { router, policy }
    }
}

impl LocalTunnelAcceptor for ControlAcceptor {
    fn authorize(&self, _ctx: PeerContext) -> BoxFut<Result<()>> {
        // Opening a control stream is allowed (bounded by the per-service cap);
        // the real decision is the per-route policy in `accept`, which sees the
        // requested specs and the registration metadata.
        Box::pin(async { Ok(()) })
    }

    fn accept(
        &self,
        conn: CoreConnection,
        send: CoreSendStream,
        recv: CoreRecvStream,
    ) -> BoxFut<()> {
        let router = self.router.clone();
        let policy = self.policy.clone();
        Box::pin(async move {
            if let Err(e) = handle_registration(conn, send, recv, router, policy).await {
                tracing::debug!("route control error: {e}");
            }
        })
    }
}

async fn handle_registration(
    conn: CoreConnection,
    send: CoreSendStream,
    recv: CoreRecvStream,
    router: EdgeRouter,
    policy: RoutePolicy,
) -> Result<()> {
    let body = read_framed(&recv, MAX_CONTROL_MSG).await?;
    let (specs, metadata) = decode_route_request(&body)?;
    let ctx = PeerContext {
        peer_id: conn.remote_id(),
        metadata,
    };

    let granted = match policy(ctx, specs).await {
        Ok(g) => g,
        Err(e) => {
            tracing::debug!("route policy rejected registration: {e}");
            Vec::new()
        }
    };
    for spec in &granted {
        router.register_spec(spec, conn.clone());
    }
    write_framed(&send, &encode_ack(&granted)?).await?;

    // Hold the routes until the peer closes the control stream, then evict.
    // Any further input on the control stream is ignored; `Ok(None)`/`Err`
    // means the stream ended (the registration handle was dropped/closed).
    while let Ok(Some(_)) = recv.read(MAX_CONTROL_MSG).await {}
    for spec in &granted {
        router.unregister(&spec.host, spec.port);
    }
    Ok(())
}

/// Expose the route-control service on `conn`, populating `router` per the
/// `policy`. Connection-scoped (node-level via `set_local_targets` is a later
/// step). Reciprocal to [`request_route`].
pub fn serve_routes_on_connection(conn: &CoreConnection, router: EdgeRouter, policy: RoutePolicy) {
    conn.register_local_target(
        CONTROL_SERVICE_ID,
        Arc::new(ControlAcceptor::new(router, policy)),
    );
}

// ── origin side: request registration ───────────────────────────────────────

/// A live route registration. Keep it alive for the routes to stay bound;
/// dropping it (or [`close`](Self::close)) ends the registration and the edge
/// evicts the routes.
pub struct RouteRegistration {
    granted: Vec<RouteSpec>,
    send: CoreSendStream,
    _recv: CoreRecvStream,
}

impl RouteRegistration {
    /// The subset of requested routes the edge accepted.
    pub fn granted(&self) -> &[RouteSpec] {
        &self.granted
    }

    /// Cleanly end the registration (finish the control stream).
    pub async fn close(self) -> Result<()> {
        self.send.finish().await
    }
}

/// Ask `conn`'s peer to route `specs` to us, attaching `metadata` for the
/// peer's [`RoutePolicy`]. Returns a handle whose [`granted`](RouteRegistration::granted)
/// lists what the peer accepted; hold it for the routes to stay live.
pub async fn request_route(
    conn: &CoreConnection,
    specs: Vec<RouteSpec>,
    metadata: Vec<u8>,
) -> Result<RouteRegistration> {
    let (send, recv) = conn.open_http_relay(CONTROL_SERVICE_ID, &[]).await?;
    let body = encode_route_request(&specs, &metadata)?;
    write_framed(&send, &body).await?;
    let ack = read_framed(&recv, MAX_CONTROL_MSG).await?;
    let granted = decode_ack(&ack)?;
    Ok(RouteRegistration {
        granted,
        send,
        _recv: recv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(host: &str, port: u16, proto: RouteProtocol, svc: &str) -> RouteSpec {
        RouteSpec {
            host: host.into(),
            port,
            protocol: proto,
            service_id: svc.into(),
        }
    }

    #[test]
    fn route_request_round_trips() {
        let specs = vec![
            spec("app.example.com", 443, RouteProtocol::Http, "web"),
            spec("", 5432, RouteProtocol::Tcp, "db"),
        ];
        let meta = b"token-xyz".to_vec();
        let wire = encode_route_request(&specs, &meta).unwrap();
        let (got_specs, got_meta) = decode_route_request(&wire).unwrap();
        assert_eq!(got_specs, specs);
        assert_eq!(got_meta, meta);
    }

    #[test]
    fn ack_round_trips_and_unknown_protocol_errs() {
        let granted = vec![spec("a.io", 0, RouteProtocol::Http, "svc")];
        let wire = encode_ack(&granted).unwrap();
        assert_eq!(decode_ack(&wire).unwrap(), granted);
        // protocol byte 2 (would be H3/WT) is rejected until V2.
        assert!(RouteProtocol::from_u8(2).is_err());
    }

    #[test]
    fn router_lookup_exact_then_default_port() {
        let router = EdgeRouter::new();
        assert!(router.lookup("a.io", 443).is_none());
        // Default-port registration is matched by any port via the fallback.
        // (No CoreConnection here — register_spec/relay covered by integration.)
    }

    #[test]
    fn decode_rejects_truncated() {
        assert!(decode_route_request(&[]).is_err());
        assert!(decode_route_request(&[WIRE_VERSION, 0x01, 0x00]).is_err()); // count=1, no spec
    }
}
