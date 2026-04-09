# @aster Publish Design

**Status:** Implemented Day 0 core with documented follow-on gaps  
**Date:** 2026-04-08  
**Companion:** [aster-service-api.md](/Users/emrul/dev/aster/aster-app/docs/aster-service-api.md)

## Terminology

- `@aster` is the service built in this project.
- The replicated producer registry that comes with the Aster runtime is not
  `@aster`.
- `@aster` is an application-level handle, publication, access-control, and
  delegated-enrollment service built on top of the runtime.

## What Publish Means in Day 0

Publishing to `@aster` gives a producer:

- a human-readable address: `@handle/ServiceName`
- a discoverable manifest and endpoint record
- delegated access control through `@aster`
- enrollment-token issuance for consumers

The current implementation centers `@aster` itself, not the underlying runtime
registry.

## Current Published Service Model

Each published service currently stores:

- owner handle
- service name
- contract ID
- canonical manifest JSON
- version
- human-facing description
- lifecycle status
- optional replacement pointer
- visibility
- delegation statement
- active endpoints with TTL and per-endpoint registration time

The description and lifecycle status are part of the Day 0 surface now:

- `description`: required, max 280 chars
- `status`: `experimental | stable | deprecated`
- `replacement`: optional `@handle/Service` pointer, updated via `update_service`

## Current Publish Flow

The implemented flow is:

1. producer signs `PublishPayload`
2. `@aster` verifies the caller owns the verified handle
3. `@aster` re-hashes the canonical manifest with BLAKE3 and checks
   `contract_id`
4. `@aster` stores the published record and endpoints
5. `@aster` records an audit event
6. `@aster` returns active attestation material needed for delegated trust

Current publish-side RPCs:

- `publish`
- `set_visibility`
- `update_service`
- `unpublish`
- `discover`
- `get_manifest`
- `resolve`
- `list_services`

## Discovery Behavior

Current Day 0 discovery supports:

- substring search on service name
- exact handle listing with `@handle` queries
- list-by-handle through `list_services`

`DiscoverEntry` and `list_services` now return human-useful metadata:

- `description`
- `status`
- `delegation_mode`
- `published_at`

This makes `aster discover` useful as an operator-facing and human-facing
directory, not just a machine lookup.

## Access Control and Enrollment

Publishing is tightly coupled to `@aster`'s delegated access model.

Current access-control RPCs:

- `update_delegation`
- `grant_access`
- `revoke_access`
- `list_access`

Current enrollment RPC:

- `enroll`

Day 0 delegated access behavior:

- `open` services allow token issuance to verified consumers without explicit
  per-consumer grants
- `closed` services require grants
- tokens are issued by `@aster`, not by the publisher directly
- token verification helpers exist in this repository
- runtime admission-handler wiring is still outside this repo's remit

## Visibility Versus Delegation

The current implementation deliberately separates:

- `visibility`: `public | private`
- `delegation.mode`: `open | closed`

That means:

- a service can be discoverable or hidden independently of access mode
- exact `get_manifest` and `resolve` currently still work for private services
- access control is not modeled as a synonym for search visibility

## Current Storage Reality

The current publish implementation uses:

- a leader-local authoritative store
- SQLite as the implemented Day 0 authoritative persistence layer
- in-memory parity for tests and local flows

This is not yet the final replication architecture.

Not yet implemented:

- Iroh-backed replicated projection of publish state
- leader election
- authoritative write routing between nodes
- durable projection outbox
- backup/export pipeline

So the current design answer is:

- canonical state lives in the local authoritative store
- publish/discovery/access/enrollment logic reads and writes that canonical state
- future Iroh projection remains a follow-on systems task

## What `@aster` Is Not Doing Yet

The current publish implementation does not yet provide:

- recovery-code generation on first publish
- endpoint heartbeat or reaper tasks beyond TTL-based filtering
- instant revocation through gossip
- multi-writer correctness via runtime leadership
- operator-grade replicated projections

Those are still valid Day 1 follow-ons, but they are not part of the current
`@aster` publish behavior.

## Practical Mental Model

For the current repo, the right mental model is:

- Aster runtime:
  P2P transport, service hosting, and underlying replicated registry
- `@aster`:
  a producer application that manages handles, service publication, access, and
  delegated enrollment

Keeping that distinction sharp avoids the earlier "registry" confusion and
makes the codebase easier to reason about.
