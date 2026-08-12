There is a credible Aster database product here. My recommendation is:

  1. Build aster-sqlite first, using Fly.io’s maintained cr-sqlite fork and Corrosion as the correctness reference.
  2. Use Iroh as the replication plane, not as SQLite’s physical backing store.
  3. Explore FoundationDB separately as an Aster-authenticated gateway/layer—not by embedding or modifying fdbserver.

                         aster-sqlite                                     aster-foundationdb
  ━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
   Consistency           Eventual CRDT convergence                        Strict serializability
  ────────────────────  ───────────────────────────────────────────────  ─────────────────────────────────────────────────
   Offline writes        Yes                                              No
  ────────────────────  ───────────────────────────────────────────────  ─────────────────────────────────────────────────
   Data model            Full local SQLite, with CRR restrictions         Ordered key/value; no SQL
  ────────────────────  ───────────────────────────────────────────────  ─────────────────────────────────────────────────
   Aster advantage       Very high: P2P, NAT traversal, identity, sync    Moderate: secure gateway and cross-language API
  ────────────────────  ───────────────────────────────────────────────  ─────────────────────────────────────────────────
   Initial complexity    Medium                                           Medium as gateway; extreme if bundling servers
  ────────────────────  ───────────────────────────────────────────────  ─────────────────────────────────────────────────
   Best use              Local-first and edge state                       Authoritative online service state

  ## The SQLite direction

  The important distinction is “replicated over Iroh,” not “stored in Iroh.”

  iroh-docs is a CRDT key/value replica in which (namespace, author, key) points to content stored in iroh-blobs; it already uses gossip for
  live notification and range-based reconciliation for recovery. Automerge is separate: Iroh currently demonstrates synchronizing a
  JSON-like Automerge document over a bidirectional stream. Neither understands SQLite pages, WAL files, transactions, indexes, or
  constraints. Iroh documents (https://docs.iroh.computer/protocols/documents), Iroh Automerge example
  (https://docs.iroh.computer/protocols/automerge).

  Therefore:

  - Never synchronize the .sqlite or WAL file through blobs/docs.
  - Synchronize logical cr-sqlite changesets.
  - Keep SQLite authoritative for local queries and persistence.
  - Use Iroh for identity, discovery, delivery, anti-entropy, and snapshots.

  A production shape would be:

  Application
      │ local SQL
      ▼
  SQLite + cr-sqlite
      │ committed ChangeBatch
      ▼
  aster-sqlite replicator
      ├── gossip: small “new head” hints
      ├── Aster bidi RPC: vector/gap reconciliation and change streaming
      ├── iroh-blobs: large batches and logical snapshots
      └── iroh-docs: database catalog, schema and snapshot manifests

  Gossip must only be a hint: Aster explicitly surfaces a receiver-lagged event in aster/src/gossip.rs:7. Recovery must come from durable
  state or anti-entropy.

  ### A very attractive first spike

  For the prototype, iroh-docs can temporarily replace the custom anti-entropy protocol:

  tx/<schema-epoch>/<replica-id>/<db-version> -> zstd(ChangeBatch)
  snapshot/<schema-epoch>/<snapshot-hash>     -> SnapshotManifest
  head/<replica-id>                           -> ReplicaHead
  schema/<schema-epoch>                       -> SchemaManifest

  Each transaction becomes one immutable docs entry. Once ContentReady arrives, the receiver applies the entire batch inside one SQLite
  transaction. Docs supplies signed metadata reconciliation and blob delivery almost for free.

  That is excellent for proving convergence, but I would not assume it is the final implementation. A permanent transaction-per-entry log
  creates retention, compaction, dormant-writer, and capability-revocation problems. Production likely wants Corrosion-style vector/gap
  reconciliation with blobs for snapshots, leaving docs as the control plane.

  ### Start from the Fly.io fork

  The original vlcn upstream is quiet: its latest release is 0.16.3 from January 2024. Fly.io now maintains a materially divergent 0.17 fork
  specifically for Corrosion. It adds per-site database versions and makes gap detection, sequence tracking, and buffering explicit. vlcn
  release (https://github.com/vlcn-io/cr-sqlite/releases/tag/v0.16.3), Fly.io cr-sqlite fork (https://github.com/superfly/cr-sqlite).

  Corrosion (https://github.com/superfly/corrosion) is almost the direct precedent:

  - SQLite on every node
  - cr-sqlite conflict resolution
  - gossip for fresh changes
  - periodic peer synchronization
  - QUIC transport

  Aster can replace its IP-address bootstrapping, TLS setup, and SWIM membership with endpoint identities, relays/NAT traversal, admission,
  topology, and service contracts. I would borrow Corrosion’s bookkeeping model rather than designing another one.

  ### Non-negotiable correctness rules

  - A replica_id is not a NodeId. Multiple database replicas can exist on one node; persist a random replica identity and bind it to the
    Aster identity.

  - Batch by cr-sqlite db_version and include the final seq; receivers buffer until complete, then apply atomically.
  - Serialize local writes through an owned SQLite writer actor. Otherwise a crash or later overwrite can occur before a changeset is
    captured.

  - Treat duplicates and reordering as normal.
  - Require an exact schema epoch/hash before applying a batch.
  - Use bound SQL parameters when inserting into crsql_changes.
  - Begin with whole-database replication only—one database/namespace per sharing group.
  - Validate that the docs author is an admitted NodeId. The current working tree already binds the default docs author to the node identity
    in aster/src/docs.rs:188.

  cr-sqlite is history-free: later writes can replace metadata from earlier database versions, so preserving replicated transaction
  boundaries requires networking-layer work. It also cannot enforce foreign keys, non-primary-key uniqueness, or some checks on CRR tables.
  Transaction behavior (https://portal.vlcn.io/blog/how-crsqlite-transactions-work-today), constraint limitations
  (https://www.vlcn.io/docs/cr-sqlite/constraints).

  ## The FoundationDB direction

  FoundationDB solves a different problem: it provides an always-online, distributed ordered key/value store with strict serializable
  transactions. It deliberately does not supply SQL or disconnected operation. Consistency model
  (https://apple.github.io/foundationdb/consistency.html), anti-features (https://apple.github.io/foundationdb/anti-features.html).

  There are three possible interpretations:

  1. An Aster gateway to an externally managed FDB cluster — recommended.
  2. Bundle fdbserver, supervise a cluster, and distribute configuration — much larger.
  3. Replace FoundationDB’s internal network with Iroh — effectively a major FoundationDB fork and not worthwhile initially.

  The gateway could expose:

  Get
  GetRange          -> server stream
  Transact(program) -> atomic result
  Watch             -> server stream

  A declarative transaction program is preferable to interactive v1 transactions because the gateway can safely perform FoundationDB retry
  loops. An interactive transaction could later use Aster’s existing incremental bidi support in aster/src/rpc/client.rs:192, but must
  expose conflict/restart and “possibly committed” semantics honestly.

  This gateway has real Aster-specific value:

  - Clients need neither libfdb_c nor fdb.cluster.
  - Aster identity can enforce key-prefix/subspace permissions.
  - Remote clients do not need direct connectivity to every FDB process.
  - It addresses FoundationDB’s explicit lack of user-level access control. FDB limitations
    (https://apple.github.io/foundationdb/known-limitations.html).

  It should run as a Linux-sidecar/service near the FDB cluster. The Rust binding still requires the native FoundationDB client, has
  process-global one-time network initialization, and has weaker macOS/Windows support. foundationdb-rs
  (https://docs.rs/foundationdb/latest/foundationdb/), FDB client requirements (https://apple.github.io/foundationdb/api-general.html).

  I would not put it behind an aster core feature. Make it a sibling aster-foundationdb crate or aster-fdb-gateway binary so the native
  dependency does not contaminate Aster’s Windows/mobile/binding matrix. Database logic also belongs above the transport core, matching the
  existing separation in aster/Cargo.toml:8.

  ## Suggested sequence

  1. Create a three-replica aster-sqlite spike using Fly’s cr-sqlite fork and the docs-backed immutable transaction log.
  2. Test partitions, duplicates, reordering, concurrent edits, restart during publication/application, schema mismatch, and a peer offline
     beyond retention.

  3. Measure docs metadata growth and snapshot/bootstrap time.
  4. If the model holds, replace the permanent docs log with Corrosion-style Aster anti-entropy; retain docs for membership/schema/snapshot
     manifests.

  5. Separately spike an external-cluster FDB gateway with Get, streamed ranges, declarative transactions, watches, and prefix
     authorization.

  If funding only one now, I would choose aster-sqlite: it exercises Aster’s distinctive P2P strengths and has a much shorter path to
  something compelling. FoundationDB is the stronger later server-side offering, but only after narrowing it to a gateway/layer rather than
  “FoundationDB packaged inside Aster.”

