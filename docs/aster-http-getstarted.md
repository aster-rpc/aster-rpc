# Serving Aster over HTTP, HTTP/3 & WebTransport

`aster-transport-salvo` is a second transport for Aster, alongside Iroh. It
serves your existing Aster RPC services over **HTTPS (HTTP/1.1, /2, /3)** and
**WebTransport**, adds **browser-friendly JSON** projections, **TLS**, **static
files**, **session-scoped services**, and **pluggable auth** — all on the *same*
services and the *same* dispatcher you already serve over Iroh. No service
rewrite: everything above the transport (codec, contract identity, capabilities)
is transport-agnostic.

> Prerequisite: the Rust RPC basics — `#[aster::service]`, `AsterServer`,
> payload types — are covered in [Getting started with the `aster`
> crate](./aster-rust-getstarted.md). This guide is the HTTP layer on top.

---

## 1. Add the dependencies

```toml
[dependencies]
aster = { git = "https://github.com/aster-rpc/aster-rpc-internal", branch = "main", features = ["rpc"] }
aster-transport-salvo = { git = "https://github.com/aster-rpc/aster-rpc-internal", branch = "main" }

# Typed payloads (as in the core guide):
fory-core   = "1.3"
fory-derive = "1.3"

# Only if you serve the browser JSON projection (see §5):
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
```

The HTTP transport uses the **Aster Salvo fork** (it carries the WebTransport
stream-control surface and a raw-quinn accessor). Copy this into your
`[patch.crates-io]` **in addition to** the iroh/noq block from the core guide —
pin the same rev across your whole workspace:

```toml
[patch.crates-io]
# ... the iroh / noq block from the core guide ...
salvo        = { git = "https://github.com/aster-rpc/salvo", rev = "cdfdc90f604aa83ee13fba0b55e849d9fbc34915" }
salvo_core   = { git = "https://github.com/aster-rpc/salvo", rev = "cdfdc90f604aa83ee13fba0b55e849d9fbc34915" }
salvo_macros = { git = "https://github.com/aster-rpc/salvo", rev = "cdfdc90f604aa83ee13fba0b55e849d9fbc34915" }
# Propagation gotcha: [patch.crates-io] doesn't cross workspaces, so re-declare
# the vendored h3 stack the fork patches internally:
salvo-http3  = { git = "https://github.com/aster-rpc/salvo", rev = "cdfdc90f604aa83ee13fba0b55e849d9fbc34915" }
h3           = { git = "https://github.com/aster-rpc/salvo", rev = "cdfdc90f604aa83ee13fba0b55e849d9fbc34915" }
h3-quinn     = { git = "https://github.com/aster-rpc/salvo", rev = "cdfdc90f604aa83ee13fba0b55e849d9fbc34915" }
```

> A crypto provider: with only one TLS stack in your binary, rustls auto-selects
> it. If you also link another (e.g. `reqwest`, `wtransport`), install one once
> at startup: `rustls::crypto::aws_lc_rs::default_provider().install_default()`.

The runnable reference for everything below is
[`examples/rust/mission-control`](../examples/rust/mission-control).

---

## 2. The fastest path — `AsterServer::with_http`

Register your services once; serve them over Iroh **and** HTTPS from one builder.

```rust
use aster::rpc::{AsterServer, ProjectionRegistry};
use aster_transport_salvo::{generate_webtransport_cert, HttpConfig, TlsMaterial};

# async fn run<S: aster::rpc::ServiceDispatch>(svc: S) -> aster::Result<()> {
// A dev TLS cert (ECDSA, <=14 days — valid for WebTransport serverCertificateHashes).
let cert = generate_webtransport_cert("my-node", &["localhost".into()])
    .map_err(aster::Error::Connection)?;

let http = HttpConfig::new("[::]:443", TlsMaterial::pem(cert.cert_pem, cert.key_pem))
    .webtransport(true);                       // also mount /aster/wt

let srv = AsterServer::builder()
    .service(svc)                              // your #[aster::service] server
    .with_http(http)                           // <-- serve over HTTP too
    .start()
    .await?;

srv.run().await;                               // serves until shutdown
# Ok(())
# }
```

That's it: `POST https://host/aster/<Service>/<method>` now reaches the same
services as Iroh. `with_http` is **off by default** — without it you get an
Iroh-only node, exactly as before.

### Low-level composition (own your own Salvo app)

When you run other servers in the same process (your own routes, a separate
listener), grab the shared dispatcher and compose Salvo yourself:

```rust
use aster::rpc::Server;

let server = Server::new(&node).register(svc);
let dispatcher = server.dispatcher();          // cheap, Clone-able, shareable
let _iroh = server.serve();                    // Iroh accept loop

let app = salvo::Router::new()
    .push(aster_transport_salvo::router(dispatcher.clone()))   // /aster/{svc}/{method}
    .push(salvo::Router::with_path("healthz").get(my_health)); // your own routes
aster_transport_salvo::serve_https("[::]:443", tls, salvo::Service::new(app)).await?;
```

