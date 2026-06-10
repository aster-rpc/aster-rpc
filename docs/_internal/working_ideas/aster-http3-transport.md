# Aster over HTTP — Transport Sketch (HTTP/1, /2, /3)

**Status:** Design sketch (2026-05-02, revised). Not implemented.
**Scope:** A second transport for Aster, alongside the existing Iroh
(QUIC + NAT traversal + Ed25519 peer identity) transport. Serves
HTTP/1.1, HTTP/2 and HTTP/3 from a single server (Salvo), with each
version gated to only the call patterns it can correctly support.
**Target:** Browsers and HTTP-addressable servers, without giving up the
Aster RPC model (4 call patterns, contract identity, capabilities,
session-scoped services).
**Server framework:** Salvo, with a custom `NoqListener` so the HTTP
transport and the Iroh transport share one QUIC implementation (`noq`).

## Why a second transport

The current Iroh transport is excellent for peer-to-peer:
- NAT traversal via relay + holepunch
- Ed25519 pubkey identity baked in
- One ALPN per service family — clean protocol negotiation
- Reactor-friendly: `read_into` + `PollDriver` give zero-copy non-Tokio FFI

It is not what a browser wants. Browsers want:
- A reachable `:authority` (DNS name + port + cert)
- Standard HTTP semantics (caches, proxies, observability tooling)
- Fetch / WebTransport APIs, not raw QUIC

An HTTP/3 transport unlocks browser clients without rewriting Aster's
codec, dispatch, capabilities, or session model. Everything above the
transport (framing, contract identity, rcan, interceptors) is already
transport-agnostic.

## What HTTP/3 gives us

HTTP/3 is QUIC streams + framing — the same foundation as Iroh,
different framing. The four Aster call patterns map directly:

| Pattern | HTTP/3 mechanism |
|---------|------------------|
| Unary | One request stream, request body + response body |
| Server-stream | One request stream, response body is a stream of frames |
| Client-stream | Request body is a stream of frames (Fetch `duplex: 'half'`) |
| Bidi-stream | Request + response stream concurrently on the same QUIC stream |

HTTP/3's request and response halves of a stream are independent at the
QUIC layer (no shared HPACK like HTTP/2), so true bidi works without the
per-frame interleaving headaches HTTP/2 has.

## Browser support reality (2026-05)

| Pattern | Chromium | Firefox | Safari |
|---------|----------|---------|--------|
| Unary | ✅ | ✅ | ✅ |
| Server-stream (Fetch streaming response) | ✅ | ✅ | ✅ |
| Client-stream (Fetch streaming request, `duplex: 'half'`) | ✅ | ✅ | ❌ |
| Bidi-stream over Fetch | ⚠️ partial | ⚠️ partial | ❌ |
| Bidi over WebTransport (H3-only) | ✅ | ✅ | ❌ (roadmap) |

Conclusion: ship two flavours under a single transport umbrella.

- **Aster-over-HTTP/3 (Fetch-style):** unary + server-stream + client-stream
  where the browser supports it. This is the gRPC-Web shape, generalised.
- **Aster-over-WebTransport:** bidi + reactor-style sessions. H3-only,
  Chromium/Firefox today, Safari later.

A non-browser HTTP/3 client (curl, Go, Rust) gets all four patterns
unconditionally.

## Server support matrix

What the server accepts on each version, and how it rejects unsupported
combinations. This is the canonical contract enforced by the
`version_guard` middleware described in the layering section below.

| Pattern | HTTP/1.1 | HTTP/2 | HTTP/3 | WebTransport (H3-only) |
|---------|----------|--------|--------|-------------------------|
| Unary | ✅ | ✅ | ✅ | ✅ |
| Server-stream | ✅ chunked transfer | ✅ | ✅ | ✅ |
| Client-stream | ❌ → `426 Upgrade Required` | ✅ | ✅ | ✅ |
| Bidi-stream | ❌ → `426 Upgrade Required` | ⚠️ works direct; many proxies buffer the response | ✅ | ✅ |
| Reactor session anchor (Option 3) | ❌ | ❌ idle-timeout fragile | ⚠️ Fetch-bidi only on Chromium/Firefox | ✅ recommended |

Rules the version-guard middleware enforces:

- **Unary / server-stream** land cleanly on every version; the guard is
  a no-op.
- **Client-stream or bidi on H/1.1** → `426 Upgrade Required` with
  `Upgrade: h2c, h3` and a small JSON body explaining which pattern was
  rejected. Browsers and SDKs both understand this code.
- **A method whose manifest pins `transport=webtransport`** (Option 3
  reactor session) arriving on anything else → `501 Not Implemented`
  with a pointer to the WebTransport endpoint URL.
- **Bidi on H/2** is allowed but advertised as best-effort; the
  TypeScript SDK should prefer H/3 when QUIC is reachable and treat H/2
  bidi as a corporate-network fallback.

