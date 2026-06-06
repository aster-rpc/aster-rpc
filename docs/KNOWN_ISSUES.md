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

**Retention roots (what GC protects):** tags, temp-tags, **and** the iroh-docs
live-content set. The core constructors wire iroh-docs' own GC protect callback
into `GcConfig.add_protected` (and replay it in `gc_run_once`), because iroh-docs
stores entry content as *untagged* blobs in the shared store — without this a
sweep would delete synced docs content. This is internal and abort-on-error (a
failed enumeration skips the sweep rather than over-collecting); see
`docs/portal-blob-gc-requirements.md`.

**Embedder-supplied protect callback (not yet exposed):** beyond the internal
docs callback, iroh-blobs' `GcConfig` accepts an arbitrary `ProtectCb` for
embedders that compute an *additional* protected set out-of-band. It is a Rust
closure that does not cross the FFI boundary, so exposing a user-supplied one is
deferred until a concrete consumer needs more than tag- and docs-driven
retention.
