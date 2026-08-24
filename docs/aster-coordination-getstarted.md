# Aster coordination: a consumer walkthrough

> **Draft — the coordination feature has not landed as a supported public
> API.** The examples below describe the intended Rust consumption pattern and
> use prospective API names from the coordination design. They are deliberately
> marked `rust,ignore`: do not expect them to compile until this banner is
> removed. Public wire declarations, durable result delivery, status cuts,
> recovery import, and compatibility gates must still be completed and may
> refine the exact spelling without changing the responsibilities described
> here.

**Status date:** 2026-08-24.

Aster coordination gives an Aster application a small, durable place to decide
facts for which there must be exactly one current answer:

- who may act as the owner of a resource;
- which immutable version is the active one;
- which generation of a singleton workload is authoritative; or
- which small set of producer nodes currently maintains that decision.

It is not a general database. Large data remains in blobs, documents, or the
application's own store. Coordination holds a compact pointer or generation and
orders changes to it.

The intended consumer experience is:

1. enable coordination on a few persistent producer nodes;
2. tell Aster which nodes are eligible and what placement constraints matter;
3. grant exact callers access to exact resources;
4. acquire a fenced lease or update a small register; and
5. let Aster handle leaders, replication, retries, membership changes,
   snapshots, and restart recovery.

This guide follows that path using a ten-producer network with exactly three
voters.

For the underlying contracts, see the
[coordination design](_internal/working_ideas/aster-coordination.md) and
[implementation plan](_internal/working_ideas/aster-coordination-implementation-plan.md).
The existing [lease guide](aster-leases-getstarted.md) explains fencing in more
depth, and the [topology guide](aster-topology-getstarted.md) explains the
network observations used for placement.

## 1. Consensus without the terminology

Imagine three trusted nodes keeping identical, numbered notebooks. One is the
leader. A change becomes authoritative only after at least two notebooks have
accepted it, and every notebook applies accepted changes in the same order.

That gives the group one answer even when messages are delayed, duplicated, or
delivered in different orders:

- with all three voters healthy, the group operates normally;
- with one unavailable, the other two still form a majority and continue;
- with two unavailable, the survivor stops rather than inventing an answer;
- when a voter returns or is replaced, it catches up before it may vote; and
- after a leader fails, another voter can take over from the committed history.

That is the useful part of Raft for an application. Consumers should not call
Raft APIs or implement those transitions themselves. Aster embeds the runtime,
durable storage, authenticated network, snapshots, and membership controller
behind the coordination service.

The roles are independent:

| Role | Meaning | Typical count in the example |
| --- | --- | ---: |
| Producer | A node operated by the application and admitted to its producer trust boundary | 10 |
| Eligible host | A producer the application permits to host coordination durably | up to 10 |
| Voter | A currently committed member that participates in consensus | exactly 3 |
| Learner | A non-voting member catching up or deliberately retained | 0–2 |
| Client or lease holder | A caller using a coordinated resource | any number |
| Deployment ROOT | Aster's immutable trust anchor and break-glass super-administrator | exactly 1 |
| Delegated group administrator | A mutable identity allowed to change ACLs or request membership plans | application-defined, at least 1 |

A voter receives no automatic application permission. An administrator need
not be a voter, and a consumer laptop may hold a lease without ever becoming a
producer or voter. ROOT is not a voter role and has no implicit right to read or
mutate a coordinated resource; it can grant an exact right when an operator
deliberately brings the ROOT identity online.

**Benefit:** a large Aster network does not become a large consensus group.
Only the small committed voter set pays the replication cost; every other node
uses it as a client or remains an eligible replacement.

## 2. Choose the right state

Use coordination for small state where two simultaneous answers would be
unsafe:

- an active manifest hash;
- a primary workload generation;
- an exclusive volume activation;
- a catalog head;
- a monotonic fence; or
- another bounded value whose update must be linearizable.

Do not use it for:

- large files or manifests — put those in `iroh-blobs` and coordinate a hash;
- collaborative or offline multi-writer data — use iroh-docs or a local-first
  store;
