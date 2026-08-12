# aster-sqlite — Local-First SQLite Replication Over Aster

Status: working idea / product boundary accepted

## Decision

Build `aster-sqlite` as Aster's offline-capable SQLite replication product,
using Fly.io's maintained `cr-sqlite` fork and Corrosion as the primary
correctness reference.

Use Iroh as the replication plane, not as SQLite's physical backing store.
Synchronize logical changes and snapshots; never synchronize SQLite database or
WAL files.

`aster-sqlite` is deliberately an AP/local-first system. It is not the backing
store for lease epochs, exclusive ownership, singleton selection, or other
state that cannot tolerate two concurrent truths. Those use the separate
[`aster-coordination`](aster-coordination.md) CP state machine.

| Property | `aster-sqlite` contract |
| --- | --- |
| Local transactions | SQLite ACID on each replica |
| Cross-node consistency | Eventual CRDT convergence |
| Offline writes | Supported |
| Query model | Full local SQLite, subject to CRR restrictions |
| Replication unit | Logical committed change batch |
| Best fit | Local-first and edge application state |
| Not suitable for | Fencing, exclusive ownership, or authoritative mutable pointers |

## The important distinction

The product is **replicated over Iroh**, not **stored in Iroh**.

`iroh-docs` is a CRDT key/value replica in which `(namespace, author, key)`
points to content in `iroh-blobs`. It provides signed metadata reconciliation,
blob delivery, and live notifications. Automerge is a separate JSON-like CRDT
example. Neither layer understands SQLite pages, WAL files, transactions,
indexes, or constraints.

Therefore:

- never copy a `.sqlite`, `-wal`, or `-shm` file through blobs or docs;
- capture and replicate logical `cr-sqlite` changesets;
- keep SQLite authoritative for local query execution and persistence;
- apply each complete remote batch inside one local SQLite transaction;
- use Aster/Iroh for identity, admission, discovery, delivery, anti-entropy,
  and snapshot transfer;
- treat gossip only as a freshness hint and recover gaps from durable state.

Relevant Iroh material:

- <https://docs.iroh.computer/protocols/documents>
- <https://docs.iroh.computer/protocols/automerge>

## Proposed architecture

```text
Application
    | local SQL transaction
    v
SQLite + cr-sqlite
    | committed ChangeBatch
    v
aster-sqlite replicator
    |-- gossip: small new-head hints
    |-- Aster bidi RPC: vector/gap reconciliation and change streaming
    |-- iroh-blobs: large batches and logical snapshots
    `-- iroh-docs: database catalog, schema, and snapshot manifests
