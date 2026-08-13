# aster-coordination — Linearizable Fenced Registers On The Aster Mesh

- **Status:** Architecture agreed; v1 implementation plan ready; implementation
  not started
- **Date:** 2026-08-12

**Related:**

- [`../aster-leases.md`](../aster-leases.md) — grant/fence model and current
  designated-grantor implementation
- [`../../aster-leases-getstarted.md`](../../aster-leases-getstarted.md) — public
  lease guide
- [`aster-db.md`](aster-db.md) — separate AP/local-first SQLite product
- [`aster-orchestrator.md`](aster-orchestrator.md) — singleton and stateful
  failover consumers
- [`aster-coordination-implementation-plan.md`](aster-coordination-implementation-plan.md)
  — normative, checkable v1 build contract
- Portal first consumer:
  `/Users/emrul/dev/emrul/portal-sync/docs/phase6-aster-adoption-plan.md`

## Decision

Build **aster-coordination** as a first-party Aster built-in service, then
consume it from Portal. It is a small CP substrate for state that must have one
linearizable order: fenced leases, active-head pointers, singleton ownership,
and similar coordination metadata.

The service is **opt-in and off by default**. Merely running an Aster producer,
admitting a peer, or linking the client library does not start a voter or expose
coordination. A host explicitly enables the service with persistent storage, a
group/bootstrap configuration, a verified producer-admission source, and the
mandatory built-in replicated ACL. Applications author ACL changes; they cannot
disable Aster's enforcement or turn admission into a resource permission.

"Built-in" is a product and security boundary: Aster owns the reserved
`aster.coordination.*` wire contracts, authenticated dispatch, membership
checks, storage contract, and safe defaults. It does not mean that OpenRaft and
SQLite enter Aster's default dependency graph or that every Aster node runs a
coordination service.

The first public abstraction is an opaque **fenced register**, not arbitrary
distributed SQL:

```text
ResourceRow {
    resource_id,
    epoch,
    holder,
    lease_state,
    ttl,
    lease_revision,
    value_revision,
    value: bytes,
}
```

OpenRaft orders commands. SQLite durably stores the Raft log and replicated
state machine on each voter. Aster supplies authenticated NodeIds, RPC,
discovery, NAT traversal, replicated authorization enforcement, and
observability.

Applications own the meaning of `resource_id` and `value`. For example, Portal
stores a versioned active-manifest hash; an orchestrator stores a primary's
activation record. Large or immutable data stays in the CAS and the register
points to it.

This creates three deliberately different data shapes in the Aster family:

| Shape | Consistency | Use |
| --- | --- | --- |
| iroh-docs / `aster-sqlite` | AP/eventual, offline multi-writer | Desired state, status, local-first application data |
| `aster-coordination` | CP/linearizable, majority required | Fences, singleton ownership, small mutable heads/registers |
| iroh-blobs | Immutable/content-addressed | Payloads, snapshots, manifests, large values |

Do not blur them. In particular, CRDT convergence cannot allocate a unique
fence epoch, and a Raft commit of a blob hash does not prove the blob survives
on another node.

## Why this belongs in Aster

The lease design already names several consumers:

- Portal roaming workspaces;
- orchestrator singleton workloads and stateful failover;
- exclusive topology bridges;
- exec/session exclusivity;
- future catalogs, monotonic allocators, and active-version pointers.

They all need the same hard machinery: exact peer identity, quorum membership,
leader routing, durable log/state snapshots, idempotent retries, linearizable
reads, and partition tests. Rebuilding that in each application would create
multiple subtly different consensus systems over the same mesh.

Aster already has the right lease seams:

- `LeaseSerializer` abstracts ACQUIRE/RENEW/RELEASE/REVOKE;
- `FencedResource` requires the fence check and protected mutation to share one
  serialization point;
- `LeaseGrantor` exposes a designated single-node serializer over authenticated
  RPC.

