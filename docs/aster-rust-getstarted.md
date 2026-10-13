# Getting started with the `aster` Rust crate

`aster` is the first-class Rust API for the Aster peer-to-peer stack: a node with
content-addressed **blobs**, CRDT **docs**, **gossip**, connection **admission**
(Gate 0) + root-signed **enrollment** credentials, and — behind the `rpc` feature — a
**Rust-native RPC framework** (services, clients, streaming, capability checks,
interceptors, and cross-binding contract publication).

> Aster 0.3 is a preview release. RPC interoperability is exercised by the
> Rust, Python, TypeScript, Go, Java and .NET matrix in `tests/interop/`.
> See “Current limits” for restrictions that affect application design.

---

## 1. Add the dependency

Released Rust crates are distributed from Aster's public Forgejo Cargo registry.
Configure the registry in `.cargo/config.toml` and select one released Aster
version:

```toml
# .cargo/config.toml
[registries.aster]
index = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
```

```toml
# Cargo.toml
[dependencies]
aster = { version = "0.3", registry = "aster", features = ["rpc"] }
```

Commit `Cargo.lock` in applications so the resolved patch is reproducible.
Consumers do not copy Aster's fork pins. See
[Consuming the Aster Rust SDK](rust-sdk-consumer-guide.md)
for registry setup, native Iroh/Fory/Salvo interop and version policy.

To define **typed RPC payloads** you also need the Apache Fory derive in scope
(the `#[derive(ForyStruct)]` macro expands to `::fory_core::` paths, so both
crates must be direct dependencies):

```toml
fory-core   = "=1.7.6"
fory-derive = "=1.7.6"
```

---

## 2. Start a node

```rust
use aster::{AsterConfig, Node, RelayMode};

# async fn run() -> aster::Result<()> {
let node = Node::start(
    AsterConfig::builder()
        .persistent("/var/lib/my-app")   // omit for an in-memory node
        .relay(RelayMode::Default)
        .build(),
)
.await?;

println!("node id: {}", node.id());

// Identity is restart-stable only if you persist and restore the secret key —
// the on-disk store does NOT keep the endpoint key.
let secret = node.export_secret_key()?;

node.shutdown().await; // flushes the store, then tears down
# Ok(())
# }
```

`Node` is cheap to **clone** (no `Arc<Node>` needed to share it). Note that
`shutdown(self)` closes the *shared* underlying node — if you hand out clones,
only the owner should shut it down.

Blobs / docs / gossip handles hang off the node (feature-gated):

```rust
let hash = node.blobs().add_bytes(b"hello".to_vec()).await?;
let doc  = node.docs().create().await?;
let topic = node.gossip().subscribe(topic_id).await?;
```

