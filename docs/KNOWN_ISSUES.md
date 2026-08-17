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

## An explicit docs `start_sync` may be coalesced with an in-flight sync

**Scope:** Aster's docs `start_sync` facade delegates to iroh-docs. For each
namespace and peer, iroh-docs permits only one set-reconciliation sync at a
time. If an explicit `start_sync` requests a `DirectJoin` while that peer is
already syncing, `PeerState::start_connect` returns without starting another
operation. Unlike a concurrent `NewNeighbor` or `SyncReport` request, a
`DirectJoin` does not currently set `resync_requested`, so no follow-up pass is
guaranteed. The RPC still returns success because it reports that the request
was accepted by the live actor, not that a distinct reconciliation ran.

This surfaced while repairing
`sync_redrives_known_missing_content_after_resync`: the reconciliation queued by
phase one can still be running when phase two calls `start_sync`. In that
interleaving the explicit `DirectJoin` is coalesced away, while the already
running `Resync` can legitimately perform the missing-content redrive. A test
must therefore assert the semantic result (`ContentReady`, followed by
`PendingContentReady`, and the expected bytes) rather than require a
phase-specific `SyncFinished` origin. `SyncFinished` carries no request or
attempt identifier with which to make that attribution.

**Impact:** This is a contract question and a potential product gap, not a
confirmed data-loss defect. Coalescing is harmless when the in-flight operation
covers the caller's intent or its completion performs the required content
redrive. It is insufficient if callers understand `start_sync` to guarantee a
reconciliation whose linearization point is after their call: a running sync
may have taken its snapshot earlier, and the explicit request currently creates
neither a distinct attempt nor a queued pass. Callers also cannot tell whether
their request started work or was coalesced.

**Decision required:** Define one of these contracts before changing the state
machine:

- **Ensure-running semantics:** `start_sync` means that a synchronization is
  already running or has been started. Document the coalescing explicitly and,
  if callers need to observe it, return a typed `Started` / `Coalesced` outcome.
  The contract must state what protects updates outside the in-flight sync's
  snapshot.
- **Follow-up semantics:** an explicit call made during a running sync promises
  another reconciliation afterwards. Queue `DirectJoin` like `NewNeighbor` and
  `SyncReport` (or use an equivalent request-generation mechanism), while still
  coalescing repeated requests into at most one follow-up pass.

Whichever contract is selected needs state-machine coverage for idle and
running `DirectJoin`, repeated calls, completion of the in-flight operation,
and preservation of the existing `NewNeighbor` / `SyncReport` behaviour. An
integration test should hold one sync in flight, make the explicit call after
the state it needs reconciled changes, and assert convergence without assigning
uncorrelated `SyncFinished` events to test phases.

This decision should remain separate from the row-20 test-oracle repair unless
the semantic collector still demonstrates missing content. The test can be made
correct without silently choosing a stronger public `start_sync` contract.