An OpenRaft-backed serializer can implement `LeaseSerializer`, but that alone
is insufficient: the protected value must live in the same replicated state
machine. The built-in fenced register supplies that missing resource-side half.

## Goals

- One linearizable order for a bounded set of small coordination resources.
- Fenced lease transitions and protected value mutation in the same state
  machine.
- Automatic leader failover while a majority of configured voters is healthy.
- Aster NodeId-bound peer and caller authentication; no bearer epoch.
- First-party, opt-in service registration with a default-deny authorization
  posture.
- Producer/consumer-aware hosting rules without treating either classification
  as a resource permission.
- Durable SQLite storage, restart recovery, log compaction, and snapshot install.
- A client that can dial any known member, follow leader redirects, and retry an
  ambiguous result idempotently.
- Root/application-controlled membership and operation authorization.
- A reusable internal replicated-SQLite harness for later Aster-native command
  state machines, without stabilizing arbitrary SQL as a public contract.
- Rust-first implementation isolated from default Aster/FFI dependency graphs.

## Non-goals

- General distributed SQL, interactive SQL sessions, or WAL/page replication.
- Offline writes to coordinated resources. A minority partition stops.
- Replacing iroh-docs for desired state, status, discovery, or audit display.
- Replicating application blobs or proving their durability.
- Byzantine-fault tolerance. Voters are trusted infrastructure nodes.
- One Raft group per key/resource.
- Cross-group transactions.
- Making every admitted device a voter.
- Treating connection admission, or the `producer` role by itself, as
  authorization to read or mutate coordination state.
- Delegating enforcement to each application; applications choose policy, but
  Aster enforces it before dispatch and again at the resource operation.
- Hiding two-voter unavailability behind unsafe force-takeover behavior.

## Consistency contract

### Writes

A successful mutating call means its command is committed by the current Raft
quorum and applied durably to the leader's state machine. Followers apply the
same deterministic command in log order. The client receives the state-machine
result, not merely an accepted/proposed acknowledgement.

The state-machine apply path rechecks every load-bearing predicate: wrong
revision, stale epoch, wrong holder, or already-held state. A leader may reject
an obviously invalid request before proposing it, but that is only an
optimization; no leader-local precheck may authorize a mutation. Once a command
is proposed, its applied success or rejection is recorded for idempotent retry
rather than reported as a transport failure.

### Reads

Load-bearing reads use an OpenRaft linearizable-read barrier and then read a
state machine that has applied through the returned log id. The API must name
the distinction:

- `read_linearizable` — authority, fencing, and decisions;
- `read_local` or directory observation — dashboards/routing hints only.

No caller may silently substitute a follower-local read for an authority check.

### Availability

- A three-voter group tolerates one unavailable voter.
- A five-voter group tolerates two unavailable voters.
- A two-voter group requires both; losing either stops progress.
- A minority partition cannot renew, acquire, mutate, release, or reconfigure.
- Lease holders need not be voters. Mobile or intermittently connected clients
  should normally be non-voting callers.

This is CP by design. When quorum is lost, the current `LeaseHandle` self-limit
causes the holder to stop presenting a fence. Applications may preserve later
local work, but it cannot become authoritative until reconciled explicitly.

Embedding the quorum in Aster removes the need to deploy a separate coordination
product; it does not remove the control plane. Voter placement, membership,
backup, monitoring, and quorum-loss behavior remain real operational concerns
and must be visible to the application.

### Fault and trust model

Raft protects against crashes, restarts, delay, duplication, reordering, and
network partitions under non-Byzantine voters. A compromised voter can violate
the protocol's assumptions. Coordination membership is therefore a stronger
trust class than ordinary admission or resource read/write grants.

Each internal consensus connection must prove the exact configured Aster
NodeId. An address that resolves to a different identity is rejected before any
Raft message is handled.

## Architecture

