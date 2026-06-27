# aster-expose-http — exposing local HTTP services over Aster

**Status:** Working idea
**Date:** 2026-06-26
**Scope:** Let an Aster node publish a local HTTP service (most often running in the *same process* — a Salvo/hyper/tower server, an HTTP/3 + WebTransport origin) to authorised remote Aster nodes, and the inverse: a public edge node that reverse-proxies inbound browser traffic through Aster to a backend node's local service (ngrok-shaped). Mechanism only; policy/posture/JWT live in handler code as with `Aster-tunneling.md`.

References:
- Layer-1 transport primitive: `ffi_spec/Aster-tunneling.md` (shipped — `core/src/tunnel.rs`)
- Stream multiplexing: `ffi_spec/Aster-multiplexed-streams.md`
- The heavy L3 sibling (TUN/smoltcp/DNS, *not* this): `docs/_internal/working_ideas/aster-tunneld-linux.md`

---

## 1. What this is (and what it is not)

`aster-tunneld` intercepts *arbitrary, unknown* app traffic at L3 — hence TUN, smoltcp, DNS. This feature has none of that problem: there is exactly **one known local service**, usually in the same OS process. That deletes the entire userspace-netstack layer. What remains is stream/packet plumbing between an Aster QUIC connection and a local HTTP service, with the local node holding final say on admission.

Both product use cases reduce to **one acceptor role** (the node next to the service) plus a thin front-end that differs only in what feeds the other end:

| Use case | Front-end (the "other end") | Acceptor (next to service) |
|---|---|---|
| **1. Expose localhost to a remote Aster node** | a remote Aster node calls `dial_http` directly | this node |
| **2. ngrok-style public ingress** | a public edge terminates browser HTTP and relays over a warm Aster connection | the backend node |

In both, the service-side node authorises, then bytes/requests flow. Case 2 is just *Case 1 + an HTTP edge in front*. We build one acceptor and one edge.

---

## 2. Architecture — H3/WT-native, terminate-and-relay

> **Revised 2026-06-26.** Pivoted from a stream-pool + HTTP/1.1-over-stream design to a QUIC-native datapath. The browser can't speak Aster, so the edge still terminates browser H3/WT and re-originates to the backend (the terminate-and-relay reasoning in §3 is unchanged and still decisive). What changed is the *inter-node wire*: instead of smuggling HTTP/1.1 over a pool of reused reliable streams, the relay preserves QUIC/H3 primitives 1:1. Earlier pool/keep-alive prose is superseded by this section.

**Three principles.**

1. **One request → one Aster stream — no pool.** H3 already gives every request its own QUIC stream, and opening a quinn stream is RTT-free. Pooling/reusing streams HTTP/1.1-style would re-serialize exactly the parallelism H3 provides. So the relay maps 1:1; the Aster stream count floats with the connection's `max_concurrent_bidi_streams`, like H3 itself. (The "stream cliff" that motivated pooling is Aster's *RPC dispatch/codec* overhead — see `Aster-multiplexed-streams.md §1` — **not** a QUIC cost; the raw relay datapath never enters RPC dispatch, so it doesn't pay it.)

2. **Thin H3-object relay — not HTTP/1.1 re-encode.** Both ends are ours, so the edge↔backend hop carries a minimal object — `[headers block][body]` — not wire-compatible HTTP/1.1 and not full H3+QPACK. The backend decodes the object straight into the user's `tower::Service` (`Service::call`) and encodes the response back: **no `hyper::serve_connection`, no HTTP/1.1 parser on the relay hop.** This is *less* machinery than the superseded design and it preserves the request-stream model, headers, and priorities instead of rebuilding an inferior HTTP/1.1.

3. **Datagrams are first-class.** WebTransport is half datagrams; they are not deferred. Each WT/H3 datagram → one Aster unreliable datagram tagged `[flow-id][payload]`, demuxed to its session at the backend (§3.4, §5.4). Built into the design from day one, staged after request/response.

