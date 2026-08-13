# aster-coordination v1 — Implementation Plan

- **Status:** Implementation-ready v1 build specification
- **Date:** 2026-08-12
- **Parent design:** [`aster-coordination.md`](aster-coordination.md)
- **First consumer:** Portal's
  [`phase6-aster-adoption-plan.md`](../../../../../emrul/portal-sync/docs/phase6-aster-adoption-plan.md)

This document turns the architecture in `aster-coordination.md` into a pinned,
checkable implementation contract. The parent document explains why the
subsystem exists; this document is normative for v1 package placement, protocol,
state transitions, authorization, bootstrap, persistence, delivery order, and
acceptance tests. If the two documents differ on an implementation detail, this
plan wins until both are updated in the same change.

The words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are requirements in the
RFC sense. Checked boxes under "Locked v1 decisions" are design work already
completed; unchecked boxes are implementation or verification work still to do.

## 1. Scope lock

V1 is one opt-in Raft group per `AsterServer` process, containing many small
fenced registers. It is not a distributed SQLite interface. SQLite is the local
durable implementation of the Raft log and deterministic state machine; no SQL
text, arbitrary table, page, WAL, or transaction API crosses the public wire.

The safety boundary is:

```text
authenticated Iroh NodeId
        +
committed resource ACL
        +
committed lease epoch / holder / value revision
        +
protected value mutation
        |
        v
one OpenRaft log order and one SQLite apply transaction
```

The following are not v1 extension points: multiple groups in one process,
custom application tables, arbitrary replicated commands, HTTP/WebTransport
hosting, non-Rust voters, cross-resource transactions, and automatic unsafe
quorum reset. Adding any of them requires a new reviewed design and, where the
wire or safety contract changes, a new protocol version.

## 2. Locked v1 decisions

- [x] Implement the reserved wire types and services inside the `aster` crate.
  `aster-macros` permits `aster.*` packages only while expanding that crate, so
  a sibling implementation crate would either break the namespace rule or need
  an unsafe special case.
- [x] Add a lightweight `coordination-client` feature and a hosting
  `coordination` feature. Both are off by default; `coordination` includes
  `coordination-client` and `rpc`.
- [x] Host at most one `CoordinationGroupId` in an `AsterServer` in v1. Every
  request still carries the group id and a mismatch fails closed.
- [x] Serve coordination over authenticated Iroh RPC only. The shared HTTP
  dispatcher and application-overridable principal are not sufficient evidence
  of an Aster transport NodeId.
- [x] Use an application-authority-signed genesis envelope and an independently
  configured/pinned authority public key. A self-signed envelope is not trust.
- [x] Make committed OpenRaft membership authoritative. A producer policy or
  eventually replicated directory can request a change but cannot itself add a
  voter.
- [x] Require verified producer admission **and** the exact allowed bootstrap or
  committed membership NodeId for consensus RPC. Producer status alone grants
  nothing.
- [x] Make a built-in, replicated, exact-NodeId/resource ACL the only granting
  authorizer. Application hooks may add a denial but cannot grant access.
- [x] Keep lease expiry deterministic by committing an explicit `Expire`
  command. `Acquire` never consults a clock and never steals a held row.
- [x] Use separate `raft.sqlite` and `state.sqlite` storage actors. A completed
  OpenRaft durability callback means the corresponding SQLite transaction has
  committed with `synchronous=FULL`.
- [x] Redirect rather than proxy client calls to the leader. The new connection
  lets the leader authenticate the actual actor.
- [x] Return an ordinary authenticated leader hint in v1; do not add a second
  signature format. The destination authenticates the leader again and rejects
  a stale hint safely.
- [x] Pin one public wire version, one internal Raft codec version, and one
  storage schema version. There is no best-effort mixed-codec decoding.
- [x] Fail closed on permanent quorum loss. Disaster recovery creates a new
  group id; it never rewrites the old group's membership or reuses its fences.

No unresolved decision in the parent design blocks Slice 0.

## 3. Package, features, and public host API

### 3.1 Physical layout

Add the following beneath the existing `aster` crate:

```text
aster/src/coordination/
    mod.rs                 public client types and feature-gated host exports
    contract.rs            Fory v1 wire roots and service declarations
    ids.rs                 strict group/node/resource identifiers
    client.rs              redirecting client and durable request sequencer
    config.rs              host configuration and start modes
    admission.rs           verified producer source; deny-only policy hook
    acl.rs                 rights and pure ACL transitions
    command.rs             deterministic CommandV1 / ReplyV1
    state.rs               pure reference state machine
    service.rs             Store and Raft RPC handlers
    runtime.rs             lifecycle, readiness, expiry and membership loops
    network.rs             OpenRaft network factory over Aster RPC
    bootstrap.rs           signed genesis/join verification
    metrics.rs             health, counters, and structured status
    storage/
        mod.rs
        log.rs             RaftLogStorage
        machine.rs         RaftStateMachine and linearizable read actor
        schema.rs           exact schema v1 and validation
        snapshot.rs         create, stream, validate, and atomic install
```

Do not add OpenRaft, rusqlite, snapshot, or host runtime code to
`aster_transport_core`, the FFI crates, or the default feature graph.

The two v1 services hand-implement `ServiceDispatch` and typed client stubs,
following `rpc/lease.rs`, because generated service methods do not receive the
trusted `Call` context. `contract.rs` still emits the normal Aster
`ServiceContract`/payload registry and golden contract id. Do not add a
request-supplied actor merely to use the service macro. A future generic
context-injection macro feature is separate work.

### 3.2 Cargo features and dependency pins

The intended feature shape is:

```toml
[features]
coordination-client = [
    "rpc",
    "dep:postcard",
    "dep:serde",
    "dep:blake3",
    "dep:fs2",
    "dep:windows-sys",
]
coordination = [
    "coordination-client",
    "dep:openraft",
    "dep:rusqlite",
]
```

Pin the first implementation to:

```toml
openraft = { version = "=0.9.25", default-features = false,
             features = ["serde", "storage-v2", "generic-snapshot-data"],
             optional = true }
rusqlite = { version = "=0.40.2", default-features = false,
             features = ["bundled", "backup", "limits"], optional = true }
postcard = { version = "=1.1.3", default-features = false,
             features = ["use-std"], optional = true }
serde = { version = "1", features = ["derive"], optional = true }
blake3 = { version = "1", optional = true }
fs2 = { version = "=0.4.3", optional = true }

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "=0.61.2",
                features = ["Win32_Foundation", "Win32_Storage_FileSystem"],
                optional = true }
```

`storage-v2` is required to implement OpenRaft's split `RaftLogStorage` and
`RaftStateMachine` traits. `generic-snapshot-data` is required so Aster can use
one bounded client-streaming snapshot RPC instead of serializing a complete
snapshot into a unary request. These exact pins stay in `Cargo.lock`; an
OpenRaft upgrade is a protocol/storage compatibility task, not a routine patch
bump.

### 3.3 Host API

The host path is available only under `feature = "coordination"`:

```rust,ignore
let config = CoordinationConfig::builder()
    .storage_dir("/var/lib/my-app/aster-coordination")
    .start_mode(CoordinationStart::Existing {
        expected_group_id,
        expected_genesis_hash,
        authority_public_key,
    })
    .verified_admission(admission_source)
    .build()?;

let server = AsterServer::builder()
    .builtin_coordination(config)
    .start()
    .await?;

server.coordination().unwrap().wait_ready().await?;
```

Required config is:

- an absolute or application-resolved persistent storage directory;
- exactly one `CoordinationStart` mode from section 7;
- the pinned application authority public key;
- a `VerifiedAdmissionSource` whose producer results carry provenance from the
  application's trusted enrollment/admission layer;
- explicit acknowledgement for a development-only one- or two-voter group.

There is no configurable allow-all granting authorizer. The replicated ACL is
always installed, starts with only genesis administrators, and denies every
resource operation until one of those administrators commits a grant. A host
may install `CoordinationDenyPolicy` to reject additional requests before
proposal; it MUST NOT return an allow decision or override the replicated ACL.

`AsterServer::start()` validates and opens storage, then starts the RPC/runtime
tasks.
It may return while the group is `Bootstrapping`, `Joining`, or temporarily
`NoQuorum`; callers that need authority wait on `wait_ready()`. Invalid config,
identity mismatch, bad signatures, corrupt storage, newer schema, or a storage
directory bound to another group fail `start()` itself.

Startup ordering is: validate static config; start the Aster node to obtain its
stable NodeId; acquire/open/validate coordination storage against that NodeId;
register the Iroh-only services; start the RPC acceptor; then spawn
bootstrap/join/OpenRaft controllers. Bootstrap cannot run before peers can dial
the services. Any failure before return closes storage and the just-started
node. `AsterServer` owns the `CoordinationRuntime` handle so dropping/shutdown
cannot orphan voter tasks.

`coordination-client` exposes contract types and `CoordinationClient`, but no
builder host method, storage, OpenRaft type, or background voter task.

### 3.4 Client API

The client binds one group and one local Aster NodeId:

```rust,ignore
let client = CoordinationClient::builder(node.clone(), group_id)
    .seed_tickets(seed_tickets)
    .journal_dir(app_state.join("coordination-client"))
    .build()?;
```