```text
application client / lease holder
        |
        | Aster RPC; immutable Iroh peer NodeId is the actor
        v
aster.coordination.Store on any member
        |
        | follower returns authenticated leader NodeId
        v
leader: validate auth + propose command
        |
        v
OpenRaft AppendEntries / Vote / InstallSnapshot
        |
        +---------> voter SQLite raft log
        |
        v
committed deterministic apply
        |
        +---------> voter SQLite coordination state
                         resources + client sessions + last_applied
```

There are two reserved, opt-in RPC surfaces:

1. **`aster.coordination.Raft`**, callable only by the group's exact committed
   voters/learners: AppendEntries, Vote, and streamed snapshot installation.
2. **`aster.coordination.Store`**, default-deny and callable only after the
   mandatory replicated ACL approves the immutable transport actor, group,
   resource, and operation: acquire, renew, mutate, release, revoke, and read.

A follower returns a leader redirect instead of proxying a mutating request.
The client redials the leader directly so the leader authenticates the real
actor; forwarding through a follower would otherwise turn the follower into
the apparent caller or require a new delegation protocol.

Because a follower cannot prove that its resource ACL is current without
proxying or a delegation protocol, v1 treats a leader NodeId/term as bounded
operational metadata for an admitted peer that already knows the random group
id. The follower reveals no resource existence, ACL, holder, or value. The
directly dialed leader performs the authoritative replicated-ACL check before a
read or proposal.

## Built-in service and package boundary

Ship it inside the official `aster` crate as an optional, Rust-first subsystem.
This is required by the macro-enforced reserved `aster.*` namespace and keeps
the services, configuration, conformance tests, and release lifecycle inside
their owning package. Split Cargo features keep the dependency graph isolated:
`coordination-client` contains client/contracts, while `coordination` adds the
OpenRaft/SQLite host. Both are off by default.

Target Rust UX is pinned in the companion implementation plan:

```rust,ignore
let coordination = CoordinationConfig::builder()
    .storage_dir("/var/lib/my-app/aster-coordination")
    .start_mode(genesis_join_or_existing)
    .verified_admission(admission_source)
    .build()?;

let server = AsterServer::builder()
    .builtin_coordination(coordination) // the explicit opt-in
    .start()
    .await?;
```

Compiling a coordination client must not host the service. Enabling the service
without persistent storage, signed/expected group identity, a pinned authority
key, verified producer admission, or non-empty genesis administrators fails
startup. There is no allow-all granting authorizer: the replicated ACL exists
on every host and starts deny-by-default for resources.

Possible physical layout:

```text
aster/src/coordination/
        mod.rs
        contract.rs
        client.rs
        service.rs
        bootstrap.rs
        command.rs
        state.rs
        acl.rs
        network.rs
        storage/
            log.rs
            machine.rs
            snapshot.rs
```

The implementation depends on Aster RPC, OpenRaft, and SQLite. It should not
enter `aster_transport_core` or Aster's default feature set: doing so would push
a Rust-specific consensus/database stack into Python, TypeScript, mobile, and
consumers that only need transport.

Rust hosts opt into the first-party coordination package/feature explicitly.
Non-Rust clients may later consume the stable client RPC contract without
hosting a voter.

Keep the existing single-node `aster.lease.Grantor`; it remains the simplest
safe choice where loss of the designated authority may stop progress. Add a
`CoordinationSerializer` implementing `LeaseSerializer` for callers that need
quorum-backed availability.

Also provide a `CoordinationLease` convenience wrapper that uses the same
holder/renewal engine as `LeaseHandle` but exposes register reads,
`compare_and_set`, and `release_with_value`. Do not create a second renewal
loop or a second source of holder identity merely to add value operations.

## Group identity, bootstrap, and membership

Each group has a random, persistent `CoordinationGroupId` and an explicit
genesis membership. Group identity is not inferred from a mutable member list.

Bootstrap must prevent two independent clusters from initializing the same
group id. The creator supplies an application-authorized genesis record that
binds:

- group id;
- initial voter NodeIds;
- creation nonce/version;
- application/authority domain.