- telemetry, discovery, or advisory status;
- one Raft group per key; or
- arbitrary SQL and cross-resource transactions.

One coordination group normally serves one administrative fleet or trust
domain and contains many independently authorized resource rows.
V1 hosts at most one coordination group in an `AsterServer`; applications that
truly need another trust domain use another server instance rather than making
one group per resource.

A useful application pattern is:

```text
immutable content in blobs
          |
          | prove the content is durable enough for the application
          v
small versioned head in Aster coordination
          |
          | current fence + expected value revision
          v
one authoritative activation
```

Consensus orders the head. It does not prove that the content named by the head
exists or has enough replicas; that remains the application's responsibility.

**Benefit:** the expensive consensus path is limited to the few bytes that need
one global answer, while bulk data keeps the scaling and distribution model
best suited to it.

## 3. The ten-node, three-voter shape

Assume ten admitted producers, `A` through `J`. The application declares all
ten eligible, and the placement policy asks for three voters. Aster's topology
snapshot provides current locality and positive separation evidence.

```text
existing Aster producer swarm

  positively separated cluster alpha     cluster beta       cluster gamma
          A*  D  E  F                       B*  G  H             C*  I  J
          |                                  |                    |
        voter                              voter                voter
          \____________ committed three-voter group ____________/

  D through J remain eligible clients/replacements, not voters.

placement manager (one application control-plane task)
    topology snapshot + verified application policy
                       |
                       v
              deterministic recommendation
                       |
             application provisioner, if needed
                       |
                       v
       authorized, revision-fenced membership request
                       |
                       v
            current quorum commits membership
```

The asterisk is a committed role, not a local configuration label. Topology may
recommend a candidate, but only a membership entry committed by the current
quorum makes it a voter.

If `B` disappears, `A` and `C` can still commit. After the configured
replacement grace, the manager may select one eligible candidate, provision it
as a learner, wait for it to catch up, and request a transition back to exactly
three voters. Ordinary automatic maintenance moves at most one new voter per
membership plan.

If two voters disappear before a replacement commits, the group has no
majority. It stops. Neither topology nor an administrator can safely force a
local node into membership; the operator must restore a voter or use the
new-group recovery procedure.

**Benefit:** the application describes the desired shape and supplies hosts;
Aster performs the safe, resumable membership workflow without turning all ten
nodes into voters.

## 4. Enable only the surface each process needs

The hosting feature is opt-in and brings the consensus and storage stack:

```toml
# A process that may host a voter or learner.
[dependencies]
aster = {
  version = "0.3",
  registry = "aster",
  features = ["coordination"]
}
```

A client-only process does not need OpenRaft or redb:

```toml
# A caller, holder, administrator, or placement control-plane process.
[dependencies]
aster = {
  version = "0.3",
  registry = "aster",
  features = ["coordination-client"]
}
```

The hosting feature remains off by default. Enabling `coordination-client`
does not start a service, open a voter database, or make the node eligible for
membership.

V1 hosting and the supported client are Rust-first. The public payloads are
defined so later language bindings can carry them, but Python, TypeScript, C,
Java, and Kotlin coordination surfaces are not part of this initial delivery.

**Benefit:** ordinary Aster applications and language bindings do not inherit a
consensus/database stack merely because the SDK supports it.

## 5. Prepare identity, trust, and durable paths

Four application-owned inputs exist before a group can start.

### 5.1 Persistent node identity

Every potential coordination host needs a restart-stable Aster identity. Raft
membership names the exact NodeId; restarting with a new secret creates a
different node, not a recovered copy of the old one.

The NodeId is the address. Aster discovery resolves a NodeId to its current
route, and a node ordinarily binds an ephemeral UDP port, so the same identity
keeps working across IP and port changes. Tests, examples, and deployment
tooling must not pin direct ports or treat coordination endpoints like HTTP
host:port pairs — a rendered address is at best an advisory routing hint.