`build()` requires writable durable journal storage, takes a per-actor/group
lock, and enables mutations. `build_read_only()` omits the journal and returns a
`CoordinationReadClient` exposing only linearizable reads/status methods. Seed
tickets populate routing only; the builder rejects non-Open credentials, and
they grant no coordination right.
Both clients may learn routes from authenticated redirects/discovery. Neither
starts a service, opens server SQLite, or changes membership.

Opening a journal created for another group or local NodeId fails. Applications
choose its parent directory and backup policy; Aster owns its format, atomic
write/fsync behavior, and migrations. `CoordinationLease` can be constructed
only from the full durable client.

## 4. Identity, resource, and protocol types

### 4.1 Strict identifiers

Use fixed bytes internally and on the wire:

```text
CoordinationGroupId = exactly 32 random bytes
CoordinationNodeId  = exactly 32 Ed25519 public-key bytes
ClientId            = exactly 16 random bytes
OperationId         = exactly 16 random bytes
```

The current public `aster::NodeId` is a hex-string wrapper and its constructor
does not validate length. `CoordinationNodeId([u8; 32])` therefore MUST perform
strict lower/upper-case-independent hex decoding at the boundary and implement
the ordering/serde traits OpenRaft requires. Never compare unvalidated NodeId
strings inside coordination.

`CoordinationGroupId` is generated from the OS CSPRNG, rendered as 64 lowercase
hex characters in config/logs, and never inferred from membership or an
application name. All fences include the group id so a fence from a
disaster-recovery replacement group is invalid even if its numeric epoch is the
same.

### 4.2 Resource key

```text
ResourceKeyV1 {
    namespace: UTF-8 string,
    key: bytes,
}
```

Rules are exact, not prefix based:

- `namespace` is 1–64 ASCII bytes and matches
  `[a-z0-9][a-z0-9._-]{0,63}`;
- `key` is 1–512 bytes;
- equality is `(namespace bytes, key bytes)` with no normalization;
- v1 has no wildcards, inherited grants, path semantics, or prefix grants.

Portal will use namespace `portal.roaming` and the raw canonical `TreeId` bytes
as the key. The register value format remains Portal's separately versioned
contract.

### 4.3 Fence and snapshot

```text
FenceV1 {
    group_id,
    resource,
    epoch: u64,
    holder: CoordinationNodeId,
}

ResourceSnapshotV1 {
    group_id,
    resource,
    epoch: u64,
    holder: Option<CoordinationNodeId>,
    held: bool,
    ttl_ms: u64,
    lease_revision: u64,
    value_revision: u64,
    acl_revision: u64,
    value: Option<bytes>,
}
```

All counters MUST fit SQLite's non-negative signed `INTEGER` range; applying an
increment at `i64::MAX` returns `CounterExhausted`, changes nothing, emits a
critical metric, and prevents further mutation of that row. No counter wraps.

There is deliberately no authoritative wall-clock expiry in the snapshot. A
human-facing estimate MAY be added to status output and MUST be labelled
advisory.

The internal state always has value bytes. Public snapshots contain
`value = Some(bytes)` only when the actor has `READ`; other authorized
operations receive the lease/revision metadata with `value = None`. An empty
value is therefore distinguishable from a redaction. A group administrator's
audited recovery export includes all values because that administrator could
already grant itself `READ`; ordinary group/status administration does not.

## 5. Public and internal wire contract

### 5.1 Versioning and encoding

- Service version: `1`.
- Public payload encoding: Aster XLANG/Fory with explicit wire roots under
  `aster.coordination.v1/*`.
- Internal OpenRaft payload encoding: postcard codec version `1`, over exact
  dependency `openraft = 0.9.25`.
- Storage schema version: `1`.
- Every request includes `protocol_version = 1` and `group_id`.
- Unknown versions, fields that violate bounds, trailing internal-codec bytes,
  unknown rights bits, and non-canonical signed documents are rejected before
  dispatch/proposal.

Rust names ending `WireV1` map mechanically to the reserved wire root
`aster.coordination.v1/<name-without-WireV1>`; for example
`AcquireRequestWireV1` is `aster.coordination.v1/AcquireRequest`. RPC method
strings are the lowercase snake-case names in the tables below. There are no
v1 aliases or native-serialization alternatives.

The contract tests MUST keep byte-level golden vectors for every signed
document and internal Raft envelope. Public Fory compatibility tests MUST prove
that adding an optional field does not renumber or reinterpret existing fields.

### 5.2 `aster.coordination.Store` v1

The reserved service exposes the following methods. All are unary except the
server-streaming `audit_stream` and `recovery_export`.

| Method | Required committed permission | Result |
| --- | --- | --- |
| `acquire` | resource `ACQUIRE` | applied outcome + fence + snapshot |
| `renew` | current holder + resource `RENEW` | applied outcome + lease revision |
| `read_linearizable` | resource `READ` | snapshot after a leader barrier |
| `compare_and_set` | current holder + resource `MUTATE` | applied snapshot |
| `release` | current holder + resource `RELEASE` | applied released snapshot |
| `release_with_value` | holder + `RELEASE` + `MUTATE` | atomic value/release snapshot |
| `revoke` | resource `REVOKE` | applied fenced snapshot |
| `set_grant` | group administrator | new ACL revision; may fence holder |
| `set_admin` | group administrator | new admin revision |
| `change_membership` | group administrator | durable operation status |
| `cancel_membership` | group administrator | durable cancellation status |
| `membership_status` | group administrator | intent + committed membership |
| `inspect_acl` | group administrator | exact resource ACL snapshot |
| `inspect_admins` | group administrator | exact administrator snapshot |
| `retire_client` | same actor/client or administrator | permanent tombstone |
| `group_status` | group administrator | bounded operational status |
| `audit_stream` | group administrator | bounded retained audit range |
| `recovery_export` | group administrator | bounded, checksummed state export |

The actor is absent from every request. The service derives it only from the
immutable Iroh transport peer described in section 6. Request fields that look
like a calling actor, producer role, or Raft sender are contract errors. A
`target_node_id` in an ACL/admin/retirement request names the object being
changed, not the caller.

The following Fory structs and declaration order are fixed. Byte-vector lengths
are validated as section 4 specifies.

```text
ResourceKeyWireV1 { namespace: String, key: Vec<u8> }
FenceWireV1 { group_id: Vec<u8>, resource: ResourceKeyWireV1,
              epoch: u64, holder_node_id: Vec<u8> }
ResourceSnapshotWireV1 { group_id, resource, epoch, holder_node_id: Vec<u8>,
                         held, ttl_ms, lease_revision, value_revision,
                         acl_revision,
                         value: Option<Vec<u8>> }
LogIdWireV1 { term: u64, leader_node_id: Vec<u8>, index: u64 }
MemberSeedWireV1 { node_id: Vec<u8>, ticket: String,
                   signed_join_permit: Option<Vec<u8>> }
AclGrantWireV1 { subject_node_id: Vec<u8>, rights: u64 }
AclSnapshotWireV1 { resource: ResourceKeyWireV1, acl_revision: u64,
                    grants: Vec<AclGrantWireV1> }
AdminSnapshotWireV1 { admin_revision: u64, administrators: Vec<Vec<u8>> }
```

An absent snapshot holder is the empty vector; a present holder is exactly 32
bytes. No other field uses an empty vector as a sentinel.

Every mutating request repeats this declaration prefix:

```text
protocol_version: i32
group_id: Vec<u8>
client_id: Vec<u8>
sequence: u64
```

The method-specific fields follow in this exact order:

| Method/code | Fields after the mutation prefix |
| --- | --- |
| `acquire` / 1 | `resource`, `ttl_ms` |
| `renew` / 2 | `fence` |
| `compare_and_set` / 3 | `fence`, `expected_value_revision`, `new_value` |
| `release` / 4 | `fence` |
| `release_with_value` / 5 | `fence`, `expected_value_revision`, `final_value` |
| `revoke` / 6 | `resource` |
| `set_grant` / 7 | `resource`, `target_node_id`, `rights`, `expected_acl_revision` |
| `set_admin` / 8 | `target_node_id`, `enabled`, `expected_admin_revision` |
| `change_membership` / 9 | `operation_id`, `expected_membership_log_id`, `desired_voters`, `retained_learners`, `member_seeds` |
| `retire_client` / 10 | `target_node_id`, `target_client_id` |
| `cancel_membership` / 11 | `operation_id`, `expected_phase_code` |

`desired_voters` and `retained_learners` are sorted unique 32-byte NodeId
vectors. `member_seeds` is sorted by NodeId, contains every new node, and may
also refresh route material for an existing node. A valid signed join permit is
mandatory exactly for a new node and absent for an existing member. Duplicates,
missing new-node seeds, or mismatched ticket/permit NodeIds are invalid before
proposal.

The read/status request fields are also fixed:

```text
ReadRequestV1             { protocol_version, group_id, resource }
AclStatusRequestV1        { protocol_version, group_id, resource }
AdminStatusRequestV1      { protocol_version, group_id }
MembershipStatusRequestV1 { protocol_version, group_id,
                            operation_id: Option<Vec<u8>> }
GroupStatusRequestV1      { protocol_version, group_id }
AuditStreamRequestV1      { protocol_version, group_id,
                            after_log_id: Option<LogIdWireV1>, limit: u32 }
RecoveryExportRequestV1   { protocol_version, group_id }
```