V1 uses a domain-separated canonical document signed by an application
authority and verified against a public key pinned independently in host
configuration. Aster owns the generic envelope/verification; the embedding
application decides which authority key to trust rather than Aster assuming
Portal's root policy. Exact fields, start modes, join permits, and golden vectors
are pinned in the companion implementation plan.

After genesis, committed Raft membership is authoritative:

1. authorize the requested membership change;
2. add a node as a learner and wait for catch-up;
3. use joint membership to promote/remove voters;
4. publish any directory membership record only as discovery/observability.

An eventually replicated directory record can request or advertise a change,
but cannot itself add a voter.

Only verified producer nodes are eligible to become voters/learners, but
producer admission is merely eligibility. Promotion still requires an
authorized membership command committed by the existing quorum. Removing the
producer admission or trust relationship must trigger an explicit committed
removal workflow; a local directory observation cannot rewrite membership.

Use one small group per administrative fleet/domain initially, with many
resource rows. Do not create one group per Tree, workload, or lease. Sharding is
a later measurement-driven decision.

## Replicated state

Use two SQLite databases or equivalently isolated storage handles:

- `raft.sqlite`: vote, log entries, committed index, snapshot metadata;
- `state.sqlite`: schema metadata, last applied log id, applied membership,
  resource rows, client sessions, and authoritative audit entries.

Separating log and state-machine I/O lets committed log replay recover an
interrupted apply and avoids coupling log append latency to application read
transactions. SQLite durability must satisfy OpenRaft's rule that a completed
storage write is persistent; select WAL/synchronous settings only after crash
tests prove that contract.

Suggested state-machine schema:

```text
coordination_meta(
    singleton,
    schema_version,
    last_applied_term,
    last_applied_index,
    membership_bytes
)

resources(
    resource_id        BLOB PRIMARY KEY,
    epoch              INTEGER NOT NULL,
    holder_node_id     BLOB,
    held               INTEGER NOT NULL,
    ttl_ms             INTEGER NOT NULL,
    lease_revision     INTEGER NOT NULL,
    value_revision     INTEGER NOT NULL,
    value              BLOB
)

client_sessions(
    actor_node_id      BLOB NOT NULL,
    client_id          BLOB NOT NULL,
    last_sequence      INTEGER NOT NULL,
    last_result        BLOB NOT NULL,
    PRIMARY KEY(actor_node_id, client_id)
)

audit(
    log_term,
    log_index,
    actor_node_id,
    resource_id,
    operation,
    result_code,
    epoch,
    value_revision,
    PRIMARY KEY(log_term, log_index)
)
```

The state-machine effects, result/session update, authoritative audit row, and
`last_applied` marker commit in one SQLite transaction. Reapplying a committed
entry after a crash returns the recorded result without duplicating effects.

Commands are deterministic. State-machine apply must not read wall clock,
randomness, network, filesystem state outside its database, or application
services. IDs and non-authoritative display timestamps are supplied in the
committed command; time never decides a fence inside apply.

Snapshot creation uses a consistent SQLite snapshot/backup and includes schema
version, last-applied id, membership, and a checksum. Installation replaces the
state atomically before advertising the applied index. Schema migration itself
must be a versioned, deterministic state-machine operation or an explicitly
coordinated binary upgrade—not an ad hoc local migration on one voter.

## Fenced-register API

The stable v1 API is deliberately narrower than the internal SQLite state
machine:

```text
acquire(resource, ttl) -> Granted(Fence, ResourceSnapshot) | Denied(...)
renew(fence) -> Renewed | Denied(...)
read_linearizable(resource) -> ResourceSnapshot
compare_and_set(fence, expected_value_revision, new_value) -> ResourceSnapshot
release(fence) -> Released
release_with_value(fence, expected_value_revision, final_value) -> ReleasedSnapshot
revoke(resource) -> RevokedSnapshot
```

Every protected mutation accepts the authenticated caller plus the fence and
checks, inside the committed state-machine command:

```text
carried_epoch == row.epoch
&& authenticated_writer == row.holder
&& row.held
&& expected_value_revision == row.value_revision
```

