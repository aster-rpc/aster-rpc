# Aster: native Rust crate

> Distribution note: this implementation-era document preserves the old Git
> dependency + copied-patch workflow for historical context. It is not the
> consumer contract. Use `docs/rust-sdk-consumer-guide.md` and
> `docs/_internal/rust-sdk-maintainer-guide.md` for the Forgejo Cargo registry,
> native reexports, and release process.

> Status: **Step 1 implemented** (`aster/` crate — node + blobs/docs/gossip + admission + deterministic namespaces). Step 2 (Rust RPC, `rpc` feature) **in progress** (2026-06-01): codec (envelope + payload), status/errors, server dispatch + client stub for **all four call patterns** (steps 1–3), **identity + manifest publication (step 4)**, and **Gate-3 auth (step 5)** are implemented and tested. Contract identity is **cross-binding-verified** — `#[derive(AsterType)]` reproduces the Python reference producer's golden contract-ids byte-for-byte (`scripts/cross_lang_echo_contract_id.py`); `publish_contract`/`fetch_and_verify_contract` round-trip a contract collection + manifest through a registry doc with `blake3(contract.bin)==contract_id` verification. Gate 3: `ServiceDispatch::{service,method}_requires` + an `AttributeStore` the app populates → the server rejects calls lacking the required capability with `PERMISSION_DENIED` before dispatch (Gate 1 enrollment + Gate 2 session-authorize remain deferred — see "Outstanding"). **Step 6 (`#[aster::service]` macro)** generates, for **all four call patterns**, a `…Server<T>` dispatcher, a `…Client` stub, and a cross-binding `ServiceContract` (verified: the generated contract's request type matches the golden, and each method's `pattern` is recorded). Streaming methods are declared with `#[rpc(server_stream|client_stream|bidi_stream)]` and typed stream handles — server handlers take a `RequestStream<Req>` and/or `ResponseSink<Resp>`; clients return a `MessageStream<Resp>`. `#[rpc(requires = …)]` wires Gate-3. **Step 7 (interceptors)** is implemented: an `RpcConnection` carries a `Pipeline` (`with_interceptor`/`with_retry`/`with_circuit_breaker`/`with_deadline`) that wraps unary calls — an `Interceptor` chain (`on_request`/`on_response`/`on_error`), a `RetryPolicy` (retryable status codes + exponential backoff), per-attempt deadline → `DEADLINE_EXCEEDED`, and a `CircuitBreaker` that fails fast when open; no-op by default. **2A (Rust-native RPC) is complete** and green against the iroh/noq rc1 forks (45 aster + 35 core-contract tests). Remaining: the deferred **2B** cross-binding payload work (Fory version gap) and **Gate 1/2 auth** (see "Outstanding"). Cross-binding RPC *payloads* remain deferred on the Fory version gap (2B); manifests/contract-ids are cross-binding now. Step 1 details are under "Implementation status & portal-sync migration" at the end.

## Context

Aster is written in Rust (`core/` = `aster_transport_core`) with bindings for Python, TypeScript, Java, and Kotlin — but we never shipped a first-class **Rust** crate. Now `portal-sync` (a separate Rust project of ours) wants to consume Aster natively. Today it reaches directly into the internal `aster_transport_core` types (`CoreNode`, `CoreBlobsClient`, …) — commit `bf82ed9` even added `add_path`, persistent docs, and `CoreDocsClient::open()` *specifically* for portal-sync. That couples portal-sync to FFI-shaped internals (flat 23-field config, hex-string IDs, `Core*` names) with no stability guarantee.

This document specifies a clean, stable public Rust crate:

- **Step 1 (implement now):** an idiomatic `aster` facade crate that **covers the portal-sync contract** (checklist below) — config builder, friendly names, start a node, use Blobs/Gossip/Docs, admission, and deterministic config namespaces. Unblocks portal-sync against a stable surface.
- **Step 2 (designed, deferred):** defining Aster RPC *services* from Rust. The transport plumbing already exists in Rust (`reactor.rs`, `framing.rs`); the Fory-XLANG envelope/payload codec does not. This doc captures the design and the path; implementation is a follow-up.

### Decisions

| Decision | Choice |
|----------|--------|
| Scope of first delivery | Step 1 now; Step 2 designed, deferred |
| Distribution | **Superseded:** public Forgejo source crates; see the maintainer guide |
| API shape | Idiomatic facade; `core` stays the FFI-shaped backend |
| RPC envelope + payload codec | **Static Fory registration** (Apache Fory Rust crate, `#[derive(ForyObject)]`) |

Architecture principle preserved: **Rust owns transport; the language layer owns the RPC API.** Step 2 makes Rust one of those language layers.

---

## Step 1 — The `aster` facade crate (implement now)

### Crate setup

New workspace member `aster/` named **`aster`** (unused name; current crates are `aster_transport_core`, `aster_transport_ffi`, `aster_rs`).

- `aster/Cargo.toml`: `name = "aster"`, version `0.2.0` (match workspace), `edition = "2021"`.
  - `aster_transport_core = { path = "../core" }`
  - Re-expose `iroh`, `iroh-blobs`, `iroh-docs`, `iroh-gossip` for type signatures (workspace deps), plus `anyhow`, `tokio`, `bytes`.
  - **Cargo features** for optional protocols: `default = ["blobs", "docs", "gossip"]`, plus `metrics`, `hooks`, `discovery`, and (later) `rpc`. Features gate the *facade accessors*; they do not change `core` yet.
- Add `"aster"` to `members` in the root `Cargo.toml` (`members` array, currently line 2).

### Config builder (wraps the flat `CoreEndpointConfig`)

`core/src/lib.rs:69-117` defines `CoreEndpointConfig` (23 fields, already has `Default`). Wrap it so Rust users never see the flat struct:

```rust
// aster/src/config.rs
pub struct AsterConfig { inner: CoreEndpointConfig, data_dir: Option<PathBuf> }

impl AsterConfig {
    pub fn builder() -> AsterConfigBuilder { ... }
}

AsterConfig::builder()
    .relay(RelayMode::Default)          // -> relay_mode: Some("default")
    .persistent("/var/lib/portal-sync") // -> data_dir
    .bind_addr("0.0.0.0:9000")
    .secret_key(key_bytes)
    .discovery(true).monitoring(true)   // gated behind features
    .build();
```

Map enums to the string forms `core` expects (`relay_mode: "default"|"disabled"|…`, `portmapper_config: "enabled"|"disabled"`). Provide a typed `RelayMode` enum rather than stringly-typed input.

### Node + protocol accessors

Wrap `CoreNode` (constructors `memory` / `memory_with_alpns` / `persistent` / `persistent_with_alpns`; accessors `blobs_client` / `docs_client` / `gossip_client` / `net_client` / `node_id` / `node_addr_info` / `close` / `export_secret_key` / `add_node_addr` / `take_hook_receiver` / `transport_metrics_prometheus`).

```rust
// aster/src/node.rs
pub struct Node { inner: CoreNode }

impl Node {
    pub async fn start(config: AsterConfig) -> Result<Node>;       // mem vs persistent chosen by data_dir
    pub async fn start_with_alpns(config, alpns: Vec<Vec<u8>>) -> Result<Node>;
    pub fn id(&self) -> NodeId;                 // strong type wrapping the hex String
    pub fn addr(&self) -> NodeAddr;
    pub fn add_peer_addr(&self, addr: NodeAddr) -> Result<()>;     // wraps add_node_addr

    // Identity persistence (required — see contract). Input via AsterConfig::secret_key.
    pub fn export_secret_key(&self) -> SecretKey;                  // 32 bytes; restart-stable id

    // Admission / Gate 0 (requires AsterConfig::hooks(true)).
    pub fn take_admission(&self) -> Option<Admission>;             // wraps take_hook_receiver

    #[cfg(feature = "blobs")]  pub fn blobs(&self)  -> Blobs;
    #[cfg(feature = "docs")]   pub fn docs(&self)   -> Docs;
    #[cfg(feature = "gossip")] pub fn gossip(&self) -> Gossip;
    pub async fn shutdown(self);                // wraps close() (flushes store)
}
```

`Blobs` / `Docs` / `Gossip` are thin newtypes over `CoreBlobsClient` / `CoreDocsClient` / `CoreGossipClient`, re-exposing methods with idiomatic types where cheap (e.g. `iroh::Hash` / typed IDs at the boundary instead of hex `String`; keep `Vec<u8>` for payloads).

### Portal-sync contract (Step 1 must cover all of this)

Derived from portal-sync's `portal-cas` usage. Each item maps to an existing `core` method unless flagged **[core gap]**.

**Node**
- [x] persistent start — `CoreNode::persistent_with_alpns`
- [x] secret-key **input** — `AsterConfig.secret_key` → `CoreEndpointConfig.secret_key`
- [x] secret-key **export** + restart-stable id — `export_secret_key()` (verified by the restart test)
- [x] id / addr / shutdown — `node_id` / `node_addr_info` / `close`
- [x] admission access — `take_hook_receiver()` (gated by `enable_hooks`)

**Blobs**
- [x] `add_path`, `add_path_with_named_tag`, `read_to_bytes`
- [x] named tags — `tag_set`; delete — `tag_delete`; delete-prefix — `tag_delete_prefix`
- [x] presence/status — `blob_has`, `blob_status`
- [x] download a specific hash from a specific peer — `download_hash(hash_hex, node_id_hex, format)`

**Docs**
- [x] authors — `create_author`; **`default_author()`** returns the node identity key (`AuthorId == NodeId`; Aster imports the node secret as the docs author and sets it default), so entry authorship maps directly to the node.
- [x] open / create / join — `open`, `create`, `join`
- [x] join + subscribe — `join_and_subscribe`, `join_and_subscribe_namespace`
- [x] share with addresses — `share_with_addr` (and `share`)
- [x] start sync — `start_sync(peers)`
- [x] queries + content read — existing query methods + `read_entry_content`
- [x] `doc_id()` accessor
- [x] live event receiver — `subscribe()` → `CoreDocEventReceiver::recv()`, including `CoreDocEvent::InsertRemote { from, entry }` and `ContentReady { hash }` (both already carry the needed metadata)

**Gossip**
- [x] subscribe / join — `CoreGossipClient::subscribe`
- [x] broadcast — `CoreGossipTopic::broadcast`
- [x] recv events — `CoreGossipTopic::recv`

**Docs namespace (config namespaces)** — distinguish *open by id* from *import by capability*:
- [x] reopen a namespace already in the local replica — `open(namespace_id)` (exists)
- [ ] **[core gap]** bootstrap a **writable** namespace on a fresh node from a deterministic 32-byte secret — portal-sync's `BLAKE3("portal-node-config/v1")` / `BLAKE3("portal-tree-config/v1")` are *capability/secret* material, not the public id, so `open(id)` cannot create them. The client must be able to say "this 32-byte value is a writable namespace capability" **without importing `iroh_docs::{NamespaceSecret, Capability}`**.
- [ ] **[core gap]** import a **read-only** namespace from its public id (`Capability::Read`), idempotently.

### Admission (Gate 0) surface

`take_hook_receiver()` returns a `CoreHookReceiver` with two `mpsc` channels, each yielding a `oneshot` decision sender. The facade wraps it as an `Admission` handle with an idiomatic decision API:

```rust
// aster/src/admission.rs
pub struct Admission { rx: CoreHookReceiver }

pub struct ConnectRequest   { pub peer: NodeId, pub alpn: Vec<u8>, reply: oneshot::Sender<bool> }
pub struct HandshakeRequest { pub peer: NodeId, pub alpn: Vec<u8>, pub is_alive: bool,
                              reply: oneshot::Sender<CoreAfterHandshakeDecision> }

impl ConnectRequest   { pub fn accept(self); pub fn reject(self); }
impl HandshakeRequest { pub fn accept(self); pub fn reject(self, code: u32, reason: Vec<u8>); }

impl Admission {
    pub async fn next_connect(&mut self) -> Option<ConnectRequest>;
    pub async fn next_handshake(&mut self) -> Option<HandshakeRequest>;
}
```

