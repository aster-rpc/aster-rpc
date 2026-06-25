# Getting started with the `aster` Rust crate

`aster` is the first-class Rust API for the Aster peer-to-peer stack: a node with
content-addressed **blobs**, CRDT **docs**, **gossip**, connection **admission**
(Gate 0) + ownership **attestations**, and — behind the `rpc` feature — a
**Rust-native RPC framework** (services, clients, streaming, capability checks,
interceptors, and cross-binding contract publication).

> Status: the RPC layer is **Rust↔Rust** today. Contract identity and published
> manifests are cross-binding-correct (byte-identical to the Python/Java/TS
> bindings); RPC *payload* wire-compat with the other bindings is deferred on a
> Fory version gap. See "Current limits" at the end.

---

## 1. Add the dependency

`aster` is distributed as a git dependency and pins patched `iroh`/`noq` forks.
Both the dependency **and** the `[patch.crates-io]` block are required — patches
do not propagate across crates.

```toml
[dependencies]
aster = { git = "https://github.com/aster-rpc/aster-rpc-internal", branch = "main", features = ["rpc"] }

[patch.crates-io]
# Copy this block verbatim from this repo's root Cargo.toml (the fork revs change
# over time). Without it, iroh/noq won't resolve to the Aster forks.
iroh        = { git = "https://github.com/aster-rpc/iroh",        rev = "…" }
iroh-base   = { git = "https://github.com/aster-rpc/iroh",        rev = "…" }
iroh-relay  = { git = "https://github.com/aster-rpc/iroh",        rev = "…" }
iroh-blobs  = { git = "https://github.com/aster-rpc/iroh-blobs",  rev = "…" }
iroh-docs   = { git = "https://github.com/aster-rpc/iroh-docs",   rev = "…" }
iroh-gossip = { git = "https://github.com/aster-rpc/iroh-gossip", rev = "…" }
noq         = { git = "https://github.com/aster-rpc/noq",         rev = "…" }
noq-udp     = { git = "https://github.com/aster-rpc/noq",         rev = "…" }
noq-proto   = { git = "https://github.com/aster-rpc/noq",         rev = "…" }
```

To define **typed RPC payloads** you also need the Apache Fory derive in scope
(the `#[derive(ForyStruct)]` macro expands to `::fory_core::` paths, so both
crates must be direct dependencies):

```toml
fory-core   = "1.3"
fory-derive = "1.3"
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

---

## 3. Admission (Gate 0) and ownership attestations

Use Aster's `Gate0` rather than a separate static allowlist. Built-in
docs/blobs/gossip ALPNs are always registered; add your admission ALPN(s)
explicitly and enable hooks.

```rust
use aster::{alpns, AsterConfig, Gate0, Node, PublicKey, RelayMode};
use aster::attestation::{verify_chain, Chain};

# async fn run(root_pub: PublicKey) -> aster::Result<()> {
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

// 2) Accept loop on the admission ALPN: read the peer's attestation chain,
//    verify it binds the peer to a trusted anchor, then admit.
let srv = node.clone();
tokio::spawn(async move {
    while let Ok((alpn, conn)) = srv.accept().await {
        if alpn != alpns::PRODUCER_ADMISSION { continue; }
        let (send, recv) = conn.accept_bi().await?;
        let raw = recv.read_to_end(64 * 1024).await?;        // bound the request
        let chain = Chain::from_bytes(raw);
        let expected = PublicKey::from_hex(conn.peer().as_str())?;
        let ok = verify_chain(&chain, &[root_pub], &expected).is_ok();
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

Minting a chain (vendor/root side):

```rust
use aster::attestation::{attest_root_node, public_key, AttestOptions};

let chain = attest_root_node(&root_secret, &node_secret, &AttestOptions::default())?;
let text = chain.to_text();                  // "aster.attestation.chain.v1:<base64url>"
let trusted_anchor = public_key(&root_secret);
```

`attestation::public_key(&node.export_secret_key()?)` equals `node.id()` — the
attestation identity *is* the transport identity, so at admission the peer's
`NodeId` is the value to verify the chain against.

Wire your own versioned envelope for admission rather than raw chain bytes
long-term (e.g. `portalsync-admission/1 chain=…` → `…-response/1 accepted=true`).

### 3.1 Allowlist specific node IDs

When the set of callers is known ahead of time (e.g. only the **root node** from
your `AsterConfig`, or a fixed list of operators), skip attestation chains and
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
`f32/f64`, `Vec<u8>`→binary, `Vec<T>`/`HashSet<T>`→list/set, `HashMap<K,V>`→map,
`Option<T>`→nullable, and a nested `AsterType` struct → a typed reference.

Nested payload types — a struct field of another payload type, `Vec<Segment>`,
`Option<Foo>`, `HashMap<String, Bar>`, and so on at any depth — are registered
automatically: the macro names each method's top-level request/response type and
the derive walks every field's element/value/key type into the Fory runtime.
Every nested type just needs to derive `#[derive(ForyStruct, aster::AsterType)]`
like any other payload (it must anyway to serialize).

You can lower a scalar default into the contract with
`#[aster(default = <expr>)]` (supported for `String` / `bool` / `i32` / `i64` /
`f64`); a changed default changes the `contract_id`. See the cross-binding
caveat under "Current limits" before relying on it.

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
[`run`](https://docs.rs/aster) — or end it with `shutdown` for a clean,
store-flushing close (Rust has no `async with`).

`.identity(path)` persists only the endpoint **secret key** (64 hex chars,
load-or-create) — enough for a restart-stable `NodeId`. Python's `.aster-identity`
is a richer JSON credential file (root pubkey + enrollment material); Gate-1
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
populates the store (e.g. after verifying the peer's attestation chain). A
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

## 5. Cargo features

| Feature | Default | What it enables |
|---------|:-------:|-----------------|
| `blobs` / `docs` / `gossip` | ✅ | the corresponding `Node` accessors |
| `attestation` | ✅ | ownership-attestation mint/verify |
| `rpc` | — | the RPC framework (pulls Fory + the proc-macros) |
| `discovery` / `hooks` / `metrics` | — | DNS/mDNS resolution, admission hooks, Prometheus metrics |

Admission (`Node::take_admission`) additionally requires the node to be started
with `AsterConfigBuilder::hooks(true)`.

---

## 6. Current limits

- **Cross-binding RPC payloads** are Rust↔Rust only today (the Rust Fory crate
  and the other bindings' Fory are on different majors). **Contract identity and
  published manifests are cross-binding-correct now** — a Rust `contract_id` is
  byte-identical to the equivalent Python/Java/TS contract.
- **Defaults caveat:** Rust lowers `#[aster(default = …)]` into the contract-id
  per the spec, but the other bindings currently emit every field as `required`
  regardless of declared defaults. So a Rust type *with* a default produces a
  contract-id the other bindings don't yet match — use no-default fields for
  exact cross-binding parity until the ecosystem catches up.
- **Gate 1 / Gate 2** auth (enrollment-credential verification, session
  `authorize` → rcan) are not yet in the Rust crate.
- **Client-side request streaming** (client-stream / bidi inputs) is eager —
  you pass a `Vec<Req>`. Server-side streaming is fully incremental.
- **Interceptors** apply to unary calls; streaming calls are not yet intercepted.
- `Admission` exposes separate `next_handshake()` (inbound) and `next_connect()`
  (outbound) streams; with hooks enabled, unhandled outbound connect hooks wait
  for the hook timeout before a default accept.