`compare_and_set` increments `value_revision`. It does not renew the lease
implicitly. `release_with_value` either commits the final value and advances
the fence together, or does neither. A value-revision mismatch leaves the lease
held so the caller may inspect and decide.

Fence epochs are strictly monotonic, not necessarily contiguous. Expiry,
release, revoke, and a later acquire may each advance the epoch; callers compare
for exact equality and must never infer meaning from a gap.

`value` is opaque, versioned application bytes with a strict size bound (start
at 64 KiB unless measurements justify less/more). Larger values belong in
iroh-blobs; the register stores their hash and compact metadata.

Do not expose raw SQL, SQL text replication, arbitrary table access, or an
interactive transaction protocol in v1.

## Lease timing under consensus

TTL controls availability, not safety. All fence safety comes from the
committed epoch/holder/state and authenticated mutation.

Leader-local timers drive expiry proposals:

1. A committed acquire or renew increments `lease_revision` and arms the
   leader's monotonic timer.
2. When it fires, the leader proposes `Expire(resource, epoch,
   lease_revision)`.
3. Apply expires only the exact still-current lease revision and advances the
   fence; a committed concurrent renew changed the revision and defeats the
   stale expiry command.
4. A newly elected/restarted leader conservatively re-arms every held row for a
   full TTL. This may delay takeover but cannot create overlap.
5. A holder reports success only after its renew commits. Without quorum, its
   existing send-time self-limit expires and it stops presenting a fence.

Renew and expiry races are therefore decided by Raft log order, not by follower
clocks. Wall-clock expiry fields may be published for dashboards but confer no
authority.

The existing pure lease transition model should remain the semantic reference.
The coordination backend adds the explicit `Expire` transition and
`lease_revision` pinned in the implementation plan while preserving the current
designated-grantor wire behavior.

## Idempotency and ambiguous results

Every mutating client request carries a persistent random `client_id` and a
monotonic sequence, scoped to the authenticated NodeId. The state machine keeps
the last sequence and serialized result per client:

- same sequence: return the prior result;
- next sequence: apply and replace the saved result;
- older or skipped sequence: reject with a resynchronization error.

This makes "committed, then connection lost before response" safe without an
unbounded request-id table. Client identity/sequence state is included in
snapshots.

The client library serializes renewals and application mutations through one
session sequencer. Callers that genuinely need parallel mutation streams use
distinct persistent client ids. Session retirement/garbage collection requires
an explicit protocol; age alone must not erase the answer to a request that may
still be retried.

## Authorization

Security is a shared-responsibility boundary, not an application opt-out:

- **Aster owns enforcement and safe defaults:** the service is off by default,
  client access is deny-by-default, consensus RPC checks exact membership,
  caller identity is transport-bound, and every operation is checked before it
  can reach the Raft log.
- **The embedding application owns policy facts:** which authenticated NodeIds
  may use which application resources, who is an administrator, and which root
  or trust directory is authoritative for those decisions.

The granting authorizer is a built-in replicated exact ACL, conceptually:

```text
authorize_applied(
    actor: immutable Iroh NodeId,
    group_id,
    exact_resource_key,
    operation: Read | Acquire | Renew | Mutate | Release | Revoke |
               ChangeMembership | ChangeAcl
) -> Allow | Deny
```

Genesis installs exact group administrators. They commit exact
NodeId/resource/right grants and later changes through the same Raft log.
Applications supply those policy decisions and a provenance-preserving producer
admission source, but a leader-local or AP policy callback cannot grant a
resource operation. An optional application hook may add an early denial only.

Authorization is enforced in four layers:

1. **Connection admission:** an unknown peer cannot reach the normal RPC ALPN.
   This limits exposure but grants no coordination operation.
2. **Consensus peer:** `aster.coordination.Raft` requires both an admitted
   `producer` and the exact NodeId in this group's committed voter/learner set.
   The transport NodeId must equal the Raft sender id in the message. Producer
   status alone never grants Vote, AppendEntries, snapshot, or membership
   access.
