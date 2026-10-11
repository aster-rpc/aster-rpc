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

This guide describes the Rust transport, including its broader TLS, projection
and routing options. Scoped owned PEM listeners, with explicit authenticated
or public serving APIs and certificate helpers, are also
available in [Go](../bindings/go/README.md#authenticated-https-and-webtransport),
[Java and Kotlin](../bindings/java/README.md#authenticated-https-and-webtransport-listeners),
[.NET](../bindings/dotnet/README.md#authenticated-https-and-webtransport-serving),
[Python](../bindings/python/README.md), and
[TypeScript](../bindings/typescript/packages/aster/README.md#authenticated-https-and-webtransport-listeners).
Those language guides define their available configuration and cleanup APIs;
the Rust options below are not a claim of identical foreign-language coverage.

---

## 1. Add the dependencies

```toml
[dependencies]
aster = { version = "0.3", registry = "aster", features = ["rpc"] }
aster-transport-salvo = { version = "0.3", registry = "aster" }

# Typed payloads (as in the core guide):
fory-core   = "=1.7.6"
fory-derive = "=1.7.6"

# Only if you serve the browser JSON projection (see §5):
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
```

Configure the `aster` registry as shown in the
[Rust SDK consumer guide](rust-sdk-consumer-guide.md). The transport reexports
its exact fork as `aster_transport_salvo::salvo`; add `salvo` directly from the
Aster registry only when a Salvo proc macro or extra supported feature requires
an extern-prelude dependency.

> A crypto provider: with only one TLS stack in your binary, rustls auto-selects
> it. If you also link another (e.g. `reqwest`, `wtransport`), install one once
> at startup: `rustls::crypto::aws_lc_rs::default_provider().install_default()`.

The runnable reference for everything below is
[`examples/rust/mission-control`](../examples/rust/mission-control).

---

## 2. Serve with `AsterServer::with_http`

Register your services once; serve them over Iroh **and** HTTPS from one builder.

```rust
use aster::rpc::{AsterServer, ProjectionRegistry};
use aster_transport_salvo::{generate_webtransport_cert, HttpConfig, TlsMaterial};

# async fn run<S: aster::rpc::ServiceDispatch>(svc: S) -> aster::Result<()> {
// A dev TLS cert (ECDSA, <=14 days — valid for WebTransport serverCertificateHashes).
// `None` = no identity binding; pass `Some(NodeBinding::Signed(&secret))` to bind
// it to your node key so peers can verify it (see §6).
let cert = generate_webtransport_cert("my-node", None, &["localhost".into()])
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

`POST https://host/aster/<Service>/<method>` now reaches the same
services as Iroh. `with_http` is **off by default** — without it you get an
Iroh-only node, exactly as before.

### Own an authenticated listener

Use `HttpConfig::start_authenticated` when startup must report bound readiness
and shutdown must release the listener's ports. It accepts operator-supplied
PEM and an explicit `Arc<dyn Authenticator>`; the `BearerAuth` example in §7
rejects missing credentials and resolves an authenticated principal.

```rust
use std::{sync::Arc, time::Duration};
use aster::rpc::Server;
use aster_transport_salvo::{HttpConfig, TlsMaterial};

let server = Server::new(&node).register(svc);
let dispatcher = server.dispatcher();
let _iroh = server.serve();
let mut listener = HttpConfig::new(
    "127.0.0.1:0",
    TlsMaterial::pem(cert_pem, key_pem),
)
.webtransport(true)
.start_authenticated(dispatcher, Arc::new(BearerAuth))
.await?;

println!("HTTPS: {}", listener.tcp_addr());
println!("HTTP/3 + WebTransport: {:?}", listener.quic_addr());

// Wait for your application's shutdown signal, then drain and join.
listener.shutdown(Duration::from_secs(2)).await?;
```

Both sockets are bound before startup returns. With `webtransport(true)`, QUIC
uses the exact TCP address and port, including the assigned port when binding
to `:0`; otherwise `quic_addr()` is `None`. Binding or PEM errors are returned.
This listener uses a dispatcher clone with the supplied authentication policy;
the original dispatcher's policy, including its Iroh serving policy, is unchanged.
Authentication covers routed RPCs. Configured static mounts remain public files.

`stop(grace)` and its alias `shutdown(grace)` stop admitting new calls, allow
existing work to finish during the grace period, then cancel remaining work and
join cleanup. QUIC's protocol close/drain and final socket release can take
longer than the application grace period. Successful shutdown guarantees that
TCP and UDP ports are released. Cancelling a shutdown future retains ownership:
call shutdown again to finish joining. Dropping the listener schedules forced
cleanup on its runtime; await shutdown when port release must be complete before
continuing. Repeated successful shutdown is harmless.

### Explicit public serving

Use `HttpConfig::start_public(dispatcher).await` only when uncredentialed RPC
calls are intentional. It has the same PEM, bound-readiness and owned-shutdown
contract as `start_authenticated`, but supplies empty capability attributes.
Gate 3 and Iroh-only checks still apply. All session-scoped services return
`UNAUTHENTICATED` before their factories run: an address or client-supplied
session ID is not an authenticated identity. This policy is local to the
listener and does not change the original dispatcher's Iroh policy.

Foreign bindings expose separately named public-start methods; authenticated
start never falls back to public mode when its authenticator is absent or
fails. C ABI 4.12, Go, Java/Kotlin and .NET require zero authentication-limit
fields for public start; Python and TypeScript likewise reject nonzero limits.
See their guides above for method names and lifecycle details.

### Compose a Salvo application

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
// 3b. ...bound to your Aster identity (stamps an aster://<node_id> SAN — see §6).
TlsMaterial::self_signed_for_node(node.id().to_string(), ["localhost".into()]);
```

> **ACME can't carry Aster identity.** A public CA issues only the SANs you prove
> control of and ignores custom fields — you cannot put a node key in a
> Let's-Encrypt cert. Bind identity only on the self-signed / WebTransport certs
> we mint ourselves (§6); for ACME, carry the node id in a domain you control
> (`<id>.nodes.example.com`) or publish a node-signed assertion out of band.

`serve_https` serves **H1/H2 over TCP + H3 over QUIC** on one address for PEM /
self-signed (ACME is H1/H2 today). For PEM/self-signed you can also build a
`RustlsConfig` yourself with `rustls_config(&tls)`.
The owned `start_authenticated` API accepts PEM only; generated development
certificates can be passed through `TlsMaterial::pem(cert.cert_pem, cert.key_pem)`.

---

## 4. Canonical Aster-over-HTTP (`application/aster-frames`)

Canonical HTTP RPC uses the same Fory payloads, frame envelope and status
trailer as Iroh. The body contains length-prefixed Aster frames. Each call:

```
POST /aster/<Service>/<method>
content-type: application/aster-frames
<aster frames>            →    <response frames><status trailer>
```

`router(dispatcher)` serves unary, server-stream, client-stream and bidi calls.
HTTP request bodies are buffered; see the limits below.

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

Authentication runs separately for every RPC stream. Put credentials in each
call's `StreamHeader.metadata_keys` / `metadata_values`, for example
`["authorization"]` / `["Bearer secret"]`. HTTP headers on the WebTransport
session handshake do not supply RPC credentials to its streams. Opening a
session successfully does not authorize its calls; each call still passes
authentication and Gate 3. An authenticated principal remains
`PeerProvenance::Other`, so an Iroh-only service remains unavailable over WT.

### Stream priority

Tag a call with an **RFC 9218 urgency** (`0` = most urgent … `7` = background) via
the `aster-priority` metadata header; the server applies it to the response
stream (`SendStream::set_priority`). Media apps map their own classes onto it
(e.g. audio `1`, video `5`). Priority is a transport accelerator — application
scheduling (supersession, drop-stale) stays in your app.

### Browser cert pinning (`serverCertificateHashes`)

A browser can connect to a self-signed WebTransport server *without* a CA by
pinning the cert's SHA-256. `generate_webtransport_cert`
produces a WT-valid cert (ECDSA, ≤14 days, CN bound to your node id) and its hash:

```rust
let cert = generate_webtransport_cert(&node.id().to_string(), None, &["localhost".into()])?;
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

### Binding the cert to your Aster identity

The cert's keypair is a fresh ECDSA key — **not** your Ed25519 node key (they
can't be: WebTransport requires ECDSA). So a cert is only as trustworthy as how
its hash reached the client. Two binding strengths:

**Claim (an identifier, not a proof).** `NodeBinding::Claim(node_id)` stamps an
`aster://<node_id>` URI SAN. Convenient for routing/identification, but
**unauthenticated** — a MITM can mint a cert with the same SAN. A claim is only
meaningful if you *also* pin the cert hash, delivered over a trusted channel
(e.g. the Ed25519-authenticated Aster connection — dialing a `NodeId` is itself
MITM-resistant, so a hash you receive over it is trustworthy).

**Signed (a verifiable proof).** `NodeBinding::Signed(&node_secret)` has your
node key sign the cert's public key and embeds the signature as an `aster-sig`
SAN. The cert becomes **self-attesting** — a verifier proves it without any
pinned hash or TOFU window:

```rust
use aster_transport_salvo::{generate_webtransport_cert, verify_cert_binding, NodeBinding};

// Server: bind the cert to the node identity (node_secret is the 32-byte key).
let cert = generate_webtransport_cert(
    &node.id().to_string(),
    Some(NodeBinding::Signed(&node_secret)),
    &["localhost".into()],
)?;

// Client/relay: after learning the peer's node id over a trusted Aster channel,
// verify the served cert really belongs to it — a substituted (MITM) cert fails.
verify_cert_binding(&served_cert_pem, &expected_node_id)?; // Ok ⇒ node key signed this TLS public key
```

`verify_cert_binding` requires the node id you expect (from the ticket /
authenticated connection) — verifying against the id the cert merely *claims*
would accept an attacker's self-consistent cert. With it, an attacker can't serve
a substitute cert: they can't produce your Ed25519 signature over their TLS key.
Browsers don't check this extension (they ignore unknown SANs); it's for
**Aster-side** verifiers (a relay, a native peer, the edge proxy).

Foreign-language certificate helpers return copied certificate/key PEM and
the SHA-256 of the complete DER certificate. They generate either a normal
self-signed ECDSA certificate or WebTransport-suitable P-256 material with
validity at most 14 days. An optional 32-byte Aster node secret adds a signed
binding; without it the certificate is identity-unbound. Foreign helpers do not
expose unsigned `NodeBinding::Claim`. Their verification API requires an
expected node ID learned through a trusted channel and proves the signed TLS
public-key binding, not the whole certificate, chain, hostname, validity period
or trust in that expected ID. Protect the private-key PEM. These helpers are
not ACME and make no FIPS validation claim.

> ⚠️ ACME certs can't carry this — see the note in §3. Identity binding applies to
> the self-signed / WebTransport certs only.

---

## 7. Authentication

Custom auth is a server-side **`Authenticator`** — it runs before the Gate-3
capability check on every call, sees the request metadata (HTTP headers), and can
reject or resolve a principal + attributes. (The transport copies HTTP headers
into Aster metadata, lowercased, so `authorization` is visible.)

```rust
use aster::rpc::{Authenticator, AuthContext, AuthOutcome, RpcStatus, StatusCode};
use std::collections::HashMap;

struct BearerAuth;

#[aster::rpc::async_trait]
impl Authenticator for BearerAuth {
    async fn authenticate(&self, ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        if ctx.metadata.get("authorization").map(String::as_str) != Some("Bearer secret") {
            return Err(RpcStatus::error(StatusCode::Unauthenticated, "invalid credential"));
        }
        let mut attributes = HashMap::new();
        attributes.insert("aster.role".into(), "operator".into());
        Ok(AuthOutcome { principal: Some("operator-1".into()), attributes })
    }
}

// AsterServer::builder().authenticator(BearerAuth)... — or Server::authenticator(...)
```

Resolved attributes feed the capability gate: a method tagged
`#[rpc(requires = require_role("operator"))]` then passes only with the right
credential. Attributes merge with the enrollment `AttributeStore` —
**enrollment wins on collision** (a per-request delegate can't overwrite a
vouched attribute). When configured on `Server` or `AsterServer`, the same
`Authenticator` applies over Iroh too.
`start_authenticated` instead supplies a policy for the owned HTTP/WT listener
without changing the original dispatcher. HTTP/WT authentication resolves an
application principal; it never upgrades the call to Iroh-authenticated
transport provenance. Authentication is cancelled when the caller disconnects
or its RPC deadline expires, before creating an HTTP-scoped session or starting
the handler.

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
This also applies to mounts on `start_authenticated`: its authenticator runs
for RPCs and does not protect static files.

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

## 11. Current support

**Supported:** HTTPS (H1/H2/H3) + WebTransport, TLS (PEM / ACME / self-signed +
WebTransport `serverCertificateHashes`), browser JSON/NDJSON projections,
per-call WebTransport stream priority, pluggable auth, session-scoped services,
filesystem static files, no-request methods — all on the same dispatcher as Iroh.
Owned authenticated PEM listeners additionally report bound readiness and offer
joined graceful shutdown. C ABI 4.12 adds explicit public serving and certificate
helpers to the authenticated listener surface introduced in 4.11. Go,
Java/Kotlin and .NET use that ABI; Python and TypeScript use their native Rust
bridges. The authenticated and explicitly public serving APIs support the four
RPC patterns over HTTPS and WebTransport. They do not expose Rust's full TLS,
static-mount or projection configuration. ACME listener ownership remains
pending. They add no
HTTP/WT client wrappers. See the language guides linked above for each API.

**Not yet:** `#[aster::service(scoped = "session")]` macro sugar (use
`register_session`), session idle-TTL / connection-close reaping (use
`end_session`), true incremental HTTP request-body streaming (request bodies are
buffered — fine for the eager Rust client), HTTP `Content-Encoding` compression
rules, and ACME over HTTP/3 (ACME serves H1/H2 today).