Keep these together in the host's durable state:

```text
/var/lib/my-app/
    node.secret
    coordination/
        LOCK
        raft.redb
        state.redb
        snapshots/
        incoming/
```

Each host has its own directory. Never copy a live voter's directory to create
another voter.

### 5.2 Reuse Aster's deployment ROOT

Coordination does not introduce another authority key. It reuses the one Aster
deployment ROOT already defined by the
[Aster trust specification](../ffi_spec/Aster-trust-spec.md) and configured
through `AsterConfig`, including the normal production environment inputs:

```bash
# Inline hex, or point at a hex/JSON public-key file.
export ASTER_ROOT_PUBKEY=<64-hex-characters>
# export ASTER_ROOT_PUBKEY_FILE=/secure/config/aster-root.pub
```

That ROOT signs the group's genesis, join permits, admission set, and later
recovery documents. A voter receives only the public key and signed material.
The coordination-specific authority domain remains an independent expectation:
one ROOT may operate more than one environment, so a correctly signed staging
genesis must not open a production group.

The ROOT secret does not live in a voter runtime or placement manager. Keep it
offline in normal operation. When a break-glass or provisioning action really
needs ROOT authority, a short-lived administration tool may deliberately use
the ROOT secret as its Aster/Iroh endpoint identity. Its authenticated NodeId is
then the configured ROOT public key, which every voter recognizes as the
immutable super-administrator. A host-local setting that merely claims “ROOT is
loaded” grants nothing.

ROOT is separate from the mutable delegated administrator set recorded in the
group. It cannot be added or removed with `set_admin`, does not consume a
delegated-admin slot, and receives no implicit resource rights. The signed
genesis supplies at least one day-to-day delegated administrator so routine
operations do not require bringing ROOT online.

### 5.3 Verified producer admission

The application implements `VerifiedAdmissionSource`. It answers whether an
authenticated NodeId is a verified producer and preserves evidence from the
application's enrollment or policy layer. Missing, unavailable, malformed, or
consumer-only results deny consensus traffic.

Producer admission is necessary but deliberately insufficient:

- it does not grant resource rights;
- it does not make the producer eligible under placement policy; and
- it does not add the producer to committed membership.

### 5.4 A random group identity

The deployment tool creates one random 32-byte `CoordinationGroupId`. The group
id, signed genesis hash, deployment profile, Aster ROOT key, and authority domain
are bound to both databases and checked on every restart.

**Benefit:** an address, role claim, copied database, or self-signed document
cannot silently create or join the authoritative group.

## 6. Bootstrap the initial three voters

Before anything can commit membership, deployment tooling uses the verified
eligible-host policy and one topology snapshot to run the same pure placement
planner without starting a manager. For the reference 4/3/3 topology it selects
one host from each positively separated cluster. The deployment ROOT then freezes
those exact three NodeIds into genesis; after bootstrap, the optional manager
maintains the committed set rather than inventing a second ranking algorithm.

The deployment tool constructs one signed genesis containing:

- the random group id;
- the production profile;
- exactly three initial voter NodeIds and their canonical, credential-free
  addressless tickets (routes are resolved from NodeId by Aster discovery);
- one or more initial administrator NodeIds;
- the authority domain and nonce; and
- no recovery hash for a new group.

The same signed genesis is supplied to `A`, `B`, and `C`. Each host uses its own
identity and storage directory. Before starting, each of the three must also
resolve the other two as verified producers through the application's admission
source:

```rust,ignore
// Contract preview: exact constructors may change before release.
let aster_config = AsterConfig::from_env()?;

let coordination = CoordinationConfig::builder()
    .storage_dir("/var/lib/my-app/coordination")
    .authority_domain("example.my-app.production")
    .start_mode(CoordinationStart::Bootstrap {
        signed_genesis: signed_genesis.clone(),
        recovery_export_path: None,
    })
    .verified_admission(admission_source.clone())
    .build()?;

let server = AsterServer::builder()
    // Coordination consumes the ROOT public key resolved by ordinary Aster
    // trust configuration. There is no second coordination authority key.
    .config(aster_config)
    .identity("/var/lib/my-app/node.secret")
    .builtin_coordination(coordination)
    .start()
    .await?;

// start() may return while the nodes are still finding one another.
server.coordination().unwrap().wait_ready().await?;
```