3. **Client preflight:** `aster.coordination.Store` uses its applied ACL view to
   reject obvious denials before proposal. Read/inspect is gated too;
   coordination values and holder identities are not public metadata.
4. **Committed authorization and resource predicate:** state-machine apply
   rechecks ACL/admin revision, actor, epoch, held state, and value revision in
   log order. A leader-local precheck cannot grant or bypass fencing.

Producer and consumer describe **placement/trust topology**, not permission:

| Authenticated caller | Consensus RPC | Client/resource operations |
| --- | --- | --- |
| Admitted consumer with no coordination grant | Denied | Denied, including read |
| Consumer granted one resource | Denied | Only allowed operations on that resource |
| Producer not in committed group membership | Denied | Denied unless separately granted |
| Committed voter/learner | Allowed only for its exact group/role | No implicit client or admin rights |
| Coordination administrator | No consensus right unless also a member | Only explicitly authorized admin/resource operations |

This separation is deliberate. A roaming Portal laptop may be a consumer and a
legitimate holder without becoming a voter. Conversely, a producer that serves
blobs or participates in topology must not automatically gain the ability to
burn epochs, read heads, revoke leases, or reconfigure the quorum.

The current Aster trust model already defines producer/consumer enrollment and
verified attributes, and RPC Gate 3 can reject calls before dispatch. The Rust
implementation does not yet derive all Gate-3 attributes from credentials
automatically: its `AttributeStore` may be populated by the application, and
Gate 1/2 work remains. Coordination must not pretend an unverified or
request-supplied role is authoritative. Until the verified admission bridge is
complete, use exact transport NodeIds plus an application-owned trusted-policy
adapter; add the first-party admission adapter only when provenance is
preserved end to end.

The client never supplies the authoritative actor. V1 adds immutable Iroh peer
provenance alongside the existing application-overridable `Call::peer`; the
leader writes only that transport NodeId into the command. HTTP/custom
principals cannot satisfy this check. The epoch is not a bearer token.

Obvious authorization failures are rejected before proposing a command, with
bounded request sizes, per-actor concurrency/rate limits, and denial metrics.
This keeps an arbitrary admitted peer from filling the Raft log with denied
operations. If authorization changes between preflight and apply, the committed
ACL decision wins and the applied rejection is recorded idempotently.

An AP directory/policy record may drive a committed ACL command, but cannot
provide instantaneous revocation itself. Removing holder rights commits the ACL
change and fence advance in one state-machine transaction before eventual
directory publication.

Revoke ordering remains fence first, directory second:

1. commit `Revoke`, advancing the fence;
2. publish the application's directory tombstone/status record.

## Portal integration

Portal is the first consumer, not the owner of consensus machinery.

Recommended resource and value shape:

```text
resource_id = "portal/tree/<tree-id>/roaming"

PortalActiveHeadV1 {
    manifest_hash,
    manifest_format_version,
    content_policy_version,
    durability_evidence,
}
```

Portal remains responsible for:

- constructing a crash-consistent or quiesced immutable manifest;
- satisfying its selected content-replication/durability rule before advancing
  the register;
- periodically committing recovery heads while a workspace is active;
- committing the quiesced final head on graceful release;
- activating the last committed head after forced takeover;
- preserving post-fence/unreplicated local work as an explicit conflict;
- selecting authorized coordination voters through root policy and driving
  the actual Raft membership workflow.

A coordination quorum agreeing on `manifest_hash` does not store that
manifest's blob closure. Portal's acceptance test must kill the holder
immediately after a head commit and reconstruct the complete head from a
different surviving content replica.

Collaborative Trees do not use this register for ordinary per-file writes. The
consensus tax applies only to roaming/exclusive features that opt into strict
single-owner coordination.

## Other consumers

- **Orchestrator singleton:** value identifies the active workload generation
  and node; the runtime accepts stateful actions only under its fence.
