# Session Instructions: Registry Rust Centralization

Design doc: `ffi_spec/registry-rust-centralization.md`
Python reference implementation: `bindings/python/aster/registry/`

## Status summary (2026-04-13)

The first-cut centralization has landed. The pure resolution logic (mandatory
filters + ranking) now lives in Rust and every FFI binding calls into it
instead of re-implementing the rules. The async doc-backed operations
(resolve / publish / renew_lease / ACL add/remove/list_writers) are now
exposed through the event-based FFI model as well, with persistent
ResolveState and per-doc RegistryAcl on the bridge — closing items A and B.
Per-language wiring of the new async FFI ops, the Python switchover
(item C) and differential tests (item D) are still outstanding.

### Done

- **Phase 1 — data models + keys** ported to Java, Go, .NET. Each binding
  compiles clean.
  - Java: `bindings/java/src/main/java/com/aster/registry/` (Jackson for
    JSON; `jackson-databind` added to `pom.xml`).
  - Go: `bindings/go/registry.go` (encoding/json).
  - .NET: `bindings/dotnet/src/Aster/Registry/` (System.Text.Json).

- **Phase 2 — Rust registry module**: `core/src/registry.rs` (~750 lines).
  - Serde wire types (`ArtifactRef`, `EndpointLease`, `GossipEvent`,
    `ServiceSummary`) matching the Python JSON shape.
  - Key helpers (`contract_key`, `version_key`, `channel_key`, `lease_key`,
    `lease_prefix`, `acl_key`) and `REGISTRY_PREFIXES` constant.
  - `ResolveOptions`, `ResolveState` (per-contract round-robin + monotonic
    `lease_seq` cache), `apply_mandatory_filters` (5 filters in normative
    order), `rank` (round_robin / least_load / random, READY before
    DEGRADED).
  - Async doc I/O: `resolve`, `list_leases`, `publish_lease`,
    `publish_artifact`, `renew_lease`.
  - `RegistryAcl` with open/restricted modes, `reload` / `add_writer` /
    `remove_writer`.
  - **12 unit tests passing** covering every filter rule, all three
    ranking strategies, seq monotonicity, key-schema parity with Python,
    ACL open mode, and `GossipEvent` round-trip.

- **Phase 3 — FFI exposure (pure-function subset)** in `ffi/src/lib.rs` +
  `ffi/iroh_ffi.h`:
  - `aster_registry_now_epoch_ms` — shared wall clock.
  - `aster_registry_is_fresh` — freshness check against a lease JSON.
  - `aster_registry_is_routable` — READY/DEGRADED check.
  - `aster_registry_filter_and_rank` — the critical centralized logic.
    Takes a JSON array of leases + `ResolveOptions` JSON, returns the
    ranked survivors JSON.
  - `aster_registry_key` — produce a registry doc key (6 kinds via enum).

- **Phase 4 — language wiring** for the pure-function FFI subset:
  - Java: `IrohLibrary` adds the five method handles; high-level
    `bindings/java/.../registry/Registry.java` wrapper uses Jackson to
    encode/decode through the FFI.
  - Go: `bindings/go/registry_ffi.go` cgo bindings +
    `ResolveOptionsJSON` + `FilterAndRankRust` / `IsFreshRust` /
    `RegistryKeyRust`.
  - .NET: `Native.cs` declarations + `bindings/dotnet/src/Aster/Registry/
    Registry.cs` wrapper.

All three bindings build clean; Rust unit tests pass; FFI crate builds
clean.

## Outstanding

### A. Async doc-backed FFI operations — DONE (Rust side)

All six async operations are now exposed through the event-based FFI
operation model in `ffi/src/lib.rs` (and declared in `ffi/iroh_ffi.h`):

1. `aster_registry_resolve(runtime, doc, opts_json, user_data, out_op)`
   — full pipeline against a real doc handle. Emits
   `IROH_EVENT_REGISTRY_RESOLVED` (80); payload is EndpointLease JSON
   (NOT_FOUND status when no candidate survives).
2. `aster_registry_publish(runtime, doc, author_id, lease_json,
   artifact_json, service, version, channel, gossip_topic, ...)` —
   wraps `publish_lease` + `publish_artifact`. Either lease/artifact may
   be empty. Emits `IROH_EVENT_REGISTRY_PUBLISHED` (81).