`start()` validates static configuration and opens bound storage before it
starts the runtime. `wait_ready()` is level-triggered: it returns immediately
if this node is already ready, otherwise it waits for readiness. It is an
observation, not a promise that the network will remain healthy forever.

Bootstrap has two committed steps. OpenRaft first establishes the native voter
membership. The leader then commits the exact signed genesis as a system
command; every voter re-verifies it and atomically installs the signed delegated
administrator set at revision 1. The group remains `Bootstrapping` between
those steps. Retrying the same genesis after a lost acknowledgement is
idempotent; another committed genesis is fatal. This is why initial
administrators are reproduced by log replay and snapshots instead of being
local rows written beside Raft.

Only empty stores use `Bootstrap`. An initialized voter restarts with
`CoordinationStart::Existing` and independently supplies the expected group id,
genesis hash, and deployment profile. A candidate uses `CoordinationStart::Join`
with a signed permit; the permit lets it communicate with bootstrap senders but
does not add it to membership. A joining node's own first start binds its
storage directory, so a `Join` restart reuses the same stable secret, the same
NodeId, and that already-bound directory — the permit is re-verified on every
start rather than persisted, and it stays valid across restarts because it
binds the candidate's NodeId, not an address: its `candidate_ticket` is the
canonical addressless ticket derived from that NodeId. An ephemeral IP or UDP
port change therefore never requires another permit.

For routine control, run a durable client under one of the NodeIds named in
`initial_admins`; do not turn a voter into ROOT. Keep a separate break-glass
client profile whose endpoint secret is the ROOT secret, load it only for an
explicit operation, and take it offline again. Both paths hit the same
replicated authorization predicate and audit trail.

**Benefit:** bootstrap is reproducible and identity-bound. Accidental second
clusters, wrong profiles, foreign storage, and ephemeral voter identities fail
at startup instead of becoming a split brain later.

## 7. Open a durable client

A mutating client binds its local Aster identity to one group and owns a durable
sequence journal:

```rust,ignore
let client = CoordinationClient::builder(node.clone(), group_id)
    .seed_tickets(initial_member_tickets)
    .journal_dir(app_state.join("coordination-client"))
    .build()?;
```

Persist the client's Aster identity and journal together. The journal is bound
to the exact actor and group, and one process owns its lifetime lock; opening it
under another identity or group fails.
Seed tickets are routing hints only. The Iroh handshake authenticates the
connected identities; possessing a ticket grants no coordination membership or
resource right.

Before sending a mutation, the client durably records the exact request bytes,
client id, and next sequence. Redirects, leader changes, timeouts, and retries
must resend those same bytes. The replicated session row makes the server
effect idempotent even when the command committed but the reply was lost.

> **Client-delivery decision (frozen):** a resolved mutation is fsynced as
> `ResolvedAwaitingAck` before the caller sees it and survives process death.
> The application acknowledges explicitly after its own durable handling;
> until then no new sequence is permitted, and on restart
> `unacknowledged()`/`resume()` redeliver the exact saved result. The
> guarantee is exactly-once **server effect** plus durable **redelivery**;
> exactly-once application-side processing additionally requires the
> application to make its durable handling and its acknowledgement atomic.

A caller that only needs authoritative reads can omit the journal:

```rust,ignore
let reads = CoordinationClient::builder(node.clone(), group_id)
    .seed_tickets(initial_member_tickets)
    .build_read_only()?;
```

The client can dial any known member. Followers return an authenticated leader
hint; the client reconnects directly so the leader authenticates the real
caller. The follower never becomes an authorization proxy.

**Benefit:** applications do not write leader-routing or ambiguous-retry logic,
and a lost response cannot make the same sequenced operation take effect twice.