- **Stateful volume activation:** value is an immutable volume/snapshot head.
- **Topology bridge:** value records the current bridge generation/config;
  cross-cluster actions carry the fence.
- **Exec/session:** value records the one active executor/session generation.
- **Catalog/head:** value is a content-addressed catalog root with conditional
  revision updates.

These consumers validate the generic fenced-register boundary. If a second
consumer needs richer tables, expose an experimental deterministic
`ReplicatedSqlite<Command, Reply>` harness. Do not stabilize arbitrary SQL until
at least two command sets prove the migration, snapshot, authorization, and
upgrade contracts.

## Observability

Expose at least:

- group id, local role, leader NodeId, term, membership;
- last log, committed, and applied indexes;
- per-peer replication lag and snapshot progress;
- quorum available/unavailable state;
- command and linearizable-read latency;
- lease grants, renewals, expiry, release, revoke;
- fence/revision/auth rejection counts;
- leader changes and full-TTL re-arm events;
- SQLite/log/snapshot sizes and compaction duration.

Authoritative audit is state-machine-authored and keyed by applied log id.
Directory/status publication may mirror it for dashboards but is advisory.

## Limits and backpressure

Pin conservative defaults and test them:

- 3–5 voters; learners bounded separately;
- bounded resource-id and value sizes;
- bounded in-flight client commands per actor/group;
- TTL min/max clamps;
- snapshot threshold and log-retention floor;
- streamed snapshot chunk limits and checksums;
- storage-pressure safety stop before SQLite or snapshot disk exhaustion;
- no new writes when the state machine cannot durably apply committed entries.

Topology ranking may select which member a client tries first, but never changes
membership, leader authority, or quorum rules.

## Test plan

### State machine and storage

- Run OpenRaft's full storage/state-machine conformance suite.
- Model/property test every command against a pure reference state machine.
- Prove effects, saved idempotency result, audit, membership, and last-applied
  marker are one SQLite transaction.
- Crash after log commit and at every apply transaction boundary; restart and
  compare against the reference history.
- Snapshot while writes continue; install on an empty learner and compare all
  resource/session/audit state.
- Upgrade/migrate schema with mixed-version nodes according to the declared
  compatibility rule.

### Consensus and network

- Coordination services are absent until explicitly enabled; a client-only
  build/node neither opens storage nor advertises the reserved services.
- Service startup fails without persistent storage, signed/expected group
  identity, a pinned authority key, verified producer admission, and non-empty
  genesis administrators.
- Three- and five-voter election, write, read, snapshot, and membership tests.
- Minority leader isolation: no renew/acquire/mutate response can commit.
- Majority failover: one new leader, monotonic epoch/value revision.
- Delay, drop, duplicate, reorder, reconnect, and process-kill fault injection.
- Leader loss during learner promotion and joint membership change.
- Node address resolving to the wrong Aster identity is rejected.
- Admitted non-voter cannot send Vote/AppendEntries or change membership.
- Producer in group A cannot send Raft traffic for group B.
- Client follows leader redirects without losing authenticated actor identity.
- Linearizability-history checker over concurrent acquire/renew/mutate/release.

### Authorization

- Unknown peer is rejected at connection admission.
- Arbitrarily admitted consumer with no coordination grant cannot read,
  acquire, renew, mutate, release, revoke, or change membership.
- Admitted producer that is not an exact committed member cannot call Raft RPC.
- Committed voter has no implicit client-resource or administrator permission.
- Consumer granted one resource cannot read or mutate another resource or
  group, including by prefix/encoding tricks.
- Request fields that claim another actor, role, producer status, or Raft id
  never override the transport NodeId or verified admission record.
- Unverified/application-self-asserted attributes do not satisfy the verified
  producer-admission source or the replicated resource ACL.
- Authorization denial occurs before Raft proposal and remains bounded under a
  per-peer denial flood.
- Revoke advances the fence before eventual directory/policy publication; the
  mandatory replicated ACL orders ACL revocation and protected mutation in the
  same group.