All mutating methods return one `MutationReplyWireV1` in this order:

```text
protocol_version: i32
group_id: Vec<u8>
client_id: Vec<u8>
sequence: u64
request_hash: Vec<u8>             // exactly 32 bytes
disposition_code: i32             // 1 Consumed, 2 NotConsumed
outcome_code: i32
reason_code: i32
snapshot: Option<ResourceSnapshotWireV1>
acl_revision: u64                 // 0 when not applicable
admin_revision: u64               // 0 when not applicable
operation_id: Option<Vec<u8>>     // exactly 16 bytes when present
membership_log_id: Option<LogIdWireV1>
```

Outcome codes are fixed: `1 Granted`, `2 Renewed`, `3 Mutated`, `4 Released`,
`5 Revoked`, `6 GrantUpdated`, `7 AdminUpdated`, `8 MembershipAccepted`,
`9 ClientRetired`, `10 MembershipCancelled`, and `100 Rejected`. Reason `0`
means none; rejection reasons
are `1 HeldByOther`, `2 NotHeld`, `3 NotHolder`, `4 EpochMismatch`,
`5 ValueRevisionMismatch`, `6 AclRevisionMismatch`, `7 AdminRevisionMismatch`,
`8 SequenceTooOld`, `9 SequenceGap`, `10 SequenceConflict`, `11 ClientRetired`,
`12 PermissionChanged`, `13 MembershipRevisionMismatch`,
`14 MembershipIntentActive`, `15 CounterExhausted`, and
`16 MembershipCannotCancel`, and `17 OperationIdConflict`. Unknown codes are
preserved for diagnostics but never interpreted as success.

An exact next sequence has disposition `Consumed` even when its business
outcome is rejected; its result is saved. `SequenceTooOld`, `SequenceGap`,
`SequenceConflict`, and a request against an already retired client have
disposition `NotConsumed` and do not change that client session. A replay of the
last sequence/hash returns its originally saved disposition/result.

`ReadReplyWireV1` contains `(protocol_version, group_id, snapshot)`.
`AclStatusReplyWireV1` contains `(protocol_version, group_id, acl_snapshot)`;
`AdminStatusReplyWireV1` contains `(protocol_version, group_id,
admin_snapshot)`.
`MembershipStatusReplyWireV1` contains `(protocol_version, group_id,
operation_id, status_code, expected_membership_log_id,
observed_membership_log_id, desired_voters, retained_learners, error_code)`.
`GroupStatusReplyWireV1` contains `(protocol_version, group_id, lifecycle_code,
local_node_id, local_role_code, leader_node_id, term, last_log_id,
committed_log_id, applied_log_id, voters, learners, pending_operation_id,
quorum_available, fatal_code)`. Optional NodeIds/log ids use `Option`, not empty
sentinels. Status/lifecycle/error codes are declared constants with golden
tests; adding descriptive fields later requires optional tail fields only.

`AuditRecordWireV1` contains `(protocol_version, group_id, log_id,
actor_node_id, operation_code, resource: Option<ResourceKeyWireV1>,
result_code, epoch, value_revision)`. `audit_stream` first takes a linearizable
barrier, fixes an upper log id, then returns at most `limit <= 10,000` retained
rows strictly after the requested id in ascending order. If `after_log_id`
precedes `audit_floor`, it returns `OUT_OF_RANGE` plus the floor and emits no
partial stream. Audit never contains register values, tickets, or credentials.
Audit operation codes 1–11 are the mutation method codes above; `100` is
system `Expire` and `101` is `MembershipProgress`. The result code is the
public outcome code for client commands and a separately declared system phase
code for system commands.

Lifecycle codes are `1 Bootstrapping`, `2 Joining`, `3 Ready`, `4 NoQuorum`,
`5 Draining`, and `6 Fatal`; local-role codes are `0 None`, `1 Learner`, and
`2 Voter`. Membership-intent status codes are `1 Pending`, `2 AddingLearners`,
`3 ChangingVoters`, `4 Completed`, `5 Failed`, `6 CancelRequested`, and
`7 Cancelled`. Zero means no value only for the explicitly documented
status/error integer fields.

The request hash is BLAKE3 over
`"aster-coordination/action/v1\0" || method_code || postcard(ActionV1)`, where
`ActionV1` contains only the method-specific fields above in the same order. It
does not include transport actor, client id, or sequence. The leader adds actor,
client id, sequence, and hash to `ClientCommandV1` before proposal.

The client obeys `MutationReplyWireV1.disposition_code`; it does not infer
consumption from the outcome. A pre-proposal error is definitively
`NotConsumed` only when its authenticated `RpcStatus` contains
`aster.coordination.disposition=not_consumed`; when available it also echoes
group/client/sequence/request-hash and leader NodeId/term in namespaced detail
keys. A deadline, disconnect, malformed trailer, or error without that detail is
ambiguous and the client retries the exact journaled request.

`recovery_export` performs an authorized linearizable barrier, takes a
consistent state snapshot, and streams a manifest followed by strictly ordered
1 MiB-or-smaller chunks using the same offset/length/checksum rules as a Raft
snapshot. Recovery import is deliberately not an RPC or log command; section 13
defines the identical genesis-bound bootstrap input required on every initial
voter.

### 5.3 `aster.coordination.Raft` v1

The reserved internal service exposes:

- unary `vote`;
- unary `append_entries`;
- one client-streaming `install_snapshot`.

Unary calls carry:

```text
RaftEnvelopeV1 {
    protocol_version,
    raft_codec_version,
    group_id,
    sender_node_id,
    payload: bounded postcard bytes,
}
```

`sender_node_id` MUST equal the immutable Iroh transport NodeId. The receiver
checks group/bootstrap state, verified producer admission, and the exact sender
allowlist before decoding `payload` or invoking OpenRaft.

Unary responses use the same field order with `responder_node_id` in place of
`sender_node_id`; `payload` is the postcard v1 OpenRaft response. The service
rejects a unary envelope above 4 MiB and requires postcard decode to consume all
bytes.

The snapshot request stream is a Fory union with exactly two message variants:

```text
RaftSnapshotHeaderWireV1 {
    protocol_version, raft_codec_version, group_id, sender_node_id,
    vote_payload, snapshot_meta_payload, total_len, blake3_checksum
}
RaftSnapshotChunkWireV1 { offset, data, final_chunk }
```

The header is the first and only header frame. `vote_payload` and
`snapshot_meta_payload` are bounded canonical postcard values; checksum is 32
bytes. The terminal unary reply is a normal `RaftEnvelopeV1` carrying
OpenRaft's snapshot response. An unknown union variant is rejected rather than
ignored.

The first snapshot frame is a bounded manifest; subsequent frames contain
strictly increasing `offset`, at most 1 MiB of data, and a final flag. The
receiver rejects gaps, overlaps, bytes after final, a total beyond the configured
limit, wrong metadata, or a BLAKE3 mismatch. It writes to a unique temporary
file on the same filesystem and never exposes a partial snapshot.

### 5.4 Leader routing and errors

Only the leader serves Store reads and mutations. A follower cannot prove a
linearizable resource-ACL decision without either proxying the caller or adding
a delegation protocol, so v1 does not pretend its local ACL is current. After
immutable transport admission plus group/version/bounds checks, it returns
`UNAVAILABLE` with structured v1 details containing the last known leader NodeId
and term. Group ids are random and leader identity is treated as admitted-peer
operational metadata; no resource existence, holder, ACL, or value is included.
The leader performs the authoritative replicated-ACL check.

The client imports any already-validated route for the hinted NodeId and dials
it directly. It treats the hint as staleable, authenticates the destination,
and caps redirects at the number of known members plus two. It never forwards a
caller credential or lets a follower proxy the mutation.

Minimum status mapping is:

| Condition | Aster status |
| --- | --- |
| malformed/bounds/version | `INVALID_ARGUMENT` |
| no immutable Iroh identity | `UNAUTHENTICATED` |
| admission/member/ACL denial | `PERMISSION_DENIED` |
| stale fence/revision/sequence or unsafe state | `FAILED_PRECONDITION` |
| concurrent compare/membership intent conflict | `ABORTED` |
| configured limits/storage pressure | `RESOURCE_EXHAUSTED` |
| follower/no leader/no quorum | `UNAVAILABLE` |
| corrupt bytes/checksum/invariant | `DATA_LOSS` |
| durable storage failure | `INTERNAL`, followed by local not-ready/fatal state |

Machine decisions live in structured details/result enums; clients MUST NOT
parse human error strings.

## 6. Authenticated transport identity

The current RPC dispatcher has one mutable `peer_id`: the Iroh reactor supplies
it, but `Authenticator` may replace it, and non-Iroh transports construct
`CallParts` directly. That is adequate for application RPC but not for Raft
membership or a fence holder.

Slice 0 MUST add immutable peer provenance:

```text
PeerProvenance::IrohNode(CoordinationNodeId)
PeerProvenance::Other(transport name)
```

The Iroh reactor constructs `IrohNode` from the authenticated remote connection.
HTTP and custom transports construct `Other`; request metadata and
`Authenticator::principal` cannot change it. `Call::peer()` remains the resolved
application principal for compatibility, while a new read-only accessor exposes
the transport provenance.