**Which gate matters:** inbound Gate 0 (the config-doc admission boundary) is enforced at **`after_handshake`** — that is the only hook with a reject-with-code decision (`core/src/lib.rs:714`, `CoreAfterHandshakeDecision::Reject { error_code, reason }`), and it sees the negotiated ALPN and remote id. `before_connect` (`lib.rs:690`) is the **outbound** dial gate (bool accept/reject) and is not where inbound peers are admitted. So portal-sync drives admission via `next_handshake()`; `next_connect()` is only for gating its own outbound dials.

This is the boundary the portal config design leans on. It is fully backed by existing `core` types (`CoreHookConnectInfo`/`CoreHookHandshakeInfo`/`CoreAfterHandshakeDecision`); no core change needed — only the facade wrapper and `AsterConfig.hooks(true)` to populate the receiver. Networked config docs are therefore **not** blocked.

### Namespace capability API (facade + core)

The facade exposes the capability concept with **aster newtypes** (defined in `aster/src/id.rs`), never leaking `iroh_docs` types:

```rust
pub struct NamespaceSecret([u8; 32]);   // writable capability material (e.g. a BLAKE3 seed)
pub struct NamespaceId([u8; 32]);       // public, read capability

impl NamespaceSecret { pub fn from_bytes(b: [u8; 32]) -> Self; pub fn id(&self) -> NamespaceId; }

impl Docs {
    pub async fn open(&self, id: NamespaceId) -> Result<Option<Doc>>;                    // reattach local only
    pub async fn open_or_import_write_namespace(&self, secret: NamespaceSecret) -> Result<Doc>; // deterministic writable (upgrades read→write)
    pub async fn open_or_import_read_namespace(&self, id: NamespaceId) -> Result<Doc>;   // read-only
    pub async fn default_author(&self) -> Result<AuthorId>;                              // == node id (node key is the docs author)
}
```

Usage portal-sync wants (no iroh-docs imports):
```rust
let doc = node.docs()
    .open_or_import_write_namespace(NamespaceSecret::from_bytes(blake3_seed))
    .await?;
```

**Core changes needed (small, in `core`)** — thin wrappers over capabilities iroh-docs already exposes; the only non-facade work in Step 1:

1. `CoreDocsClient::import_write_namespace(secret_bytes: [u8;32]) -> Result<CoreDoc>` — builds `Capability::Write(NamespaceSecret::from_bytes(..))` → `import_namespace(cap)` so `doc.id() == secret.id()` (stable across peers/restarts). **Idempotent open-or-import that upgrades capability**: if the namespace is absent, import write; if present **read-only** (a prior `Capability::Read` import), it must **install/upgrade to the write capability** and return a writable doc — *not* fall back to `open(id)` and hand back a read-only replica; if already present writable, reattach. (iroh-docs `import_namespace` accepts a higher capability for an existing namespace — verify it upgrades in-place and assert via the test below.)
2. `CoreDocsClient::import_read_namespace(namespace_id_bytes: [u8;32]) -> Result<CoreDoc>` — `Capability::Read(id)`, idempotent open-or-import; never downgrades an existing write capability to read.
3. **Default docs author = the node identity key (`AuthorId == NodeId`).** `CoreNode::finalize` imports the node's own Ed25519 secret as the docs author (`author_import`) and sets it default (`author_set_default`), overriding iroh-docs' random default. So `default_author()` returns the node id, and a doc entry's author is exactly the node that wrote it — attribution is direct (no published binding). Author **export** is deliberately not surfaced by Aster (the author secret == the node secret); the only door to the node identity is `export_secret_key()`. Trade-off: on a persistent node the node secret is persisted in the docs `authors-1` table (`docs.redb`).

The `open_or_import_*` open-or-reattach-or-upgrade composition lives in core (it derives `secret.id()`, checks local presence and current capability kind); the facade just maps the aster newtypes onto these calls. `open(namespace_id)` stays as-is (reattach existing only, no import/upgrade).

**"Optional" protocols:** `CoreNode` always builds Blobs/Docs/Gossip internally. For now, "enable" = which accessor you call, gated by Cargo features (no `core` change; clients are cheap handles). A later optional `core` change can skip router registration when a protocol is disabled — tracked as a follow-up, not a blocker.

### Errors

`aster::Error` (thiserror) wrapping the `anyhow::Error` that `core` returns, with variants mirroring the Python exception taxonomy (`BlobNotFound`, `DocNotFound`, `ConnectionError`, `TicketError`). `pub type Result<T> = std::result::Result<T, Error>`.

### Distribution (git dependency)

An external consumer must, in its own `Cargo.toml`:

```toml
[dependencies]
aster = { git = "https://github.com/aster-rpc/aster-rpc-internal", branch = "main" }

[patch.crates-io]   # copy verbatim from this repo's root Cargo.toml
iroh        = { git = "https://forge.emrul.dev/Aster/iroh.git",        rev = "77a68a5…" }
iroh-base   = { git = "https://forge.emrul.dev/Aster/iroh.git",        rev = "77a68a5…" }
iroh-relay  = { git = "https://forge.emrul.dev/Aster/iroh.git",        rev = "77a68a5…" }
iroh-blobs  = { git = "https://forge.emrul.dev/Aster/iroh-blobs.git",  rev = "ede454c…" }
iroh-docs   = { git = "https://forge.emrul.dev/Aster/iroh-docs.git", rev = "be04181…" }
iroh-gossip = { git = "https://forge.emrul.dev/Aster/iroh-gossip.git", rev = "d7d1358…" }
noq         = { git = "https://forge.emrul.dev/Aster/noq.git",         rev = "617899f…" }
noq-udp     = { git = "https://forge.emrul.dev/Aster/noq.git",         rev = "617899f…" }
noq-proto   = { git = "https://forge.emrul.dev/Aster/noq.git",         rev = "617899f…" }
```