### Lease and register

- Two candidates race acquire; exactly one fence is granted.
- Renew versus expiry command: committed log order decides; stale expiry
  revision is rejected.
- Old holder wakes after takeover; mutation is rejected as stale.
- Same epoch replayed by another admitted NodeId is rejected.
- Compare-and-set revision conflict changes nothing.
- `release_with_value` is all-or-nothing and advances the fence.
- Revoke immediately rejects the old holder before directory convergence.
- Leader restart re-arms a full TTL without regressing or advancing the fence.
- Ambiguous committed response retried with the same client sequence returns
  the original result exactly once.
- Concurrent renew/value calls share one client sequencer; crash/restart never
  reuses a sequence for a different command.
- Client-session retirement cannot turn a late retry into a second mutation.

### Availability honesty

- Two-voter group stops when either voter is unavailable.
- Three-voter group progresses with one unavailable and stops with two.
- Holder loses quorum; self-limit expires and no further fenced commit succeeds.
- Directory record lies or is stale; it never changes authority.

### Portal acceptance

- Graceful laptop-to-desktop handoff commits a quiesced head, releases, acquires
  at a newer epoch, and activates byte-identically.
- Forced takeover activates the last committed crash-consistent head.
- Kill the holder immediately after head commit; successor fetches the complete
  closure from another node.
- Old holder returns with later local edits; they surface as a conflict and
  never overwrite the active head silently.

## Delivery sequence

1. Keep the existing lease state machine, `LeaseHandle`, and designated grantor
   stable as the semantic baseline.
2. Create the first-party, optional coordination subsystem and its explicit
   `builtin_coordination` registration path; pin OpenRaft/SQLite.
3. Implement immutable Iroh peer provenance, the mandatory replicated
   exact-NodeId/resource ACL, and the default-deny service skeleton before any
   mutating RPC is exposed.
4. Implement and conformance-test durable Raft log/state/snapshot storage.
5. Implement authenticated Raft RPC and explicit membership/bootstrap.
6. Implement the fenced register, linearizable reads, client sessions,
   `CoordinationSerializer`, and `CoordinationLease` wrapper.
7. Connect verified producer/consumer admission attributes/capabilities without
   trusting request-supplied or provenance-free attributes.
8. Run deterministic fault injection and a linearizability checker until the
   failure matrix is green.
9. Integrate Portal as the first real consumer.
10. Validate a second Aster consumer before stabilizing a generic custom-command
   state-machine API.
11. Add non-Rust client bindings only after the wire and operational contracts
   settle; hosting voters remains Rust-first initially.

## V1 resolution and post-v1 questions

The former implementation blockers—bootstrap envelope, package placement,
storage settings, redirects, limits, release method shape, replicated ACL, and
membership controller—are resolved normatively in
[`aster-coordination-implementation-plan.md`](aster-coordination-implementation-plan.md).

Post-v1, measurement/evidence may justify multi-group hosting, a generic custom
command state machine, non-Rust clients/voters, sharding, or different hard
limits. None is required to implement or ship the v1 fenced register.

## Sources

- OpenRaft getting started and storage contract:
  <https://docs.rs/openraft/0.9.25/openraft/docs/getting_started/>
- OpenRaft state-machine contract:
  <https://docs.rs/openraft/0.9.25/openraft/storage/trait.RaftStateMachine.html>
- OpenRaft linearizable reads:
  <https://docs.rs/openraft/0.9.25/openraft/docs/protocol/read/>
- OpenRaft membership changes:
  <https://docs.rs/openraft/0.9.25/openraft/docs/cluster_control/dynamic_membership/>
- Aster lease implementation: `core/src/lease.rs`,
  `aster/src/rpc/lease.rs`

## One-line version

> Put only the state that cannot tolerate two truths into a small
> Aster-authenticated Raft group; keep its value opaque and bounded, enforce
> lease and mutation in the same committed state machine, and leave all large,
> immutable, offline, or convergent data on the AP/CAS planes.