## 8. Grant exact rights to an exact resource

Coordination is deny-by-default. The immutable ROOT or a delegated genesis
administrator may commit ACL changes, but administrative authority does not
implicitly grant access to resource values. `inspect_admins` lists the mutable
delegates; ROOT remains part of the group's static identity and cannot be added
or removed with `set_admin`.

An application chooses an exact resource key:

```text
namespace = "my-app.active-head"
key       = raw canonical workload or document id bytes
```

It then grants only the rights the subject needs:

```rust,ignore
// Illustrative API shape. ACL writes are revision-checked mutations.
admin_client
    .set_grant(
        resource.clone(),
        worker_node_id,
        Rights::READ
            | Rights::ACQUIRE
            | Rights::RENEW
            | Rights::MUTATE
            | Rights::RELEASE,
        expected_acl_revision,
    )
    .await?;
```

Rights are exact: `READ`, `ACQUIRE`, `RENEW`, `MUTATE`, `RELEASE`, and `REVOKE`.
There are no namespace prefixes, wildcards, inherited grants, or implicit
producer/voter permissions in v1.

The service checks access before proposing obvious denials, then the replicated
state machine checks it again in committed order. If an ACL changes during a
request, the committed check wins.

**Benefit:** authorization follows the state through leader changes, snapshots,
and restarts. An application policy adapter supplies facts, but cannot bypass
Aster's replicated enforcement.

## 9. Acquire, mutate, and release a fenced register

The stable abstraction is a fenced register: one current holder, a monotonic
epoch, a value revision, and a small opaque value.

The intended convenience API reuses Aster's existing holder/renewal engine:

```rust,ignore
let lease = match CoordinationLease::acquire(
    client.clone(),
    resource.clone(),
    LeaseOptions::default(),
)
.await?
{
    AcquireOutcome::Granted(lease) => lease,
    AcquireOutcome::Denied(reason, current) => {
        return handle_contention(reason, current).await;
    }
};

// Fetch the current fence immediately before protected work.
let fence = lease.fence().ok_or(Error::LeaseLost)?;
let current = client.read_linearizable(resource.clone()).await?;

let next_head = encode_active_head(manifest_hash, format_version)?;
let updated = client
    .compare_and_set(
        fence,
        current.value_revision,
        next_head,
    )
    .await?;

// A graceful handoff can commit the final value and release atomically.
lease
    .release_with_value(updated.value_revision, final_head)
    .await?;
```

The value belongs to the application and must be explicitly versioned. It is
bounded; large content belongs in blobs. `compare_and_set` requires both the
current fence and expected value revision. `release_with_value` either commits
the final value and advances the fence together, or does neither.

The fence is a `(group, resource, epoch, holder)` fact. A mutation succeeds only
when all of these are true inside the committed operation:

```text
the carried epoch is current
the authenticated caller is the current holder
the lease is still held
the expected value revision is current
```

Do not cache a fence across awaits. Ask the lease for it immediately before
each protected action, and stop when `fence()` returns `None`.

If the coordinated register itself is the protected resource, Aster performs
the fence check and value update in one replicated transaction. If the fence
protects an external database, volume, or service, that resource must check the
same fence in its own serialization point. A lease around an unfenced external
write API is only a suggestion.

TTL controls takeover latency, not safety. If the leader changes, timers are
re-armed conservatively. If quorum disappears, renewals stop and the holder's
self-limit removes its usable fence before a successor could be granted one.

**Benefit:** a paused, partitioned, or superseded process cannot continue
publishing authoritative mutations merely because it still believes it is the
owner.

## 10. Maintain three voters with topology-aware placement

Placement is split between three authorities:

- the application says which hosts are eligible and supplies policy;
- Aster takes one immutable topology snapshot and computes a deterministic
  recommendation; and
- the current Raft quorum alone commits the membership change.