> Running an **RPC producer**? Prefer [`AsterServer`](#43-serve-it) — it starts
> the node, registers the ALPNs, manages identity/persistence, and serves your
> services from one builder. Drop to `Node` directly only for non-RPC use or
> custom wiring.

### 2.1 Identities and keypairs

An Aster identity is a **32-byte Ed25519 secret key**; its public key *is* the
`NodeId` peers see. If you don't supply one, `Node::start` generates a fresh key
each boot (a new identity every restart). To control identity yourself — a
restart-stable node, or an offline **root/vendor** key that signs enrollment
credentials —
mint a key with `SecretKey::generate()`:

```rust
use aster::{AsterConfig, Node, SecretKey};

# fn demo() -> aster::Result<()> {
// Fresh random identity (OS CSPRNG). Persist `to_bytes()` to keep it — losing
// the secret loses the identity; the public key is what others verify against.
let secret = SecretKey::generate();
let public = secret.public_key();        // the NodeId form of this identity
println!("identity: {}", public.to_hex());

// Restore later from the 32 bytes you stored.
let restored = SecretKey::from_bytes(secret.to_bytes());

// Pin a node to it (otherwise the node makes its own key).
let cfg = AsterConfig::builder().secret_key(restored).build();
let _node_fut = Node::start(cfg);
# Ok(())
# }
```

`SecretKey::generate()` is the same construction as the C FFI
(`aster_secret_key_generate`) and the Python binding (`ed25519_generate_keypair`)
— 32 CSPRNG bytes — so a key minted in any binding is usable from the others.

**On disk, a node key is a secret key file**: the 32 bytes as 64 hex characters
(plus a trailing newline). This is the one identity-file format shared by the
Rust SDK and every binding. `SecretKey::{to_hex, from_hex}` give the text form;
`SecretKey::read_file`, `read_file_if_exists`, `write_file` (refuses to
overwrite, mode `0600` on Unix, creates parent dirs) and `load_or_generate`
handle the file. Most code just points the config at it and lets `Node::start`
load-or-create it:

```rust
# use aster::{AsterConfig, Node};
# async fn run() -> aster::Result<()> {
let node = Node::start(
    AsterConfig::builder()
        .secret_key_file("/var/lib/my-app/node.key") // or ASTER_SECRET_KEY_FILE
        .persistent("/var/lib/my-app")
        .build(),
)
.await?;
# node.shutdown().await;
# Ok(())
# }
```

If the file exists its key is used (and an explicit `secret_key` that disagrees
is an error); if not, the explicit `secret_key` — or a fresh key — is written
there first. `AsterServer`'s `.identity(path)` is the same file (§4.3). Inline
keys (`ASTER_SECRET_KEY`, `[network] secret_key`, `.aster-identity`'s
`[node] secret_key`) accept the same 64 hex characters, or base64 for older
files. The `.aster-identity` TOML (peers, root key, enrollment material, read by
`AsterConfig::load_identity`) is a credential file, not a node-key file.

The same secret key can also **bind a TLS certificate** to this identity: when
serving over HTTP/WebTransport, the node key signs the cert so peers can verify
it (and reject a MITM) without trusting a CA — see [Binding the cert to your
Aster identity](./aster-http-getstarted.md#binding-the-cert-to-your-aster-identity).

---

### 2.2 Configuration reference

`AsterConfig` resolves defaults < `aster.toml` (`AsterConfig::from_file`) <
`ASTER_*` env (`from_env`) < builder calls (`AsterConfig::builder().load_file(p)?
.…`). Every endpoint/transport knob the bindings expose has a builder method:

| Builder | Env | TOML | Notes |
|---|---|---|---|
| `relay(RelayMode)` | `ASTER_RELAY_MODE` | `[network] relay_mode` | `default` / `staging` / `disabled` / `custom` (a bare URL means custom) |
| `relay_urls([..])` | `ASTER_RELAY_URLS` (comma-separated) | `[network] relay_urls = [..]` | selects `RelayMode::Custom`; with a mode set, it must be `custom` |
| `secret_key(k)` | `ASTER_SECRET_KEY` | `[network] secret_key` | 64 hex (or base64) |
| `secret_key_file(p)` | `ASTER_SECRET_KEY_FILE` | `[network] secret_key_file` | load-or-create at start (§2.1) |
| `persistent(dir)` | `ASTER_STORAGE_PATH` | `[storage] path` | in-memory when unset |
| `gc_interval(d)` | `ASTER_GC_INTERVAL_MS` | `[storage] gc_interval_ms` | `0` / unset = no GC |
| `bind_addr(a)` | `ASTER_BIND_ADDR` | `[network] bind_addr` | e.g. `0.0.0.0:9000` |
| `alpn(bytes)` | — | — | extra inbound ALPNs, merged with `Node::start_with_alpns` |
| `discovery(b)` | `ASTER_LOCAL_DISCOVERY` | `[network] local_discovery` | mDNS |
| `monitoring(b)` | `ASTER_ENABLE_MONITORING` | `[network] enable_monitoring` | |
| `hooks(b)` | `ASTER_ENABLE_HOOKS` | `[network] enable_hooks` | needed for `take_admission` |
| `hook_timeout_ms(ms)` | `ASTER_HOOK_TIMEOUT_MS` | `[network] hook_timeout_ms` | default 5000 |
| `hook_failure_mode(m)` | `ASTER_HOOK_FAILURE_MODE` | `[network] hook_failure_mode` | `fail_open` (default) / `fail_closed` |
| `clear_ip_transports(b)` | `ASTER_CLEAR_IP_TRANSPORTS` | `[network] clear_ip_transports` | relay-only |
| `clear_relay_transports(b)` | `ASTER_CLEAR_RELAY_TRANSPORTS` | `[network] clear_relay_transports` | direct-only |
| `portmapper(b)` | `ASTER_PORTMAPPER_CONFIG` | `[network] portmapper_config` | `enabled` / `disabled` |
| `proxy_url(u)` | `ASTER_PROXY_URL` | `[network] proxy_url` | HTTP/SOCKS |
| `proxy_from_env(b)` | `ASTER_PROXY_FROM_ENV` | `[network] proxy_from_env` | `HTTP(S)_PROXY` |
| `transport_<knob>(v)` | `ASTER_TRANSPORT_<KNOB>` | `[network] transport_<knob>` | `max_concurrent_bidi_streams`, `max_concurrent_uni_streams`, `stream_receive_window`, `receive_window`, `send_window`, `max_idle_timeout_ms`, `keep_alive_interval_ms`, `initial_mtu`, `datagram_receive_buffer_size`, `datagram_send_buffer_size`, `send_fairness`, `enable_segmentation_offload` |
| `identity_file(p)` | `ASTER_IDENTITY_FILE` | `[identity] file` | `.aster-identity` credential file |
| `health_port(p)` | `ASTER_HEALTH_PORT` | `[health] port` | health/metrics HTTP port; `0` (default) = disabled unless `health(true)` |
| `health_host(h)` | `ASTER_HEALTH_HOST` | `[health] host` | default `127.0.0.1`; `0.0.0.0` for pod probes |
| `health(b)` | `ASTER_HEALTH_ENABLED` | `[health] enabled` | force on (port `0` → ephemeral) or off; unset = on iff port ≠ 0 |

The `[health]` keys start the `aster::ops` HTTP listener (feature `health`,
which implies `otel`) automatically under `AsterServer`; with a bare `Node`,
call `aster::ops::HealthServer::start(&node, cfg.health_config().unwrap())`.
Routes: `GET /healthz` (liveness), `GET /readyz` (readiness: the server is
ready, `HealthState::set_ready`, and every named check passes;
`health.check("db").set(true)`; the JSON lists them under `checks`) and
`GET /metrics` (Prometheus text, `text/plain; version=0.0.4`).

`/metrics` is the OpenTelemetry Prometheus pull reader: the built-in
`aster.*` instruments (RPC calls, durations, streams, connections, Gate 0
decisions, upcalls), the `aster.transport.*` iroh counters, `aster.health.*`,
`process.uptime`, and any custom instruments. The listener joins the
`aster_otel` pipeline if one is running, or starts one from `OTEL_*` /
`ASTER_*`, and adds the pull reader whatever `OTEL_METRICS_EXPORTER` says.
The instruments record wherever calls happen, so a hand-built `Server` or a
bare `Node` needs no extra wiring. `aster::ops::route` serves the same routes
from another HTTP stack, and `aster::ops::metrics_text()` returns the scrape
body.

With feature `otel`, `aster::rpc::current_trace_context()` returns the trace
context of the running code (the current span's, else the inbound `traceparent`
of the call a Rust-native handler is serving, even with no exporter), and
`aster::rpc::with_trace_context(ctx, fut)` runs a future so that its client
calls carry `ctx`. Logs, metrics and traces for all six languages are covered
in the [observability guide](aster-observability-getstarted.md).

The ALPNs are exported as `aster::RPC_ALPN` and `aster::alpns::{RPC,
CONSUMER_ADMISSION, PRODUCER_ADMISSION, DELEGATED_ADMISSION, BLOBS, DOCS,
GOSSIP}`.

## 3. Admission (Gate 0) and enrollment credentials

Use Aster's `Gate0` rather than a separate static allowlist. Built-in
docs/blobs/gossip ALPNs are always registered; add your admission ALPN(s)
explicitly and enable hooks.

This needs the `enrollment` feature. For a managed fleet (reciprocal handshake,
replicated root policy, revocation) use `aster::enrollment::Enrollment::install`
instead; see `docs/_internal/aster-enrollment-portal-porting-guide.md`. The
manual loop below shows the moving parts.

```rust
use aster::{alpns, AsterConfig, Gate0, Node, PublicKey, RelayMode};
use aster::enrollment::{verify_connection, JwtEnrollmentVerifier};

# async fn run(root_pub: PublicKey, now: u64) -> aster::Result<()> {
let verifier = std::sync::Arc::new(
    JwtEnrollmentVerifier::new(root_pub.to_bytes(), "my-fleet").expect("valid root"),
);
let cfg = AsterConfig::builder().relay(RelayMode::Default).hooks(true).build();
let node = Node::start_with_alpns(cfg, vec![alpns::PRODUCER_ADMISSION.to_vec()]).await?;
let gate = Gate0::new();

// 1) Hook loop: admission ALPNs are always open (to present a credential);
//    every other ALPN requires an already-admitted peer.
let mut admission = node.take_admission().expect("hooks enabled");
let gate_hook = gate.clone();
tokio::spawn(async move {
    while let Some(req) = admission.next_handshake().await {
        if gate_hook.should_allow(&req.peer, &req.alpn) {
            req.accept();
        } else {
            req.reject(403, b"not admitted".to_vec()); // don't leak detail on the wire; log locally
        }
    }
});

// 2) Accept loop on the admission ALPN: read the peer's enrollment credential
//    and verify it was issued by the root for exactly this authenticated peer.
let srv = node.clone();
tokio::spawn(async move {
    while let Ok((alpn, conn)) = srv.accept().await {
        if alpn != alpns::PRODUCER_ADMISSION { continue; }
        let (send, recv) = conn.accept_bi().await?;
        let raw = recv.read_to_end(16 * 1024).await?;        // bound the request
        let token = String::from_utf8(raw).unwrap_or_default();
        // The credential's subject must be the peer the QUIC handshake authenticated.
        let ok = verify_connection(&verifier, &token, &conn, now).is_ok();
        if ok { gate.admit(conn.peer()); }
        send.write_all(if ok { b"ok".to_vec() } else { b"no".to_vec() }).await?;
        send.finish().await?;
        conn.closed().await; // IMPORTANT: keep the connection alive until the peer has read
    }
    aster::Result::Ok(())
});
# Ok(())
# }
```

> **Gotcha:** a responder must `conn.closed().await` after `finish()` —
> dropping the connection immediately truncates the peer's read ("closed by
> peer: 0"). `Connection::closed()` exists for exactly this.

Issuing a credential (root side; `aster enroll node` does the same from the CLI):

```rust
use aster::SecretKey;
use aster::enrollment::{issue_credential, EnrollmentIssueRequest};

# fn issue(root_secret: &SecretKey, node_public: [u8; 32], now: u64) {
// The root key is your trust anchor: generate it once, keep it offline,
// persist `to_bytes()`. The node generates its own key; only its public key
// reaches the root. Every credential expires (at most 30 days).
let request = EnrollmentIssueRequest::new(node_public, "my-fleet", now + 7 * 24 * 3600);
let credential = issue_credential(root_secret, &request, now).expect("issued");
let chain: String = credential.into_chain();  // hand this to the node
let trusted_root = root_secret.public_key();  // what verifiers pin
# }
```

A credential is a delegation chain (`aster-chain-v1~...`) of compact Ed25519
JWTs. `node.export_secret_key()?.public_key()` equals `node.id()`: the
enrollment subject *is* the transport identity, so a stolen credential is
useless on any other node. An online issuer can sign credentials for an
offline root under a root-signed delegation (`issue_delegated_credential`,
`aster enroll delegate`); see `docs/_internal/ownership-attestations.md` and
`docs/_internal/working_ideas/aster-enrollment.md`.

### 3.1 Allowlist specific node IDs

When the set of callers is known ahead of time (e.g. only the **root node** from
your `AsterConfig`, or a fixed list of operators), skip enrollment credentials and
gate at Gate 0 directly on the peer's node ID. The hook runs **after** the QUIC/
TLS handshake, so `req.peer` is the peer's TLS-certificate ed25519 key — it is
cryptographically authenticated and cannot be spoofed. Rejecting here closes the
connection before *any* protocol (RPC, blobs, docs, gossip) can be used.

```rust
use std::collections::HashSet;
use aster::{AsterConfig, AsterServer, HookFailureMode, NodeId, PublicKey, RelayMode};

# async fn serve(root_pubkey: PublicKey, also_allow: Vec<NodeId>) -> aster::Result<()> {
// Node IDs allowed to reach this node at all. A node ID *is* an ed25519 public
// key, so the root key from config goes straight in (compare as hex).
let mut allowed: HashSet<String> = HashSet::new();
allowed.insert(root_pubkey.to_hex());
for id in &also_allow {
    allowed.insert(id.as_str().to_string());
}

let config = AsterConfig::builder()
    .relay(RelayMode::Default)
    .hooks(true)                                    // enable the admission hook
    .hook_failure_mode(HookFailureMode::FailClosed) // REQUIRED — see the warning below
    .build();

let srv = AsterServer::builder()
    .service(EchoServer::new(EchoImpl))
    .config(config)
    .start()
    .await?;

// Gate-0 loop: fires per inbound connection, before any stream is dispatched.
let mut admission = srv.take_admission().expect("hooks enabled");
tokio::spawn(async move {
    while let Some(req) = admission.next_handshake().await {
        if allowed.contains(req.peer.as_str()) {
            req.accept();
        } else {
            req.reject(403, b"not allowed".to_vec()); // closes the connection
        }
    }
});

srv.run().await;
# Ok(())
# }
```

For a single allowed identity (e.g. *only* the root node), use a one-element set.

> **You must set `HookFailureMode::FailClosed`.** The hook's default fallback is
> **fail-open**: if your admission loop stalls past `hook_timeout_ms` (default
> 5 s) or is dropped, the connection is *accepted*. `FailClosed` flips that to a
> reject, so a stalled or missing loop denies rather than admits. Also make sure
> the loop actually runs (`take_admission` returns the handle once) — without a
> live loop draining `next_handshake`, every decision hits the fallback.

**Per-method instead of per-connection?** If you'd rather admit a broad set but
restrict only some methods to specific identities, use Gate 3 (§4.6): map each
allowed node ID to a role at startup (`attrs.set_role(id.as_str(), "root")`) and
tag those methods `#[rpc(requires = require_role("root"))]`. Gate 3 only checks
methods that declare `requires` — an untagged method stays open — so for a
whole-service lockdown the Gate-0 allowlist above is the surer choice.

---

## 4. RPC

> To serve these same services over **HTTPS / HTTP3 / WebTransport** (browsers,
> JSON, TLS, sessions, static files), see
> [Serving Aster over HTTP](./aster-http-getstarted.md).

Enable the `rpc` feature. The public surface lives under `aster::rpc::*`, with
`#[aster::service]` and `#[derive(aster::AsterType)]` also re-exported at the
crate root.

### 4.1 Define payload types

Payload types derive **`ForyStruct`** (wire codec) and **`AsterType`** (contract
identity). The `#[aster(wire = "namespace/Name")]` tag pins the type's
cross-binding identity.

```rust
use fory_derive::ForyStruct;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "echo/EchoRequest")]
struct EchoRequest {
    message: String,
    count: i32,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "echo/EchoResponse")]
struct EchoResponse { reply: String }
```

Field-type mapping: `String`→string, `bool`, `i8/i16/i32/i64`, `u16/u32/u64`,
`f32/f64`, `Vec<u8>`→binary (needs `#[fory(bytes)]`, see below),
`Vec<T>`/`HashSet<T>`→list/set, `HashMap<K,V>`→map, `Option<T>`→nullable, and a
nested `AsterType` struct → a typed reference.

**Byte fields.** A byte buffer is published as `binary`, the type Python
`bytes` and TypeScript `Uint8Array` use. Fory's Rust derive only writes a
`Vec<u8>` as binary when the field carries `#[fory(bytes)]`; without it, the
field goes on the wire as a list of `u8` that other bindings cannot read as
bytes. `AsterType` therefore refuses to compile an unannotated `Vec<u8>`, so
the contract and the wire cannot disagree:

```rust
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "files/Chunk")]
struct Chunk {
    #[fory(bytes)]
    data: Vec<u8>,                  // binary
    #[fory(bytes)]
    checksum: Option<Vec<u8>>,      // nullable binary (`Option` is transparent)
    digest: [u8; 32],               // binary; Fory writes `[u8; N]` as binary already
    #[fory(list(element(bytes)))]
    parts: Vec<Vec<u8>>,            // list<binary>
    #[fory(map(value(bytes)))]
    blobs: HashMap<String, Vec<u8>>, // map<string, binary>
    #[aster(uint8_list)]
    levels: Vec<u8>,                // list<uint8>: an explicit opt-out
}
```

- `#[aster(uint8_list)]` is the opt-out for a `Vec<u8>` (or `VecDeque<u8>`)
  that really is a list of small integers. The field stays a Fory list and is
  published as `list<uint8>`, not `binary`.
- A `[u8; N]` field decodes only a payload of exactly N bytes. If a peer may
  send any length, use `#[fory(bytes)] Vec<u8>` and check the length yourself.
- `VecDeque<u8>` and `#[fory(array)] Vec<u8>` cannot be binary, so they are
  rejected unless the field is marked `#[aster(uint8_list)]`. Fory 1.7 has no
  `bytes::Bytes` support, so `Bytes` cannot be a field type.

Nested payload types — a struct field of another payload type, `Vec<Segment>`,
`Option<Foo>`, `HashMap<String, Bar>`, and so on at any depth — are registered
automatically: the macro names each method's top-level request/response type and
the derive walks every field's element/value/key type into the Fory runtime.
Every nested type just needs to derive `#[derive(ForyStruct, aster::AsterType)]`
like any other payload (it must anyway to serialize).

You can lower a scalar default into the contract with
`#[aster(default = <expr>)]` (supported for `String` / `bool` / `i32` / `i64` /
`f64`); a changed default changes the `contract_id`. Matching cross-language
contracts must declare the same default.

### 4.2 Define a service

A trait annotated with `#[aster::service]` generates a server adapter, a client
stub, and a cross-binding `ServiceContract`. Methods are `async fn(&self, …) ->
aster::Result<…>`; the streaming kind is declared per method.

```rust
use aster::rpc::{require_role, RequestStream, ResponseSink};

#[aster::service(name = "Echo", version = 1)]
trait Echo {
    // unary
    async fn echo(&self, req: EchoRequest) -> aster::Result<EchoResponse>;

    // server-stream: one request, many responses
    #[rpc(server_stream)]
    async fn echo_n(&self, req: EchoRequest, out: ResponseSink<EchoResponse>) -> aster::Result<()>;

    // client-stream: many requests, one response
    #[rpc(client_stream)]
    async fn collect(&self, reqs: RequestStream<EchoRequest>) -> aster::Result<EchoResponse>;

    // bidi: many requests, many responses
    #[rpc(bidi_stream)]
    async fn chat(&self, reqs: RequestStream<EchoRequest>, out: ResponseSink<EchoResponse>) -> aster::Result<()>;

    // per-method capability requirement (Gate 3); also #[rpc(idempotent)]
    #[rpc(requires = require_role("operator"))]
    async fn admin(&self, req: EchoRequest) -> aster::Result<EchoResponse>;
}
```

Implement it with `#[aster::rpc::async_trait]`:

```rust
struct EchoImpl;

#[aster::rpc::async_trait]
impl Echo for EchoImpl {
    async fn echo(&self, req: EchoRequest) -> aster::Result<EchoResponse> {
        Ok(EchoResponse { reply: format!("echo: {}", req.message) })
    }

    async fn echo_n(&self, req: EchoRequest, out: ResponseSink<EchoResponse>) -> aster::Result<()> {
        for i in 0..req.count {
            out.send(&EchoResponse { reply: format!("{}#{i}", req.message) })?;
        }
        Ok(()) // Ok → OK trailer; Err(aster::Error::Rpc{..}) → that status
    }

    async fn collect(&self, mut reqs: RequestStream<EchoRequest>) -> aster::Result<EchoResponse> {
        let mut all = String::new();
        while let Some(r) = reqs.recv().await? { all.push_str(&r.message); }
        Ok(EchoResponse { reply: all })
    }

    async fn chat(&self, mut reqs: RequestStream<EchoRequest>, out: ResponseSink<EchoResponse>) -> aster::Result<()> {
        while let Some(r) = reqs.recv().await? {
            out.send(&EchoResponse { reply: r.message })?;
        }
        Ok(())
    }

    async fn admin(&self, req: EchoRequest) -> aster::Result<EchoResponse> {
        Ok(EchoResponse { reply: format!("admin: {}", req.message) })
    }
}
```

### 4.3 Serve it

Use **`AsterServer`** — the declarative, recommended way to run a producer, and
the Rust peer of the Python/TS `AsterServer`. One builder resolves config, starts
the node (RPC + the built-in blobs/docs/gossip ALPNs registered for you), and
serves your services; it also exposes the protocol clients and a stable identity
in one place.

```rust
use aster::rpc::AsterServer;

# async fn serve() -> aster::Result<()> {
let srv = AsterServer::builder()
    .service(EchoServer::new(EchoImpl))   // repeat .service(…) per service
    .identity(".aster-identity")          // restart-stable endpoint key (load-or-create)
    .persistent("/var/lib/my-app")        // durable blobs / docs / gossip
    .relay(aster::RelayMode::Default)
    // .attributes(attrs)                 // shared Gate-3 store (see §4.6)
    // .hooks(true)                       // enable Gate-0 admission (.take_admission())
    // .alpn(aster::alpns::PRODUCER_ADMISSION.to_vec())  // extra inbound ALPN (see §3)
    .start()
    .await?;

println!("address: {}", srv.address()?); // aster1… ticket — hand this to clients
let _hash = srv.blobs().add_bytes(b"hi".to_vec()).await?; // blobs/docs/gossip on the same node

srv.run().await;       // serve until the node closes
// or: srv.shutdown().await;  // graceful close, flushes the store
# Ok(())
# }
```

`EchoServer` is generated from the `Echo` trait. Add one `.service(…)` per
service (Rust can't hold heterogeneous instances in one `[…]` list the way Python
does). The server keeps serving as long as you hold `srv` and call
`run()` — or end it with `shutdown` for a clean,
store-flushing close (Rust has no `async with`).

`.identity(path)` persists only the endpoint **secret key** (64 hex chars,
load-or-create) — enough for a restart-stable `NodeId`. Python's `.aster-identity`
is a TOML credential file (root pubkey + enrollment material); Gate-1
credential plumbing isn't in the Rust crate yet.

#### Low-level: building a `Server` by hand

Reach for this only when you need to own the node yourself — e.g. a custom accept
loop on a non-RPC ALPN alongside RPC, or to start the node before services exist.
You register the RPC ALPN (`aster::rpc::RPC_ALPN`, `b"aster/1"`) and wire the
`Server` directly; `AsterServer` does exactly this for you.

```rust
use aster::rpc::{AttributeStore, Server, RPC_ALPN};

# async fn serve(cfg: aster::AsterConfig) -> aster::Result<()> {
let node = Node::start_with_alpns(cfg, vec![RPC_ALPN.to_vec()]).await?;

let attrs = AttributeStore::new(); // populate per-peer roles for Gate-3 checks
let _server = Server::new(&node)
    .register(EchoServer::new(EchoImpl)) // EchoServer is generated from `Echo`
    .attributes(attrs.clone())
    .serve();                            // spawns the accept+dispatch loop
# Ok(())
# }
```

### 4.4 Call it

A client dials the producer's `address()` ticket — register its address
material, then open an RPC connection by id:

```rust
use aster::rpc::RpcConnection;
use aster::Ticket;

# async fn call(node: &Node, address: &str) -> aster::Result<()> {
let ticket = Ticket::from_base58(address)?; // the aster1… string from srv.address()
let peer = node.add_ticket_addr(&ticket)?;  // teach this node how to reach it

let conn: RpcConnection = node.rpc_connect(&peer).await?;
let client = EchoClient::new(conn);         // EchoClient is generated from `Echo`

let resp = client.echo(EchoRequest { message: "hi".into(), count: 0 }).await?;
assert_eq!(resp.reply, "echo: hi");
# Ok(())
# }
```

If the peer is already reachable by id (relay/discovery, or a prior
`add_ticket_addr`), skip the ticket and call `node.rpc_connect(&peer_id)`
directly.

### 4.5 Streaming, client side

```rust
# async fn stream(client: &EchoClient) -> aster::Result<()> {
// server-stream → a typed MessageStream you read
let mut s = client.echo_n(EchoRequest { message: "x".into(), count: 3 }).await?;
while let Some(item) = s.recv().await { println!("{}", item?.reply); }
// or: let all = s.collect().await?;

// client-stream / bidi take the requests up front (eager) for now
let one = client.collect(vec![
    EchoRequest { message: "a".into(), count: 0 },
    EchoRequest { message: "b".into(), count: 0 },
]).await?;

let mut chat = client.chat(vec![EchoRequest { message: "hi".into(), count: 0 }]).await?;
let replies = chat.collect().await?;
# Ok(())
# }
```

### 4.6 Per-call authorization (Gate 3)

`#[rpc(requires = …)]` is checked **before** the handler runs, against the
caller's attributes in the server's `AttributeStore`. Your admission logic
populates the store (e.g. after verifying the peer's enrollment credential). A
missing capability fails with `PERMISSION_DENIED`.

Give `AsterServer` the store with `.attributes(store)` (or read it back via
`srv.attributes()`) and keep a clone to populate from admission:

```rust
use aster::rpc::{require_any_of, require_role, AttributeStore};

let attrs = AttributeStore::new();
// let srv = AsterServer::builder().service(…).attributes(attrs.clone()).start().await?;

// In your admission accept loop, once a peer's chain verifies and you know its role:
attrs.set_role(conn.peer().as_str(), "operator");
// attrs.remove(peer)  // revoke

// Requirements are constructed with require_role / require_any_of / require_all_of.
let _ = require_any_of(["operator", "admin"]);
```

Gate 0 (connection admission) and Gate 3 (per-call capability) are independent
layers. Gate 1 (enrollment-credential verification) and Gate 2 (session
`authorize` → rcan) are not yet in the Rust crate — inject attributes from your
own admission for now.

### 4.7 Client interceptors (deadline / retry / circuit breaker)

Attach policies to a connection with builder methods; they wrap **unary** calls.
A connection with no policies is a no-op pass-through.

```rust
use std::time::Duration;
use aster::rpc::{CircuitBreaker, RetryPolicy};

let conn = node.rpc_connect(peer).await?
    .with_deadline(Duration::from_secs(5))               // per-attempt timeout → DEADLINE_EXCEEDED
    .with_retry(RetryPolicy::new(3))                     // retry retryable codes w/ backoff (idempotent only!)
    .with_circuit_breaker(CircuitBreaker::new(5, Duration::from_secs(30)));

let client = EchoClient::new(conn);
```

Custom interceptors implement `aster::rpc::Interceptor`
(`on_request`/`on_response`/`on_error`) and attach via `.with_interceptor(…)`.

### 4.8 Publish & discover a contract (manifests)

A producer publishes its contract collection (`contract.bin` + per-type defs +
`manifest.json`) into a registry doc; a consumer resolves it and verifies
`blake3(contract.bin) == contract_id`.

```rust
use aster::rpc::{fetch_and_verify_contract, publish_contract, AsterType};
use std::time::{SystemTime, UNIX_EPOCH};

# async fn publish(node: &Node) -> aster::Result<()> {
let doc = node.docs().create().await?;
let author = node.docs().default_author().await?;
let type_defs = vec![EchoRequest::aster_type_def(), EchoResponse::aster_type_def()];
let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;

let published = publish_contract(&doc, &node.blobs(), &author,
    &EchoClient::contract(), &type_defs, now_ms).await?;
println!("published contract_id = {}", published.contract_id);

// Consumer (after the registry doc has synced to it):
let fetched = fetch_and_verify_contract(&doc, &node.blobs(), &producer_id, "Echo", 1).await?;
assert!(fetched.verified);
# Ok(())
# }
```

---

## 5. Logging

`aster` logs through the [`tracing`](https://docs.rs/tracing) facade and
installs no subscriber: nothing is printed until your application installs
one. With none installed, each log site costs a level check and allocates
nothing. The usual choice is `tracing-subscriber` with an `EnvFilter`:

```toml
# Cargo.toml of your application
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

```rust,ignore
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Warnings from everything, routine server events from aster.
                .unwrap_or_else(|_| "warn,aster=info".into()),
        )
        .init();
    // ... start the runtime, the node, the server ...
}
```

Then `RUST_LOG=aster=debug` shows every call, `RUST_LOG=aster::rpc::dispatch=debug`
only the per-call events, and `RUST_LOG=warn,aster::admission=info` the Gate 0
refusals. The targets (also listed in `aster::log_targets`):

| Target | Level | Events |
|---|---|---|
| `aster::rpc::server` | info | `AsterServer` started / shutting down |
| | debug | serve loop start/end; connection closed (with the number of sessions reaped); connection on an ALPN nobody handles |
| `aster::rpc::dispatch` | debug | an `rpc` span around every native handler (`service`, `method`, `call_id`, `peer`), so your handler's own events carry the call; `rpc completed` (`status`, `elapsed_us`) for every call; `rpc refused` (status and message) for calls stopped before a handler; deadline expiry |
| | warn | a corrupt request frame: a frame that does not inflate, or a client-stream / bidi request that does not decode. The call fails with `INVALID_ARGUMENT` |
| `aster::rpc::session` | debug | session opened, sessions reaped with their connection, HTTP session ended |
| `aster::rpc::foreign` | debug / trace | binding-implemented services: call cancelled (peer, deadline, invalid request) or abandoned (debug); call delivered (trace) |
| `aster::admission` | info | Gate 0: inbound peer rejected (`peer`, `alpn`, `code`, `reason`) |
| | debug | Gate 0: inbound peer accepted; outbound connect allowed / blocked |
| `aster::ops` | info / debug | health listener started / stopped; accept and connection errors |

The transport underneath logs under `aster_transport_core` (reactor, framing)
and `iroh` / `iroh_*` (QUIC, relay, discovery); `RUST_LOG=warn,aster=debug`
keeps those quiet while showing everything from `aster`.

Python, TypeScript, Go, Java and .NET forward Rust events to their language
loggers through the shared observability pipeline. See the
[observability guide](aster-observability-getstarted.md) for initialization,
exporters and log-level synchronization.

---

## 6. Cargo features

| Feature | Default | What it enables |
|---------|:-------:|-----------------|
| `blobs` / `docs` / `gossip` | ✅ | the corresponding `Node` accessors |
| `enrollment` | — | JWT enrollment credentials, root policy and the enrollment handshake |
| `rpc` | — | the RPC framework (pulls Fory + the proc-macros) |
| `discovery` / `hooks` | — | Local discovery and admission hooks |
| `otel` / `health` | — | Observability integration and the health/Prometheus listener |

Admission (`Node::take_admission`) additionally requires the node to be started
with `AsterConfigBuilder::hooks(true)`.

---

## 7. Current limits

- **Gate 1 / Gate 2** auth (enrollment-credential verification, session
  `authorize` → rcan) are not yet in the Rust crate.
- **Generated client request-stream methods** take a `Vec<Req>`. For incremental
  request sending, use the lower-level `RpcConnection::client_stream_open` or
  `bidi_open` APIs.
- **Interceptors** apply to unary calls; streaming calls are not yet intercepted.
- `Admission` exposes separate `next_handshake()` (inbound) and `next_connect()`
  (outbound) streams; with hooks enabled, unhandled outbound connect hooks wait
  for the hook timeout before a default accept.

---

## 8. See also

- [aster-baseline-services-getstarted.md](aster-baseline-services-getstarted.md) —
  the default-on `aster.ops.NodeInfo` service, the `aster/Value` typed
  name/value primitive, the reserved `aster.*` namespace, and defining your own
  union payloads with `#[derive(ForyUnion, AsterType)]`.
- [aster-sealed-grants-getstarted.md](aster-sealed-grants-getstarted.md) —
  `aster::grants`: hand a secret (e.g. a docs-namespace capability) to one node
  through a publicly-replicated policy doc.
- [aster-expose-getstarted.md](aster-expose-getstarted.md) /
  [aster-http-getstarted.md](aster-http-getstarted.md) — additional transports
  on the same node.
