# Exposing local HTTP services over Aster (`aster-expose`)

`aster-expose` tunnels HTTP services over Aster's peer-to-peer QUIC transport.
It covers two shapes:

1. **Expose a local service** so a remote Aster peer can reach it — without
   opening a port to the internet.
2. **Run an ngrok-style edge** — a public HTTPS endpoint that reverse-proxies
   browser traffic through Aster to a service running behind NAT.

Both ride one QUIC connection and are **symmetric**: any node can expose
services *and* act as an edge on the same connection. The data path is a thin
HTTP-object relay (one request → one QUIC stream, **streaming** bodies, no
buffering), so SSE, chunked transfer, and long-lived HTTP/2 all pass through.

> The public edge is built on the [Salvo](https://salvo.rs) fork — it terminates
> browser TLS (HTTP/1.1 and HTTP/2) and brings rustls + Let's Encrypt listeners.
> HTTP/3 + WebTransport and gRPC trailers are **V2** (see the end of this guide).

A complete, runnable version of everything below is in
[`aster-expose/examples/reverse_proxy.rs`](../aster-expose/examples/reverse_proxy.rs):

```bash
cargo run -p aster-expose --example reverse_proxy --features edge
```

---

## 1. Add the dependency

```toml
[dependencies]
aster-expose = { version = "0.3", registry = "aster" }
# The public edge (Salvo + TLS/ACME) is behind a feature; origin-only
# consumers leave it off and don't pull in Salvo:
# aster-expose = { ..., features = ["edge"] }
```

Configure the public registry and select versions as described in the
[Rust SDK consumer guide](rust-sdk-consumer-guide.md). With `edge`, the exact
Salvo fork is available as `aster_expose::salvo`.

| You want to… | Need `edge` feature? |
|---|---|
| Expose a local service to a peer | no |
| Register routes with an edge (`request_route`) | no |
| Accept registrations + run a public HTTPS listener | **yes** |

---

## 2. Expose a local HTTP service

An exposed service is identified by a string `service_id` and gated by an
**admission policy**. Register it node-wide with [`ExposeNode`], then `attach`
the node to each connection before feeding it to the reactor.

```rust
use std::sync::Arc;
use aster_expose::node::ExposeNode;
use aster_expose::relay::{AuthorizeFn, HttpHandler};
use aster_transport_core::stream_io::BoxFut;
use aster_transport_core::tunnel::PeerContext;

// Admission policy: runs once per (connection, service). The PeerContext
// carries the verified peer id AND an opaque metadata blob the caller
// attached (a token / capability) — so policy isn't limited to node id.
fn authorize() -> AuthorizeFn {
    Arc::new(|ctx: PeerContext| Box::pin(async move {
        if ctx.metadata == b"secret-token" { Ok(()) }
        else { anyhow::bail!("peer {} not allowed", ctx.peer_id) }
    }) as BoxFut<anyhow::Result<()>>)
}

let node = ExposeNode::new();
node.expose_http("web", authorize(), my_handler());      // in-process handler
// node.expose_socket("db", authorize(), "127.0.0.1:5432".parse()?); // raw L4 splice
```

`my_handler()` is an [`HttpHandler`] — a streaming `tower::Service`-shaped
closure. Wrap any `tower::Service` with [`relay::tower_handler`], or write the
closure directly (see the example).

### Attributing a request to its authenticated peer

The `AuthorizeFn` above sees the peer once, at admission. A **handler** that needs
the peer identity per request — for a per-peer quota, an audit record, or
routing — reads it from the request's extensions:

```rust
use aster_expose::relay::AuthenticatedPeer;

// inside your HttpHandler:
let peer = req.extensions().get::<AuthenticatedPeer>();
match peer {
    Some(p) => { /* attribute to p.as_str() — the verified remote node id */ }
    None    => { /* fail closed: no relay-inserted identity → reject */ }
}
```

`AuthenticatedPeer` is inserted by the relay **after** decoding the request head
and **before** dispatch, straight from the verified connection
(`conn.remote_id()`). Its constructor is private to `aster-expose` and HTTP input
cannot populate or override extensions, so the value is unforgeable — a handler
can trust it for attribution and should treat its absence as a hard failure.

Then run the node's accept loop, attaching the registry to each connection:

```rust
use aster_transport_core::reactor::create_reactor;

let (_reactor, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
tokio::spawn(async move {
    while let Ok(mut conn) = endpoint.accept().await {
        node.attach(&mut conn);   // node-wide services become reachable on conn
        feeder.feed(conn);
    }
});
```

### From a high-level `aster::Node`

The snippet above is the core layer (`CoreConnection` + a hand-rolled reactor).
If you already run an `aster::Node`, the `expose` feature folds all of it onto
the node — mirroring how `AsterServer::with_http` folds RPC on:

```toml
aster = { git = "...", features = ["expose"] }   # add "expose-edge" for the edge
```

```rust
const ALPN: &[u8] = b"aster-expose/demo";

let node = Node::start_with_alpns(cfg, vec![ALPN.to_vec()]).await?;
let expose = node.expose();
expose.expose_http("signaling", authorize(), my_handler());

// Dedicated host: let the node own the accept loop (attach + feed per conn).
node.serve_expose(&expose, &[ALPN]).await?;
```

A node has **one** inbound accept loop (it drains a single connection queue), so
don't run `serve_expose` *and* RPC *and* a manual `accept()` on the same node —
they'd compete and connections would land in the wrong consumer. Instead, give
the one accept owner an ALPN fan-out.

**RPC + expose on one node** — the common case (e.g. a host serving an RPC
consent gate plus a signaling relay) — is a one-call on `AsterServer`:

```rust
use aster::expose::Expose;
use aster::rpc::AsterServer;

let expose = Expose::new();
expose.expose_http("signaling", authorize(), my_handler());

let srv = AsterServer::builder()
    .service(my_rpc_service)
    .with_expose(b"portal/signaling/1".to_vec(), expose.clone())   // expose on its ALPN
    .route_alpn(b"portal/media/1".to_vec(), |conn| { /* spawn your handler */ })  // any other protocol
    .start()
    .await?;
```

The RPC reactor is the single accept owner: it routes `aster/1` to RPC and hands
every other ALPN to the handler you registered (`with_expose` attaches + feeds;
`route_alpn` gives you the raw `Connection`). The ALPNs are registered on the
node for you.

If you run your **own** accept loop instead (no `AsterServer`), route connections
into the expose handle yourself:

```rust
let (alpn, conn) = node.accept().await?;
if alpn == ALPN {
    expose.handle(&conn);   // attach + feed this connection
}
```

The consumer side needs no `Expose` handle — `aster::expose::relay_request`,
`relay_request_streaming`, and `request_route` take an `aster::Connection`
directly. All the types below (`AuthorizeFn`, `HttpHandler`, `RouteSpec`, …) are
re-exported from `aster::expose`, so an Aster consumer never adds `aster-expose`
or `http` itself.

### Reaching an exposed service from a peer

The other side dials and relays a request over one stream:

```rust
use aster_expose::relay::relay_request;

let resp = relay_request(
    &conn, "web",
    http::Request::builder().uri("/hello").body(b"ping".to_vec())?,
).await?;
```

`relay_request` is the buffered convenience; [`relay::relay_request_streaming`]
returns a streaming response body for large/long-lived responses.

---

## 3. Register routes with an edge

To have a public edge forward inbound traffic to you, ask it with
[`request_route`]. A route is granular — `host` / `port` / `protocol` /
`service_id`:

```rust
use aster_expose::control::{request_route, RouteProtocol, RouteSpec};

let reg = request_route(
    &edge_conn,
    vec![RouteSpec {
        host: "app.example.com".into(),
        port: 0,                       // 0 = the edge listener's default port
        protocol: RouteProtocol::Http, // Http (H1/H2/WS) | Tcp (raw splice)
        service_id: "web".into(),      // what you exposed in §2
    }],
    b"demo-token".to_vec(),            // metadata for the edge's policy
)
.await?;

println!("granted: {:?}", reg.granted()); // the edge may grant a subset
```

Keep `reg` alive — the routes are bound for as long as the control stream is
open. Dropping it (or `reg.close().await`) evicts them at the edge.

> The connection you call `request_route` on must also be fed to **your** reactor
> (and `attach`ed), so the edge's relay streams reach your exposed service.

---

## 4. Run a public edge

With the `edge` feature, a node accepts registrations and serves public HTTP.

```rust
use aster_expose::control::{EdgeRouter, RoutePolicy, RouteSpec};
use aster_expose::edge::{serve_edge, EdgeConfig, EdgeTls};
use aster_expose::node::ExposeNode;
use aster_transport_core::tunnel::PeerContext;
use std::sync::Arc;

// Route policy: decide which routes a peer may bind. Return the granted subset
// (or Err to reject all). Keyed on the verified peer id + metadata.
let policy: RoutePolicy = Arc::new(|ctx: PeerContext, specs: Vec<RouteSpec>| {
    Box::pin(async move {
        if ctx.metadata != b"demo-token" { anyhow::bail!("bad token"); }
        Ok(specs.into_iter().filter(|s| s.host.ends_with(".example.com")).collect())
    })
});

let router = EdgeRouter::new();
let edge = ExposeNode::new();
edge.serve_routes(router.clone(), policy);   // accept registrations node-wide
// … accept loop with `edge.attach(&mut conn)` as in §2 …

// Public HTTPS listener:
serve_edge(router, EdgeConfig::new("0.0.0.0:443").tls(EdgeTls::Acme {
    domains: vec!["app.example.com".into()],
    cache_path: "/var/cache/acme".into(),
    staging: false,
})).await;
```

### TLS modes

```rust
EdgeTls::None                                  // plaintext (dev / behind a terminator)
EdgeTls::Static { cert_pem, key_pem }          // your own certificate
EdgeTls::Acme { domains, cache_path, staging } // Let's Encrypt (binds :80 for the challenge)
```

### Per-request rejection — Salvo middleware

The edge *is* a Salvo router, so mount any `Handler` (the fork ships
rate-limiter, jwt-auth, cors, …) or a custom hoop in front of the relay. Hoops
are scoped to the proxy route, so ACME challenges stay open:

```rust
EdgeConfig::new("0.0.0.0:443")
    .hoop(MyRateLimiter::new())
    .hoop(MyAuthCheck::new())
    .tls(EdgeTls::Static { cert_pem, key_pem })
```

### Host / Origin rewrite

For strict-origin backends, rewrite the outbound `Host`/`Origin` before
relaying (off by default):

```rust
EdgeConfig::new("0.0.0.0:443").rewrite_host("backend.internal")
```

---

## 5. The three admission layers

Rejection is defense-in-depth, all operator-controlled:

| Layer | Where | Hook |
|---|---|---|
| **Registration** | edge | `RoutePolicy` — which `(peer, metadata)` may bind which routes |
| **Per-request** | edge | Salvo hoops (rate-limit / auth / CORS / …) |
| **Service admission** | origin | `AuthorizeFn` — the origin's independent veto, even of a permissive edge |

The origin's `AuthorizeFn` is the **final say**: an over-permissive edge can't
force the origin to serve.

---

## 6. Staying registered (reconnect)

A registration lives as long as its connection. Re-dial and re-register on
disconnect:

```rust
loop {
    let mut conn = origin_net.connect(edge_id.clone(), ALPN.to_vec()).await?;
    node.attach(&mut conn);
    feeder.feed(conn.clone());

    let reg = request_route(&conn, specs.clone(), token.clone()).await?;
    conn.closed().await;   // block until the connection drops
    drop(reg);             // routes evicted; loop re-dials
}
```

---

## 7. What's V2

These are deferred and land on the *same* Salvo stack (the edge listener swaps
`TcpListener` → `QuinnListener`):

- **HTTP/3 + WebTransport** at the edge — WT streams map to Aster streams, WT
  datagrams to congestion-controlled Aster datagrams.
- **gRPC trailers** — until then, plain request/response, SSE, chunked, and
  long-lived h2 work; gRPC (which needs HTTP trailers for `grpc-status`) does
  not.
- **Raw-TCP edge listener** — `RouteProtocol::Tcp` is modeled on the wire today
  but not yet bound to an L4 listener.

---

## See also

- Design + rationale: [`docs/_internal/working_ideas/aster-expose-http.md`](./_internal/working_ideas/aster-expose-http.md)
- Runnable demo: [`aster-expose/examples/reverse_proxy.rs`](../aster-expose/examples/reverse_proxy.rs)