The manager's node must have joined the producer-only shared topology namespace
described in the [topology guide](aster-topology-getstarted.md). That gives it a
view, not coordination permission. A new candidate must be both present in that
snapshot and named in the application's exact eligible-host map; producer
admission alone never makes it selectable.

The application policy for the ten-node example might look like this:

```rust,ignore
let policy = PlacementPolicy {
    revision: policy_revision,
    digest: verified_policy_digest,
    target_voters: 3,
    target_retained_learners: 0,
    eligible: eligible_hosts_a_through_j,
    pinned_voters: Default::default(),
    excluded: maintenance_exclusions,
    // With three voters there are three pairs. This test policy requires
    // positive separation evidence for all three; it is not a geography claim.
    minimum_separated_voter_pairs: 3,
    replacement_grace: Duration::from_secs(300),
    allow_healthy_rebalance: false,
};
```

The placement manager is an optional application control-plane task. Run one
active instance, or tolerate several racing instances: the membership revision
and one-active-intent rule make only one plan authoritative.

```rust,ignore
let mut manager = CoordinationPlacementManager::new(
    admin_client.clone(),
    node.topology(),
)?;

let report = manager
    .reconcile_once(policy, &candidate_provisioner)
    .await?;
```

One reconciliation:

1. reads membership and any pending intent through a linearizable status cut;
2. takes one atomic topology snapshot;
3. computes a pure recommendation;
4. asks the application provisioner to start a selected candidate, if needed;
5. re-reads policy, membership, and topology to reject stale work; and
6. sends a revision-fenced membership plan through the durable client.

The provisioner owns machines and credentials. It receives the idempotency tuple
`(group, policy digest, membership LogId, candidate NodeId)`, starts a
persistent-identity `Join` candidate, and returns its NodeId and signed join
permit; the permit's `candidate_ticket` is the canonical addressless ticket
derived from that NodeId, and the candidate's current route comes from Aster
discovery. Repeating that tuple must be safe. Aster never receives the Aster
ROOT signing secret.

The planner normally preserves healthy current voters. It replaces one only
after an explicit exclusion, removal from the eligible set, or continuous
topology absence beyond `replacement_grace`. Restarting the manager resets an
uncommitted absence timer, delaying replacement rather than making it earlier.
Moving a healthy voter requires the explicit `allow_healthy_rebalance` policy.

Topology provides evidence, not geography:

- `Separated` is positive, corroborated evidence and improves placement;
- `Unknown` is not silently treated as far apart;
- a known-close cluster is not a promise about rack, power, region, or
  availability zone; and
- topology bridge rank is never voter rank.

The manager reports `Stable`, `Degraded`, `Converging`, `NeedsProvisioning`,
`InsufficientEvidence`, `InvalidPolicy`, or an exact `Plan`. It never edits a
database or membership locally.

**Benefit:** Aster turns current topology and application policy into a safe
membership request, while the application retains control over trust, machines,
failure-domain requirements, and planned maintenance.

## 11. What common failures look like

The important property is not that failures disappear. It is that each failure
has a bounded, unsurprising outcome.