Coordination services have an `IrohOnly` transport policy checked before the
application authenticator and check `IrohNode` again in their handler. They have
no HTTP projection. An HTTP/JWT caller whose application principal string is
identical to a voter NodeId is still denied.

Add `ServiceDispatch::transport_policy()` with a backward-compatible default of
`Any`; existing/generated services and guards preserve/forward it. The
dispatcher checks it against immutable provenance before invoking the
application authenticator. The coordination handlers then read the provenance
from `Call`, never from `Call::peer()`.

The in-process embedding application is inside Aster's trust boundary: it can
replace binaries or storage and therefore is not defended against by making
`CallParts` constructors cryptographically unforgeable. Remote request data is
outside that boundary and can never select provenance.

## 7. Genesis, start modes, and membership

### 7.1 Signed documents

Signed bootstrap documents use canonical postcard v1 bytes with these signing
domains:

```text
aster-coordination/genesis/v1\0 || document_bytes
aster-coordination/join/v1\0    || document_bytes
aster-coordination/recovery/v1\0 || document_bytes
```

The envelope carries `document_bytes`, `authority_public_key[32]`, and
`signature[64]`. Verification requires the envelope key to equal the public key
pinned separately in `CoordinationConfig`; accepting the key from the envelope
alone is forbidden. Decoding and re-encoding MUST produce the exact original
document bytes, and all NodeId sets are sorted and unique before signing.

Provisioning APIs expose `canonical_signing_bytes()` and
`SignedCoordinationDocument::from_parts(document, public_key, signature)` plus
verification. A small `CoordinationAuthoritySigner` trait may be implemented by
application control tooling, but the voter runtime accepts only the signed
bytes and pinned public key; it never requires or stores the authority secret.
Aster reuses its existing Ed25519 verification implementation and golden-tests
signatures from an independent implementation.

The pinned authority key and domain are immutable for the lifetime of a v1
group. Rotating a lost/compromised bootstrap authority uses the section 13
new-group recovery procedure and a new group id; there is no local authority-key
rewrite.

`GenesisV1` contains:

```text
format_version = 1
group_id
authority_domain: 1..128 ASCII bytes
genesis_nonce: 32 random bytes
deployment_profile_code: 1 Production | 2 DevelopmentSingle | 3 DevelopmentEven
initial_voters: sorted VoterSeedV1 values; count constrained by profile
initial_admins: sorted 1..64 NodeIds
optional_recovery_export_hash
```

`VoterSeedV1` contains the exact NodeId and an Aster ticket whose embedded
NodeId must match. It must be an address-only/Open ticket; embedded enrollment,
RCAN, or registry credentials are rejected rather than replicated. One voter is
allowed only with an explicit development flag.
A two-voter genesis uses profile 3, requires a matching local
`allow_even_quorum` flag, and emits a persistent warning that either voter loss
stops progress. A one-voter genesis similarly uses profile 2 plus an explicit
local development flag. Profile 1 accepts only 3 or 5 voters.

The genesis hash is BLAKE3 over the domain-separated signed document and is
persisted in both databases. Reusing a group id with a different genesis hash
is a hard startup failure.

`JoinPermitV1` contains, in order:

```text
format_version = 1
group_id
genesis_hash
candidate_node_id
candidate_ticket
bootstrap_senders: sorted non-empty NodeIds
join_nonce: 32 random bytes
```

It has no wall-clock expiry: time is not a portable bootstrap authority, the
candidate must prove the bound NodeId key, and an existing committed
administrator must still accept a membership intent. Applications revoke an
unused permit operationally by refusing the corresponding membership plan and
issuing a new nonce/permit if needed.

### 7.2 Start modes

Exactly three modes exist:

1. `Bootstrap { signed_genesis, recovery_export_path? }` — only for an empty
   directory. The local
   Aster NodeId must be an initial voter. Every initial voter may call
   `Raft::initialize` with the same signed membership; identical concurrent
   initialization is tolerated. Messages are accepted only from signed initial
   voters that also pass verified producer admission until committed membership
   is applied. If genesis binds a recovery export, the path is mandatory and
   its verified logical import completes before `initialize`; if genesis does
   not bind one, supplying a path is an error.
2. `Existing { expected_group_id, expected_genesis_hash,
   authority_public_key }` — only for initialized storage. All configured and
   persisted values must match. It never calls `initialize`.
3. `Join { signed_join_permit }` — only for an empty directory. The permit binds
   the candidate NodeId, group id, genesis hash, candidate route ticket, sorted
   bootstrap-sender NodeIds, and a 32-byte nonce. Until the candidate applies a
   membership containing itself, it accepts Raft traffic only from those
   senders, and only if they pass verified producer admission. Afterwards the
   committed membership replaces the permit allowlist.

A join permit does not add a member. An existing administrator must separately
commit a membership intent through the current quorum.

Before bootstrap, every initial voter must already be Gate-0 admitted and
resolve as a verified producer on every other initial voter. Before dynamic
join, the candidate and current voters must be mutually admitted in the same
way. Bootstrap/join does not bypass Gate 0 and the address ticket's embedded
credential must be empty/Open and is never treated as producer proof.

### 7.3 Membership controller

`change_membership` takes one desired plan:

```text
MembershipPlanV1 {
    operation_id,
    expected_membership_log_id,
    desired_voters: sorted NodeIds,
    retained_learners: sorted NodeIds,
    routes_and_join_permits_for_new_nodes,
}
```

Only one membership intent may be pending. Its acceptance is a normal
idempotent state-machine command: apply checks the current group administrator,
expected membership revision, sizes, signatures, and exact request hash, then
stores the intent durably. A later administrator revocation does not
retroactively cancel an already committed intent.

`operation_id` is group-global. Reusing it with the identical canonical plan
returns the existing intent/status; another plan returns
`OperationIdConflict`, even when submitted from another administrator/client
session.

A production plan targets exactly 3 or 5 voters and may add at most two new
voter identities in one intent; retained and transient learners are each
bounded to two. Larger replacements use successive committed plans. A plan may
not reduce below three voters, create an even production set, include the same
NodeId in voter/learner sets, or violate the signed genesis deployment profile.

The leader-only controller resumes pending work after restart/failover:

1. import validated route tickets into the local Aster address book;
2. add every missing node as an OpenRaft learner;
3. wait until each promoted learner has applied through the leader's committed
   index observed when the promotion step began;
4. call `change_membership` with the exact desired voter set, allowing
   OpenRaft's joint-consensus transition;
5. retain/remove learners exactly as requested;
6. commit an intent completion record with the resulting membership log id.

`cancel_membership` is committed and revision-checked too. `Pending` cancels
immediately. During `AddingLearners` it records `CancelRequested`; the
controller removes only learners introduced by that intent, restores the
pre-intent voter/retained-learner set, then records `Cancelled`. Once the phase
is `ChangingVoters`, cancellation returns `MembershipCannotCancel`; the caller
waits for the joint change to settle and submits a new desired plan. A failed or
cancelled intent is terminal and no longer occupies the one-active-intent slot.
Transient dial/catch-up failure does not become `Failed` by wall-clock timeout;
the intent remains observable/retrying until an administrator cancels it.
`Failed` is reserved for a deterministic invalid/invariant/storage outcome that
cannot succeed unchanged.

OpenRaft membership entries remain the authority. `node_routes` and application
policy records are routing/desired-state inputs only. The controller compares
state after every retry, so losing a response or leader is idempotent.

The OpenRaft `Node` metadata contains stable identity/display data, not a socket
address. Network dialing uses the target NodeId plus Aster's address book,
discovery, relay, and direct-route mechanisms. A route that connects to another
identity is rejected by the Iroh handshake and by the Raft envelope check.

### 7.4 Removal and admission changes

For routine removal, commit the membership plan first and revoke producer
admission second. For a suspected compromise, deny connection admission as an
immediate containment step, then use the surviving quorum to commit removal.
An admission record disappearing locally never edits Raft membership by itself.

Voters receive no implicit resource or group-admin rights. Administrators need
not be voters, and resource holders should normally be consumers/non-voters.

### 7.5 OpenRaft runtime defaults

Construct and validate one `openraft::Config` rather than relying on upstream
defaults:

```text
cluster_name                    = "aster-coordination/<group-id-hex>"
heartbeat_interval              = 250 ms
election_timeout_min            = 1,500 ms
election_timeout_max            = 3,000 ms
install_snapshot_timeout        = 30,000 ms
max_payload_entries             = 32
replication_lag_threshold       = 20,000 entries
snapshot_policy                 = LogsSinceLast(10,000)
snapshot_max_chunk_size         = 1 MiB
max_in_snapshot_log_to_keep     = 1,000
purge_batch_size                = 500
enable_tick/heartbeat/elect     = true
```

V1 does not enable OpenRaft's `single-term-leader`, `singlethreaded`, or
compatibility feature flags. Tuning may change the timing/count values through
an explicit advanced config, but validation enforces heartbeat below the
minimum election timeout, minimum below maximum, snapshot chunks at or below
1 MiB, replication lag above the snapshot trigger, and the section 12 wire/log
caps. Tuning changes availability/performance, never membership or lease
authority, and the effective values appear in status.

The `RaftTypeConfig` is also fixed:

```text
D             = CommandV1
R             = ReplyV1
NodeId        = CoordinationNodeId
Node          = CoordinationNodeV1 { format_version: u16 }
Entry         = openraft::Entry<CoordinationRaftConfig>
SnapshotData  = CoordinationSnapshotFile
AsyncRuntime  = openraft::TokioRuntime
```

`CoordinationSnapshotFile` is a private RAII path/length/checksum handle. With
`generic-snapshot-data`, the network implementation opens and streams its file
directly; snapshots never become a `Vec<u8>`. The handle distinguishes a
durable current snapshot from a delete-on-drop incoming temporary so error
paths cannot remove the installed snapshot.

## 8. Replicated authorization

### 8.1 Rights

The v1 resource-right bits are fixed:

```text
READ    = 0x01
ACQUIRE = 0x02
RENEW   = 0x04
MUTATE  = 0x08
RELEASE = 0x10
REVOKE  = 0x20
```

Unknown bits are rejected. Grants bind one exact `(group, resource, subject
NodeId)` and never imply producer, voter, administrator, or another resource.
Group administrators may change ACL/admin/membership state but receive no
implicit resource rights.

Genesis installs a non-empty exact administrator set. `set_admin` requires an
expected admin revision and MUST NOT remove the final administrator. `set_grant`
requires an expected resource-ACL revision. Both are committed commands and are
audited.

Fresh genesis admin revision is 1; a fresh resource ACL revision is 0. Every
successful change increments its revision with checked arithmetic. Grant rights
`0` deletes the exact grant row while still advancing the ACL revision and
performing the holder-fence rule below. Repeating a desired grant with a stale
expected revision is a consumed conflict, not a silent no-op.
The 64-administrator and 256-grants-per-resource limits are enforced inside
apply as well as preflight.

### 8.2 Enforcement points

Authorization is deliberately redundant:

1. Gate 0 limits which remote nodes can establish normal Aster RPC.
2. The Raft service checks immutable Iroh identity, group/start state, verified
   producer admission, and bootstrap/committed membership before decoding.
3. Store performs bounds/rate checks and a leader-local snapshot of the
   replicated ACL to avoid logging obvious denials.
4. State-machine apply rechecks the committed ACL/admin revision and actor in
   log order. This is the granting decision.
5. Holder operations additionally check exact actor, epoch, held state, and
   expected value revision in that same apply transaction.

The leader stamps the immutable transport actor into `CommandV1`; the client
never supplies it. A deny-only application hook runs at step 3 if configured.
Its inconsistency can reduce availability but cannot make an unauthorized
command pass step 4.

### 8.3 Revocation semantics

If `set_grant` removes `RENEW`, `MUTATE`, or `RELEASE` from the current holder,
the same applied command clears the holder, advances the epoch, advances the
lease revision, and records the new ACL revision. Removing only `READ` or
`ACQUIRE` does not terminate an existing lease. Explicit `revoke` always fences
the current epoch, whether or not a holder is present.

Application policy revocation order is:

1. commit the ACL reduction or `revoke` in the coordination group;
2. observe its applied result;
3. publish eventual policy/directory tombstones and UI status.

This makes a failover leader see the same authorization order. A root-policy
watcher may drive these commands, but the AP record itself is not the check used
by protected mutation.

### 8.4 Verified admission source

The host-facing source is a local, non-networked lookup suitable for the Raft
hot path:

```rust,ignore
trait VerifiedAdmissionSource: Send + Sync {
    fn get(&self, node: CoordinationNodeId) -> Option<VerifiedAdmission>;
}

enum VerifiedAdmissionClass { Producer, Consumer }

struct VerifiedAdmission {
    subject: CoordinationNodeId,
    class: VerifiedAdmissionClass,
    authority_fingerprint: [u8; 32],
    evidence_digest: [u8; 32],
    revision: u64,
}
```

The application populates it only after cryptographic verification against its
pinned trust directory; request attributes and `NodeInfo` self-description are
not inputs. Consensus accepts only an exact-subject `Producer`. Missing,
unavailable, malformed, or consumer results deny. The built-in test/small-deploy
source is an authority-signed exact producer set, not an allow-all closure.

Admission revision is not Raft membership and never grants Store rights. It is
checked before every new Raft call/stream; an already-open snapshot stream is
rechecked at bounded chunk intervals and before install. This can reduce
availability during policy disagreement, but cannot add a voter or authorize a
resource command.

## 9. Deterministic state machine

The OpenRaft application data has exactly three private variants:

```text
CommandV1 =
    Client { actor_node_id, client_id, sequence, request_hash, action: ActionV1 }
  | Expire { resource, epoch, lease_revision }
  | MembershipProgress { operation_id, expected_phase, new_phase,
                         observed_membership_log_id, result_code }
```

Only Store handlers construct `Client`; only the elected leader's timer and
membership controller can submit the other variants through an in-process
handle. No public Fory payload decodes directly into `CommandV1`. OpenRaft blank
and membership entries are handled as their native entry types; apply persists
membership but does not reinterpret it as an application command.

### 9.1 Resource initialization and transitions

A resource is lazily created on the first authorized `acquire`, `set_grant`, or
administrator initialization command with:

```text
epoch = 0
held = false
holder = NULL
ttl_ms = 0
lease_revision = 0
value_revision = 0
value = empty bytes
acl_revision = 0
```

All transition arithmetic is checked. Apply performs no clock, random, network,
filesystem, policy-service, or address-book read.

| Command | Preconditions | Atomic effects |
| --- | --- | --- |
| `Acquire(ttl)` | free; actor has `ACQUIRE`; TTL in bounds | epoch + 1, held, actor holder, lease revision + 1, set TTL |
| `Renew(fence)` | exact group/resource/epoch/actor, held, `RENEW` | lease revision + 1; epoch/value unchanged |
| `Expire(epoch, lease_revision)` | system command; exact held lease | clear holder, epoch + 1, lease revision + 1 |
| `CompareAndSet(fence, expected, value)` | exact holder/fence, `MUTATE`, exact value revision | replace value, value revision + 1 |
| `Release(fence)` | exact holder/fence, `RELEASE` | clear holder, epoch + 1, lease revision + 1 |
| `ReleaseWithValue(...)` | release and mutate rights; exact fence/value revision | replace value, value revision + 1, clear holder, epoch + 1, lease revision + 1 |
| `Revoke(resource)` | actor has `REVOKE` | clear holder, epoch + 1, lease revision + 1 |

`Acquire` never takes a held row, even if a leader timer believes it is old. A
leader must first commit the exact `Expire`; log order then decides an expiry vs
renew race. Failed preconditions change no resource counter or value, but their
applied result is saved for idempotency and audit.

`compare_and_set` does not renew. A failed `release_with_value` leaves both the
value and lease unchanged.

### 9.2 Lease timers

After observing an applied acquire/renew, the current leader arms a monotonic
timer for that row's full committed TTL. On fire it proposes
`Expire(resource, epoch, lease_revision)`. Stale timers become harmless applied
rejections.

On election, restart, or a gap in timer reconstruction, a leader loads all held
rows and gives each a fresh full TTL. This may delay failover but cannot create
overlap. The holder's client self-limit starts at request send time and expires
without a committed renew, so quorum loss makes it stop presenting a fence
before the conservative server timer can transfer ownership.

TTL bounds are 5,000–300,000 ms; the client default is 30,000 ms and renew
target is one third of TTL. Configuration may narrow but not widen these v1
limits.

### 9.3 Linearizable reads

`read_linearizable` is leader-only:

1. authorize the actor/resource;
2. call OpenRaft's linearizable-read barrier;
3. wait until `state.sqlite.last_applied` is at least the barrier log id;
4. read ACL and resource from one state-actor SQLite read transaction;
5. recheck permission at that applied point and return the snapshot.

The public service has no follower-local read. Local state may feed metrics or
debugging through an explicitly advisory in-process API, never a fence decision.

### 9.4 Existing lease-engine adapter

`CoordinationSerializer` binds one client/group/namespace and implements the
existing `LeaseSerializer`, but its string resource is a canonical adapter
encoding:

```text
ac1:<group-id-hex>:<namespace>:<base64url-no-pad(resource-key-bytes)>
```

It rejects non-canonical input and a group other than the client-bound group.
The encoded group remains in the `core::Fence.resource` used by the renew loop,
so an old-group handle cannot be reinterpreted against a recovered group. It
also verifies every `LeaseOp` actor string equals the client's immutable local
NodeId; the wire still omits that actor. Coordination permission rejections map
to `DenyReason::Unauthorized`, and advisory `LeaseSnapshot.remaining` is
`None`.

`CoordinationLease` is the normal public API. It owns the underlying
`LeaseHandle`, parses/validates the canonical adapter resource, and exposes only
the full `FenceV1` containing group/resource/epoch/holder. Register mutation
methods take that full fence; they never accept `core::Fence` directly.

One additive `LeaseHandle` seam is required for atomic final release:

```rust,ignore
pub fn stop_renewing(self) -> Option<Fence>;
```

It aborts the existing renew task, marks the handle released locally, and
returns the fence only if status/self-limit still permits acting; it does not
call `LeaseSerializer::Release`. `CoordinationLease::release_with_value`
consumes the wrapper through this seam and journals the one atomic coordination
RPC. The existing `LeaseHandle::release`, designated grantor, and renewal loop
retain their behavior. This avoids a second renewal implementation while still
making final value + fence advance one committed command.