Aster owns `/aster/*`; every other path is yours.

---

## 3. TLS

`TlsMaterial` is data-only (crosses FFI; no closures). Three modes:

```rust
use aster_transport_salvo::TlsMaterial;

// 1. Bring your own PEM (bytes or read files yourself).
TlsMaterial::pem(cert_pem, key_pem);

// 2. ACME / Let's Encrypt (public domains; TLS-ALPN-01, single port).
TlsMaterial::Acme {
    domains: vec!["rpc.example.com".into()],
    contact_email: Some("ops@example.com".into()),
    cache_dir: "/var/lib/acme".into(),
};

// 3. Generated self-signed (dev + pinned mesh).
TlsMaterial::self_signed(["localhost".into()]);
```

`serve_https` serves **H1/H2 over TCP + H3 over QUIC** on one address for PEM /
self-signed (ACME is H1/H2 today). For PEM/self-signed you can also build a
`RustlsConfig` yourself with `rustls_config(&tls)`.

---

## 4. Canonical Aster-over-HTTP (`application/aster-frames`)

This is *real* Aster RPC over HTTP — same Fory payloads, same frame envelope and
status trailer as Iroh, full transport parity. The body is length-prefixed Aster
frames; SDK clients use it directly. Each call:

```
POST /aster/<Service>/<method>
content-type: application/aster-frames
<aster frames>            →    <response frames><status trailer>
```

All four call patterns work (unary, server-stream, client-stream, bidi). Nothing
to configure — `router(dispatcher)` serves it.

---

## 5. Browser JSON (no Fory, no framing)

A web frontend usually wants JSON, not Fory frames. Opt a service into a
generated **gateway projection** — it transcodes JSON ⇄ Fory at the edge; the
dispatcher still runs pure Fory.

```rust
use fory_derive::ForyStruct;
use serde::{Deserialize, Serialize};

#[derive(ForyStruct, aster::AsterType, Serialize, Deserialize, Default, Clone)]
#[aster(wire = "demo/StatusReq")]
struct StatusReq { agent_id: String }
// ... StatusResp likewise ...

#[aster::service(name = "Demo", version = 1, codecs = ["json"])]   // <-- opt in
trait Demo {
    async fn get_status(&self, req: StatusReq) -> aster::Result<StatusResp>;

    #[rpc(server_stream)]
    async fn tail(&self, req: StatusReq, out: aster::rpc::ResponseSink<StatusResp>)
        -> aster::Result<()>;
}
```

`codecs = ["json"]` makes the macro emit `DemoProjection`. Register it:

```rust
use aster::rpc::ProjectionRegistry;

let projections = ProjectionRegistry::new().register(DemoProjection::new());
// high-level: HttpConfig::new(addr, tls).projections(projections)
// low-level:  aster_transport_salvo::router_with(dispatcher, projections)
```

Now browsers POST plain JSON:

```bash
# unary → JSON
curl -X POST https://host/aster/Demo/get_status \
  -H 'content-type: application/json' -H 'accept: application/json' \
  -d '{"agent_id":"a1"}'

# server-stream → NDJSON
curl -X POST https://host/aster/Demo/tail \
  -H 'content-type: application/json' -H 'accept: application/x-ndjson' \
  -d '{"agent_id":"a1"}'
```

Negotiation follows HTTP: `Content-Type` picks the request codec (unsupported →
`415`), `Accept` the response (unsatisfiable → `406`). Per pattern: unary /
client-stream return JSON, server-stream returns NDJSON, **bidi is not projected
over plain HTTP** (use WebTransport) → `406`. The payload types must derive
`serde` when `json` is enabled; Fory-only services pull no serde.

The wire protocol, dispatcher, and `serialization_mode` are untouched — JSON is
purely additive at the edge.

---

## 6. WebTransport (HTTP/3)

WebTransport gives browsers bidi streams. Mount it (or set
`HttpConfig::webtransport(true)`):

```rust
let app = salvo::Router::new()
    .push(aster_transport_salvo::router(dispatcher.clone()))
    .push(aster_transport_salvo::wt_router(dispatcher));   // /aster/wt
```

A client opens a **WebTransport session to `/aster/wt`**, then **one bidi stream
per call**: write a header frame (the `StreamHeader`) + request frames; read back
response frames + the status trailer — the same bi-stream-per-call model as Iroh.
All four patterns ride it.

### Stream priority