**Modes** (what's behind the door):

| Mode | Aster wire | Backend dispatch | For |
|---|---|---|---|
| **L7 object relay** | 1 stream/request carrying a `[headers][body]` thin object; 1 datagram per WT/H3 datagram tagged by flow-id | decode → `tower::Service` (same process, zero loopback) | HTTP/1.1/2/3 + WebTransport origins |
| **L4 splice** | reliable bidi stream, raw bytes, **one-shot ticket** (shipped) | `TcpStream::connect(127.0.0.1:port)` | separate-process TCP: HTTP/1.1, WS, VNC/SSH/Postgres |
| **Packet forward** | **unreliable** datagrams — **forward-proxy only** (§3.2) | UDP socket / re-inject | non-HTTP UDP between two Aster nodes; WireGuard/DNS/game |

**Authorization — session admission, unchanged from the load-bearing decision (§6.1).** Per-request `Broker.open` would force an RPC round-trip before every HTTP request (a throughput killer for an edge). So the relay authorizes once per `(connection, service)` and then admits that connection's request-streams. The 1:1-stream model doesn't change this — admission is per connection-service, not per stream.

**Abuse cap, reframed.** The old pool size doubled as the DoS bound. With 1:1 streams the bound is two-layered: the connection's `max_concurrent_bidi_streams` (QUIC-level backstop, `core/src/lib.rs:107`) plus a per-`(connection, service)` concurrent-stream cap (app-level, for the *edge-is-fully-trusted-and-can-fan-out* case in §6). Same security property as before (§6.1) — tune the cap by peer shape, not pool size.

**`AsterStreamIo` is demoted, not deleted.** The duplex adapter (§5.1) is no longer the L7 spine; it serves the **L4 splice** path, **streaming-body pumping**, and **WebSocket/CONNECT upgrades** where a raw byte pipe is wanted. The L7 object relay reads length-delimited frames off the Aster stream directly via core's existing `read_one_frame`/framing (§5.3), not via hyper.

---

## 3. The HTTP/3 decision (the crux)

You use HTTP/3 heavily and asked whether **packet forwarding** is the ideal way to preserve loss/congestion control. The honest answer: **for HTTP/3, packet forwarding is the worst option, and terminate-and-relay (L7) is the ideal one.** Here is the reasoning, because it determines everything downstream.

### 3.1 Why you must not forward QUIC packets over a reliable stream

Forwarding QUIC/UDP packets over an Aster **reliable, ordered** bidi stream is the classic "TCP-over-TCP meltdown," generalised:

- **Double retransmission.** A lost packet on the outer (Aster) leg is retransmitted by the outer QUIC *and* by the inner QUIC's own loss detection. Wasted bandwidth, fighting timers.
- **Head-of-line blocking restored.** The outer stream is ordered, so one lost outer packet stalls *every* inner stream behind it — destroying the exact property HTTP/3 exists to provide.
- **Stacked congestion controllers.** Two CC loops nested oscillate and collapse throughput under loss.

So reliable-stream packet forwarding is off the table for QUIC. Full stop.

### 3.2 Why even datagram forwarding (MASQUE-style) is only "acceptable", not "ideal"

You *can* forward each UDP packet as one **unreliable** Aster/QUIC DATAGRAM (the CONNECT-UDP / MASQUE model): the outer QUIC never retransmits, the inner QUIC sees real loss and runs its CC correctly. This avoids the meltdown. But it is still not ideal:

- The outer connection's CC still rate-limits datagram emission, so the inner CC adapts to a *shaped* path, not the true one.
- QUIC datagrams must fit one packet (~1200 B usable after overhead); larger inner UDP packets need fragmentation/PMTU handling you now own.
- You need Aster to expose unreliable datagrams *and* per-tunnel datagram demux framing (none of the existing reliable-stream splice applies).

It is the right tool only when the endpoints are two Aster nodes that genuinely need **end-to-end QUIC** (0-RTT, connection migration, E2E encryption past the relay) and cannot terminate — e.g. relaying WireGuard, a UDP game protocol, or DNS-over-UDP. It is not the right tool for HTTP/3.

### 3.3 Why terminate-and-relay is ideal for HTTP/3

Browsers cannot speak Aster. So a browser's QUIC connection **ends at your edge no matter what** — there is no genuine end-to-end QUIC to preserve in the first place. Given that, the ideal shape is what every real CDN does: terminate QUIC at the edge, re-originate to the backend over a fresh, properly-terminated transport. Each segment is native and single-CC:

```
browser ──HTTP/3 (native QUIC, full CC)──▶ edge node
edge node ──Aster QUIC, pooled reused streams──▶ backend node
backend node ──in-memory duplex──▶ hyper/tower Service (your app)
```

No nesting anywhere. Browser↔edge is real HTTP/3. Edge↔backend is real Aster QUIC carrying HTTP *semantics* (method/path/headers/body) over a **bounded pool of reused streams** (§2): concurrent requests ride separate pooled streams (no cross-request head-of-line blocking, per-stream QUIC flow-control backpressure), sequential requests reuse a hot stream (no per-request open cost), client-known-streaming takes a dedicated off-pool stream, and response-driven streaming (SSE/long-poll) pins its pool slot bounded by the cap (§2). Backend↔app is a memory copy. This is **strictly better** than packet forwarding for HTTP/3: it keeps congestion control honest on every hop *and* gives you Aster identity/auth at the relay.

**Recommendation:** HTTP/3 (and HTTP/1.1, h2) → **L7 terminate-and-relay**. Reserve packet forwarding for non-HTTP UDP or hard E2E-QUIC requirements, shipped later, with the unreliable-datagram constraint baked in.

**Why there is no single-E2E-QUIC option here (name MASQUE so nobody reaches for it).** The technique that *does* preserve one end-to-end QUIC connection through a proxy is MASQUE CONNECT-UDP (RFC 9298) — but it only works as a **forward proxy**, where the client *explicitly opts in* to proxying. A reverse proxy is the opposite: the browser believes it's talking to the origin and will never opt into tunnelling, so the edge must terminate. Therefore two serial CC domains + one extra hop are **inherent** to this design — not a shortcoming to engineer away. We minimise the penalty (edge placement, BBR, warm/0-RTT — §3.4, Stage C) but we do not remove it. MASQUE is "the thing we'd use *if this were a forward proxy*"; it is unreachable for a reverse proxy.

### 3.4 WebTransport relay — latency & congestion control

WebTransport stresses terminate-and-relay hardest: a WT session multiplexes **two** transport semantics on one QUIC connection — reliable streams *and* unreliable datagrams — which relay over different Aster paths. The browser can't speak Aster, so the edge terminates the WT session and re-originates it, splitting into two independent relays:

```
browser ──WT/HTTP3──▶ edge ──┬─ WT streams   → Aster reliable streams (L7, §2)
                             └─ WT datagrams → Aster unreliable datagrams (§3.2)
                                    ──▶ backend re-injects both into the local WT handler
```

**Streams half.** Onto the pooled L7 streams. Each hop is native QUIC with its own CC — no nesting. Hops couple via *flow control*: a slow edge↔backend hop backpressures the browser, so the browser throttles to the slowest hop. Latency cost is one cut-through hop (forward as bytes arrive), placement-dominated, not an added steady-state RTT.

**Datagrams half.** Must ride Aster *unreliable* datagrams (`send_datagram`/`read_datagram`, `core/src/lib.rs:2039`) — never a reliable stream (that reintroduces retransmit + HoL blocking for data the app explicitly marked droppable; §3.1 on the worst possible payload). The CC behaviour is then correct by construction: per RFC 9221, QUIC DATAGRAM frames are congestion-controlled but **not** flow-controlled and **not** retransmitted — under congestion the sender drops rather than queues. For real-time payloads (game state, media, telemetry) drop-the-stale-frame is the right semantic.

**The one degradation vs native WT.** Relaying both halves over one Aster connection collapses streams + datagrams onto **one congestion window per hop** (datagrams share the CW even though they skip flow control). Native WT has this too — but the relay has it twice (per hop), and because iroh dedups to one connection per peer pair (§9), WT datagrams may share that CW with *unrelated Aster RPC traffic* to the same backend. The starvation surface is larger than native, not smaller. If datagram latency under load matters, that's the argument for a dedicated datagram-plane connection — which iroh may not cleanly provide. Open.

**Two hard constraints (when this is built):**
1. **The edge clamps the `maxDatagramSize` it advertises to the browser** to `max_datagram_size() (core/src/lib.rs:2076) − demux header`. WT-datagram + per-session demux prefix must fit one Aster datagram; fragmenting unreliable datagrams is loss-amplifying (lose one shard → lose the whole datagram).
2. **Bounded drop-queue at the relay for datagrams**, not an unbounded channel — tail-drop (or head-drop for freshness) when edge↔backend is momentarily slow, so datagrams don't bufferbloat. Streams get flow-control backpressure for free; datagrams need an explicit small ring + drop rule. Each Aster datagram carries a varint WT-session id so the backend re-injects into the right session.

Net: latency ≈ +1 cut-through hop; streams keep honest per-hop CC via flow-control coupling; datagrams stay correctly lossy. The only things categorically worse than native are the shared-CW contention (per-hop, possibly against unrelated RPC) and the clamped datagram size. **WT is Stage B of this doc (§7) — first-class, not a separate spec.** The §2 datapath (1:1 streams + flow-id-tagged datagrams) is shaped to carry it directly: WT bidi streams → Aster streams, WT datagrams → tagged Aster datagrams, established by an Extended CONNECT request-stream.

---

## 4. The API

One ergonomic entry point on the service side, mirroring the shape you sketched — accept *either* a socket (separate process, L4) *or* a `tower::Service` (same process, L7):

```rust
/// Companion crate `aster-expose` (rust facade) + per-binding mirror.

pub enum LocalHttpTarget {
    /// Separate process. L4 raw splice to a loopback HTTP server.
    /// Works for HTTP/1.1, h2c, and WebSocket upgrade transparently.
    Socket(SocketAddr),

    /// Same process. Zero loopback: each request arrives on its own Aster
    /// stream as a thin H3-object `[headers][body]` (§2, §5.3), is decoded
    /// to an `http::Request`, dispatched to this Service, and the response
    /// is encoded back on the same stream. No hyper, no HTTP/1.1 re-encode,
    /// no pool — one stream per request (§2).
    Service(BoxCloneService<Request<BoxBody>, Response<BoxBody>, Infallible>),
}

impl Node {
    /// Publish a local HTTP service to authorised Aster peers under
    /// `service_id`. `authorize` is your final say: it runs **once per
    /// (connection, service)** — when a peer first opens a stream for
    /// this service — with the *peer's* identity, and returns Ok(()) to
    /// admit that connection's subsequent streams (see §6.1). For an
    /// edge front-end (Case 2) the peer is the *edge*, not the browser.
    /// Returns a handle; drop or `.revoke()` to stop accepting.
    ///
    /// `authorize` is **async**: real policy is often a network call —
    /// JWT introspection, a policy-service round-trip, a replica lookup
    /// (the Layer-3 case `Aster-tunneling.md §11` envisions). A sync
    /// policy just returns a ready future.
    pub fn expose_local_http(
        &self,
        service_id: &str,
        target: LocalHttpTarget,
        authorize: impl Fn(&PeerIdentity) -> BoxFuture<'static, Result<()>>
            + Send + Sync + 'static,
    ) -> ExposeHandle;
}
```

Consumer side, Case 1 (a remote Aster node reaching the exposed service):

```rust
// Returns an http client bound to an Aster connection. The connection is
// admitted once for `service_id`; each request opens ONE Aster stream,
// writes the request object, reads the response object, done (§2) — no
// per-request RPC, no pool. Concurrency = concurrent streams, bounded by
// the per-(connection, service) cap (§6.1).
let client = node.dial_http("backend-endpoint-id", "service_id").await?;
let resp = client.get("/health").send().await?;
```

Consumer side, Case 2 (public edge — ngrok-shaped). Two planes:

```rust
// CONTROL PLANE: the backend registers its hostname with the edge and
// holds the Aster connection open (reconnect on drop). Default direction
// is BACKEND -> EDGE: the backend is typically NAT'd and the edge public,
// so only this direction connects without holepunch, and reconnect is the
// backend's job. (Edge -> NAT'd-backend via iroh holepunch is an opt-in,
// not the default.) The edge applies its OWN policy here — it owns the
// public cert/listener, so it decides whether this backend may claim
// `portal.example.com` (§6.2).
backend.register_with_edge("edge-endpoint-id", "portal.example.com").await?;

// DATA PLANE (on the edge): terminates browser HTTP/1.1/2/3 + WebTransport
// (ALPN routing), relaying each request as a thin H3-object over its own
// Aster stream (and WT datagrams as tagged Aster datagrams) to the
// registered backend. Resolves hostname -> backend via the registry.
let edge = HttpEdge::bind("0.0.0.0:443")
    .route("portal.example.com", /* resolved from registry */)
    .run().await?;
```

The data plane reuses `dial_http`'s machinery; the edge is a public listener wired to a warm, pre-authorised connection. `dial_http` and `expose_local_http(Service(..))` are the two ends of the same primitive.

---

## 5. What core needs (small, hyper-free)

Core stays dependency-clean — no hyper, no tower. The L7 spine is the **thin H3-object codec** (§5.3) reading frames off the Aster stream directly; the datagram demux (§5.4) is Stage B. `AsterStreamIo` (§5.1) is a supporting adapter, not the spine.

### 5.1 `AsterStreamIo` — duplex adapter (DONE; supporting role) ✅

An `AsyncRead + AsyncWrite` over a `(CoreSendStream, CoreRecvStream)` pair (`core/src/stream_io.rs`). It is *not* a loopback socket; pure tokio IO, buffers partial `CoreRecvStream::read` chunks for `poll_read`. Validated against real hyper (`aster-expose/tests/hyper_spike.rs`).

> **Demoted by the 2026-06-26 pivot (§2).** It is no longer the L7 request datapath. It serves: the **L4 splice** path, **streaming-body pumping**, and **WebSocket/CONNECT upgrades** — places where a raw `AsyncRead+AsyncWrite` byte pipe is genuinely wanted (e.g. running a body through `tokio::io::copy`, or handing the post-upgrade stream to a WS library). The L7 request/response path does *not* go through it — it reads length-delimited object frames via `read_one_frame` (§5.3).

### 5.2 Service-keyed local acceptor + session admission (new, core)

`core/src/tunnel.rs` currently hardcodes redeem → `TcpStream::connect` behind a one-shot ticket. The L7 path needs a *different admission shape* (§6.1): authorize a `(connection, service)` once, then admit many streams. Rather than overload the one-shot ticket registry, add a parallel node-level acceptor map keyed by `service_id`:

```rust
#[async_trait]
pub trait LocalTunnelAcceptor: Send + Sync {
    /// The connection's first stream for this service. Return Ok(()) to
    /// admit the connection (the `authorize` closure runs here); on Ok,
    /// core records (connection_id, service_id) as admitted.
    async fn authorize(&self, peer: &PeerIdentity) -> Result<()>;

    /// Hand a redeemed raw stream to the local handler. Called for the
    /// first and every subsequent stream once admitted. The companion crate
    /// decodes the H3-object off `recv` (§5.3), dispatches to the
    /// `tower::Service`, and encodes the response onto `send` — core stays
    /// hyper-free and never sees an HTTP type.
    async fn accept(&self, send: CoreSendStream, recv: CoreRecvStream);
}

impl CoreNode {
    pub fn register_local_target(&self, service_id: &str, acc: Arc<dyn LocalTunnelAcceptor>);
}
```

**Wire shape — a subtype byte, because `FLAG_TUNNEL` is now overloaded.** The frame header is `[4-byte LE len][1-byte flags][payload]` and *every* flag bit `0x01..0x80` is already assigned (`core/src/framing.rs:8-22`; `0x80` is `FLAG_TUNNEL`, the last free bit). There is no spare flag bit and "32 bytes ⇒ ticket, else service name" is a fragile heuristic (nothing stops a 32-byte `service_id`). So the first byte of every `FLAG_TUNNEL` payload becomes a subtype/version discriminator:

```
FLAG_TUNNEL payload:  [subtype: u8][...]
  subtype 0 = ticket-redeem   → [32-byte ticket]                       (L4, existing — migrate the shipped path to prefix 0)
  subtype 1 = http-relay       → [u16 len][service_id utf8][H3-object]  (L7, new — request object follows, §5.3)
```

This removes the ambiguity and versions the L7 frame for free. On accept the reactor reads the subtype: `0` → existing `handle_tunnel_redeem` ticket arm (L4); `1` → the service-keyed arm below.

> **Wire break, called out.** Prefixing subtype `0` changes the shipped L4 `FLAG_TUNNEL` payload format (today it's a bare 32-byte ticket). Old and new ends are not interoperable — both must upgrade (flag day; no negotiation planned). This is acceptable only because L4 tunneling shipped recently (v0.1.0), is pre-1.0, and is believed to have **no external consumers** — confirm that before landing. If consumers exist, gate behind a negotiated capability instead.

L7 service-open flow:

1. Look up the node acceptor map by `service_id` (unknown → reset).
2. If `(connection_id, service_id)` is already admitted → check the per-service concurrent-stream cap (below) and `accept(send, recv)`.
3. Otherwise run admission **single-flight**: the first stream for an unadmitted `(connection_id, service_id)` runs `acc.authorize(peer)`; concurrent first-streams await that same in-flight result rather than each firing their own `authorize()`. This matters because `authorize` is now async (§4) and often a policy/JWT round-trip — bursty edge startup opens many streams at once, and without single-flight every one would trigger a separate round-trip. On `Ok`, record admitted + proceed; on `Err`, reset all waiters.

**Bounded concurrency — the abuse cap that the one-shot model used to provide.** Dropping per-request tickets also dropped the one-shot model's per-connection ceiling (`max_outstanding`, default 64). Session admission must replace it or an admitted-but-misbehaving peer — *or a compromised edge, which §6.2 fully trusts* — can fan out unbounded object-relay tasks (one Aster stream each) as cheap DoS. Two layers:

- **Per-(connection, service) concurrent-stream cap** (§2). Request-streams beyond it for that admission are refused with a typed `PeerStreamLimitReached`, not silently accepted. Edge-shaped peers get a raised cap; ordinary peers stay low. App-level accounting in the acceptor.
- **Connection-wide ceiling** rides QUIC's own `transport_max_concurrent_bidi_streams` (`core/src/lib.rs:107`) — a backstop across *all* services on the connection. (Connection-wide, so it cannot by itself give per-service fairness — hence the per-service cap above.)

Connection close drops the admitted set + aborts all in-flight streams (mass revoke), exactly like the ticket registry.

### 5.3 Thin H3-object codec (companion: `aster-expose`)

The L7 spine. A minimal, **HTTP-version-agnostic** wire object — method, target, headers, body — that both legs encode/decode over the Aster stream's existing length-delimited framing (`read_one_frame`/`encode_frame`). It is *not* HTTP/1.1 (no parser, no chunked grammar) and *not* H3+QPACK (no dynamic table). Sketch:

```
request:  [u8 method][u16 path-len][path][u16 hdr-count][(u16 nlen)(name)(u16 vlen)(value)]*[u8 priority?] then body frames .. finish()
response: [u16 status][u16 hdr-count][headers..] then body frames .. finish()
```

**Why version-agnostic.** HTTP/1.1, HTTP/2, and HTTP/3 requests all decompose to the same `(method, target, headers, body)` — only their *wire encoding* and *multiplexing* differ, and both are terminated at the edge. One object carries all three. The edge maps each H2/H3 request-stream → one Aster stream (1:1, §2), so H2's multiplexing is preserved, not collapsed. Lives in the companion (it touches `http::Request`/`Response` types, which it already pulls via hyper); core stays http-free.

### 5.4 Datagram demux (core) — Stage B

`[flow-id varint][payload]` over the *already-exposed* datagram API (`CoreConnection::send_datagram`/`read_datagram`, `core/src/lib.rs:2039`; `max_datagram_size()` at `:2076`). A per-connection `flow-id → session` map routes inbound datagrams to the right WT session; a bounded drop-queue (Appendix A) sheds load instead of buffering; the edge clamps the browser-advertised `maxDatagramSize` to fit one Aster datagram minus the flow-id header (§3.4). Byte-level and reusable, so it lives in core (no http types).

---

## 6. Security — final say, two authorization shapes

`Aster-tunneling.md`'s Layer-0 guarantee is unchanged: the peer is QUIC + Aster-identity authenticated before any of this runs. On top of that:

### 6.1 L4 (ticket) vs L7 (session cap)

These are deliberately different and the difference is the crux of the design:

- **L4 — one-shot ticket per tunnel (existing, unchanged).** `authorize_tunnel` mints a 32-byte CSPRNG ticket; redeem pops it; tickets stay opaque, connection-bound, TTL'd. Right when each tunnel is a distinct authorized resource.
- **L7 — session capability per `(connection, service)` (new).** `expose_local_http(..., authorize)` runs *once*, on the connection's first stream for that service. On `Ok` the connection is admitted and its subsequent streams flow without re-authorizing. This is a deliberate divergence from one-shot tickets: per-request authorization would force a `Broker.open` RPC before every HTTP request. Security is preserved because (a) the connection is already Aster-authenticated, (b) connection-close mass-revokes the admitted set, (c) replay *within an already-admitted, authenticated connection* is not a threat — the peer is, by definition, already in.

**Admission ≠ unbounded.** The session cap admits a *connection*, not unlimited work: each admitted `(connection, service)` is still bounded by the per-service stream/pool cap from §5.2 (replacing the one-shot model's `max_outstanding=64`). Without it, admission would be a DoS amplifier — most acute for the edge, which §6.2 fully trusts and which legitimately needs a *high* cap. So the cap is configurable by peer shape, not a low global constant.

`ExposeHandle::revoke()` stops new admissions in both modes. A peer never learns whether the door is a TCP socket or an in-process Service.

**`service_id` enumeration (accepted).** Unknown-service resets immediately while known-but-unauthorized runs `authorize()`; the timing difference leaks which `service_id`s exist. Accepted: the prober is already an authenticated Aster peer and `service_id`s are not secrets. Deployments wanting constant-time can run `authorize()` uniformly before the existence check.

### 6.2 Case 2 — "final say" is over the edge, not the browser

In the ngrok shape the `authorize` closure on the backend sees the **edge's** `PeerIdentity`, not any end user's. So the backend's final say is *"which edge may front me"* — not per-request admission of browser traffic. That is the correct ngrok trust model (you trust your edge; the edge is the relay), but it must be explicit: end-user authn/authz, if needed, is the backend *application's* job over the relayed HTTP, not this layer's.

There is a **second, independent authorization** on the edge side: the edge owns the public listener and TLS cert, so `register_with_edge(hostname)` is subject to the *edge's* policy — it decides whether a given backend may claim `portal.example.com`. Two parties, two consents: backend consents to the edge; edge consents to the backend's hostname claim.

### 6.3 TLS / headers

- **Edge terminates TLS** facing the browser; the Aster hop is already QUIC-encrypted, so no double-TLS to the backend. The backend's local service sees cleartext over the in-memory duplex (same process) or loopback (separate process).
- **No end-to-end secrecy through the edge.** Because the edge terminates browser TLS, it can read and modify *all* relayed content — browser↔backend is **not** E2E encrypted; the Aster hop protects edge↔backend only. This is inherent to terminate-and-relay (the same property a CDN has). Do not assume the mesh hop confers E2E secrecy. True E2E-past-the-relay is the future work flagged in `Aster-tunneling.md §11` (blind-forwarder mode) and is out of scope here.
- **Header rewriting (Host/Origin):** L7 mode owns the `Request` and can rewrite before dispatch — covers the DevTools/strict-origin case that raw L4 splice cannot. Make `Host`/`Origin` rewrite a field on the edge config, off by default.

---

## 7. Build order (staged — H3/WT-native)

**Stage A — request/response object relay (kills the pool + the HTTP/1.1 re-encode).**
1. **`AsterStreamIo` duplex adapter** (core) — **DONE** (`core/src/stream_io.rs`, 3 unit tests; hyper round-trip + keep-alive + 256 KiB body proven in `aster-expose/tests/hyper_spike.rs`). Retained for L4 splice, body pumping, and WS/CONNECT upgrades — *not* the L7 spine (§2).
2. **Thin H3-object codec** (companion `aster-expose`, §5.3) — **DONE** (`aster-expose/src/codec.rs`, 6 unit tests: req/resp round-trip incl. multi-value headers + obs-text, truncation/trailing/bad-version errors). HTTP-version-agnostic head encode/decode; no networking.
3. **Service-keyed acceptor + session admission** (core, §5.2) — **DONE** (`core/src/tunnel.rs`: `LocalTunnelAcceptor`/`LocalTargetRegistry`/`AdmissionState`/`handle_http_relay`; subtype dispatch in `accept_aster_bi`, subtype-0 prefix in `open_tunnel`). `FLAG_TUNNEL` subtype byte (`0` = L4 ticket — flag-day migrated, `1` = http-relay carrying `service_id`); admit `(connection, service)` once, single-flight (`OnceCell`); per-service concurrent-stream cap. 6 admission unit tests + the L4 `tunnel_contract` integration (7) as the flag-day regression guard; full core lib suite (221) green.
4. **`expose_local_http(Service(..))`** — **DONE** (`aster-expose/src/relay.rs`: `serve_request` + `ServiceAcceptor` over `AsterStreamIo`; `expose_http_on_connection`). Decode request object → handler → encode response; one stream per request, zero loopback. **Bodies stream end-to-end** (`aster-expose/src/body.rs`: `RelayBody` reads chunks off the recv side until clean EOF; `pump_body` writes the response body frame-by-frame; `tower_handler` adapts any `tower::Service` — Salvo is tower-compatible). No buffering, so SSE / chunked / long-lived h2 pass through. *gRPC trailers are V2 (§9): `[body…until finish]` carries no trailers frame.*
5. **`dial_http` consumer** — **DONE** (`relay_request` + core `CoreConnection::open_http_relay`). One Aster stream per request, write request object, read response. No pool. **Proven end-to-end** by `aster-expose/tests/relay_integration.rs` (4 tests over a real QUIC connection: round-trip, admission reuse across 3 requests, unknown-service drop, denied-authorize drop).
6. **`expose_local_http(Socket(..))`** — **DONE** (`aster-expose/src/relay.rs`: `SocketAcceptor` + unified `LocalHttpTarget`/`expose_local_http`). Raw L4 splice to a loopback TCP server via the **service-keyed admission path** (not the ticket path — A3 superseded it), reusing core's `splice_with_failover`; protocol-blind. Integration test `socket_mode_raw_splice` ✅.

**✅ Stage A complete.** The QUIC-native datapath works end-to-end: non-UDP HTTP (L7 object relay) + raw TCP (L4 splice), both behind one `expose_local_http` facade with shared session admission. 15 `aster-expose` tests + core suites green.

**Stage B — datagrams + WebTransport (makes WT actually work).**
7. **Datagram demux** (core, §5.4) — **DONE** (`core/src/datagram.rs`: `DatagramRouter` + `DatagramTransport` seam, LEB128 varint, per-flow bounded drop-queue via `try_send`, send-time max-size clamp, `dropped()` counter). `[flow-id varint][payload]` over the exposed `send_datagram`/`read_datagram`. 4 unit tests (mock transport) + `datagram_demux_round_trip` integration over real QUIC.
8. **WT session relay** — *depends on the Stage-C edge WT-termination stack.* Extended CONNECT request-stream establishes a session; WT bidi streams → Aster streams, WT datagrams → tagged Aster datagrams (router from step 7). Edge clamps advertised `maxDatagramSize` (§3.4). Build alongside `HttpEdge` (step 10) since it needs `web-transport-quinn` to terminate browser WT.

**Stage C — edge.** Scoped as **cut 1 (H1/H2) now, H3/WT as V2** (see below). The relay datapath (steps 4–5, now streaming) is HTTP-version-agnostic, so the entire H1/H2 datapath already exists; cut 1 only adds the public listener + control plane.

- Streaming relay bodies (step 4) — **DONE** as the cut-1 foundation: `RelayBody` + `pump_body` + `tower_handler` (`aster-expose/src/body.rs`).
9. **Case 2 control plane** — **DONE** (`aster-expose/src/control.rs`, Salvo-free). Modelled as an Aster service (not the RPC framework): the edge exposes a reserved service `__aster_route_control__`; an origin opens an http-relay stream to it (`request_route`) and writes a length-framed registration. So it inherits the `authorize`/admission/cap machinery for free.
   - **Granular routes.** `RouteSpec { host, port, protocol: {Http|Tcp}, service_id }` (H3/WebTransport are reserved protocol variants → rejected until V2). Registration = `Vec<RouteSpec>` + metadata blob. Policy `Fn(PeerContext, Vec<RouteSpec>) -> Result<Vec<RouteSpec>>` grants a **subset** (or errs → none). `EdgeRouter` is keyed `(host, port)` with a default-port (`0`) fallback; eviction on control-stream close.
   - **Metadata-carrying admission.** Core seam upgraded (flag-day): `LocalTunnelAcceptor::authorize(PeerContext { peer_id, metadata })` + `accept(conn, send, recv)`; the relay open-frame carries an optional `[u16 mlen][metadata]` blob (`open_http_relay(service_id, metadata)`). Admission policy is no longer limited to node id. Registration metadata rides the structured request body; the open-frame blob serves the relay direction (a consumer presenting a token to reach a service).
   - **P2P-symmetric.** The QUIC hop is symmetric, so the same machinery runs in either direction: `request_route` (ask a peer to route inbound traffic to me) and the existing relay path (`relay_request`/`open_http_relay` — reach a peer's internal service, now metadata-gated) are the two directions; any node can do either on one connection. `serve_routes_on_connection` makes a node act as the edge.
   - **Rejection model:** metadata-aware `authorize` + route policy (admission) + Salvo hoops in front of `RelayHandler` (per-request: rate-limit/jwt/cors/… from the fork) + the origin's independent `AuthorizeFn` veto. Tested: `tests/control_integration.rs` (register→populate→evict; subset grant + metadata enforcement over real QUIC). *Remaining: node-level (`set_local_targets`) instead of connection-scoped; warm-connection/reconnect management.*
10. **`HttpEdge`** (Case 2 data plane, **cut 1 = H1/H2**) — public listener built on the **Salvo fork** (`/Users/emrul/dev/aster/salvo`, already a workspace dependency for the web transport). Salvo is hyper-based (not a second HTTP stack) and bundles `rustls`/`acme` listeners, so **TLS termination + Let's Encrypt come for free** (collapses the Appendix-A "lift ACME" item).
   - **Data-plane handler — DONE** (`aster-expose/src/edge.rs`, behind the `edge` cargo feature): `RelayHandler` impls Salvo's `Handler`; per request it resolves the hostname (`:authority` then `Host`), looks it up in `EdgeRouter` (`(host, port) → {CoreConnection, service_id, protocol}`), rebuilds the request dropping hop-by-hop headers (with optional Host/Origin rewrite), calls `relay_request_streaming`, and streams the origin response straight back via `ResBody::stream(body.into_data_stream())` — no buffering. 404 unknown host, 502 relay error / non-HTTP route.
   - **Listener + TLS — DONE.** `serve_edge(router, EdgeConfig)` binds per `EdgeTls`: `None` (plaintext `TcpListener`, dev), `Static { cert_pem, key_pem }` (Salvo `RustlsConfig`/`Keycert`), or `Acme { domains, cache_path, staging }` (Let's Encrypt HTTP-01 via the fork's `AcmeListener`, joining `:80` for the challenge; challenge routes are mounted before the catch-all). `EdgeConfig` carries `route_port` (for `(host, port)` matching) + `rewrite_host`. **TLS termination + ACME come from the fork — no hand-rolling** (collapses the Appendix-A "lift ACME" item).
   - **Middleware — DONE.** `EdgeConfig::hoop(H: Handler)` mounts any Salvo middleware (the fork's rate-limiter / jwt-auth / cors / … or a custom hoop) in front of the relay, scoped to the proxy route so ACME challenges stay open. This is the per-request rejection layer.
   - **Tests:** 3 unit (`forward_headers` rewrite/hop-by-hop, host-parse, router miss) + `tests/edge_integration.rs` (4: real-QUIC relay via Salvo `TestClient`; unknown-host 404; **`serve_edge_plaintext_real_socket` — a raw HTTP/1.1 request over a real TCP socket → Salvo H1 → QUIC relay → origin echo**; `serve_edge_hoop_blocks_before_relay`).

**Polish — DONE.**
- **Node-level facade** (`aster-expose/src/node.rs`): `ExposeNode` owns one shared `LocalTargetRegistry`; `expose_http`/`expose_socket`/`serve_routes` register node-wide, `attach(&mut conn)` (→ core `set_local_targets`) makes them reachable on every connection. Replaces the connection-scoped wiring. `tests/node_integration.rs`.
- **Reconnect** documented as a pattern (`conn.closed().await` → re-dial → re-`request_route`) in the getting-started guide, not a fragile core helper.

**Rust-consumer gate (§8) — DONE (the polish brought it forward).** `aster-expose/examples/reverse_proxy.rs` wires origin + edge end-to-end in one process (`ExposeNode` both sides, `request_route`, `serve_edge` plaintext), drives one real HTTP request through the edge, asserts it reaches the origin, exits 0. Doubles as the runnable demo. User-facing guide: `docs/aster-expose-getstarted.md`.

**V2 (deferred, all land on the *same* Salvo stack):**
- **H3 + WebTransport at the edge** — the fork already carries `quinn` + `salvo-http3` + `h3-datagram` listeners, so V2 is a *listener swap* (`TcpListener` → `QuinnListener`, joined) + **Stage B step 8** (WT session relay; the datagram demux, step 7, is already built). No second QUIC stack is ever introduced. Edge clamps advertised `maxDatagramSize` (§3.4). RFC 9218 priority pass-through + BBR + 0-RTT belong here.
- **gRPC trailers** — a body-framing bump (a `[trailer-head]` frame after the body) so `grpc-status` survives the relay. Until then, plain request/response, SSE, chunked, and long-lived h2 work; gRPC does not.

**Then:**
11. **Rust-consumer verification gate (§8) — DONE** via `aster-expose/examples/reverse_proxy.rs` (origin + edge end-to-end, real HTTP request asserted). Guide: `docs/aster-expose-getstarted.md`.
12. **FFI surface + per-binding propagation (§8)** — record the C FFI in `ffi_spec/FFI_API_SURFACE.md` (path classification + coverage matrix), then mirror to Python/TypeScript/Java/Kotlin. *(Next major step.)*

Each stage is independently shippable. Stage A is the MVP that proves the QUIC-native datapath; Stage B is what distinguishes this from a plain HTTP proxy.

---

## 8. Rust-first, then every binding

This surface is Rust-core logic with thin bindings on top (the standing rule in this repo). Sequencing:

1. **Verify against a Rust consumer first.** The Rust facade (`aster-expose` + `dial_http`/`HttpEdge`) is the reference. Everything in the build order is exercised end-to-end from Rust — including the async `authorize` round-trip, single-flight admission under a concurrent first-stream burst, the per-service stream cap refusing overflow, and a response-driven-streaming (SSE) request holding a pooled slot. Nothing proceeds to bindings until Rust is green.

2. **Then propagate to every foreign-language interface.** Because business logic lives in `core` and the wire/admission/cap behavior is enforced there, bindings are mechanical mirrors — but the surface still has to *appear* in each one. The FFI work must be tracked, not assumed:

   - Add the new C FFI functions (e.g. `aster_expose_local_http`, `aster_dial_http`, pool/cap config, the async `authorize` upcall, `register_with_edge`) to `ffi_spec/FFI_API_SURFACE.md`: classify each on the hot/warm/cold path and add a row to the **Binding Coverage Matrix** so per-language status is visible.
   - **Mark this surface as requiring application to each foreign-language interface** — Python, TypeScript, Java, Kotlin — none are exempt. The async `authorize` callback in particular crosses the FFI as an upcall and needs per-binding attention (each runtime's async↔FFI bridge differs).
   - Mirror per binding, keeping names aligned with the Rust facade, and tick the matrix as each lands.

   > The cap, single-flight, subtype-byte framing, and admission live in `core`, so bindings inherit the security/perf properties for free — but the *callbacks* (`authorize`, the `tower::Service`/`Socket` target, hostname registration) are binding-surface and must be re-expressed idiomatically in each language.

---

## 9. Open questions

- **Datagram exposure — RESOLVED.** Core already surfaces unreliable QUIC datagrams (`CoreConnection::send_datagram`/`read_datagram`, `core/src/lib.rs:2039`, with send/receive buffer config). The remaining work for the §3.2 packet path is per-tunnel demux framing + PMTU/fragmentation handling, *not* an iroh capability gap. Confirm the configured max datagram size when that follow-up is scoped.
- **Streaming bodies / SSE / chunked — RESOLVED.** `RelayBody` streams chunks off the recv side until clean EOF; `pump_body` writes response frames one at a time. Backpressure composes through the Aster stream's flow control (the `write_all` awaits; the reader polls). Covered by `body.rs` unit tests + the `large_body_streams_without_buffering_cap` relay test. gRPC trailers remain V2 (no trailers frame on the wire yet).
- **`tower::Service` adapter — RESOLVED.** `relay::tower_handler<S>` wraps any `tower::Service<Request<RelayBody>>` into the `HttpHandler` (boxing its response body into `ResponseBody`). Salvo is tower-compatible, so the edge handler and any in-process service drop in without users hand-rolling an adapter.
- **Edge connection warmth policy.** How aggressively does the edge keep backend connections warm vs. dial-on-demand (holepunch latency on first hit)? Per-route config; defaults TBD after measuring iroh cold-dial latency.
- **Shared congestion control per connection.** All of an edge↔backend's pooled streams ride one QUIC connection, so they share one congestion controller and one connection-level flow-control window — a slow *reader* can pin connection credit and a single CC bounds aggregate throughput. This is inherent to QUIC multiplexing (HTTP/3 has the identical property over one connection) and is *acceptable* for v1. Whether to pool **multiple connections** per backend to isolate CC is iroh-dependent (iroh tends to dedup connections per peer pair, so this may not be cleanly available) and an optimization to revisit only if profiling shows a single connection ceilings out. Distinct backends already get distinct connections.
- **WebTransport relay demand.** Real now, or deferred? If users need remote browsers to reach a *WebTransport* origin through Aster, that's the §3.4 datagram-bearing variant and wants its own doc.

---

## Appendix A — Prior art: what to lift from iroh-relay

Reviewed `/Users/emrul/dev/aster/iroh/iroh-relay`. **Architectural verdict: not a structural template.** iroh-relay is a *symmetric P2P packet-rendezvous* relay — it forwards opaque framed datagrams between peers with no request/response semantics, no stream splicing, and no client/server asymmetry. Our edge is an *application-layer HTTP/WT reverse proxy* with backend→edge initiation, stateful routing, and stream reuse. Different shape. But it has several self-contained primitives worth lifting (these are *internal* server modules, so it's copy-and-adapt, not a crate dependency).

| Pattern | iroh-relay location | For us | Verdict |
|---|---|---|---|
| **Bounded drop-queue** — `mpsc::channel(512)` + `try_send` + drop-on-`Full` + drop metric (`PER_CLIENT_SEND_QUEUE_DEPTH`) | `src/server/clients.rs:200-234`, `src/server/client.rs:128-129`, `src/protos/relay.rs:40` | **the §3.4 WT-datagram relay queue, exactly** — bounded ring, non-blocking send, count drops | **LIFT** |
| **Token-bucket rate limiter** — `Bucket` / `RateLimited`, read-path, 100 ms refill, burst cap | `src/server/streams.rs:349-445` | a *byte-rate* complement to the per-service *stream-count* cap (§5.2/§6.1); wire on the `AsterStreamIo` read path | **LIFT** |
| **TLS + ACME/Let's-Encrypt setup** | `src/server.rs:712-833` | the edge needs public certs facing browsers — ACME is directly useful for `HttpEdge` | **LIFT** |
| **RTT-adaptive keep-alive / liveness** — `PingTracker` (3× measured RTT, 5 s floor) | `src/ping_tracker.rs:86-105` | warm-connection health for the §6 control plane | **REFERENCE** |
| **AccessControl hook** | `src/server.rs:279-335` | shape reference for the `authorize` admission hook | **REFERENCE** |
| **hyper `Service` + per-conn task scaffolding** | `src/server/http_server.rs:442-520, 723-790` | `HttpEdge` listener skeleton — but see caveat below | **REFERENCE** |
| **Metrics module** | `src/server/metrics.rs` | observability pattern | **REFERENCE** |

**Two things iroh-relay does *not* give us:**

- **No byte-splice/pump helpers** — it's datagram-framed and never splices streams. Our `AsterStreamIo` + the pumps are ours to build; the closer in-repo prior art is the existing `core/src/tunnel.rs` `pump_*` functions (the L4 splice), not anything in iroh-relay.
- **No client reconnect/backoff** — iroh-relay's keep-alive detects death but never reconnects (P2P peers re-dial independently). The backend-side warm-connection reconnect (build-order step 6) is entirely ours.

**Two corrections to flag before anyone copies blindly:**

- **Protocol negotiation: use ALPN, not `Sec-WebSocket-Protocol`.** iroh-relay multiplexes one protocol (HTTP/1.1 + WS) on one port and version-negotiates via the WebSocket subprotocol header (`src/http.rs:50-101`). `HttpEdge` must terminate HTTP/1.1, HTTP/2, **HTTP/3**, and WebTransport — that's ALPN-based routing across two stacks (hyper for H1/H2; the quinn-based `h3` + `h3-webtransport` / `web-transport-quinn` stack for H3/WT), not subprotocol headers. The iroh-relay HTTP scaffolding is a reference for the H1 leg only.
- **Demux framing is ours.** iroh-relay routes datagrams by an endpoint id carried in the proto message; we need the §5.2 subtype byte (L4 ticket vs L7 service-open) plus the §3.4 varint WT-session id. Don't reuse its frame format.

Net: mine iroh-relay for the **drop-queue, token bucket, and ACME** (lift), and the keep-alive/access-control/metrics/hyper-listener shapes (reference). Ignore its architecture, its datagram framing, and its protocol negotiation.