## 10. Idempotency and the client journal

State is keyed by `(actor_node_id, client_id)` and stores last sequence, BLAKE3
request hash, serialized applied result, and retired state.

For a non-retired client:

- first accepted sequence is 1;
- `sequence == last + 1` applies and saves result;
- `sequence == last` plus the same hash returns the exact saved result;
- `sequence == last` with another hash returns `SequenceConflict`;
- `sequence < last` returns `SequenceTooOld`;
- `sequence > last + 1` returns `SequenceGap`.

Resource/ACL effects, session result, authoritative audit row, and
`last_applied` commit in one SQLite transaction. A client may have only one
outstanding request per client id. The standard lease wrapper serializes renew,
value, and release calls through one sequencer; callers needing independent
streams create distinct durable client ids.

The client journal algorithm is mandatory:

1. fsync a new random client id on first use;
2. fsync `(sequence, canonical action bytes, hash, Pending)` before network I/O;
3. retry the exact pending bytes after timeouts, redirects, reconnects, or
   process restart;
4. after a definitive authenticated `NotConsumed` response, either retry the
   action or replace it while reusing the same sequence; advance the sequence
   only after a matching `Consumed` result has been fsynced;
5. never guess whether a disconnected request committed.

Journal records carry format version and checksum and use write-temp/fsync/
atomic-replace/directory-fsync. Corruption, wrong actor/group, or an unknown
newer format fails closed; the library never deletes/reinitializes it
automatically. Creating a new client id after losing an ambiguous journal is an
operator/application reconciliation decision, not an automatic retry of the
same high-level mutation.

No age-based session GC exists in v1. `retire_client` replaces the saved result
with a permanent compact tombstone containing the last sequence/hash and a
terminal `Retired` result, so an ambiguous retirement retry is still answered
without reapplying. That client id can never mutate again. The actor may retire
its own idle client after no pending request; an administrator may retire one
operationally. Limits require clients to retire old ids rather than make
ambiguous retries unsafe.

## 11. SQLite durability and schema

### 11.1 Directory and pragmas

```text
<storage>/
    raft.sqlite
    state.sqlite
    snapshots/
    incoming/
    LOCK
```

Open `<storage>/LOCK` read/write and hold
`fs2::FileExt::try_lock_exclusive()` for the runtime lifetime before opening
either database; lock contention fails startup. Reject symlinked database files
and mismatched ownership/permissions according to the host's documented
platform policy. The lock is advisory against cooperating processes; arbitrary
code already running as the storage owner is inside the host threat boundary.

New Unix directories/files use mode `0700`/`0600`; broader existing write
permissions fail production startup unless an explicitly development-only
override is set. Windows uses the caller's application-data directory and
inherited user ACL, covered by the platform test/runbook. V1 provides no
application-layer at-rest encryption: deployments use OS volume encryption if
opaque register values or ACLs are sensitive.

Both databases use and verify:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA wal_autocheckpoint = 1000;
PRAGMA journal_size_limit = 67108864;
```

Open separate, single-writer blocking actors for log and state-machine I/O.
Async OpenRaft tasks communicate over bounded channels and never run rusqlite
on a Tokio worker. State reads are serialized through the state actor in v1;
optimize with a read pool only after correctness tests and measurement.

First-open metadata is written/fsynced in both databases before the runtime is
eligible to initialize Raft. On restart their schema/group/genesis identities
must agree. If a crash left one side uninitialized, startup may recreate it only
when both sides prove that no vote, log, applied entry, resource, ACL, or client
state ever existed; otherwise it fails as partial/corrupt storage. Normal
`raft.sqlite`-ahead-of-`state.sqlite` recovery is not partial initialization:
OpenRaft replays its committed log after the durable `last_applied` marker.

### 11.2 `raft.sqlite` schema and contract

Schema v1 contains:

```text
raft_meta(key TEXT PRIMARY KEY, value BLOB NOT NULL)
raft_log(log_index INTEGER PRIMARY KEY, entry BLOB NOT NULL)
```

The allowlisted meta keys hold schema/group/genesis identity, the immutable
local NodeId, vote, committed log id, and last-purged log id. Values and entries
use the exact internal codec v1 and have decode/size checks.

- `save_vote` commits before returning.
- `append` serializes all log/vote writes, commits a contiguous batch, makes it
  readable, and invokes OpenRaft's `LogFlushed` callback only after durable
  commit succeeds.
- truncate and purge are transactions and cannot create a hole.
- `save_committed` is implemented and persisted even though the durable state
  machine can recover without it.
- a failed durability callback moves the local runtime to fatal/not-ready; it
  is never reported as success.

Run OpenRaft `testing::Suite::test_all` against fresh and reopened stores.

### 11.3 `state.sqlite` schema and transaction

Schema v1 contains at least:

```text
coordination_meta(singleton, schema_version, group_id, genesis_hash,
                  last_applied, membership_bytes, admin_revision,
                  recovery_export_hash, aggregate_value_bytes, audit_floor)
resources(namespace, resource_key, epoch, held, holder_node_id, ttl_ms,
          lease_revision, value_revision, value, acl_revision,
          PRIMARY KEY(namespace, resource_key))
resource_acl(namespace, resource_key, subject_node_id, rights,
             PRIMARY KEY(namespace, resource_key, subject_node_id))
group_admins(subject_node_id PRIMARY KEY)
client_sessions(actor_node_id, client_id, last_sequence, request_hash,
                last_result, retired,
                PRIMARY KEY(actor_node_id, client_id))
membership_intents(operation_id PRIMARY KEY, actor_node_id, request_hash,
                   expected_membership, desired_plan, status, result)
node_routes(node_id PRIMARY KEY, ticket_bytes, source_operation_id)
audit(log_term, log_index, actor_node_id, operation, resource_namespace,
      resource_key, result_code, epoch, value_revision,
      PRIMARY KEY(log_term, log_index))
```

Use `CHECK` constraints for byte lengths, booleans, non-negative counters, known
rights, and configured hard bounds where SQLite can enforce them. Decode then
validate every BLOB; database bytes are not assumed trustworthy merely because
they are local.

Each non-empty OpenRaft `apply()` batch runs entries in order inside one SQLite
transaction. For every entry that transaction persists:

- the membership entry or command effects;
- ACL/admin changes and any atomic fence;
- idempotency result/tombstone;
- membership intent/route changes;
- exactly one audit row for a user/system command;
- the new `last_applied` log id.

No entry in the batch becomes visible without the others; this batching is only
an fsync optimization and does not change log order. Only after COMMIT does
apply return all responses to OpenRaft. On restart,
OpenRaft replays committed log entries after `last_applied`; duplicate application
returns the saved result and does not duplicate effects or audit.

Audit retention is deterministic and bounded: retain the newest 100,000
user/system command rows by log order, advance `audit_floor`, and delete older
rows in bounded batches inside apply transactions. Applications needing longer
history export/mirror it to their AP audit plane; no local policy or wall clock
decides which authoritative rows remain.

### 11.4 Snapshots and install

Snapshot creation uses rusqlite's online backup API to produce a consistent
SQLite file, then runs `quick_check`, reads its metadata, fsyncs it, and writes a
manifest containing format/schema/codec versions, group id, genesis hash,
last-applied id, membership, length, and BLAKE3 checksum. Later state writes do
not change the snapshot.

Install writes a same-filesystem temporary file, enforces stream bounds,
verifies checksum/manifest/SQLite/schema/group/genesis/last-applied membership,
and runs `quick_check`. It then pauses the state actor, runs
`wal_checkpoint(TRUNCATE)` on the old database, closes all handles, and clears
its closed WAL/SHM sidecars before replacement; the checkpointed old main file remains a
complete fallback at that point. It atomically replaces `state.sqlite` using a
platform abstraction (rename and directory fsync on Unix;
`ReplaceFileW`-style semantics on Windows), fsyncs the directory, reopens it,
re-enables/verifies the pinned WAL pragmas, revalidates it, records the current
snapshot, and only then reports install success. A crash at each boundary must
leave either the old or new complete state usable, never a main-file/WAL
mixture.

Defaults are snapshot after 10,000 applied log entries, retain 1,000 log entries
behind the snapshot, 1 MiB stream chunks, 512 MiB maximum snapshot, and one
in-progress incoming snapshot. Values/resources are bounded so exceeding the
snapshot maximum is a storage-pressure failure, not a reason to stream
unbounded data.

### 11.5 Disk pressure and shutdown

At the 384 MiB state-database or configured free-space high-water mark, reject
acquire, value growth, new grants/resources/clients, and membership expansion;
continue reads and bounded safety-reducing operations such as release, revoke,
ACL removal, and client retirement while the reserved headroom remains. Below
the larger of 1 GiB free or twice the latest snapshot size, reject all new Store
mutations, emit storage-pressure health, and shut down the local coordination
runtime cleanly so another voter can lead. If a committed apply cannot be
persisted first, stop serving authoritative reads/writes, emit a fatal health
state, and terminate the local coordination runtime; never skip the entry.

Graceful `AsterServer::shutdown()` orders:

1. mark coordination draining and reject new Store mutations;
2. wait up to five seconds for in-flight Store calls;
3. stop expiry/membership controllers and call OpenRaft shutdown;
4. checkpoint/commit and close state/log actors;
5. stop the shared RPC server and Aster node.

Process kill at any step remains recoverable from committed SQLite state.

## 12. Limits and backpressure

V1 defaults and hard behavior are:

| Limit | Default |
| --- | ---: |
| production voters | 3 or 5 |
| retained learners | 2 |
| resource value | 65,536 bytes |
| aggregate resource values | 256 MiB |
| state-database high-water | 384 MiB |
| resources per group | 100,000 |
| ACL rows per group | 500,000 |
| grants per resource | 256 |
| group administrators | 64 |
| retained authoritative audit rows | 100,000 |
| active client ids per actor | 8 |
| active client ids per group | 1,024 |
| Store calls in flight per actor | 32 |
| Store calls in flight per group | 256 |
| mutating requests per actor | token bucket 20/s, burst 40 |
| denied requests per actor | token bucket 10/s, burst 20 |
| TTL | 5–300 seconds |
| Raft unary payload | 4 MiB |
| snapshot chunk / complete snapshot | 1 MiB / 512 MiB |

Configuration may lower these. Raising value, aggregate state, snapshot,
resource, ACL, audit, or client limits requires explicit `CoordinationLimits`
values, a compatible snapshot cap, and tests at that size; wire hard caps
remain. Bounds are checked before expensive decode/allocation and again before
proposal/apply. Bounded channels return backpressure rather than spawning
unbounded tasks.

## 13. Backup, replacement, and disaster recovery

Normal node repair does not restore a database backup: add a fresh learner,
allow snapshot/log catch-up, promote it, then remove the failed node. Back up
the signed genesis, authority public key, node identities through the
application's secret-management policy, and periodic verified coordination
exports. Never copy a live pair of SQLite/WAL files with ordinary filesystem
copy.

An administrative `RecoveryExportV1` is a linearizable, canonical,
checksum-addressed export of resource values/revisions, ACL/admin state, source
group/genesis, last-applied membership, and audit boundary. It is not a Raft
log backup and cannot be installed into a running group.
It can be created only while the source group has a linearizable leader, so
operators must schedule exports before a disaster; a minority cannot bless its
local state as the recovery point.

Its canonical postcard body is:

```text
format_version = 1
source_group_id
source_genesis_hash
source_last_applied
source_membership
source_audit_floor
source_audit_tip
source_admin_revision
resources: sorted (ResourceKey, source_epoch, ttl_ms,
                   lease_revision, value_revision, value, acl_revision)