The matrix is *server-side and version-keyed*. The earlier "Browser
support reality" table is *client-side and browser-keyed*. A method
must satisfy both: a browser client picks an `(HTTP version, browser)`
pair that supports the method's pattern, and the server enforces the
column.

### H/2 bidi: proxy behaviour

The H/2 column marks bidi as `⚠️`. The HTTP/2 protocol *does* allow
true bidirectional streaming on a single stream — DATA frames flow
independently in both directions — but in practice the limit is what
intermediaries do, not what the protocol permits.

| Intermediary | H/2 bidi behaviour |
|--------------|--------------------|
| **nginx (default config)** | Breaks bidi. `proxy_request_buffering on` and `proxy_buffering on` are the defaults; both must be turned off for the response to flow before the request body is closed. nginx also speaks H/1.1 to the upstream by default, which kills bidi at that hop regardless of client-side config. |
| **AWS ALB / GCP HTTPS LB** | Terminate H/2 from the client and speak H/1.1 to the origin. H/1.1 has no bidi, so the pattern is dropped at the LB. Use NLB / TCP passthrough if you need bidi through AWS. |
| **Envoy / HAProxy (h2 mode)** | Handle H/2 bidi correctly when configured for it — this is the path most production gRPC deployments take. |
| **CDNs (Cloudflare, Fastly, etc.)** | Historically uneven. Some tiers buffer responses; some serialise streams. Do not rely on bidi without testing the specific tier in your deployment. |
| **NLB / TCP passthrough / direct connection** | Transparent — bidi works whenever the client and server agree on H/2. |

The typical failure mode is *response serialisation*: the server emits
DATA frames, the proxy buffers them, and the client sees nothing until
it half-closes the request. The bidi pattern silently degrades to
"unary with extra steps" — no protocol error, just a stalled stream.

Practical rule: H/2 bidi works if the deployment uses a gRPC-friendly
proxy (Envoy, HAProxy, NGINX with buffering off, or NLB+direct), and
breaks elsewhere. The Aster TypeScript SDK should treat H/2 bidi as
opt-in fallback only and prefer H/3 bidi (or WebTransport) when QUIC
is reachable. The `aster-transport-salvo` server-side guard accepts
H/2 bidi but returns a response header (`aster-bidi-via: h2`) so the
client SDK can log when it landed on the fragile path.

## Wire mapping

### URL and headers

```
POST /aster/<service>/<method> HTTP/3
:authority: rpc.example.com
content-type: application/aster-frames
aster-contract-id: <hex-hash-of-contract-typedefs>
aster-version: 1
authorization: Bearer <jwt>          ; see "Identity & authentication"
aster-session-id: <opaque>           ; optional, see "Sessions" below
aster-trace-id: <w3c-traceparent>    ; optional, observability
```

Notes:
- **`/aster/<service>/<method>`** is the canonical path. Mirrors gRPC's
  `/<package>.<Service>/<Method>` shape. A reverse proxy can route
  by path prefix without parsing Aster framing.
- **`content-type: application/aster-frames`** signals the body is the
  same Aster framing already used over Iroh. No new framing format.
- **`aster-contract-id`** lets the server reject contract drift without
  decoding the body — mirrors what the first frame on Iroh today asserts.

### Body framing

Identical to the Iroh transport. The body is a stream of length-prefixed
Aster frames — `[u32 length][frame bytes]…`. Reusing the existing codec
keeps the codegen and interceptors transport-blind.

For unary: request body = one request frame; response body = one
response frame + trailers.
For streaming: bodies are frame streams; the half closes when the sender
emits its final-message frame.

### Trailers

Aster status, error details, optional manifest fragments are sent as
HTTP trailers:

```
aster-status: 0           ; or RPC error code
aster-message: ...        ; optional human message
aster-error-detail: <b64> ; optional codec-encoded error payload
```

HTTP/3 supports trailers natively. (Browsers historically expose trailers
poorly via Fetch; for browser clients, fall back to encoding status as
the final frame in the body. gRPC-Web does this and it works.)

## Identity & authentication

### Server identity

TLS cert. WebPKI chain for public services; pinned cert hash for
internal/mesh deployments. Same shape as Iroh's "endpoint id is a
pubkey, optionally pinned" — different primitive (X.509 vs. raw
Ed25519), same trust model.

### Client identity — the "every caller is a peer" principle

Aster's identity model is symmetric: every node has an Ed25519 keypair;
the pubkey IS the `EndpointId`. HTTP makes the *protocol* asymmetric
(clients initiate, servers accept), but it does not have to make
*identity* asymmetric. We carry caller identity in the standard
`Authorization` header, in a way that proves possession of the privkey
without ever putting the privkey on the wire.

For v1, **one mode**: Bearer JWT. A second signature-based mode
(Aster-Sig) is a v2 follow-up — sketched below so v1 doesn't paint
into a corner, but explicitly out of scope for the first cut.