The `[patch.crates-io]` block is **mandatory** for any external consumer — patches don't propagate across repos. The `aster/README.md` carries this block as the single source consumers copy; keep it in sync with the root `Cargo.toml`.

### Files (Step 1)

- New: `aster/Cargo.toml`, `aster/src/lib.rs`, `aster/src/config.rs`, `aster/src/node.rs`, `aster/src/blobs.rs`, `aster/src/docs.rs`, `aster/src/gossip.rs`, `aster/src/admission.rs`, `aster/src/error.rs`, `aster/src/id.rs` (newtypes), `aster/README.md`, `aster/examples/quickstart.rs`.
- Edit: `core/src/lib.rs` — add `CoreDocsClient::import_write_namespace` (idempotent open-or-import, upgrades read→write) and `import_read_namespace`; add `default_author`; in `CoreNode::finalize` set the default docs author to the node identity key (`author_import` + `author_set_default`, so `AuthorId == NodeId`).
- Edit: root `Cargo.toml` (add `"aster"` member).

---

## Step 2 — Rust-native RPC services (designed 2026-06-01; Rust↔Rust first)

Behind a new opt-in `rpc` Cargo feature on `aster` (off in the default set, so Step-1 consumers
pull nothing new). The work is **a codec + a dispatch loop + a client stub + manifest/identity
glue + a call-time auth gate** over plumbing that already exists — not new transport.

### Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| First target | **Rust-native RPC (Rust↔Rust)**; cross-binding payloads deferred | portal-sync is Rust↔Rust; the Fory wire gap (below) blocks cross-binding payloads, not Rust↔Rust |
| Call patterns | **All four** (unary / server-stream / client-stream / bidi) | the reactor already dispatches all four; do them together |
| RPC payload codec | Apache Fory **Rust** crate (`fory` 1.1.x, `#[derive(ForyStruct)]`, `register_by_name`, `xlang(true).compatible(true)`) | same machinery as `core/src/attestation.rs` |
| JSON serialization mode (`=3`) | **Not implemented in Rust** | per decision; XLANG-only on the Rust wire |
| Manifest / contract identity | **Cross-binding-correct now** | identity is computed by `canonical.rs` (hand-rolled, **Fory-version-independent**), so manifests/contract-ids interoperate with Python/Java/TS regardless of the Fory gap |
| Auth | **Gate 3 (per-call capability check) with app-injected attributes** | real call-time auth without porting the enrollment/rcan subsystems; Gate 1/2 explicitly deferred (see "Outstanding") |

**The Fory wire gap (why cross-binding *payloads* are deferred).** The Rust `fory` crate is
`1.1.x`; pyfory/Java/TS are `0.17`. Apache Fory does not guarantee binary compatibility across
majors, and the Rust `#[derive(ForyStruct)]` has **no field-level ID** support — so we cannot
hand-match pyfory's `field(id=N)`-pinned framework types from Rust. Sticking to the Rust crate's
own documented API gives correct Rust↔Rust wire; cross-binding RPC payloads wait on a Fory
alignment (2B). This does **not** affect manifests/identity, which never touch the Fory library.

### Reuse — don't rebuild

**Transport** (`aster_transport_core`, all `pub`):
- `framing` — `encode_frame`/`decode_frame`; flags `COMPRESSED/TRAILER/HEADER/ROW_SCHEMA/CALL/CANCEL/END_STREAM/TUNNEL`.
- `reactor` — `ReactorHandle::next_event()` → `ReactorEvent::Call(IncomingCall)` (+ `ConnectionClosed`). `IncomingCall` is payload-opaque: `header_payload`/`request_payload: Vec<u8>`, a `response_sender` (`OutgoingFrame::{Frame,Trailer,CompleteUnary}`), a `request_receiver` for client-stream/bidi, and a `cancelled` flag (flips on `FLAG_CANCEL`). `RPC_ALPN = b"aster/1"`.
- `CoreConnection` — `unary_call(header, request) -> UnaryCallResult`, `acquire_stream(key)`, `open_streaming_substream()`, `session_unary_call(...)`. Client connect: `CoreNetClient::connect(node_id, b"aster/1")`.

**Manifest / identity** (already in `core` — the bulk of the manifest work):
- `contract` — `ServiceContract`, `TypeDef`/`FieldDef`/`MethodDef`/`EnumValueDef`/`UnionVariantDef`, `canonical_xlang_bytes_service_contract[_checked]`, `canonical_xlang_bytes_type_def`, `compute_contract_id` (hex BLAKE3), `compute_type_hash` (`[u8;32]`), `tarjan_scc` (type-graph cycle detection). `FieldDef.id` is **re-derived from NFC-name-sorted position** during canonicalization, so generated `TypeDef`s only need field *names + kinds + container + optional* — not IDs. `producer_language = ""` for xlang mode (no allowlist change for Rust).
- `registry` — `ArtifactRef{contract_id, collection_hash, provider_endpoint_id, relay_url, ticket, published_by, published_at_epoch_ms, collection_format}`, `publish_artifact(doc, author, &ref, service, version, channel, gossip)` (writes `contracts/{cid}` + `services/{name}/versions/v{n}`), `resolve(...)` (lease listing + health/freshness/ALPN filter + ranking), `contract_key`/`version_key`/`channel_key`, `EndpointLease`.
- blobs — `add_collection(entries)` → collection hash; `download_collection_hash(...)`.

### To build