grants: sorted (ResourceKey, subject_node_id, rights)
group_admins: sorted NodeIds
```

It intentionally excludes holder/held authority, client sessions, node routes,
pending membership intents, and audit bodies. The stream manifest contains body
length and BLAKE3 hash; the new signed genesis binds that hash.

If the original quorum is permanently lost:

1. declare the old group unavailable; do not force a remaining minority to
   become leader;
2. generate a new random group id and signed genesis whose
   `optional_recovery_export_hash` binds the selected export;
3. copy the exact export out of band to every initial voter and configure its
   local path in `CoordinationStart::Bootstrap`;
4. on every initial voter, before `Raft::initialize`, verify the bound hash and
   deterministically create identical logical state: imported rows retain
   values and value revisions, start free, and use
   `epoch = source_epoch.checked_add(1)` and
   `lease_revision = source_lease_revision.checked_add(1)`; group
   administrators become the union of the exported set and new genesis
   administrators, with
   `admin_revision = source_admin_revision.checked_add(1)`;
5. bootstrap the new 3/5-voter group only after every online initial voter has
   validated the export;
6. have the application authority publish/select the new group id and tombstone
   the old one;
7. require clients to match the new group id before accepting any fence.

The bootstrap transformation excludes client sessions, pending membership
intents, and old membership; recovered clients generate new client ids and the
signed genesis supplies new membership. It is rejected for non-empty storage,
another export hash, any counter overflow, non-canonical ordering, or any row beyond
v1 limits. No apply command reads the export or filesystem. Old and new numeric
epochs are not comparable; the group id is part of every fence. Portal must
also reconstruct and validate the immutable content referenced by the selected
head before it allows acquisition.

V1 intentionally has no `force_new_leader`, `remove_member_without_quorum`, or
"trust this local SQLite file" API.

## 14. Observability and operational contract

`CoordinationHandle` and `group_status` expose bounded structured data:

- lifecycle: disabled/bootstrapping/joining/ready/no-quorum/draining/fatal;
- group id/genesis hash, local role, current leader/term, committed membership;
- last log/committed/applied ids and per-peer replication lag;
- pending membership intent and learner snapshot progress;
- SQLite/log/snapshot byte sizes and free-space state;
- command/read latency, redirects, quorum loss, leader changes, and audit floor;
- grant/renew/expire/release/revoke and stale-fence/revision counts;
- auth/admission/membership/rate-limit denials without resource-value data;
- full-TTL re-arm events and client self-limit expirations.

Logs never include register values, credentials, signatures, secret keys, or
full tickets. Node/group/resource identifiers are structured and may be
redacted/hashes under host policy. State-machine audit keyed by log id is the
authoritative operation history; metrics and application-directory mirrors are
advisory.

User-facing availability wording MUST say:

- three voters tolerate one unavailable voter; five tolerate two;
- two voters require both and are unsuitable for two roaming devices without a
  stable third witness;
- a minority cannot acquire, renew, mutate, release, read authoritatively, or
  reconfigure;
- losing quorum stops progress rather than risking two active heads;
- Raft protects crash/partition faults, not compromised voters or missing CAS
  content.

## 15. Implementation slices and exit gates

Complete slices in order. A later slice may be prototyped, but it does not merge
before every earlier exit gate is green. Before editing an existing Rust
function/type, follow the repository's `sem impact` rule; after signatures,
run `sem verify --diff` where supported.

### Slice 0 — Feature and identity foundation

- [ ] Add `coordination-client`/`coordination` features and exact optional
  dependencies without changing the default graph.
- [ ] Add strict IDs/resource validators and golden tests.
- [ ] Preserve immutable `PeerProvenance` through `IncomingCall`, `CallParts`,
  dispatcher, and `Call` while retaining compatible `Call::peer()` behavior.
- [ ] Add service-level `IrohOnly` enforcement.
- [ ] Add empty reserved Store/Raft v1 contracts inside `aster`.
- [ ] Prove the same apparent NodeId over HTTP/custom transport is denied.
- [ ] Prove coordination services are absent when not explicitly configured.

Exit commands/evidence:

```bash
cargo test -p aster --no-default-features --features rpc
cargo test -p aster --no-default-features --features coordination-client
cargo check -p aster --no-default-features
cargo tree -p aster --no-default-features -e features
```

### Slice 1 — Pure command, ACL, and idempotency model

- [ ] Implement `CommandV1`, `ReplyV1`, rights, exact ACL/admin revisions, and
  all resource transitions as a storage-independent pure model.
- [ ] Implement deterministic `Expire`; keep the existing designated-grantor
  lease wire behavior unchanged.
- [ ] Implement client sequence/hash semantics and permanent retirement.
- [ ] Implement membership intent state, but not network execution.
- [ ] Run `sem impact LeaseHandle`, then add/test the additive
  `stop_renewing` seam without changing existing release/designated-grantor
  behavior.
- [ ] Property/model test every successful/rejected transition, counter edge,
  sequence replay, ACL race, and renew/expire ordering.
- [ ] Add a compatibility adapter test showing
  `CoordinationSerializer: LeaseSerializer` preserves fence/holder semantics.

Exit: the pure model has no clock/network/filesystem input and mutation test
histories are reproducible from only `(initial state, ordered commands)`.

### Slice 2 — SQLite OpenRaft storage

- [ ] Implement schema creation/version validation and exclusive directory lock.
- [ ] Implement serialized durable `RaftLogStorage` including callback ordering.
- [ ] Implement transactional `RaftStateMachine`, applied membership, audit, and
  idempotency result persistence.
- [ ] Run OpenRaft's complete storage suite on fresh/reopened stores.
- [ ] Implement snapshot build/current/install with checksum and atomic replace.
- [ ] Fault-inject process death after each log, apply, backup, fsync, and rename
  boundary on Linux, macOS, and Windows.
- [ ] Prove log replay and snapshot install produce byte-equivalent logical
  state to the pure model.

Exit: no acknowledged vote/log/apply regresses after restart; OpenRaft's suite
and the platform crash matrix are green.

### Slice 3 — Authenticated Raft transport and bootstrap

- [ ] Implement signed genesis/join parsing, canonical vectors, start-mode
  validation, and storage binding.
- [ ] Implement `VerifiedAdmissionSource` with a static signed/exact-NodeId test
  source and an adapter only for admission facts with preserved provenance.
- [ ] Implement Raft network factory, unary vote/append, streamed snapshots,
  identity/group/member checks, and size/time bounds.
- [ ] Bootstrap 1-dev, 2-explicit, 3-, and 5-voter clusters; make unsafe modes
  visibly non-default.
- [ ] Implement durable membership intents, learner catch-up, joint changes,
  retry/restart, and exact completion records.
- [ ] Test wrong identity, wrong group, admitted non-member, producer non-member,
  stale permit, and leader loss at every membership stage.

Exit: a three-voter harness survives any one process loss, a minority cannot
commit, and no identity outside the exact rules reaches an OpenRaft handler.

### Slice 4 — Store service and client

- [ ] Implement replicated ACL/admin commands before exposing register
  mutations.
- [ ] Implement per-actor/group bounds, denial throttling, structured errors,
  and authorized-only leader hints.
- [ ] Implement all Store methods, linearizable reads, and leader expiry loop.
- [ ] Implement redirecting `CoordinationClient` and fsynced client journal.
- [ ] Implement `CoordinationSerializer` and `CoordinationLease` by reusing the
  existing holder/self-limit renewal engine.
- [ ] Implement readiness, metrics, audit query boundaries, disk-pressure stop,
  and ordered shutdown.
- [ ] Run concurrent history checking for acquire/renew/value/release/revoke
  under duplication, delay, partitions, redirects, and process kill.

Exit: every acknowledged operation is linearizable, every ambiguous mutation
is exactly-once on retry, and an arbitrary admitted consumer has no observable
coordination access.

### Slice 5 — Recovery, compatibility, and release hardening

- [ ] Implement/verify bounded `RecoveryExportV1` and the deterministic
  genesis-bound bootstrap transformation into a new group id on every initial
  voter.
- [ ] Test old-group fences against the recovered group and prove rejection.
- [ ] Add protocol/storage golden vectors and same-v1 rolling-restart tests.
- [ ] Reject newer schema/codec versions with actionable diagnostics.
- [ ] Benchmark renew/write/read/snapshot behavior at default limits and choose
  alert thresholds without weakening durability.
- [ ] Exercise Linux x86_64, macOS arm64, and Windows CI including SQLite atomic
  replacement and abrupt process termination.
- [ ] Write the operator/user guide: placement, bootstrap, backup, learner
  replacement, membership removal, quorum loss, and recovery.
- [ ] Run the repository-required `./scripts/build.sh` and final validation so
  Rust changes remain compatible with the wider Aster workspace/bindings.

Exit: the operational runbook has been executed from empty machines and from a
simulated permanent-quorum-loss incident, with evidence attached.

### Slice 6 — Portal as the first consumer

Portal work remains checkable in its own Phase 6 plan. The integration MUST:

- [ ] pin `portal.roaming` + canonical `TreeId` as the resource key;
- [ ] define and golden-test `PortalActiveHeadV1`;
- [ ] prove the referenced immutable closure meets Portal durability policy
  before committing the head;
- [ ] make `release_with_value` the graceful final-head/fence transition;
- [ ] reject every old-group, old-epoch, wrong-holder, and wrong-Tree commit;
- [ ] preserve post-fence local work as an explicit conflict;
- [ ] drive ACL/membership changes from exact root-authored Portal policy while
  treating committed Aster state as the enforcement authority;
- [ ] pass the Portal kill/partition/reconstruction acceptance matrix.

Exit: no Portal path treats independent local `portal-store` rows or eventual
policy records as the serializer for an active head.

## 16. Required test matrix

The slice gates are not substitutes for this release matrix.

Use four distinct harnesses so mocks do not accidentally prove their own
assumptions:

1. a pure property/model harness generates ordered `CommandV1` histories and
   compares every reply/state row;
2. OpenRaft's official storage suite plus a storage-fault adapter fails before/
   after SQLite BEGIN, row writes, COMMIT, fsync callback, checkpoint, snapshot
   checksum, replace, and reopen;
3. an in-process deterministic cluster network drops, delays, duplicates,
   reorders, and partitions Raft/client messages with a recorded seed;
4. real Aster/Iroh loopback nodes and subprocess voters prove transport
   identity, admission, redirects, SQLite restart, and actual process kill
   (`SIGKILL`/platform equivalent rather than a Rust panic).

The linearizability checker records invocation/response intervals, immutable
actor, group/resource, client/sequence/hash, and observed result, then searches
for a legal sequential history against the pure model. On failure CI preserves
the smallest seed/history, database files, logs, and membership timeline.

### Contract and security

- [ ] Reserved wire/service names compile only inside the Aster-owned package.
- [ ] Client-only/default builds start no voter, create no SQLite files, and
  advertise no coordination service.
- [ ] Wrong group/version/length/rights/encoding is rejected before allocation
  or proposal.
- [ ] Request-supplied actor/role/Raft id cannot override Iroh provenance.
- [ ] HTTP/custom principal equal to a voter/holder NodeId is denied.
- [ ] Consumer with no grant cannot read resource/ACL state, acquire, renew,
  mutate, release, revoke, administer, or call Raft; a follower hint reveals at
  most leader NodeId/term for an already-known random group id.
- [ ] Consumer granted resource A cannot observe or operate resource B through
  prefix, Unicode, alternate encoding, or binary-key tricks.
- [ ] Producer outside membership cannot call Raft; member has no implicit
  resource/admin right; group-A member cannot call group B.
- [ ] ACL removal and holder fence occur in the same applied transaction.
- [ ] Authorization denial floods are bounded and create no Raft log entries.
- [ ] A follower hint for an ungranted admitted peer contains only leader
  NodeId/term and no resource-existence, holder, ACL, or value signal.

### State, storage, and consensus

- [ ] Two acquire racers yield one holder.
- [ ] Renew/expire outcome follows log order; stale expiry is harmless.
- [ ] Old/sleeping holder and same-epoch different actor are rejected.
- [ ] CAS conflict changes nothing; release-with-value is all-or-nothing.
- [ ] Commit then drop response returns the exact saved result on retry.
- [ ] Same sequence/different hash, old sequence, gap, and retired-client
  outcomes have the pinned disposition and never duplicate an effect.
- [ ] Crash/replay does not duplicate effect, audit, or membership intent.
- [ ] Linearizable reads never return state before their barrier.
- [ ] Three/five voters meet their stated fault tolerance; two stops on one
  loss.
- [ ] Minority and isolated old leader cannot renew or mutate.
- [ ] Learner snapshot/catch-up precedes promotion.
- [ ] Leader loss during joint membership converges to one committed config.
- [ ] Snapshot under writes installs to logical equality and corruption fails.
- [ ] Disk-full/SQLite failure never acknowledges an unpersisted mutation.
- [ ] Deterministic audit pruning produces the same floor/rows on every voter
  and survives snapshot/replay.

### Lifecycle and operations

- [ ] Restart with wrong group/genesis/NodeId/authority fails.
- [ ] New leader/restart gives held rows a full TTL and never overlaps holders.
- [ ] Client self-limit stops fence presentation on quorum loss.
- [ ] Graceful and killed shutdown both recover.
- [ ] Fresh learner replacement restores redundancy without database copying.
- [ ] A minority cannot create a recovery export.
- [ ] Recovery creates a new group, preserves chosen value state, frees leases,
  and rejects every old-group fence.
- [ ] Metrics/readiness distinguish bootstrap, no quorum, disk pressure,
  draining, and corruption/fatal state.

## 17. Definition of done

V1 is complete only when:

- [ ] every implementation and release checkbox through Slice 5 is complete;
- [ ] the required test matrix is green on Linux, macOS, and Windows;
- [ ] default/client-only dependency isolation is proven in CI;
- [ ] protocol, schema, signed-document, and recovery golden vectors are checked
  in;
- [ ] the operator guide accurately describes quorum and disaster behavior;
- [ ] Portal's Slice 6 acceptance tests are green for the first supported use;
- [ ] at least one independent reviewer has audited transport identity,
  replicated authorization, SQLite durability, snapshot install, membership,
  and lease timer behavior;
- [ ] evidence records exact dependency versions, commands, platform, commit,
  and fault-injection seed.

Deferred ideas are not hidden blockers: multi-group hosting, generic custom
state machines, arbitrary SQL, non-Rust voters, HTTP clients, sharding, and
cross-resource transactions remain explicitly out of scope after v1 ships.

## 18. Pinned implementation references

- OpenRaft 0.9.25 getting started and durability requirements:
  <https://docs.rs/openraft/0.9.25/openraft/docs/getting_started/>
- OpenRaft 0.9.25 split log/state-machine traits:
  <https://docs.rs/openraft/0.9.25/openraft/storage/trait.RaftStateMachine.html>
- OpenRaft linearizable reads:
  <https://docs.rs/openraft/0.9.25/openraft/docs/protocol/read/>
- OpenRaft dynamic membership:
  <https://docs.rs/openraft/0.9.25/openraft/docs/cluster_control/dynamic_membership/>
- rusqlite 0.40.2 online backup API:
  <https://docs.rs/rusqlite/0.40.2/rusqlite/backup/>
- fs2 0.4.3 cross-platform file locking:
  <https://docs.rs/fs2/0.4.3/fs2/trait.FileExt.html>
- SQLite WAL and synchronous durability behavior:
  <https://sqlite.org/wal.html>, <https://sqlite.org/pragma.html#pragma_synchronous>

## 19. Evidence ledger

A checkbox closes only when its durable evidence is recorded here or linked
from here. Use stable slice/test ids in commits and CI artifacts.

| ID | Date | Commit | Platform | Command / fault seed | Result / artifact | Reviewer |
| --- | --- | --- | --- | --- | --- | --- |
| _pending_ |  |  |  |  |  |  |
