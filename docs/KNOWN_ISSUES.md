# Known issues

Tracked gaps and caveats in the Aster API surface. Each entry notes scope,
impact, and what closing it would take.

## Additional blob GC protection callbacks are Rust-only

Blob GC and identity-derived HPKE keys are exposed in Rust, Python, TypeScript
and the C ABI. Python config uses `gc_interval_ms`, TypeScript uses
`gcIntervalMs`, and the C ABI uses `aster_config_t.gc_interval_ms` for periodic
GC. A manual pass is available through Python `gc_run_once()`, TypeScript
`gcRunOnce()` and C `aster_blobs_gc_run_once`.

GC protects tags, temporary tags and the iroh-docs live-content set. If
protected-content enumeration fails, the sweep is skipped rather than risking
collection of live document content.

An embedder-supplied `ProtectCb` for an additional protected set is a Rust
closure and has no foreign-language binding. Applications using the bindings
must retain additional content through supported tags or document references.

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