| Event | Aster behavior | Consumer behavior |
| --- | --- | --- |
| Current leader stops | Elects another caught-up voter and resumes from committed history | Retry through the durable client; do not select a leader yourself |
| Native membership commits but Genesis acknowledgement is lost | Remains `Bootstrapping`; the leader retries the same signed Genesis and apply recognizes exact replay | Keep the same signed document; never seed admin rows locally |
| One of three voters is unavailable | Remaining two retain quorum; placement may provision one replacement | Report degraded placement; keep eligible capacity available |
| Two of three voters are unavailable | Stops authoritative writes and membership changes | Restore a voter or follow signed new-group recovery; never force a minority |
| Reply is lost after commit | Retrying the same journaled sequence returns the saved result | Do not create a replacement request with a new sequence |
| Client process dies after resolution | The result was fsynced `ResolvedAwaitingAck` before the caller could see it, and survives | Take `unacknowledged()`/`resume()` on restart and acknowledge after durably handling; only then issue new mutations |
| Holder loses quorum | Renew cannot commit; its self-limit stops yielding a fence | Stop protected work as soon as the fence disappears |
| Old holder resumes | Its stale epoch or identity is rejected | Treat fence rejection as a survived race, not as an ordinary retry |
| Membership leader changes mid-plan | New controller observes the durable intent and resumes its current phase | Do not submit a competing local membership edit |
| A delegated administrator is lost | Another delegate or break-glass ROOT adds/replaces it through `set_admin`; the final delegate cannot simply be removed | Prefer a routine delegate; load ROOT only for the explicit recovery action |
| Candidate cannot catch up | Intent remains observable and retryable until an administrator cancels | Repair connectivity/provisioning or cancel deliberately |
| Snapshot from a peer is malformed | Rejects it before handing a prepared snapshot to OpenRaft | Investigate the sender; do not repair local durable state from unverified bytes |
| Local storage becomes uncertain | Latches the voter fatal rather than serving from questionable state | Take it out of service; repair or replace that member through the surviving quorum |

Stopping on lost quorum is the safety feature. Returning an answer from one
survivor would make two isolated partitions capable of returning different
answers.

## 12. Restart, join, remove, and recover

### Restart an existing member

Use `CoordinationStart::Existing` with independently expected group id, genesis
hash, and deployment profile. Aster verifies both databases, current snapshot,
identity binding, schema, and codec before starting service. It replays any
committed log work the state machine had not applied before the process stopped.

Never point `Bootstrap` at initialized storage, initialize a missing half of a
durable store, or infer expected identity from the database being checked.

### Add or replace a member

Provision an empty host with a stable Aster identity and a signed join permit.
Starting in `Join` mode makes it reachable as a candidate; it is still not a
member. If the candidate restarts before membership lands, start it in `Join`
mode again with the same stable secret, the same NodeId, and the directory its
first start bound; the same permit is simply re-verified — it binds the NodeId
through its addressless `candidate_ticket`, so new ephemeral addresses or ports
change nothing and no new permit is minted. Never wipe the bound directory to
"retry cleanly": the group's quorum may already count the candidate a voter.
An administrator submits a membership plan, after which Aster:

1. adds the candidate as a learner;
2. waits for it to apply through the observed committed index;
3. performs the joint-consensus voter change;
4. settles the exact voter and retained-learner roles; and
5. records completion durably.

### Remove a routine member

Commit the membership plan first, then revoke producer admission. Reversing the
order can make the quorum unable to communicate with the member it must remove.
For suspected compromise, admission may be denied immediately for containment,
then the surviving quorum commits removal.

### Recover after quorum loss

Recovery does not force one survivor to become authoritative. While the source
group still has quorum, an administrator periodically exports a linearizable,
canonical recovery point and stores it outside the live voter set. After an
unrecoverable quorum loss, the operator selects an acceptable saved export and
bootstraps a **new group id** whose signed genesis binds that export hash.
Imported state records its provenance. Because the group id changes, every old
fence becomes invalid even if an epoch number happens to match.

The recovery signing document, compatibility matrix, and import API remain
draft work. Do not improvise this procedure from snapshots or by copying voter
directories.

**Benefit:** routine lifecycle operations converge through committed state, and
catastrophic recovery cannot accidentally resurrect authority from the old
group.

## 13. Observe the group honestly

The runtime distinguishes:

| State | Meaning |
| --- | --- |
| `Bootstrapping` | Initial membership has not committed yet |
| `Joining` | This node has not yet entered committed membership |
| `Ready` | This voter has a fresh leader observation and has applied through the leader's initial entry |
| `NoQuorum` | A later message or restored peer may repair the current inability to progress |
| `Draining` | The host is shutting down in order |
| `Fatal` | Local identity, storage, schema, or committed state requires intervention |

A leader's readiness includes recent quorum acknowledgement. A follower can
know that it recently heard from a leader but cannot prove that the leader can
reach a quorum, so follower readiness is advisory. Public group status therefore
reports quorum evidence as `Option<bool>`: a follower may truthfully have no
direct answer.

