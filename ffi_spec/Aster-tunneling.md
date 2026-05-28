# Aster Tunneling — Design Spec

**Status:** Draft
**Date:** 2026-05-04
**Scope:** Add a Layer-1 capability to Aster `core` that lets an RPC service authorise a peer to open a TCP/UDP/HTTP-proxy tunnel through the same QUIC connection. The RPC handler does all policy/auth/posture work; `core` only knows about opaque, short-lived, one-shot capability tokens.

This spec covers Layer 1 only. ZTNA-flavoured features (signed capabilities, replicated policy data models, posture rules, audit pipelines, JWT minting) live in higher layers built **on top of** this primitive — none of them belong in core. See §11.

---

## Table of Contents

1. [Motivation](#1-motivation)
2. [Architecture](#2-architecture)
3. [Threat Model and Security Properties](#3-threat-model-and-security-properties)
4. [Core API](#4-core-api)
5. [Wire Format](#5-wire-format)
6. [Server-Side Flow](#6-server-side-flow)
7. [Client-Side Flow](#7-client-side-flow)
8. [Ticket Lifecycle and Bounds](#8-ticket-lifecycle-and-bounds)
9. [Configuration](#9-configuration)
10. [Non-Goals](#10-non-goals)
11. [Future Variants and Higher Layers](#11-future-variants-and-higher-layers)

---

## 1. Motivation

A common deployment pattern for Aster nodes is "the node already has identity, NAT-traversed reachability, and policy code — let it act as a brokered proxy for a non-Aster TCP/UDP service behind it." Concrete examples:

- **VNC / SSH / RDP brokering**: an Aster node running on a workstation exposes a local VNC server to authorised peers without opening the host port to the network. The peer authenticates via the existing Aster identity layer, the node's policy decides whether to permit access, and only then is a tunnel allowed.
- **Local development endpoints**: exposing a local Chrome DevTools or React dev server to a teammate without ngrok-style intermediaries. (Header-rewriting is required here — see §11 — so these uses depend on a future `HttpProxy` variant.)
- **Service-mesh-style ZTNA gateways**: a connector node sitting next to a private database or admin panel, brokering identity-bound connections from authorised peers. (Higher layers — see §11.)

Today bindings can do this manually with `open_bi` + their own framing convention, but every binding reinvents the same concerns (target authorisation, replay protection, idle timeouts, target opacity to the peer). Centralising this in core gives every binding the primitive for free and pins down the security model in one place.

---

## 2. Architecture

This feature is one layer in a larger ZTNA stack. Only the bottom layer lives in `core`:

```
┌──────────────────────────────────────────────────────────────┐
│ Layer 4: Tooling — CLI, admin UI                             │
├──────────────────────────────────────────────────────────────┤
│ Layer 3: JWT minting + posture/identity policy evaluation    │
├──────────────────────────────────────────────────────────────┤
│ Layer 2: Policy data model — Service, Identity, Policy types │
│          + delta-stream sync RPC (subscribe(index) → events) │
├──────────────────────────────────────────────────────────────┤
│ Layer 1: Tunnel mechanism — authorize_tunnel, redeem path,   │
│          TCP/UDP/HttpProxy acceptors      ◄── THIS SPEC      │
├──────────────────────────────────────────────────────────────┤
│ Layer 0: Aster — identity, signed RPC, QUIC underlay         │
└──────────────────────────────────────────────────────────────┘
```

**Division of responsibility**

- **RPC handler (binding code, written by the user)**: validates the incoming request — checks the peer identity, evaluates whatever policy applies (JWT verification, posture, rate limits, business rules, replicated policy lookups). On success, it calls `authorize_tunnel(target, ttl)` and returns the resulting opaque ticket bytes to the peer in the RPC response.
- **`core` (Rust)**: owns the per-connection ticket registry; mints tickets via CSPRNG; routes inbound bidi streams whose first frame carries `FLAG_TUNNEL` to the tunnel acceptor; on a valid redeem, opens the backend socket and shovels bytes; expires tickets on TTL; bounds the registry size.

`core` has zero knowledge of JWTs, postures, identities, services, or policy. The handler did the auth; the ticket is its proof.

---

## 3. Threat Model and Security Properties

Inside the trust boundary (after QUIC + Aster identity has authenticated the peer):

| Property | How it's achieved |
|---|---|
| **Target opacity** | The peer never sees `TunnelTarget`. The ticket is 32 random bytes. The mapping `ticket → target` lives only in core's per-connection registry. A peer cannot learn the backend host:port from the ticket. |
| **Unforgeable tickets** | Tickets are 32 bytes from `OsRng` (CSPRNG). Brute-force is `2^256`. |
| **Connection-bound capability** | The registry is keyed by the `connection_id` that core observes on accept (not anything the peer sends). A ticket leaked or copied to another peer is useless because that peer has a different `connection_id` submap. |
| **One-shot redemption** | The registry entry is *removed* on lookup, not just compared. A redeemed ticket cannot be replayed even within its TTL. |
| **Short TTL** | Default 30s, hard cap 120s. Unredeemed tickets expire on their own; no revocation API needed. |
| **Bounded registry** | Per-connection cap (default 64 outstanding tickets). Protects against an authenticated-but-misbehaving peer that calls `authorize_tunnel` in a loop without redeeming. |
| **Connection close = mass revocation** | Closing the QUIC connection drops the registry and aborts every active tunnel on it. This is the only "revocation" mechanism core provides. |

**Out of scope for this layer:**

- Authorisation — the handler did it.
- Identity — Aster Layer 0 did it.
- E2E encryption past the connector — see §11.

---

## 4. Core API

```rust
/// Opaque 32-byte capability token. Returned by the policy handler to
/// the peer. The peer presents these bytes to redeem the tunnel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TunnelTicket(pub [u8; 32]);

/// What the tunnel connects to on the server side. Held in core's
/// per-connection registry; never sent over the wire to the peer.
#[derive(Clone, Debug)]
pub enum TunnelTarget {
    Tcp { addr: SocketAddr },
    // v1 ships Tcp only. Udp and HttpProxy land in follow-ups (§11).
    // Udp { addr: SocketAddr },
    // HttpProxy { addr: SocketAddr, host_header: Option<String>, origin_header: Option<String> },
}

impl CoreConnection {
    /// Issue a fresh tunnel capability covering one or more targets.
    /// Must be called by an RPC handler that has *already* validated
    /// whatever policy applies. `targets` is an ordered preference
    /// list; at redeem the acceptor tries each in order and splices
    /// the first one that connects (failing silently if every target
    /// is unreachable). Returns the ticket bytes for the handler to
    /// embed in its RPC response.
    ///
    /// Errors:
    /// - `EmptyTargets` if `targets` is empty.
    /// - `TooManyOutstanding` if the per-connection cap is reached
    ///   (one ticket = one entry, regardless of target count).
    /// - `TtlOutOfRange` if `ttl > 120s` (default hard cap).
    pub fn authorize_tunnel(
        &self,
        targets: Vec<TunnelTarget>,
        ttl: Duration,
    ) -> Result<TunnelTicket, AuthorizeTunnelError>;
}
```

**Why a list of targets, not one.** A single ticket can carry an ordered preference list — primary/standby pairs, per-protocol fallback, multi-port services. The peer never sees the list (target opacity preserved); it just redeems and the server picks the first reachable target. Cap accounting is per-ticket, so a 5-target ticket consumes one slot, not five. Replay protection is unchanged: a single ticket pops at the first redeem regardless of how many targets it covered.

**Why `CoreConnection` and not `CoreNode`.** The capability is connection-bound. Forcing the call site to be on the connection makes the binding (or anything calling it) carry that scope explicitly. RPC dispatch already passes a `connection_id` to handlers; the binding's bridge layer turns that into `&CoreConnection`.

**FFI shape (per binding).** Each binding exposes the equivalent of `connection.authorize_tunnel(targets, ttl) -> bytes`. The `targets` are constructed from the binding's native target types (`Tcp(host, port)`, etc.); the returned bytes are ready to be put into an RPC response payload.

---

## 5. Wire Format

A tunnel-redeem stream is a normal QUIC bidi stream, distinguished by a single new flag bit on its first frame.

```
First frame:
  [4B LE length = 33] [1B flags = FLAG_TUNNEL] [32B ticket]

Subsequent bytes on the stream are raw — no framing, no flags. The
acceptor splices them directly to the backend socket.
```

**New flag**

```rust
pub const FLAG_TUNNEL: u8 = 0x80;   // currently the only unused bit
```

`FLAG_TUNNEL` is mutually exclusive with all existing flags. A frame whose flags include `FLAG_TUNNEL` is a tunnel-redeem frame and the server handles the rest of the stream as opaque bytes; a frame whose flags do not include `FLAG_TUNNEL` enters the existing RPC dispatch path and the existing flag bits keep their meanings.

**Why one bit, not a separate ALPN.** A separate ALPN would force a separate QUIC connection, doubling handshake cost and losing the shared-connection property that motivates this design (single congestion controller, single 0-RTT resumption, single NAT-traversed path). One bit on the first frame discriminates with no other cost.

**Why no length-framed envelope after the first frame.** The first frame carries only the ticket; once redeemed, the wire is a raw byte pipe to the backend. For `Tcp` this is a hard requirement (TCP has no frame boundaries to preserve). For future variants (`Udp`, `HttpProxy`) the acceptor may impose its own framing on the stream after redemption — that's a per-variant decision and is out of scope here.

---

## 6. Server-Side Flow

```
1. CoreConnection accepts a bidi stream (existing path: reactor.rs).
2. Reactor reads exactly the first frame (per existing framing rules).
3. If flags & FLAG_TUNNEL == 0:
       → existing RPC dispatch path (handle_stream_inner).
4. If flags & FLAG_TUNNEL != 0:
       → spawn tunnel handler:
         a. Verify payload is exactly 32 bytes; else close stream.
         b. Pop entry from this connection's tunnel registry.
            - Not found → close stream (no error frame; tunnels aren't
              an RPC, peers learn redemption failed via stream close).
            - Expired   → close stream.
         c. Match TunnelTarget:
            - Tcp { addr }: tokio::net::TcpStream::connect(addr).
              Then tokio::io::copy_bidirectional between the QUIC
              streams and the TCP socket. Propagate close in both
              directions when either side EOFs or errors.
```

**Dispatch site.** Resolved: `CoreConnection::accept_aster_bi` performs
the `FLAG_TUNNEL` peek + dispatch transparently — its caller never sees
tunnel streams. The reactor uses this method; any future binding-driven
accept loop on the RPC ALPN should too. The plain
`CoreConnection::accept_bi` stays raw (no peek) so non-Aster protocols
that share the QUIC connection (trust admission, custom ALPNs) keep
their native wire format. Tunneling is an Aster-protocol-layer feature,
not a QUIC-layer one — `accept_aster_bi` draws that boundary explicitly.

**Registry placement.** The registry is a field on `CoreConnection`:

```rust
struct TunnelRegistry {
    inner: Mutex<HashMap<TunnelTicket, RegistryEntry>>,
    cap: usize,                // default 64
    ttl_max: Duration,         // default 120s
}
struct RegistryEntry {
    target: TunnelTarget,
    expires_at: Instant,
}
```

**Lazy expiry.** No background sweeper task. `authorize_tunnel` rejects new entries when the map is full (cap-bounded; expired entries are evicted opportunistically during `authorize_tunnel` calls). `redeem_tunnel` checks `expires_at` and treats expired entries as "not found." This keeps the implementation simple and avoids a per-connection timer.

**No error replies.** A failed redeem just closes the stream. Tunnels aren't RPC; there is no trailer/error-frame contract to honour. The peer learns failure by seeing the stream close before any bytes flow.

---

## 7. Client-Side Flow

```rust
impl CoreConnection {
    /// Open a tunnel by redeeming a ticket received from the peer.
    /// Sends the [FLAG_TUNNEL][ticket] handshake frame internally,
    /// then returns the bidi stream pair. Bytes after that point are
    /// raw — the caller may write/read application protocol data
    /// (e.g. RFB for VNC) directly.
    pub async fn open_tunnel(
        &self,
        ticket: TunnelTicket,
    ) -> Result<(CoreSendStream, CoreRecvStream)>;
}
```

The client typically:

1. Calls an RPC method on the policy node (`request_tunnel(service, ...)` or whatever the application defines).
2. Receives a 32-byte ticket in the RPC response.
3. Calls `connection.open_tunnel(ticket)` on **the same connection** the RPC was made on.
4. Reads/writes raw bytes for whatever protocol the backend speaks.

**The handshake frame is written by `open_tunnel`** so the caller never has to know about `FLAG_TUNNEL`. Writing it lazily on the first send rather than eagerly during the call would shave one round-trip; deferred to a follow-up if benchmarks justify it.

---

## 8. Ticket Lifecycle and Bounds

| Property | Default | Configurable | Rationale |
|---|---|---|---|
| Ticket size | 32 bytes | No | Standard for opaque capability tokens; any smaller is brute-forceable. |
| Source of randomness | `OsRng` | No | Anything weaker undermines unforgeability. |
| Default TTL | 30 seconds | Yes (per-call) | Real-world tunnel-establishment latency from "RPC returns ticket" to "peer redeems" is sub-second; 30s is generous. |
| Hard-cap TTL | 120 seconds | Yes (per-node config) | Prevents a misconfigured handler from issuing 1-day tickets. |
| Max outstanding per connection | 64 | Yes (per-node config) | Bounds memory + abuse. Most peers have ≤2 active flows. |
| Redemption | One-shot (pop) | No | Replay protection. |
| Cross-connection redemption | Forbidden | No | Per-connection registry; not addressable from another connection. |
| Revocation API | None | — | Short TTL + connection close suffice for v1. |

**Sizing the cap.** 64 × per-connection × small `RegistryEntry` (≈80 bytes) = 5 KiB worst-case per connection. Negligible.

---

## 9. Configuration

A single new struct, plumbed through `NodeConfig` like other tunables:

```rust
pub struct TunnelConfig {
    /// Hard cap on TTL passed to authorize_tunnel. Default 120s.
    pub max_ticket_ttl: Duration,
    /// Default TTL when a handler passes Duration::ZERO. Default 30s.
    pub default_ticket_ttl: Duration,
    /// Per-connection cap on outstanding (unredeemed, unexpired) tickets.
    /// Default 64.
    pub max_outstanding_per_connection: usize,
}
```

Defaults are fine for nearly every deployment; the knobs exist for future ZTNA gateways that may want different bounds.

---

## 10. Non-Goals

The following are deliberately **not** in core. They live in higher layers (§11) or in user RPC code:

- **JWT verification.** Tickets are not JWTs. Handlers may *accept* a JWT in their RPC request and validate it before calling `authorize_tunnel`, but core never sees one.
- **Policy data model.** No `Service`, `Identity`, `Policy`, or `PostureRule` types in core. Those are application concerns built on top of Aster RPC if you need them.
- **Replicated policy / delta sync.** Out of scope for Layer 1; see §11.
- **Revocation API.** Short TTL + `connection.close()` is the revocation surface.
- **Audit logging.** Handlers emit audit events themselves (or use existing Aster hooks). Core does not pre-decide an audit schema.
- **Service discovery.** "Which services can I reach?" is an RPC the handler implements.
- **End-to-end encryption past the connector.** See §11.
- **Wildcard / dynamic targets.** Issue a fresh ticket per request instead — that's what the policy handler is for.

---

## 11. Future Variants and Higher Layers

These are sketched here so the Layer 1 design doesn't accidentally close doors.

**`TunnelTarget::Udp { addr }`** — the acceptor relays QUIC datagrams (or, more likely, length-prefixed framing on the bidi stream) to a UDP socket. Useful for DNS, WireGuard-style tunnels, and game traffic. The wire format after redemption needs framing to preserve datagram boundaries; that's a per-variant choice deferred to its own spec.

**`TunnelTarget::HttpProxy { addr, host_header, origin_header }`** — an HTTP/1.1-aware acceptor that parses requests, rewrites `Host:` and/or `Origin:` headers, and forwards. Required for things like exposing a local Chrome DevTools endpoint (DevTools rejects mismatched `Host` and `Origin`). On `101 Switching Protocols` the acceptor falls through to raw splice, so WebSocket upgrade Just Works. Pulls in a dependency on `hyper` (probably already transitive via `iroh`); if that's unwelcome, this variant moves to a separate `aster-tunnel-http` crate.

**Layer 2 — replicated policy data model.** Modelled directly on OpenZiti's design (`controller/handler_edge_ctrl/subscribe_to_data_model.go`): a streaming delta-sync RPC where connectors send `(currentIndex, subscriptionId)` and the policy node streams forward events: identity-added, service-added, policy-changed, posture-rule-changed, revocation. Connectors cache the full data state locally and serve `authorize_tunnel`-fronting handlers from the local replica. Importantly: tickets remain `LocalOpaque` even in this world. The sync protocol distributes *policy*; tickets are still proofs of authorisation issued after the handler consults its replica.

**Layer 3 — JWTs as RPC request credentials.** When the policy plane and the connector are different nodes, a client gets a JWT from the policy plane (signed against a key in the data model), then presents it on the connector's RPC. The connector verifies the JWT against its replica, then calls `authorize_tunnel`. Core stays pure — JWT machinery lives entirely in the handler.

**Layer 5 — true E2E encryption.** Following Ziti's `peerData` pattern: when a service is flagged `e2e=true` in the data model, the policy handler returns a pubkey alongside the ticket; the client wraps tunnel payloads with that key and the connector becomes a blind forwarder. This *is* a wire-format change at the tunnel layer (the redeem path needs a "blind forwarder" mode), so it's worth flagging as a future consideration even though we're not building it now.

---

## Appendix: Pointers

- `core/src/framing.rs` — adds `FLAG_TUNNEL = 0x80`.
- `core/src/lib.rs` — adds `TunnelTicket`, `TunnelTarget`, `TunnelRegistry`, `authorize_tunnel`, `open_tunnel` on `CoreConnection`.
- `core/src/reactor.rs:306` (`connection_loop`) — adds the FLAG_TUNNEL dispatch branch.
- `core/src/tunnel.rs` (new) — registry + acceptor implementations.