**A. Codec** (`aster/src/rpc/codec.rs`). Envelope structs mirroring `bindings/python/aster/protocol.py`
field-for-field (`i8`/`i16`/`i32`/`String`/`Vec<String>` all supported by the Rust crate, so
`StreamHeader{ deadline: i16, serialization_mode: i8, … }` maps 1:1): `StreamHeader`, `RpcStatus`,
`CallHeader`, each `#[derive(ForyStruct)]`, registered once in a shared `OnceLock<Fory>` under the
`_aster` namespace (`register_by_name::<StreamHeader>("_aster", "StreamHeader")`). Payload types use
the same derive. `SerializationMode::Xlang` only. Golden tests pin **our own** bytes (Rust-internal
stability), not cross-binding goldens.

**B. Status / errors** (`aster/src/rpc/status.rs`). `StatusCode` enum mirroring Python `status.py`;
`RpcStatus` ↔ `StatusCode`; new `Error::Rpc { code, message, details }` variant on `aster::Error`.

**C. Server dispatch** (`aster/src/rpc/server.rs`). A loop over `ReactorHandle::next_event()`:
`Call(c)` → decode `StreamHeader` from `c.header_payload` → look up `(service, version)` → `MethodInfo`
→ pre-dispatch auth (E) → decode `c.request_payload` → run handler → frame response(s) + `RpcStatus`
trailer onto `c.response_sender`; `ConnectionClosed{..}` → reap per-connection session/attribute state.
Pattern → primitive mapping:

| Pattern | Requests | Responses |
|---|---|---|
| Unary | `request_payload` | `CompleteUnary(resp ‖ trailer)` |
| Server-stream | `request_payload` | N× `Frame(item)` then `Trailer(status)`; poll `cancelled` |
| Client-stream | `request_payload` + drain `request_receiver` to EOF | one `Frame` + `Trailer` |
| Bidi | `request_payload` + `request_receiver` (concurrent) | N× `Frame` + `Trailer` |

`request_receiver` closes on `FLAG_END_STREAM`/EOF and `cancelled` flips on `FLAG_CANCEL` — so
streaming termination is free.

**D. Client stub** (`aster/src/rpc/client.rs`). Unary via `conn.unary_call(header, request)`; streaming
via `conn.open_streaming_substream()` (streaming bypasses the shared pool per the multiplexed-streams
spec). `check_trailer` decodes `RpcStatus` → non-OK ⇒ `Err(Error::Rpc{..})`.

**E. Identity + manifest** (cross-binding-correct). A new `#[derive(AsterType)]` emits **two** things
per payload type: the `ForyStruct` wire impl **and** a compile-time `core::contract::TypeDef`
(field-kind mapping per ContractIdentity §11.3 so the canonical bytes — and thus `type_hash` /
`contract_id` — match the other bindings). `#[aster::service]` assembles a `ServiceContract`
(method patterns + request/response `type_hash`es + `requires`). Publish: build collection entries
`contract.bin` + `manifest.json` + `types/{type_hash}.bin` → `add_collection` → `publish_artifact`
(+ manifest shortcut). Consume: `resolve` → fetch collection → assert `blake3(contract.bin)==contract_id`.
**Small additive `core` changes:** `contract::ContractManifest` struct + JSON (`serde_json`),
`registry::manifest_key("manifests/{cid}")`, a `contract::build_contract_collection(...)` helper, and
`contract::evaluate_capability(req, attrs) -> bool` (port of Python `trust/rcan.py`).