Authoritative application reads always go through the leader's linearizable
barrier and a state transaction pinned at or beyond that barrier. Dashboards may
show local status, but no application should turn a follower-local observation
into permission to act.

Monitor at least:

- role, leader, term, voters, learners, and pending membership intent;
- committed/applied indexes and replication lag;
- quorum evidence and lifecycle state;
- command and linearizable-read latency;
- renewals, expiries, revocations, and fence rejections;
- snapshot progress and storage sizes; and
- placement outcome, missing evidence, exclusions, and replacement grace.

**Benefit:** the API distinguishes temporary unavailability, advisory local
knowledge, and local corruption instead of compressing them into a misleading
healthy/unhealthy boolean.

## 14. Responsibility boundary

The value of the feature is that this line stays sharp.

| Aster owns | The consumer owns |
| --- | --- |
| Authenticated coordination RPC over the existing Aster/Iroh network | Which application identities are producers and how that fact is verified |
| Durable Raft log, replicated state, snapshots, and restart replay | Persistent host identity, storage location, disk monitoring, and backups |
| Leader routing and linearizable reads | Which small facts truly need linearizability |
| Replicated exact ACL enforcement | Initial administrators and desired per-resource rights |
| Fenced register transitions and lease renewal engine | The meaning and versioning of opaque values |
| Durable server-side sequencing and idempotent replay | Durable acknowledgement/reconciliation through the final client API |
| Safe learner catch-up and joint membership transitions | Eligible hosts, pins, exclusions, counts, grace, and rebalance permission |
| Pure topology-aware recommendation | Machine provisioning and signed join permits |
| Refusal to continue without quorum or from uncertain local state | Operational response to no quorum, fatal storage, and capacity alarms |
| Canonical export/import checks and provenance | Authority-key custody and application data needed to reconstruct a coordinated head |

The consumer tunes policy; it does not implement a second consensus or
membership controller beside Aster.

## 15. Production checklist

Before removing the draft banner, the released feature and a consuming
application should demonstrate this end to end:

- [ ] Three initial voters start from one signed genesis under persistent,
  distinct identities and storage directories.
- [ ] A non-voter client obtains only the exact rights granted to it.
- [ ] A fenced register rejects a stale holder after takeover.
- [ ] A committed result survives a lost response and a client-process restart
  through the finalized delivery/acknowledgement API.
- [ ] One voter can stop while the remaining two continue.
- [ ] In a ten-producer topology, policy maintains exactly three voters and
  provisions at most one automatic replacement per plan.
- [ ] Unknown topology evidence is reported rather than promoted to physical
  separation.
- [ ] Losing two voters stops writes; no minority-force path exists.
- [ ] Restart, snapshot catch-up, membership leader loss, and process death at
  durability boundaries preserve complete-old-or-complete-new state.
- [ ] A recovery export can create only a signed new group, and old fences fail
  against it.
- [ ] The supported rolling-upgrade matrix and native Linux, macOS, and Windows
  fault evidence are green.
- [ ] The consumer can reconstruct every committed content pointer from its own
  durable content replicas.

The reference acceptance deployment is ten producers with exactly three
voters. Ten is a test corridor, not a maximum network size. Three is a deliberate
availability/latency tradeoff: it tolerates one unavailable voter without
making every producer participate in consensus.

## 16. What the consumer should ultimately feel

Once the feature lands, application code should mostly answer five questions:

1. What compact fact needs one authoritative answer?
2. Which identities may operate on it?
3. Which producer hosts are eligible to maintain the quorum?
4. What placement and failure policy should select three of them?
5. How does the protected resource enforce the returned fence?

Everything below that line—election, log replication, durable apply, snapshots,
ambiguous retries, learner catch-up, joint consensus, and restart recovery—is
Aster's problem.

That is the consumption promise: not “consensus has no operational cost,” but
“every Aster consumer does not have to assemble and maintain its own consensus
system.”
