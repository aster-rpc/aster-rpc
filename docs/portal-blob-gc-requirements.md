# Requirement: opt-in blob garbage collection (GcConfig + manual sweep)

Status: **requested by a consumer (portal-sync)** — design/spec for Aster to implement
Date: 2026-06-06
Requested by: portal-sync (`~/dev/emrul/portal-sync`), see
`docs/phase5-gc-lifecycle.md` there for the consumer-side design.

## Summary

Aster exposes no way to enable iroh-blobs garbage collection. `CoreNode::
persistent_with_alpns` calls `FsStore::load(&root)` and `memory_with_alpns` uses
`MemStore::new()`, neither of which passes a `GcConfig`. The store is therefore
**grow-only**: tags control retention, but nothing ever collects untagged blobs.

portal-sync uses tags as a reference count (one tag per referencing object; a
blob is live while ≥1 tag points at it) and needs untagged blobs to actually be
reclaimed. We need Aster to **opt into iroh-blobs GC** and to expose a
**deterministic manual sweep** for tests.

The underlying machinery already exists in the vendored iroh-blobs
(`docs/_internal/iroh-blobs/src/store/gc.rs`): `GcConfig { interval,
add_protected }`, `run_gc(store, config)` (timed loop), `gc_run_once(store, &mut
live)` (single pass), and `FsStore::load_with_opts(db_path, Options { gc, .. })`
/ `MemStore` with `gc_config`. Tags auto-protect via `gc_mark`. This is plumbing,
not new GC logic.

## What portal-sync needs (acceptance criteria)

### 1. Opt-in GC config on `AsterConfig` (backward-compatible)

Add a GC option to `AsterConfig` + `AsterConfigBuilder`, **defaulting to OFF** so
existing consumers are unchanged:

```rust
// AsterConfig (new field), e.g.:
/// Enable periodic blob GC at this interval. `None` (default) disables GC
/// entirely — the store keeps its current grow-only behavior.
pub gc_interval: Option<Duration>,   // or gc_interval_ms: Option<u64> for FFI parity
```

```rust
// AsterConfigBuilder:
pub fn gc_interval(mut self, interval: Duration) -> Self { ... }
```

When `Some(interval)`:
- persistent nodes: `FsStore::load_with_opts(root, Options { gc:
  Some(GcConfig { interval, add_protected: <see #3> }), ..Default })` instead of
  `FsStore::load(root)`.
- memory nodes: the `MemStore` equivalent (`gc_config: Some(GcConfig { .. })`).

When `None`: unchanged (`FsStore::load` / `MemStore::new`) — **no behavior change
for current consumers**.

### 2. Tags must remain the retention root (confirm + keep)

A blob with ≥1 persistent tag (or temp tag) pointing at it must survive a GC
sweep; a blob with **no** tag/temp-tag and no other protection must be collected.
This is the standard iroh-blobs `gc_mark` behavior — we just need it actually
running. No change to the tag API (`tag_set` / `tag_delete` / `tag_delete_prefix`
/ `add_path_with_named_tag`); portal-sync drives retention entirely through tags.

### 3. Fail-safe protected-set guard (abort, never over-collect)

GC is destructive and the blob store is shared across all of a node's trees **and
the NFS surface**, so a buggy/incomplete protected set must never delete live
data. Use the iroh-blobs `add_protected: Option<ProtectCb>` hook with
**abort-on-error semantics**: if computing the protected set fails, return
`ProtectOutcome::Abort` so the sweep is **skipped**, not run with a partial set.

Minimum: Aster may pass `add_protected: None` (tags alone protect) — acceptable,
since portal-sync's retention is 100% tag-driven. **Preferred:** expose a way for
the embedder to supply a `ProtectCb` (or at least guarantee that an internal mark
error aborts the run). Document whichever is provided.

### 4. Manual, deterministic sweep — `gc_run_once`

Tests must be able to **trigger a single GC pass synchronously** rather than wait
for the timed interval. Expose, on the node or blobs handle:

```rust
/// Run exactly one GC mark+sweep pass now and return when it completes.
/// Collects blobs not protected by any tag/temp-tag (and not added by the
/// protect callback). No-op-safe to call when GC config is None.
pub async fn gc_run_once(&self) -> Result<()>;
```

This wraps iroh-blobs `gc_run_once(store, &mut live)`. It is the single most
important item for the consumer's test harness — without it, reclamation tests
are timing-dependent and flaky.

### 5. Semantics to document

- GC is **node-wide** (one shared blob store); it reclaims any blob no tag
  protects, regardless of which tree tagged it. Consumers must keep a live tag
  for every blob they want to retain.
- Interval is best-effort; `gc_run_once` is the deterministic entry point.
- Reclamation is of **local** bytes only; it does not affect other replicas.

## Non-goals

- No new GC *algorithm* — reuse iroh-blobs `run_gc`/`gc_run_once` verbatim.
- No tag-API changes.
- No change to default behavior (GC stays off unless `gc_interval` is set).

## Suggested tests (Aster side)

1. **Default off:** a node built without `gc_interval` never collects an untagged
   blob (current behavior preserved).
2. **Tagged survives:** with GC on, `add_bytes` + `tag_set` → `gc_run_once` →
   blob still present.
3. **Untagged collected:** with GC on, `add_bytes` (temp tag dropped) +
   `tag_delete` → `gc_run_once` → blob gone.
4. **Dedup/refcount:** two tags → one hash; delete one tag → `gc_run_once` →
   present; delete the second → `gc_run_once` → gone.
5. **Manual sweep determinism:** `gc_run_once` reclaims within the single call (no
   sleep needed).
6. **Abort-on-error:** a protect callback returning `Abort` leaves all blobs
   intact.

## Pointers (already in-tree)

- `docs/_internal/iroh-blobs/src/store/gc.rs` — `GcConfig`, `ProtectCb`,
  `ProtectOutcome`, `run_gc`, `gc_run_once`, `gc_mark`.
- `docs/_internal/iroh-blobs/src/store/fs.rs:1398` — `FsStore::load_with_opts`.
- `docs/_internal/iroh-blobs/src/store/fs/options.rs:106` — `Options { gc, .. }`.
- `docs/_internal/iroh-blobs/src/store/mem.rs:71` — `MemStore` `gc_config`.
- `core/src/lib.rs:1253,1275` — the `MemStore::new()` / `FsStore::load()` call
  sites to switch to opts-taking variants.
- `aster/src/config.rs:75,712,808` — `AsterConfig` / builder / `build()`.

## How portal-sync consumes it

portal-sync (`portal-cas`) will, once `gc_interval` exists: build the daemon node
with a GC interval, keep exactly one persistent tag per live object
(`portal/<tree>/<object_id>` → current blob hash), delete that tag on
tombstone/tree-delete, and call `gc_run_once` from its test harness to assert
reclamation. Everything except enabling the interval and the final sweep is
already implementable today against the existing tag API — only collection waits
on this requirement.
```