#### v1 — `Authorization: Bearer <jwt>`

Client mints a JWT signed with its Ed25519 node privkey:

- `alg`: `EdDSA`
- `iss`: hex-encoded node pubkey (the `EndpointId`)
- `exp`: short — 60s for SDK-issued per-call tokens, up to 24h for
  human-issued CLI/dev tokens that get pasted once
- Optional Aster-specific claims (`aster-cap` for rcan-style capability
  delegation — same trust spec, JWT wire format)

Server verifies the signature against `iss`. Because `iss` IS the
pubkey, no key directory or PKI is needed: the JWT is self-signed by
the identity it claims. Works in every HTTP client (curl, browser
fetch, reqwest, SDKs) with no custom signing code on the client.

#### v2 (deferred) — `Authorization: Aster-Sig …`

Per-request HTTP message signature, for non-browser SDKs that don't
want to manage JWT TTLs. Documented now so the v1 design leaves room
for it; full canonicalisation spec and reference implementation come
in v2.

```
Authorization: Aster-Sig keyId="<pubkey-hex>",
                         sig="<b64-Ed25519-sig>",
                         ts=<unix-seconds>,
                         nonce=<random>
```

The signature would cover a canonical subset of the request: method,
path, `aster-contract-id`, `content-digest`, `ts`, `nonce`. (RFC 9421
in spirit; the full mechanism is more than we'd need.) Server verifies
the signature, then checks `ts` is within ±5 minutes and `nonce` is
unseen.

Stateless and replay-protected; no token TTL to manage. Browsers can't
produce these cleanly (no pre-TLS byte access, CORS friction), so
this mode would be **non-browser only** when shipped — SDKs and
curl-from-script.

#### v2 (deferred) — WebAuthn as a browser credential source

WebAuthn is **not a new `Authorization` scheme**; it is a way for a
browser to *acquire* a v1 Bearer JWT. Sketched here so the v1 design
leaves room for it.

**The friction.** WebAuthn doesn't expose a generic "sign these bytes"
primitive. `navigator.credentials.get()` makes the authenticator
(TPM / Touch ID / security key) sign a WebAuthn-shaped challenge with
*its own* credential keypair — that key is generated inside the
authenticator and never leaves it, and most authenticators only
support ES256 (P-256), not EdDSA. So WebAuthn cannot directly produce
a JWT whose `iss` is the Aster node pubkey signed by the Aster node
privkey. We bridge the gap with a credential-exchange endpoint.

**Exchange endpoint shape (both patterns share this):**

```
POST /aster/_auth/webauthn/exchange
content-type: application/json

{ "assertion": <webauthn assertion>, "challenge_id": "..." }

→ 200 OK
{ "jwt": "...", ... }
```

Two patterns, same endpoint shape; they differ only in what the
returned JWT *means*.

**Pattern A — server-minted Bearer (simplest).** Server validates the
WebAuthn assertion against the registered credential, mints a JWT
signed by the *server's* Ed25519 key with `sub: <user-id>`,
`cred: <webauthn-cred-id>`, short `exp`. Client uses that JWT as the
Bearer on every subsequent Aster call. Trade-off: `iss` is the
server, not the caller's pubkey — browser callers are identified by
`sub`, not by an `EndpointId`. Breaks the peer-identity symmetry but
matches conventional web-app auth.

**Pattern B — browser-held Aster key, WebAuthn-gated.** First visit:
browser generates an Aster Ed25519 keypair via WebCrypto
(non-extractable, IndexedDB) and registers a WebAuthn credential; the
server records the binding "credential C ↔ Aster pubkey P for user X."
Per session: browser performs the WebAuthn ceremony and exchanges the
assertion for a *presence-attestation* JWT (e.g. 8h `exp`) signed by
the server: "WebAuthn for credential C bound to pubkey P attested at
time T." Per call: browser mints its own Bearer JWT signed with the
held Aster privkey (the v1 mode, unchanged) and co-presents the
attestation as a sidecar header (`aster-presence: <jwt>`) or embeds
its hash as a claim. Server verifies (a) browser JWT signature against
P, (b) attestation signature against the server key, (c) attestation
not expired. Trade-off: more moving parts; preserves `EndpointId =
pubkey of caller` so browser users are first-class peers in the trust
model.

**Recommendation.** Build Pattern A's exchange endpoint first — that
endpoint shape extends cleanly to Pattern B by also returning the
credential ↔ pubkey binding metadata. Decide between A and B based on
whether browser users need to be first-class pubkey-keyed peers at
dispatch time, or whether session-scoped `sub`-keyed identity is
sufficient.

### Why not...

- **Mutual TLS.** Browsers can't easily attach client certs; cert
  distribution to clients is an ops headache; reverse proxies that
  terminate TLS lose the client cert by default. We already have a
  pubkey identity layer that doesn't need any of this.
- **Basic auth with `username=<pubkey>, password=<privkey>`.** Sends
  the privkey on every request — captured by access logs, error
  trackers, browser DevTools, credential keychains. Putting the privkey
  on the wire collapses pubkey identity into a shared-secret model
  (every server you talk to ends up holding your privkey). The whole
  point of pubkey identity is that the privkey never leaves the host.
  Send signatures, not secrets.
- **Static API keys.** Shared secret with no pubkey identity — drops
  the peer-identity invariant. Operators wanting long-lived service-
  account credentials should mint a long-`exp` JWT keyed to that
  account's pubkey instead.

### EndpointId mapping

The `EndpointId` is the pubkey hex — same identifier whether it arrived
via Iroh's TLS handshake (raw-pubkey verifier), JWT `iss`, or
`Aster-Sig keyId`. Dispatch is identity-agnostic past the transport;
the HTTP layer normalises everything to `(pubkey, claims)` before
handing the request to the dispatcher.

### Revocation

| Mode | Primitive |
|------|-----------|
| Bearer JWT | Short `exp` (60s default for SDKs). Optional pubkey/`jti` denylist consulted on each verify for hard revocation. |
| Aster-Sig | Per-request, no token to revoke. Pubkey denylist if the operator needs to cut a peer off. |

Iroh revokes at handshake time (refuse the connection); HTTP revokes
per request. Lag is bounded by JWT `exp` plus denylist refresh —
operators tune that trade-off against verify cost.

## Session-scoped services

Aster's session-scoped services need a stable "this caller, this lifetime"
anchor. On Iroh that anchor is the QUIC connection. On HTTP/3 there are
three options; we should support (1) and (2), and use (3) when a specific
service really wants reactor semantics.

### Option 1: Per-QUIC-connection anchor (server-side)

HTTP/3 = QUIC; one HTTP/3 connection has a stable QUIC connection ID.
The server stamps a `aster-session-id` cookie/header on the first
request from a new connection and tracks the session table by that ID.
Client echoes it on subsequent requests.

- Pros: cheap, transparent to clients beyond echoing one header.
- Cons: connection migration / reconnect drops the anchor unless the
  client carries the session ID through (which is option 2 anyway).

### Option 2: Explicit `aster-session-id` header (recommended default)

`OpenSession` returns an opaque session token. Client carries it on
every subsequent request that targets a session-scoped method.

- Pros: composes with proxies, load balancers, gRPC-Web tooling. Survives
  reconnects, server-side session-affinity routing, even server restarts
  if sessions are persisted. This is how every HTTP-based RPC framework
  with sessions has always done it.
- Cons: nothing structural; each request pays the token-validation cost
  (cheap with rcan-style verification + a session cache).

This is the right default because it's the *least* surprising thing to
proxies, browsers, and ops tooling.

### Option 3: Bidi-stream-as-session (for reactor-style services)

For session-scoped services where the lifecycle "session ends when the
bidi closes" matters (long-lived contexts, ordered cross-call state),
open a single HTTP/3 bidirectional stream and run all session-scoped
calls through Aster framing on it. Effectively the WebTransport flavour
(or Fetch-bidi where supported).

- Pros: closest analogue to Iroh reactor semantics. Natural ordering,
  natural lifecycle, no per-request token verification.
- Cons: opaque to HTTP intermediaries; one logical "request" rather than
  one per RPC reduces observability via standard tools. Not portable to
  Safari over Fetch (must be WebTransport).

### Recommendation

- **Default:** Option 2 (`aster-session-id` header).
- **Reactor-style services:** Option 3 over WebTransport, with a code
  generator flag (e.g. `@session(transport = "stream")`).
- **Option 1** is an implementation detail for the server-side cache
  invariants (we can use the QUIC connection ID as a hint to clean up
  sessions when their owning connection dies, even if the client
  identifies the session by header).

## Static files

RPC frameworks usually punt on static-file serving ("put nginx in
front"). That works until you remember the browser also needs the
*page* that hosts the RPC client. For self-contained Aster
deployments, static files have to be first-class, and the content-
addressed blob store Aster already runs on (iroh-blobs + Collections)
turns out to be unusually well-suited to it.

### Mount model

A *static mount* is `(path_prefix, FileSeq)`:

- **`path_prefix`** — `/app`, `/docs`, `/`, etc.
- **`FileSeq`** — an iroh Collection in upstream terms: a
  content-addressed, ordered list of `(relative_path, blob_hash)`
  entries. The Collection itself has a hash — the *manifest hash* —
  that uniquely names this version of the site.

Multiple mounts coexist on one server, routed by path prefix in the
same Salvo handler chain that handles RPC routes.

### Request handling

```
GET /app/<rel-path> HTTP/3
[authorization: Bearer <jwt>]    ; only if mount is auth-gated

HTTP/3 200 OK
content-type: <from manifest metadata or extension>
etag: "<blob-hash-hex>"
cache-control: <per mount policy>
aster-fileseq: <manifest-hash>   ; debugging / invalidation hint
```

Three things drop out of content addressing for free:

- **ETag = blob hash.** Stable across deploys for unchanged files;
  byte-identical content has byte-identical ETags. `If-None-Match`
  304s are essentially free.
- **Immutable hashed assets.** Bundler-emitted `app.abc1.js`-style
  URLs pair naturally with `Cache-Control: public, max-age=31536000,
  immutable` — asset name and ETag both encode immutability.
- **Range requests.** iroh-blobs supports BAO-tree range fetches; the
  handler maps HTTP `Range` straight through.

SPA fallback is a per-mount flag — unmatched paths serve a configured
fallback (typically `index.html`) with `200`.

### Update mechanism — "dev syncs a new FileSeq, server hot-swaps"

The server holds an `ArcSwap<MountState>` per mount. In-flight requests
against the old FileSeq complete against the old content; new requests
see the new FileSeq. RCU-style — no request stalls during the swap.

Three ways to deliver a new manifest hash to the server:

**Option A — RPC (recommended default).** A regular Aster service:

```
service StaticControl {
  rpc SetMount(SetMountRequest) returns (SetMountResponse);
  rpc GetMount(GetMountRequest) returns (Mount);
  rpc ListMounts(...) returns (stream Mount);
  rpc SubscribeMountChanges(...) returns (stream MountChanged);
}
```

Dev's deploy flow:

1. Build the site → publish files to the local iroh blob store →
   publish the FileSeq manifest, getting a manifest hash.
2. *(Optional)* Pre-warm: have the target node fetch the blob set.
   Iroh-blobs fetches lazily at request time, but pre-warming means
   the first request after the swap doesn't pay download latency.
3. Call `StaticControl.SetMount(path="/app", manifest=<hash>)` over
   Aster RPC. Capability-gated by an `aster-cap: static-control:/app`
   claim in the deploy identity's JWT.
4. Server validates the capability, fetches the manifest if not
   already cached, `ArcSwap`s the `MountState`. Returns the prior
   manifest hash for audit.

End-to-end Aster RPC; no new control plane.

**Option B — iroh-docs subscription.** A CRDT doc with
`key = mount_path, value = manifest_hash`. The server subscribes;
deploys are writes to the doc. Multi-writer CRDT semantics for
fleets, distributed via iroh-gossip. Heavier than (A); natural when
the deploy target isn't a singly-addressable node.

**Option C — sentinel polling.** Server reads a known doc key or
blob-pointer on a timer. Worst ergonomics, simplest infra. Defer
until someone asks.

**Atomicity caveat.** The pointer swap is atomic; blob fetches are
not. After a swap, a request for an asset whose blob hasn't arrived
yet either (a) blocks on iroh-blobs pulling it from a peer, or (b)
falls through to the previous FileSeq if the mount keeps a short
grace-period history of the previous version. The grace path is
worth the complexity — non-trivial deploys under load otherwise emit
transient 504s while blobs catch up. Default grace: 60s, configurable.

### Dev iteration

Deploy-then-RPC is fine for staging/canary/prod but too slow for
inner-loop dev. Two escape hatches:

- **Filesystem mount mode.** `mount.kind = fs` — handler reads from a
  local directory, no FileSeq involvement. The FileSeq path is opt-in
  for environments that want content-addressed deploys.
- **Watcher CLI.** `aster static watch ./dist --mount /app --node <id>`
  watches for changes, re-publishes the FileSeq, calls `SetMount`.
  Vite-dev-shaped ergonomics once warm.

### Auth gating

Per-mount: `auth = public | optional | required`. `public` is the
default. `required` reuses the v1 Bearer JWT mode; the handler runs
the same auth interceptor as RPC routes.

**Public is the right default for the SPA case, including the login
UI.** The expected pattern:

1. **One public mount serves the entire SPA bundle, login UI included.**
   The page loads without credentials; once the user authenticates
   (e.g. WebAuthn → exchange → JWT), the SPA attaches
   `Authorization: Bearer <jwt>` to its RPC fetches. Static assets
   stay public, only the *RPC routes* are gated.
2. **Split mounts (`/login` public, `/app` required) are an option but
   rarely the right one.** Gating *static assets* — rather than RPC
   calls — means the browser must attach a Bearer on a top-level
   navigation, which is awkward without service-worker plumbing or
   cookies. Reach for `auth = required` only when you genuinely have
   private static content (gated docs, sensitive bundles), not as a
   way to gate the post-login app shell.

In short: `auth = public` does not "leak the login UI" — it's the
correct setting for it. Auth lives on the RPC layer; static assets
are the chassis the auth flow runs in.

### Layering

```
crates/aster-transport-salvo/
  src/
    static_handler.rs   # Salvo handler — ArcSwap, range, ETag, SPA fallback
    static_control.rs   # StaticControl service impl
core/src/
  static_mount.rs       # MountState, FileSeq resolution, file lookup
                        # (transport-agnostic; iroh-blobs facing)
```

Handler is HTTP-specific. Mount-state and manifest-resolution logic
live in `core`, transport-agnostic — `StaticControl` is a regular
Aster service, reachable over either Iroh or HTTP. Devs can deploy
from their laptop over Iroh P2P or over HTTP, depending on what's
reachable.

### Open questions for static files

- **Manifest format extension.** Iroh's stock Collection is
  `Vec<(name, Hash)>`. We need per-file content-type (extension
  inference is unreliable), per-file cache policy, optionally
  pre-computed ETags for non-content-addressed cases. Either extend
  the manifest blob format (an Aster `FileSeq` variant of Collection)
  or carry sidecar metadata.
- **Pre-compressed variants (`.br` / `.gz`).** Storing
  `app.css.br` / `app.css.gz` in the FileSeq and serving per
  `Accept-Encoding` is standard practice — depends on the manifest
  extension above.
- **103 Early Hints / preload links.** A manifest that records asset
  references per HTML file would let the handler emit `103` with
  preload hints. SPA TTI win; out of scope for v1.
- **CSP and other security headers.** Per-mount config, applied by
  handler. Spec the knob shape when someone has a concrete need.

## Layering inside aster-rpc-internal

Today's layout is already transport-clean; the HTTP transport is
additive and lives in its own crate so `core/` stays free of any HTTP
framework dependency:

```
bindings/{python,typescript,java,...}        # language layers
   ↓
core/                                        # codec, framing, contract id, rcan
   ↓
crates/aster-transport/                      # Transport trait (cleaned up from core/)
   ↓
crates/aster-transport-iroh/                 # current iroh impl, refactored from core/
crates/aster-transport-salvo/                # NEW — H1/H2/H3 via Salvo + noq
crates/noq-h3-listener/                      # NEW — Salvo `Acceptor` + `h3::quic::Connection` over noq
   ↓
iroh / iroh-blobs / iroh-docs / iroh-gossip
noq (shared QUIC stack used by both transports)
```

Crate sketch for `aster-transport-salvo`:

```
src/
  lib.rs              # SalvoTransport::bind(addrs, tls, ...)
  router.rs           # /aster/<svc>/<method> route registration
  version_guard.rs    # per-version capability matrix middleware
  handlers/
    unary.rs          # one POST → one response
    server_stream.rs  # response body = aster-frames stream / SSE on H1
    client_stream.rs  # request body = aster-frames stream
    bidi.rs           # H2 / H3 only; rejected on H1
  webtransport.rs     # H3 only; reactor-style sessions
  session.rs          # aster-session-id header → session table
  rcan.rs             # authorization header → claims
  trailers.rs         # Aster status as HTTP trailers (or final-frame fallback)
```

Salvo is the *only* crate that imports a web framework; if we ever
swap to axum or back to raw `hyperium/h3`, only `aster-transport-salvo`
moves. `noq-h3-listener` stays useful regardless of framework choice.

The `Transport` trait stays version-agnostic. Salvo handles version
negotiation internally; the trait just sees opened streams.

Browser bindings (TypeScript) do **not** link the Rust HTTP server;
they use `fetch()` and `WebTransport` directly and feed bytes into the
existing aster framing decoder. The server-side Rust stack is what
listens on the wire.

## Server-side stack

**Revised decision (2026-05-22): Two QUIC stacks. Salvo (quinn) for HTTP,
noq for Iroh.** The earlier "one QUIC stack via `NoqListener`" plan was
overruled after auditing Salvo's actual code; see "Revised decision" below
for the reasoning. The sections that follow describe what the unified-stack
plan *would* have looked like, kept for historical context. Skip to
"Revised decision (2026-05-22)" for the current direction.

### Revised decision (2026-05-22): two QUIC stacks

After looking at Salvo's H3 path concretely, the `NoqListener` plan as
written doesn't hold up:

- **Salvo's H3 path is deeply quinn-coupled, not generic over h3's traits.**
  `crates/core/src/conn/quinn/builder.rs:60-79` calls
  `.build::<salvo_http3::quinn::Connection, bytes::Bytes>(conn.into_inner())`
  — a hardcoded concrete type, not the generic `h3::quic::Connection` trait
  the original plan assumed. Salvo also maintains their own fork of
  hyperium/h3 (`salvo_http3`) with a built-in `::quinn` submodule rather
  than keeping the QUIC backend pluggable. So you can't just "impl the h3
  traits for noq and hand it to Salvo" — the surface to bridge is much
  larger.
- **Our existing 6 salvo-fork patches** (raw Quinn connection exposure,
  Quinn keep-alive, WebTransport stream control) all reach into
  `quinn::Connection` types. They wouldn't apply to noq connections without
  parallel rewrites.
- **The actual benefit of unification is operational, not architectural.**
  Browser WebTransport does not enable P2P regardless of which QUIC stack
  the server uses. WebTransport is client→server only per IETF; browsers
  don't expose UDP sockets to JS, so a browser cannot participate in
  iroh's coordinated hole-punch dance. The only thing noq-base would buy
  is "one QUIC implementation in the binary," not new capabilities.

Two stacks costs ~3-5 MB binary, two QUIC bug surfaces, two TLS sessions,
two UDP socket pools. That's real but tolerable, and the alternative is
~1 week of salvo-fork work (parallel `conn/noq/` module + parallel patches)
for the operational tidy-up only. Defer until there is a concrete pain
point — e.g. a non-browser HTTP/3 client that wants to join the iroh swarm
on the same UDP socket.

**Implication for this design:** the HTTP transport uses stock Salvo
with its default `QuinnListener`. No salvo fork changes needed. The
`noq-h3-listener` crate sketched below is shelved.

---

The historical "one QUIC stack" rationale follows. **Not** the current plan.

**Decision: Salvo + a custom `NoqListener` from day one.** No
"two QUIC stacks for v1, unify later" detour.

### Why Salvo

- Single server type, three HTTP versions (`salvo::Server` accepts
  multiple `Acceptor`s — TCP for H1/H2, QUIC for H3 — and routes based
  on the protocol the listener exposes).
- `Handler` middleware model maps 1:1 onto Aster's existing interceptor
  chain (rcan, contract-id check, session resolution, codec selection
  all become `Handler`s in front of the dispatch handler).
- The per-version capability matrix is one middleware: read
  `req.version()`, reject patterns the version can't support with the
  correct HTTP status (`426 Upgrade Required` with `Upgrade: h2c, h3`,
  or `501` if a method's manifest says it requires bidi).

### Why a `NoqListener` (skip the two-stacks v1)

Salvo's stock `QuinnListener` pulls in upstream `quinn`. We use the
`noq` fork (a `quinn` cousin) for the Iroh transport — it carries our
`read_into` zero-copy receive path and the `PollDriver` used by the
non-Tokio FFI. Running both QUIC implementations side by side would:

- Double the QUIC code in the binary (~3–5 MB release-build cost).
- Double the QUIC bug surface and the operational confusion when
  diagnosing connection issues ("which stack is logging this?").
- Mean two TLS sessions, two congestion controllers, two UDP socket
  pools — none of which we want long-term.

Going straight to a `NoqListener` keeps one QUIC stack across the whole
binary. Estimated effort: 400–800 LoC, structured as:

1. **`noq-h3-listener` crate, two impls:**
   - `impl h3::quic::Connection for noq::Connection` (and the
     associated `RecvStream` / `SendStream` traits). `hyperium/h3`'s
     `quic` traits are intentionally generic for this exact reason —
     `quinn` and `s2n-quic` already implement them, so the surface
     area is well-defined.
   - `impl salvo::conn::Acceptor for NoqListener` wrapping a
     `noq::Endpoint`, yielding accepted `noq::Connection`s wrapped as
     `h3::quic::Connection` for Salvo's H3 path to consume.
2. **WebTransport hook.** `hyperium/h3` has WebTransport support; once
   the `quic::Connection` impl is in place, WebTransport plumbs through
   with minimal extra code. Used for the Option 3 session anchor below.
3. **TLS material reuse.** Both transports take cert/key bytes the same
   way; we'll factor a shared `aster-tls` helper crate (or just a module
   inside `aster-transport`) so the operator configures TLS once.

### Trade-offs we're accepting by skipping v1

- **Salvo's H3 listener won't be the one their CI tests.** We're
  responsible for our `NoqListener` working under the H3 edge cases
  (0-RTT, key updates, connection migration, GOAWAY, draining). The
  `noq`/`quinn` API parity helps — most of Salvo's quinn-listener logic
  ports straight over.
- **`hyperium/h3` upstream changes ripple.** Their `quic` trait has
  evolved across releases. We pin a specific `h3` version (matching
  Salvo's) and update intentionally rather than chasing main.
- **First-time-effort cost is up-front.** ~1–2 weeks of engineering
  before we can ship any HTTP transport at all. The alternative
  (two-stacks v1) would have given us a working H3 in days, but we'd
  pay the unification cost later anyway, plus carry the ops baggage.

### Risk register

| Risk | Mitigation |
|------|------------|
| Salvo's H3 listener relies on quinn-specific behaviour `h3::quic::Connection` doesn't fully abstract | Spike the `Acceptor` impl first; if any quinn-isms leak, file an issue with Salvo and patch our crate to bridge. Worst case: copy Salvo's H3 routing code into `aster-transport-salvo` and own that path ourselves. |
| `hyperium/h3` API drift between versions | Pin `h3 = "=x.y.z"` in `noq-h3-listener` and bump in lockstep with Salvo. |
| WebTransport support in `hyperium/h3` lags Chromium / Firefox protocol changes | Same pinning approach; track the wt extension crate explicitly. |
| `noq` lacks a feature `quinn`'s H3 listener uses (e.g. specific 0-RTT hooks) | Add it to the `noq` fork — we own that fork and have already added FFI primitives. Same pattern. |
| Salvo's smaller community vs axum | All Salvo dependence is isolated to one crate. Swap is a single-crate rewrite if needed. |

## What we lose at this layer

1. **NAT traversal / direct P2P.** HTTP/3 is client → server. Aster's
   peer-to-peer story stays on Iroh.
2. **Connection-level ALPN multiplexing.** All Aster services share one
   `h3` ALPN; routing is by URL path, not protocol negotiation. Cosmetic.
3. **Pubkey-as-identity at the TLS layer.** Moves to header-borne rcan.
   The trust model is unchanged in shape, just carried differently.

## What we gain

1. **Browser clients** without a custom protocol — Fetch + WebTransport.
2. **Standard observability** — per-request URLs, headers, trailers
   visible to existing HTTP tooling.
3. **Proxy-friendly** — services can sit behind nginx, Envoy, etc.
4. **Familiar ops surface** — anyone who's run a gRPC service knows
   what to do with this.

## Open questions

- **Codec defaults across transports.** Iroh today negotiates codec via
  contract metadata; HTTP/3 could default to the same by reading
  `aster-contract-id` and looking up the registered codec. Validate that
  the codec choice is contract-keyed, not transport-keyed.
- **JWT verify cost on hot paths.** Every Bearer-mode request costs one
  Ed25519 verify; with a small per-pubkey verifier cache keyed on the
  raw JWT bytes the cost amortises to a hash-table lookup, but the
  cache TTL needs to be ≤ token `exp` so denylist updates aren't
  shadowed. Spec the cache shape during the Phase 2 implementation.
- **WebTransport API stability.** Spec is W3C CR; Safari hasn't shipped.
  Plan for an interim "WebTransport-or-fall-back-to-Fetch-bidi" shim in
  the TypeScript binding.
- **HTTP/2 fallback for QUIC-blocked networks.** Salvo's TCP listener
  speaks H1/H2 out of the box, so the fallback comes "free" with the
  Salvo decision — corporate networks that block UDP/QUIC still get
  unary + server-stream + (Chromium/Firefox) client-stream / bidi over
  H2. No extra implementation work. The cost is operational: cert
  rotation has to cover both UDP/443 and TCP/443 listeners.
- **Server-streaming over HTTP/2 fallback in Safari.** SSE works
  everywhere; revisit if HTTP/2 fallback ships.

## Suggested rollout phases

1. **Phase 1 — `noq-h3-listener` crate.** Implement `h3::quic::Connection`
   for `noq::Connection` and `salvo::conn::Acceptor` over a `noq::Endpoint`.
   Stand-alone smoke test: a minimal Salvo H3 server backed by `noq`
   serving `/hello` to `curl --http3`. No Aster code involved yet.
2. **Phase 2 — `aster-transport-salvo` core handlers.** Wire `noq-h3-listener`
   plus Salvo's stock TCP listener (H1/H2) into one server. Implement
   the four handlers (unary, server-stream, client-stream, bidi) with
   the version-guard middleware. Test all four from Rust HTTP/3 clients
   (curl-h3, reqwest with h3 feature) and from a Node.js HTTP/2 client.
   Get unary + server-stream + client-stream + bidi all green from
   non-browser clients across H2 and H3, and unary + SSE-server-stream
   green on H1.
3. **Phase 3 — TypeScript Fetch binding.** Unary + server-stream +
   client-stream from browsers via Fetch streaming. gRPC-Web-shape
   error trailing in the body for browsers that swallow trailers.
4. **Phase 4 — TypeScript WebTransport binding.** Bidi + session-scoped
   reactor-style services on H3. Document Safari gap; plan its arrival.
   This phase exercises the WebTransport extension on top of
   `noq-h3-listener`.
5. **Phase 5 — observability & proxying.** Standard HTTP middlewares
   (auth, rate-limit, tracing) wired to the same Aster interceptors
   the Iroh transport uses today. Validate operation behind nginx /
   Envoy / a CDN.

## What this does *not* change

- **Aster contracts** — same contract identity, same codegen, same
  manifest format.
- **rcan / capability model** — same trust spec, headers carry it.
- **Session semantics** — session-scoped services work, with one of
  three anchors (recommend the explicit-header default).
- **Iroh transport** — keeps doing what it's good at: P2P, reactor FFI,
  pubkey identity.

The Iroh and HTTP transports are siblings. Most services should be
transport-agnostic; some (P2P-only, or browser-only) will pin a
transport in their manifest. The HTTP transport itself serves H1, H2
and H3 from one Salvo server, with `noq` as the QUIC stack shared
with Iroh via the `noq-h3-listener` crate.