3. `aster_registry_renew_lease(runtime, doc, author_id, service,
   contract_id, endpoint_id, health, load, lease_duration_s,
   gossip_topic, ...)` — wraps `core::registry::renew_lease`. `load`
   uses NaN as the "no load reported" sentinel. Emits
   `IROH_EVENT_REGISTRY_RENEWED` (82).
4. `aster_registry_acl_add_writer(runtime, doc, author_id, writer_id,
   ...)` and `aster_registry_acl_remove_writer(...)` — wraps
   `RegistryAcl::add_writer` / `remove_writer`. Both emit
   `IROH_EVENT_REGISTRY_ACL_UPDATED` (83).
5. `aster_registry_acl_list_writers(runtime, doc, ...)` — emits
   `IROH_EVENT_REGISTRY_ACL_LISTED` (84) with a JSON array payload of
   AuthorId strings (empty when the ACL is in open mode).

Bridge state added to support these:
- `registry_state: ResolveState` on `BridgeRuntime` — persistent
  per-contract round-robin counter and monotonic `lease_seq` cache,
  shared across every resolve call.
- `registry_acls: Mutex<HashMap<u64, Arc<RegistryAcl>>>` keyed by doc
  handle, with a `registry_acl_for_doc` lazy-create helper. Each doc
  gets its own trust set, in open mode until the first writer is added.

### B. Stateful round-robin across FFI boundary — DONE

The persistent `ResolveState` on the bridge means rotation now survives
across calls naturally. The pure-function `aster_registry_filter_and_rank`
is still stateless on its own (callers pass leases through it) but the
new `aster_registry_resolve` op uses the bridge state. Per-binding
rotation counters are no longer needed once a binding migrates to the
async resolve op.

### A'. Per-language wiring (still pending)

Java / Go / .NET / Python still need:
- New method handles in `IrohLibrary.java`, `Native.cs`, cgo bindings
  in `bindings/go/registry_ffi.go`, and PyO3 wrappers on the Python
  side.
- High-level wrappers that submit the op and pump the event queue for
  the matching `IROH_EVENT_REGISTRY_*` kind.

### C. Delete the Python reference implementation

The design doc step 5 says Python keeps the per-language logic "until
Rust is verified, then deletes". Once the async operations (A.1–A.6)
land and the Python binding switches to calling them, the following
files can be deleted:
- `bindings/python/aster/registry/client.py` (RegistryClient — 408 lines)
- `bindings/python/aster/registry/publisher.py` (RegistryPublisher — 386
  lines)
- `bindings/python/aster/registry/acl.py` (RegistryACL — 150 lines)
- `bindings/python/aster/registry/gossip.py` (RegistryGossip — 133 lines)

The data models (`models.py`) and keys (`keys.py`) stay — they're the
Python side of the per-language wire types and should match the Rust
serde shapes exactly.

### D. Differential tests

Once (A) lands, add a cross-binding test that:
1. Spawns a Java/Go/.NET publisher writing a lease.
2. Has a Python consumer resolve it.
3. Verifies that `(service, version) → EndpointLease` produces the same
   winner across all four bindings for the same candidate set.

This was the whole point of centralizing the logic — worth a test.

## Key files

- Design: `ffi_spec/registry-rust-centralization.md`
- Rust module: `core/src/registry.rs`
- FFI surface: `ffi/src/lib.rs` (bottom), `ffi/iroh_ffi.h` (bottom)
- Java: `bindings/java/src/main/java/com/aster/registry/`,
  `bindings/java/src/main/java/com/aster/ffi/IrohLibrary.java` (tail)
- Go: `bindings/go/registry.go`, `bindings/go/registry_ffi.go`
- .NET: `bindings/dotnet/src/Aster/Registry/`,
  `bindings/dotnet/src/Aster/Native.cs` (tail)
- Python reference: `bindings/python/aster/registry/`

## Build verification commands

```bash
cargo test -p aster_transport_core registry::      # 12 tests
cargo build -p aster_transport_ffi
cd bindings/java && mvn compile -P fast
cd bindings/dotnet/src/Aster && dotnet build
cd bindings/go && CGO_CFLAGS="-I$(pwd)/../../ffi" \
    CGO_LDFLAGS="-L$(pwd)/../../target/release/deps -l aster_transport_ffi" \
    go build .
```