Tag a call with an **RFC 9218 urgency** (`0` = most urgent … `7` = background) via
the `aster-priority` metadata header; the server applies it to the response
stream (`SendStream::set_priority`). Media apps map their own classes onto it
(e.g. audio `1`, video `5`). Priority is a transport accelerator — application
scheduling (supersession, drop-stale) stays in your app.

### Browser cert pinning (`serverCertificateHashes`)

A browser can connect to a self-signed WebTransport server *without* a CA by
pinning the cert's SHA-256 — perfect for mesh/dev. `generate_webtransport_cert`
produces a WT-valid cert (ECDSA, ≤14 days, CN bound to your node id) and its hash:

```rust
let cert = generate_webtransport_cert(&node.id().to_string(), &["localhost".into()])?;
// serve with TlsMaterial::pem(cert.cert_pem, cert.key_pem)
// publish cert.sha256 however you share connection info (config, an endpoint, …)
```

```js
new WebTransport("https://host/aster/wt", {
  serverCertificateHashes: [{ algorithm: "sha-256", value: <cert.sha256 bytes> }],
});
```

No Aster ticket change is needed — the hash travels however you already
distribute the address.

---

## 7. Authentication

Custom auth is a server-side **`Authenticator`** — it runs before the Gate-3
capability check on every call, sees the request metadata (HTTP headers), and can
reject or resolve a principal + attributes. (The transport copies HTTP headers
into Aster metadata, lowercased, so `authorization` is visible.)

```rust
use aster::rpc::{Authenticator, AuthContext, AuthOutcome, RpcStatus};
use std::collections::HashMap;

struct BearerAuth;

#[aster::rpc::async_trait]
impl Authenticator for BearerAuth {
    async fn authenticate(&self, ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        let mut attributes = HashMap::new();
        if ctx.metadata.get("authorization").map(String::as_str) == Some("Bearer secret") {
            attributes.insert("aster.role".into(), "operator".into());
        }
        Ok(AuthOutcome { principal: None, attributes })
    }
}

// AsterServer::builder().authenticator(BearerAuth)... — or Server::authenticator(...)
```

Resolved attributes feed the capability gate: a method tagged
`#[rpc(requires = require_role("operator"))]` then passes only with the right
credential. Attributes merge with the enrollment `AttributeStore` —
**enrollment wins on collision** (a per-request delegate can't overwrite a
vouched attribute). The same `Authenticator` applies over Iroh too.

---

## 8. Session-scoped services

A **shared** service has one instance for everyone. A **session-scoped** service
gets a fresh instance per session id — private per-client state.

```rust
// A factory builds one instance per session.
Server::new(&node).register_session(MyAgentSession::default);
// or: AsterServer::builder().register_session(MyAgentSession::default)
```

The session id comes from the `aster-session-id` header (HTTP) or
`StreamHeader.session_id` (Iroh). The first call for a new id creates the
instance; later calls with the same id reuse it; a call with **no** session id is
rejected (`INVALID_ARGUMENT`). End a session explicitly with
`dispatcher.end_session(id)`.

```bash
curl ... -H 'aster-session-id: client-42' https://host/aster/MyAgent/<method>
```

---

## 9. Static files

Serve a web app from the filesystem alongside RPC (no iroh-blobs):

```rust
// High-level: add mounts to the HTTP config.
HttpConfig::new(addr, tls).static_mount("/app", "./dist");

// Low-level: compose the router.
let app = salvo::Router::new()
    .push(aster_transport_salvo::router(dispatcher))
    .push(aster_transport_salvo::static_router("/app", "./dist")); // index.html fallback
```

`/aster/*` always wins over a catch-all static mount, so RPC and your SPA
coexist. The public-SPA pattern is the right default: serve the whole bundle
(login UI included) publicly, gate the *RPC calls*, not the static assets.

---

## 10. Methods with no request

A unary method needs no request type when it takes none:

```rust
#[aster::service(name = "Pinger", version = 1)]
trait Pinger {
    async fn ping(&self) -> aster::Result<Pong>;   // no request arg
}
// generated client: `client.ping().await` — no argument
```

Under the hood the request is an implicit `aster::rpc::Empty`; the wire protocol
is unchanged.

---

## 11. What's supported / not yet

**Supported:** HTTPS (H1/H2/H3) + WebTransport, TLS (PEM / ACME / self-signed +
WebTransport `serverCertificateHashes`), browser JSON/NDJSON projections,
per-call WebTransport stream priority, pluggable auth, session-scoped services,
filesystem static files, no-request methods — all on the same dispatcher as Iroh.

**Not yet:** `#[aster::service(scoped = "session")]` macro sugar (use
`register_session`), session idle-TTL / connection-close reaping (use
`end_session`), true incremental HTTP request-body streaming (request bodies are
buffered — fine for the eager Rust client), HTTP `Content-Encoding` compression
rules, and ACME over HTTP/3 (ACME serves H1/H2 today).
