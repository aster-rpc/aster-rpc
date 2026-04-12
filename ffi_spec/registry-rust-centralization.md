# Registry Rust Centralization

**Date:** 2026-04-12
**Status:** Design note, pre-implementation
**Relates to:** bindings/python/aster/registry/, iroh-docs FFI, iroh-gossip FFI

## Problem

The registry logic layer (~1077 lines across client.py, publisher.py,
acl.py, gossip.py) implements resolution, ranking, lease management, ACL
enforcement, and gossip broadcast in Python. Porting this to N additional
languages (Java, Go, .NET) creates N implementations of the same logic
with drift risk -- the same problem contract identity had before we
centralized canonicalization in Rust.

## Design

Move the registry logic to `core/src/registry.rs` behind C FFI functions.
All language bindings call the same Rust implementation. Data models
(pure structs) stay per-language for ergonomic construction and
inspection.

## Proposed FFI surface

```c
// Resolution: (service, version) -> EndpointLease JSON
// Applies mandatory filters (contract_id, ALPN, health, freshness,
// policy_realm) and ranking strategy (round_robin, least_load, random).
int32_t aster_registry_resolve(
    uintptr_t runtime,
    uintptr_t node,
    const uint8_t *service_ptr, uintptr_t service_len,
    int32_t version,
    const uint8_t *strategy_ptr, uintptr_t strategy_len,
    uint8_t *out_buf, uintptr_t *out_len
);

// Publish: write EndpointLease + ArtifactRef + version pointer + gossip
int32_t aster_registry_publish(
    uintptr_t runtime,
    uintptr_t node,
    const uint8_t *lease_json_ptr, uintptr_t lease_json_len,
    const uint8_t *artifact_json_ptr, uintptr_t artifact_json_len
);

// Lease renewal: update health, load, and lease expiry
int32_t aster_registry_renew_lease(
    uintptr_t runtime,
    uintptr_t node,
    const uint8_t *service_ptr, uintptr_t service_len,
    const uint8_t *contract_id_ptr, uintptr_t contract_id_len,
    const uint8_t *health_status_ptr, uintptr_t health_status_len,
    float load
);

// ACL management
int32_t aster_registry_acl_add_writer(
    uintptr_t runtime, uintptr_t node,
    const uint8_t *author_id_ptr, uintptr_t len
);
int32_t aster_registry_acl_remove_writer(
    uintptr_t runtime, uintptr_t node,
    const uint8_t *author_id_ptr, uintptr_t len
);
int32_t aster_registry_acl_list_writers(
    uintptr_t runtime, uintptr_t node,
    uint8_t *out_buf, uintptr_t *out_len
);
```

## What moves to Rust

1. **Resolution logic** (client.py lines 53-408): mandatory filters
   (contract_id match, ALPN, health, freshness, policy_realm) and
   ranking strategies (round_robin, least_load, random). Single
   implementation, all languages call `aster_registry_resolve`.

2. **Publishing logic** (publisher.py lines 1-386): ArtifactRef writes,
   EndpointLease writes, version/channel pointers, gossip broadcast.
   Single implementation, all languages call `aster_registry_publish`.

3. **ACL enforcement** (acl.py lines 1-150): read-time filtering, writer
   trust sets. Moves into the resolution path so untrusted entries are
   never returned to any language binding.

4. **Gossip integration** (gossip.py lines 1-133): GossipEvent
   broadcast/receive. Already close to the Rust layer (calls
   iroh_gossip_broadcast); the event construction and parsing moves
   to Rust.

## What stays per-language

1. **Data models** (models.py) -- HealthStatus, GossipEventType,
   ServiceSummary, ArtifactRef, EndpointLease, GossipEvent. These are
   pure data structures that devs construct and inspect. Port to all
   FFI languages (Java, Go, .NET). ~200 lines per language.

2. **Key helpers** (keys.py) -- string formatting for registry doc paths.
   Trivial, ~70 lines per language. Or could move to Rust too, but
   the overhead of an FFI call for string formatting isn't worth it.

## Implementation order

1. Port data models + keys to Java/Go/.NET (small, mechanical)
2. Add Rust registry module to `core/src/registry.rs`
3. Expose via `ffi/src/lib.rs` as the FFI functions above
4. Wire each language's AsterServer to call the FFI for publish/resolve
5. Delete the per-language resolution/publishing logic (Python keeps it
   as a reference implementation until Rust is verified, then deletes)
