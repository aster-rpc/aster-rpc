# Known Issues

Tracked gaps and caveats in the Aster API surface. Each entry notes scope,
impact, and what closing it would take.

## HPKE identity-derivation is Rust-only

**Scope:** `hpke_x25519_secret_from_identity`, `hpke_x25519_public_from_identity`,
and `hpke_x25519_public_from_node_id` are exposed on the Rust `aster` facade
(`aster/src/crypto.rs`) only. They have **no** Python, TypeScript, or FFI
(Java/Kotlin) bindings yet.

**Impact:** Low. The only current consumer is portal-sync, whose daemon/CLI is
Rust, so the facade is sufficient. Cross-language callers cannot derive an HPKE
recipient keypair from an Aster (Ed25519) identity.

**Closing it:** The logic lives in `aster_transport_core::hpke_envelope`
(`hpke_x25519_secret_from_identity` / `hpke_x25519_public_from_identity`), so each
binding is a thin wrapper following the existing `hpke_*` pattern already present
in those layers:

- Python — `bindings/python/rust/src/crypto.rs`
- TypeScript — `bindings/typescript/native/src/crypto.rs`
- FFI (Java/Kotlin) — `ffi/`

No core changes are required; this is mechanical parity work to be done when a
cross-language consumer needs it.

## Blob GC (`gc_interval` + `gc_run_once`) is Rust-only

**Scope:** Opt-in blob garbage collection is exposed on the Rust `aster` facade
only:

- `AsterConfig::gc_interval` / `AsterConfigBuilder::gc_interval(Duration)`
  (`aster/src/config.rs`) — enable the periodic GC loop.
- `Blobs::gc_run_once()` (`aster/src/blobs.rs`) — deterministic single-pass
  mark+sweep.

There is **no** Python, TypeScript, or FFI (Java/Kotlin) surface for either yet.

**Impact:** Low. The only current consumer is portal-sync, whose daemon/CLI is
Rust, so the facade is sufficient. Cross-language callers cannot enable GC or
trigger a manual sweep.

**Closing it:** The plumbing already terminates in the core layer, so each
binding is a thin wrapper — no further core work needed:

- Core entry points (done): `CoreNode::memory_with_alpns_and_gc` /
  `persistent_with_alpns_and_gc` take `gc_interval: Option<Duration>`;
  `CoreBlobsClient::gc_run_once` runs one pass (`core/src/lib.rs`). The
  signature-stable `*_with_alpns` constructors still exist and delegate with
  `None`, so existing binding call sites are unchanged until they opt in.
- Each binding needs: (1) a config field carrying the interval — for FFI, use a
  `gc_interval_ms: Option<u64>` form for ABI simplicity — plumbed into the
  `*_with_alpns_and_gc` constructor instead of `*_with_alpns`; and (2) a
  `gc_run_once` method on its blobs handle forwarding to
  `CoreBlobsClient::gc_run_once`. Touch points mirror the existing node/blob
  wrappers:
  - Python — `bindings/python/rust/src/node.rs`, `blobs.rs`
  - TypeScript — `bindings/typescript/native/src/node.rs`, `blobs.rs`
  - FFI (Java/Kotlin/Go/.NET) — `ffi/src/lib.rs` (+ `ffi/iroh_ffi.h`)

**Embedder protect callback (not yet exposed):** the core constructors pass
`add_protected: None` to iroh-blobs' `GcConfig`, so tags (and temp-tags) are the
sole retention root. iroh-blobs also supports a `ProtectCb` with abort-on-error
semantics for embedders that compute a protected set out-of-band. It is a Rust
closure that does not cross the FFI boundary, so exposing it is deferred until a
concrete consumer needs more than tag-driven retention.