**F. Auth — Gate 3** (`aster/src/rpc/server.rs` + a peer-attribute store). Pre-dispatch capability
check before request decode: read service/method `requires: CapabilityRequirement` off the registered
contract, evaluate with `core::contract::evaluate_capability` against a `CallContext.attributes` map
sourced from a per-peer **attribute store** that the **application injects** (portal-sync already
verifies attestation chains at admission and knows each peer's role). Failure ⇒ `PERMISSION_DENIED`
trailer, no payload decoded. Gate 0 (connection admission) already exists in the facade
(`Admission`/`Gate0`).

**G. `#[aster::service]` + `aster-macros`** (new proc-macro crate). Generates the `ServiceInfo`/method
table, the server `Dispatch` impl (decode→call→encode, registered by `(name, version)`), and the
client stub. **Hand-write C+D for one 4-pattern test service first; then make the macro generate
exactly that shape.**

```rust
#[aster::service(name = "AgentControl", version = 1)]
trait AgentControl {
    #[rpc(requires = role("operator"))]
    async fn assign_task(&self, req: TaskAssignment) -> Result<TaskAck>;        // unary
    async fn step_updates(&self, req: TaskId) -> BoxStream<'_, Result<StepUpdate>>; // server-stream
    async fn upload(&self, reqs: impl Stream<Item = Chunk>) -> Result<UploadAck>;   // client-stream
    async fn chat(&self, reqs: impl Stream<Item = Msg>) -> BoxStream<'_, Result<Msg>>; // bidi
}
```

**H. Interceptors** (`aster/src/rpc/interceptor.rs`). Rust trait `on_request`/`on_response`/`on_error`
over `CallContext`; port `deadline`, `retry`, `circuit-breaker`.

### Outstanding (explicitly deferred from this cut)

- **Gate 1 — enrollment → attributes.** Rust port of `trust/credentials.py` (signed
  `EnrollmentCredential` / `ConsumerEnrollmentCredential` → verified attributes), IID checks
  (`trust/iid.py`), nonce/replay store (`trust/nonces.py`). Until then, attributes are **injected by
  the application** into the attribute store; the RPC layer does not derive them from credentials itself.
- **Gate 2 — session authorize → rcan.** `authorize()` minting per-session capability grants
  (`ScopeKind::Session`). v1 supports `Shared` services; session-scoped services + rcan are future work.
- **Cross-binding RPC payloads (2B).** Gated on Fory version alignment (no JSON-mode bridge, by
  decision). Begins with a golden-byte diff of `StreamHeader`/`RpcStatus`/a sample payload vs pyfory
  0.17. Note: manifests/contract-ids are **already** cross-binding (Fory-independent), so only call
  payloads are blocked.

### portal-sync impact

**Zero today.** `rpc` is off by default and the `core` additions are additive; portal-sync consumes
only the stable facade. If portal-sync later wants to *expose* an RPC service it opts into `rpc` and
injects peer attributes from its existing admission/attestation verdict — additive, no breakage.

### Files

- New (`aster`, `rpc` feature): `aster/src/rpc/{mod,codec,status,service,server,client,interceptor}.rs`; new `aster-macros/` proc-macro crate (`#[derive(AsterType)]`, `#[aster::service]`); tests `aster/tests/rpc_*.rs`.
- Edit (`core`, additive): `contract.rs` (`ContractManifest` + JSON, `build_contract_collection`, `evaluate_capability`), `registry.rs` (`manifest_key`).
- Edit: `aster/Cargo.toml` (`rpc = ["dep:fory-core","dep:fory-derive","dep:aster-macros"]`), root `Cargo.toml` (`aster-macros` member).

### Sequencing

1. **Codec** (A) + golden self-byte tests — **before any macro work**.
2. **Status/errors** (B).
3. **Hand-written dispatch + stub** (C, D), all four patterns, Rust↔Rust round-trip tests.
4. **Identity + manifest** (E) incl. the additive `core` bits; publish/resolve/verify test.
5. **Auth Gate 3** (F).
6. **`#[aster::service]` macro** (G) generating 3–4's shape.
7. **Interceptors** (H).
8. **`rpc` feature wiring** + `--no-default-features` compile matrix.

---

## Verification

**Step 1 (now) — gate is the portal-sync contract checklist above:**
- `cargo build -p aster` and `cargo build -p aster --no-default-features --features blobs` (feature matrix compiles).
- `cargo clippy -p aster -- -D warnings`, `cargo fmt --check`.
- `aster/examples/quickstart.rs`: start a persistent node, `blobs().add_path(...)`, `docs().create()/set_bytes()`, `shutdown()`, reopen, assert data survives — mirrors `core/tests/blob_persistence_contract.rs::persistent_docs_and_blobs_survive_close_and_reopen`.
- `aster/tests/identity.rs`: start persistent node, `export_secret_key()`, `shutdown()`, restart from the **same** `secret_key` + data_dir, assert `node.id()` is unchanged (restart-stable identity — required by portal-sync).
- `aster/tests/namespace.rs`: `open_or_import_write_namespace(NamespaceSecret::from_bytes(seed))` on two nodes yields the **same** `doc_id()` (== `secret.id()`); calling it twice on one node reattaches (idempotent), not errors; a third node `open_or_import_read_namespace(id)` gets a read-only view. **Capability upgrade:** a node that first `open_or_import_read_namespace(id)` (read-only), then `open_or_import_write_namespace(secret)` for the same namespace, ends up with a **writable** doc and a subsequent `set_bytes` succeeds.
- `aster/tests/admission.rs`: with `hooks(true)`, an **inbound** peer using a non-admission ALPN is surfaced via `Admission::next_handshake()`; `reject(code, reason)` provably blocks the connection (the dialer's stream/connection fails with the close code), and `accept()` lets an admission-ALPN peer through. (`next_connect()` is exercised separately for the outbound dial gate.)
- `aster/tests/facade.rs`: two in-memory nodes connect; round-trip a blob (incl. `download_hash` from a specific peer) and a doc entry with a live `InsertRemote { from }` event.
- Git-dependency sanity check: in a scratch crate **outside** the workspace, depend on `aster` via git + the copied `[patch.crates-io]` block and run the quickstart — confirms external consumption resolves the forks.
- No regression to `core`: `cargo test -p aster_transport_core` and `uv run pytest tests/python/ -q`.

**Step 2 (2A — Rust-native) acceptance:** `cargo build -p aster --features rpc` and `--no-default-features --features rpc`; golden self-byte tests pinning `StreamHeader`/`RpcStatus`/`CallHeader`; Rust↔Rust round-trip per call pattern (unary/server/client/bidi), mirroring the hermetic `aster/tests/facade.rs` style; manifest publish→resolve→`blake3==contract_id` verify; a `requires`-gated method rejected with `PERMISSION_DENIED` when the peer lacks the attribute and admitted when present. **2B (cross-binding payloads):** golden-byte diff of envelope + a sample payload vs pyfory 0.17, then the Rust↔Python/Java/TS RPC matrix — gated on Fory alignment. (Manifest/contract-id cross-binding parity can be asserted earlier, since identity is Fory-independent.)

---

## Implementation status & portal-sync migration

**Implemented (2026-05-30).** New `aster/` workspace member; `core` gained three
additive `CoreDocsClient` methods (`import_write_namespace`,
`import_read_namespace`, `default_author`). Verified: `cargo build -p aster`
(full + `--no-default-features --features blobs`), `cargo clippy --all-features`
(clean), `cargo fmt --check`, 8 integration tests (`facade`, `identity`,
`namespace`, `admission`) + the `quickstart` example, and `cargo test -p
aster_transport_core` (no regression). The cross-node tests are hermetic: relay
disabled + bind `127.0.0.1:0` + `add_peer` (no network / no `online()`).

### Public surface (what portal-sync calls)

- `Node::start(AsterConfig)` / `start_with_alpns`; `id()`, `addr()`, `add_peer()`,
  `export_secret_key()`, `take_admission()`, `shutdown()`; `blobs()/docs()/gossip()`.
- `AsterConfig::builder().relay(..).persistent(..).bind_addr(..).secret_key(..).discovery(..).monitoring(..).hooks(..).build()`.
- `Blobs`: `add_bytes`, `add_path`, `add_path_with_named_tag`, `read_to_bytes`,
  `has`, `status`, `download_hash`, `tag_set`, `tag_delete`, `tag_delete_prefix`.
- `Docs`: `create`, `create_author`, `default_author`, `open`,
  `open_or_import_write_namespace`, `open_or_import_read_namespace`, `join`,
  `join_and_subscribe`, `join_and_subscribe_namespace`. `Doc`: `id`, `set_bytes`,
  `get_exact`, `query_*`, `read_entry_content`, `share`/`share_with_addr`,
  `start_sync`, `leave`, `subscribe`, `set/get_download_policy`. Free fn
  `aster::default_author_id(&SecretKey) -> AuthorId` (offline derivation).
- `Gossip`/`GossipTopic`: `subscribe`, `broadcast`, `recv`.
- Custom-ALPN connections: `Node::start_with_alpns`, `Node::connect(&peer, alpn)`
  (outbound by id), `Node::connect_addr(NodeAddr, alpn)` (outbound by address
  material — e.g. to present a credential before a docs join),
  `Node::add_node_addr(NodeAddr)` (register a peer's address book entry so later
  by-id connects / docs joins resolve), `Node::accept() -> (alpn, Connection)`
  (inbound). `Connection`: `peer`, `alpn`, `open_bi`/`accept_bi` → (`SendStream`,
  `RecvStream`), `close`, `closed` (keep-alive until the peer is done).
  `SendStream`: `write_all`, `finish`. `RecvStream`: `read`, `read_exact`,
  `read_to_end`, `stop`.
- `Ticket`: compact base58 address-material + optional `Credential`
  (`Open`/`ConsumerRcan`/`Enrollment`/`RegistryRead`). `new`, `from_node_addr`
  (mint from `node.addr()` — the first-class path), `from_base58`, `to_base58`,
  `node_id`, `relay`, `direct_addresses`, `credential`, `to_node_addr() ->
  Result<NodeAddr>` (retains all dialable material). Wraps core `AsterTicket`;
  Aster owns the relay interpretation — `from_node_addr` folds an IP-based relay
  URL into the compact IP:port slot (a DNS-hostname relay is deferred to
  discovery-by-id), and `to_node_addr` reconstructs it as `https://<ip>:<port>`.
  Consumers never decompose the `aster1` token by hand: `Node::connect_ticket`
  dials straight from a ticket and `Node::add_ticket_addr` registers it
  (returning the peer `NodeId`).
- `Admission`: `next_handshake()` (inbound Gate 0), `next_connect()` (outbound);
  `accept()` / `reject(code, reason)`. `Gate0` policy (clone-shared): `admit`,
  `revoke`, `is_admitted`, `should_allow(&peer, alpn)` (admission ALPNs always
  open, else peer must be admitted). `alpns::{PRODUCER_ADMISSION,
  CONSUMER_ADMISSION, DELEGATED_ADMISSION}`.
- `attestation`: `Chain`, `attest_root_node`, `verify_chain`, `public_key`,
  `AttestOptions`; `Verified { node, anchor, intermediates, depth }` with role
  predicates `is_node` / `is_anchor` / `is_intermediate` / `role_of` and the
  `Role` enum (`Node` / `Anchor` / `Intermediate`).
- Newtypes: `NodeId`, `Hash`, `AuthorId`, `NamespaceId`, `NamespaceSecret`,
  `PublicKey`, `SecretKey`, `NodeAddr` — no iroh / iroh-docs types in the public API.

**Default docs author = the node identity key, so `AuthorId == NodeId`.**
`CoreNode::finalize` imports the node's own Ed25519 secret as the docs author and
sets it default (a `core` change affecting all bindings). A doc entry's author is
therefore exactly the node that wrote it — directly verifiable against the
attestation chain and QUIC handshake, with **no** separate author↔node binding to
publish. `default_author_id(&secret)` returns it offline (== node id).
Trade-offs of this identity reuse: (1) on a persistent node the node secret is
written into the docs author store (`docs.redb`); (2) author *export* is therefore
deliberately **not** surfaced by Aster (`core` and bindings never wrap iroh-docs
`author_export`) so the secret can't be pulled via the docs API — the only
sanctioned door to the node identity is `export_secret_key()`. (A hard guard in
the iroh-docs fork is possible if belt-and-suspenders is wanted.)

### portal-sync migration

1. **Dependency.** Replace the direct `aster_transport_core` / iroh-blobs /
   iroh-docs dependencies with:
   ```toml
   [dependencies]
   aster = { git = "https://github.com/aster-rpc/aster-rpc-internal", branch = "main" }
   ```
   and copy the `[patch.crates-io]` block from this repo's root `Cargo.toml`
   into portal-sync's root `Cargo.toml` (mandatory — see `aster/README.md`).

2. **Node + identity.** Replace `CoreNode::persistent_with_alpns(..)` with
   `Node::start(AsterConfig::builder().persistent(dir).secret_key(sk).build())`.
   Persist `export_secret_key()` yourself for a restart-stable `node.id()` (the
   FsStore does not store the endpoint key).

3. **portal-cas blob surface.** `add_path` / `add_path_with_named_tag` /
   `read_to_bytes` map 1:1; tag GC via `tag_set` / `tag_delete` /
   `tag_delete_prefix`; presence via `has` / `status`; fetch-from-peer via
   `download_hash(&hash, &peer, BlobFormat::Raw)`.

4. **Manifest sync (docs).** Use `default_author()` as the node author — it
   **equals the node id**, so a synced entry's `author` *is* the node that wrote
   it, attributable directly (the same value the attestation chain binds and the
   QUIC handshake proves — no separate binding to publish or look up).
   `open_or_import_write_namespace(NamespaceSecret::from_bytes(blake3_seed))` for
   the deterministic `portal-node-config` / `portal-tree-config` namespaces
   (idempotent, upgrades read→write); `share_with_addr` + `join_and_subscribe`
   for sharing; live `DocEvent::InsertRemote { from, entry }` + `ContentReady`
   for fetch-on-read (read content after `ContentReady`, or retry the read).

5. **Gate 0.** Start with `.hooks(true)`, take `Admission`, and drive
   `next_handshake()` — `accept()` / `reject(code, reason)` per peer.

**Stop reaching into iroh.** After migration portal-sync should not import
`iroh_blobs` / `iroh_docs` directly; all surfaces are on `aster`.

---

## Ownership attestations (implemented 2026-05-30)

Per the locked spec (`docs/_internal/ownership-attestations.md`). First implementation in any language; built in **`core`** (single authoritative encoder; all bindings can reach it via FFI later) and exposed through the `aster` facade.

**Decisions:** logic in `core` + facade; wire format via the **Apache Fory Rust crate** (`fory-core`/`fory-derive` `1.1.0-rc.1`, `#[derive(ForyStruct)]`, `register_by_name`); scope = single-edge `attest_root_node` mint + an **N-tier verifier** (root→node, anchor/root/intermediate delegation, and `intermediate→intermediate` stacks via a positional leaf/middle/top form grammar; tested through 3 edges).

**Core (`core/src/attestation.rs`, gated by `attestation` feature → pulls `fory-core`/`fory-derive`/`thiserror`/`base64`):**
- Fory `@WireType` structs `Statement` / `AttestationBody` / `AttestationEdge` / `AttestationChain` (registered as `aster.attestation.{statement,body,edge,chain}` / `v1`).
- `attest_root_node(root_secret, node_secret, opts) -> Vec<u8>` — reciprocal Ed25519 (parent + child both sign `b"aster.attestation.v1\0" + body_bytes`); leaf-first single edge.
- `verify_chain(chain_bytes, &[anchor;32], &expected_node) -> VerifiedChain` — size gate → per-edge structural (version, 2–3 statements, sig count, 32-byte ids, 64-byte sigs, time bounds) → leaf binds to expected node → bridging (`bodies[i+1].statements[1] == bodies[i].statements[0]`) → top anchor ∈ trusted set → all signatures. Typed `AttestationError`.
- `encode_text`/`decode_text` for `aster.attestation.chain.v1:<base64url>`; `public_key(secret)` helper.
- **Fory runtime built once** in a `OnceLock<Fory>` (`Fory::builder().xlang(true).compatible(true)`, all 4 types pre-registered) — never re-initialised per call (per Fory's reuse guidance).

**Facade (`aster/src/attestation.rs`, `attestation` feature, default-on):**
- `Chain` (opaque artifact: `from_bytes`/`as_bytes`/`into_bytes`/`to_text`/`from_text`).
- `attest_root_node(&SecretKey, &SecretKey, &AttestOptions) -> Result<Chain>`.
- `verify_chain(&Chain, &[PublicKey], &PublicKey) -> Result<Verified>` (`Verified { node, anchor, depth }`).
- `public_key(&SecretKey) -> PublicKey`; new `PublicKey` newtype. No `fory`/`ed25519` types leak.

**Tests:** 6 core unit tests + 4 facade tests (mint/verify, text round-trip, reject untrusted-anchor/wrong-node/expired, tamper detection). `--no-default-features --features blobs` confirms Fory is excluded when attestation is off.

### portal-sync usage (Gate 0)

The attestation identity **is** the transport identity — verified by test:
`attestation::public_key(node.export_secret_key()).to_hex() == node.id()`. So at
admission, the peer's `NodeId` *is* the `expected_node` to verify against:
`PublicKey::from_hex(req.peer.as_str())`.

`HandshakeRequest` exposes only `peer` / `alpn` / `is_alive` — **no payload**. A
chain cannot be read from the handshake itself, so the supported model is:

- **Pre-enrolled (use this today).** The daemon maintains a `node_id → Chain`
  map, populated *before* admitting traffic — synced via a config doc (a
  deterministic `open_or_import_write_namespace` namespace) or shipped in config.
  - Vendor root mints once: `attest_root_node(&root_sk, &node_sk, &opts) → Chain`;
    publish `chain.to_text()` into the config doc keyed by the node's id.
  - In `Admission::next_handshake()`: look up `req.peer`'s chain; call
    `verify_chain(&chain, &trusted_anchors, &PublicKey::from_hex(req.peer.as_str())?)`.
    `accept()` iff the chain is present **and** verifies; else `reject(403, …)`.
    Fully offline.
- **Bootstrap-ALPN (now available).** Admit an unknown peer on a dedicated
  admission ALPN, verify its chain on that connection, then allow it on normal
  ALPNs. Uses the custom-ALPN connection API + `Gate0`:
  - Start with the admission ALPN registered:
    `Node::start_with_alpns(cfg.hooks(true), vec![alpns::PRODUCER_ADMISSION.into(), b"portal.rpc".into()])`,
    and a shared `let gate = Gate0::new();`.
  - **Hook loop** (gate normal ALPNs): take `Admission`; for each
    `next_handshake()`, `req.accept()` iff `gate.should_allow(&req.peer, &req.alpn)`
    (admission ALPNs are always allowed), else `reject(403, …)`.
  - **Admission accept loop**: `node.accept()` → on the admission ALPN,
    `conn.accept_bi()`, read the chain bytes, `verify_chain(&Chain::from_bytes(..),
    &anchors, &PublicKey::from_hex(conn.peer().as_str())?)`; on success
    `gate.admit(conn.peer())` and reply, keeping the connection alive with
    `conn.closed().await` until the peer has read.
  - Thereafter the peer's normal-ALPN connections pass the hook.
  Proven end-to-end by `aster/tests/connection.rs::bootstrap_alpn_admission_gates_normal_alpn`.
  (`Gate0`'s admitted set is in-memory; persist/expire it in your daemon if needed.)

**Caveats / deferred:** epoch replay-cache is not enforced in the stateless verifier (no per-root state) — callers track epochs if needed. Cross-binding wire compat is unproven: the Rust crate is Fory **1.1.0-rc.1** while pyfory here is **0.17** — likely incompatible until the Fory upgrade lands; portal-sync is Rust↔Rust today. Intermediate (multi-edge) **minting**, revocation, and trust publication remain future work — the verifier already validates N-tier chains (incl. `intermediate→intermediate`), so only the mint helpers are missing.