```

Gossip cannot be load-bearing: Aster explicitly surfaces receiver lag in
`aster/src/gossip.rs`. A missed hint must only delay convergence; durable
anti-entropy must still find and transfer the missing changes.

## First spike: docs-backed immutable transactions

For a bounded prototype, `iroh-docs` can temporarily provide the durable
catalog and anti-entropy index:

```text
tx/<schema-epoch>/<replica-id>/<db-version> -> zstd(ChangeBatch)
snapshot/<schema-epoch>/<snapshot-hash>     -> SnapshotManifest
head/<replica-id>                           -> ReplicaHead
schema/<schema-epoch>                       -> SchemaManifest
```

Each local transaction publishes one immutable entry. Once its content is
ready, a receiver validates the schema and batch envelope, then applies the
whole batch atomically.

This is a good convergence spike, not a commitment to keep one docs entry per
transaction forever. An unbounded transaction log creates retention,
compaction, dormant-writer, bootstrap, and capability-revocation problems.
Production likely wants Corrosion-style vector/gap reconciliation with blobs
for snapshots, leaving docs as the catalog/control plane.

## Start from the Fly.io fork and Corrosion

Fly.io maintains a materially divergent `cr-sqlite` fork for Corrosion. It
adds per-site database versions and makes gap detection, sequence tracking, and
buffering explicit:

- <https://github.com/superfly/cr-sqlite>
- <https://github.com/superfly/corrosion>

Corrosion is the closest operating precedent:

- SQLite on every node;
- `cr-sqlite` conflict resolution;
- gossip for fresh changes;
- periodic peer synchronization; and
- QUIC transport.

Aster can replace IP-address bootstrapping, TLS setup, and SWIM membership with
endpoint identities, relays/NAT traversal, admission, topology, and generated
service contracts. Reuse Corrosion's replication bookkeeping and failure
lessons rather than inventing a second model.

## Non-negotiable correctness rules

- A `replica_id` is not a `NodeId`. One Aster node may host multiple database
  replicas. Persist a random replica identity and bind it to the authenticated
  Aster identity.
- Batch by `cr-sqlite` `db_version` and include the final sequence. A receiver
  buffers until the batch is complete and applies it atomically.
- Serialize local writes through an owned SQLite writer actor so a committed
  change cannot be overwritten or lost before capture.
- Treat duplication, reordering, retries, and reconnects as normal.
- Require an exact schema epoch and hash before applying a batch.
- Use bound SQL parameters when inserting into `crsql_changes`.
- Begin with whole-database replication: one database/namespace per sharing
  group.
- Authenticate and authorize the publisher NodeId. The Aster docs facade binds
  the default docs author to node identity in `aster/src/docs.rs`.
- Persist receive progress and batch application in the same transaction so a
  crash cannot acknowledge unapplied work.
- Bootstrap from a verified logical snapshot plus a precise per-replica
  frontier, then reconcile every gap after that frontier.
- Compact only when retention policy accounts for every supported dormant
  replica, or force an old replica to rebootstrap explicitly.

## Constraint and transaction caveats

`cr-sqlite` is history-free: later writes can replace metadata from earlier
database versions. Preserving replicated transaction boundaries therefore
requires networking-layer batch and sequence bookkeeping.

CRR tables also cannot safely enforce every ordinary SQLite invariant across
concurrent offline writers. Foreign keys, non-primary-key uniqueness, and some
checks need application-level convergence rules or a data model that avoids
those invariants.

References:

- <https://portal.vlcn.io/blog/how-crsqlite-transactions-work-today>
- <https://www.vlcn.io/docs/cr-sqlite/constraints>

## Relationship to aster-coordination

The two products intentionally make different promises:

| Question | `aster-sqlite` | `aster-coordination` |
| --- | --- | --- |
| Can a disconnected node write? | Yes | No |
| Can two partitions accept writes? | Yes, then converge | Only the quorum side |
| Is arbitrary local SQL exposed? | Yes, with CRR constraints | No; bounded commands only |
| Is a mutable value linearizable? | No | Yes |
| Typical payload | Application rows | Lease metadata and a small opaque value/pointer |

An application may use both. For example, it may replicate ordinary user state
through `aster-sqlite`, store immutable large objects in `iroh-blobs`, and use
`aster-coordination` only for the one active-version pointer that must not
split-brain. The planes remain separate; there is no cross-plane atomic
transaction.

## Suggested delivery sequence

- [ ] Create a three-replica spike using the Fly.io fork and the docs-backed
  immutable transaction log.
- [ ] Test partitions, duplicate and reordered delivery, concurrent edits,
  restart during publication/application, schema mismatch, and a replica
  offline beyond retention.
- [ ] Add deterministic batch-completeness and idempotent-apply tests.
- [ ] Measure docs metadata growth, steady-state reconciliation traffic, and
  snapshot/bootstrap time.
- [ ] Prove logical snapshot restore plus gap catch-up after process and machine
  loss.
- [ ] Replace the permanent docs transaction log with Corrosion-style Aster
  anti-entropy if measurements show it is required; retain docs for compact
  membership/schema/snapshot manifests.
- [ ] Document which SQLite constraints are unsupported or require application
  conflict semantics before calling the product production-ready.

## One-line version

> `aster-sqlite` gives every peer a real local SQLite database and converges
> logical writes over Aster; it embraces offline/AP behavior and delegates the
> few values that require one global truth to `aster-coordination`.
