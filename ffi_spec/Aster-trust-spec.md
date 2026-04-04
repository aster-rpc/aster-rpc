## 2. Producer Mesh

### 2.1 Bootstrap

The producer mesh is established by an offline root key. This key exists outside
the running system — it is generated once, stored securely, and only brought
online to authorize new nodes or for catastrophic recovery.

Each node in the mesh has a stable producer key, generated once and reused across
restarts. This key is the node’s Iroh secret key and determines its endpoint ID.
It is distinct from the offline root key.

**Founding node (first node in the mesh)**

The founding node has no peers to find and no one to present credentials to. It:

1. Generates its stable producer key if not already present.
1. Loads its enrollment credential and verifies the root key’s signature.
1. Generates a random salt. This salt is secret — it is never encoded in the
   enrollment credential and is only handed to new nodes after they pass all
   admission checks.
1. Derives the gossip topic from the root public key and salt (§2.3).
1. Persists the salt and its accepted producer set to local storage.
1. Starts listening. Prints its endpoint ticket on startup.

The endpoint ticket is an iroh `EndpointTicket` — a postcard-encoded, base32
string containing the node’s public key, relay URL, and direct addresses. The
operator passes this to subsequent nodes via `ASTER_BOOTSTRAP_TICKET`.

**Subsequent nodes**

A node joining an existing mesh:

1. Generates its stable producer key if not already present.
1. Loads its enrollment credential and `ASTER_BOOTSTRAP_TICKET`.
1. Dials the bootstrap peer. The QUIC handshake verifies both identities.
1. Presents its enrollment credential and, if available, its IID token (§2.4).
1. The bootstrap peer runs admission checks (§2.4). If all checks pass, the
   bootstrap peer broadcasts an `Introduce` message on the gossip channel and
   returns the salt and current mesh membership to the new node.
1. The new node derives the gossip topic, joins the channel, connects to
   other mesh members directly, and persists the salt and accepted producer
   set to local storage.

**Membership persistence and restart**

Each node persists its accepted producer set and salt to local storage. On
restart, the node loads this persisted state and rejoins the gossip topic
without requiring re-admission. The persisted set is the authoritative
membership record for that node — gossip is the live delta feed, not the
source of truth.

After a network partition or extended offline period, a rejoining node
requests a full membership sync from any reachable peer rather than relying
solely on the delta it missed. `Introduce` messages are idempotent: receiving
an introduction for an already-admitted node updates that node’s entry but
does not produce an error or duplicate entry.

**CLI workflow**

```
# One-time: generate the node's stable producer key
aster keygen producer → node.key      # produces a stable NodeId

# Offline: authorize the node with the root key
aster authorize --root-key ./root.key --producer-id <endpoint_id> [--attributes key=value,...] → enrollment.token

# Run the founding node
ASTER_ENROLLMENT=<token> aster node start --key node.key
# Prints: endpoint ticket on stdout

# Run a subsequent node
ASTER_BOOTSTRAP_TICKET=<ticket> ASTER_ENROLLMENT=<token> aster node start --key node.key
```

-----

### 2.2 Enrollment Credentials

An enrollment credential is a signed token minted offline by the root key. It
authorizes a specific endpoint ID to join the mesh and encodes the admission
policy that runtime checks must satisfy.

**Structure**

```
EnrollmentCredential {
    endpoint_id:  EndpointId        // the endpoint being authorized
    root_pubkey:  PublicKey         // the root key's public key
    expires_at:   int64             // epoch seconds
    attributes:   map<string,string>
    signature:    binary            // sign(root_key, endpoint_id || root_pubkey || expires_at || attributes)
}
```

The admitting node derives the root public key from the credential itself.
Operators do not pass the root public key separately.

**Attributes**

The `attributes` map is a `string → string` dictionary. Keys prefixed with
`aster.` are reserved for the framework. Application-defined attributes use
unprefixed keys or their own namespace prefix.

Reserved keys:

|Key                  |Meaning                                                                                            |
|---------------------|---------------------------------------------------------------------------------------------------|
|`aster.role`         |Node role: `producer`, `gateway`, `consumer`                                                       |
|`aster.name`         |Human-readable node name, e.g. `"payments-node-eu-1"`                                              |
|`aster.allowed_cidrs`|Comma-separated CIDRs the source IP must match at least one of, e.g. `"10.0.4.0/24,192.168.1.0/24"`|
|`aster.iid_provider` |Required IID provider: `aws`, `gcp`, `azure`                                                       |
|`aster.iid_account`  |Expected cloud account or project ID                                                               |
|`aster.iid_region`   |Expected cloud region (optional tightening)                                                        |
|`aster.iid_role_arn` |Expected IAM role ARN (AWS-specific, optional)                                                     |

The `aster.role` attribute governs what the admitting node does after admission:

- `producer` — admitted to the producer gossip channel, full mesh membership.
- `gateway` — admitted but not introduced to the gossip channel; receives a
  separate set of connection details appropriate for a gateway node.
- `consumer` — treated as an authorized consumer; does not join the mesh.

If `aster.role` is absent, the node is treated as `producer`.

For `gateway` and `consumer` roles, post-admission lifecycle is governed by
§3 (Consumer Authorization). The enrollment credential is the pre-authorization
— it establishes that this endpoint ID is already trusted by the operator. The
service’s auth handler still runs and still mints an rcan; the enrollment
credential substitutes for the one-time token that would otherwise be required
at `Authorize` time. The handler reads the peer’s verified attributes from
`CallContext` and uses them to decide what capabilities to grant. Nothing in the
consumer authorization model changes — the enrollment credential is an
alternative token source, not a bypass of the authorization layer.

Verified attributes are available to service handlers via `CallContext` without
re-checking the signature. The framework populates this from the admitted
credential at connection time.

-----

### 2.3 Gossip Topic Derivation

The producer gossip topic ID is deterministic given the root public key and salt:

```
TopicId = blake3(root_public_key || "aster-producer-mesh" || salt)
```

**The salt is non-empty from day one.** The founding node generates a random
32-byte salt at startup. The salt is secret: it is never encoded in enrollment
credentials and is only handed to a new node after all admission checks pass
(§2.4). A node that holds a valid enrollment credential but has not been
admitted cannot derive the topic ID and cannot subscribe to the gossip channel.

The salt also serves as the mesh rotation mechanism. To shed a compromised node,
an operator generates a new salt and distributes it out of band to trusted nodes.
Trusted nodes move to the new gossip topic. The compromised node, lacking the new
salt, cannot follow. See §2.6 for the full recovery procedure.

-----

### 2.4 Admission

Admission is the gate between presenting an enrollment credential and receiving
the salt. It is enforced by the node that receives the connection request — the
bootstrap peer or any existing mesh member that a new node dials.

Admission is layered: offline checks run first (against the credential), then
runtime checks (against live data at connection time). Both must pass.

**Offline checks (always)**

1. The enrollment credential signature is valid against the root public key
   carried in the credential.
1. The credential has not expired (`expires_at` is in the future).
1. The credential’s `endpoint_id` matches the QUIC peer identity established
   by the handshake.

**Runtime checks (conditional on credential attributes)**

Runtime checks are only required if the corresponding attributes are present in
the credential. Absence of an attribute means no check is performed.

*Source IP restriction (`aster.allowed_cidrs`):*

The admitting node checks the peer’s observed source IP against the comma-separated
list of CIDRs in the credential. The source IP is the address from the QUIC
connection — it cannot be spoofed by the connecting node. The IP must fall within
at least one listed CIDR; if it matches none, admission is refused.

*IID verification (`aster.iid_provider` and related keys):*

Cloud-deployed nodes fetch their Instance Identity Document (IID) from the
hypervisor metadata endpoint (`169.254.169.254`) at startup. This is one
unauthenticated HTTP call. The IID is a signed token that identifies the VM and
its owner — signed by the cloud provider, not the application. The connecting
node attaches the IID to its introduction request.

The admitting node:

1. Verifies the IID signature against the cloud provider’s published public key.
1. Checks the IID claims against the `aster.iid_*` attributes in the credential.
   All attributes present must match.

IID verification and enrollment credential verification are independent and
complementary. The enrollment credential proves the root key authorized this
endpoint. The IID proves the endpoint is running in the expected cloud account.
Both are required if IID attributes are present in the credential.

Nodes always attach their IID if one is available, regardless of whether the
credential requires it. The admitting node decides whether to enforce it. This
means the same node binary runs in both cloud and non-cloud environments without
configuration changes.

**Admission refusal**

If any check fails, admission is refused and the connection is closed. The
refusal reason is logged by the admitting node. No partial state is written — the
new node does not receive the salt, is not added to the accepted set, and is not
introduced on the gossip channel.

**On success**

The admitting node:

1. Adds the new endpoint ID to its accepted producer set.
1. Broadcasts an `Introduce` message on the gossip channel (§2.5).
1. Returns the salt and current mesh membership to the new node.

The new node derives the gossip topic, joins the channel, and connects to other
mesh members directly using the returned membership list.

-----

### 2.5 Introduction

Once a node has been admitted (§2.4), the admitting node introduces it to the
rest of the mesh via the gossip channel.

1. The admitting node mints an rcan granting the `Producer` capability to the
   new endpoint ID, with an expiry.
1. The admitting node broadcasts a signed `Introduce` message on the producer
   gossip channel. The payload carries the rcan.
1. Other producers verify the signature (the introducer is in their accepted set)
   and add the new endpoint ID to their own accepted sets.
1. The new producer receives the current mesh membership and connects to other
   producers directly.

The rcan expiry defines the **admission window** — the time by which the new
producer must present the rcan and be admitted. A producer presenting an expired
rcan is refused. Whether to treat the rcan expiry as an ongoing membership lease
(requiring periodic re-vouching) is an implementation decision, not a protocol
requirement.

-----

### 2.6 Producer Gossip Messages

Every message on the producer gossip channel has a common envelope:

```
ProducerMessage {
    type:      uint8
    payload:   binary
    sender:    EndpointId
    epoch_ms:  int64
    signature: binary
}
```

The `signature` covers `type || payload || sender || epoch_ms` and is verified
against `sender`. `epoch_ms` is the sender’s wall-clock time in milliseconds
at the time of broadcast.

**Replay resistance**

iroh-gossip provides no replay resistance. The `epoch_ms` field is the
mechanism. Receivers reject any message whose `epoch_ms` falls outside a
configurable acceptance window (default: ±30 seconds of local wall clock).
Messages outside this window are dropped silently — they are treated as stale
or clock-skewed, not as attacks.

The replay threat is not uniform across message types. `Introduce` replays are
benign: re-admitting an already-admitted node is idempotent (§2.5). `Depart`
and `LeaseUpdate` replays are genuine problems — a replayed `Depart` can
falsely evict an active producer from peers’ accepted sets, and a replayed
`LeaseUpdate` can corrupt health and addressing state. The acceptance window
is the primary defence. Implementations may additionally track recently seen
`(sender, epoch_ms)` pairs to suppress exact duplicates within the window,
but this is not a protocol requirement.

iroh-gossip delivers raw bytes and the identity of the *forwarding neighbor* —
not the original sender. Messages are relayed through intermediate nodes, so the
peer that handed a message to you is not necessarily who sent it. `sender` and
`signature` are therefore Aster’s own application-layer origin authentication,
not a duplication of anything iroh provides. `sender` is a lookup hint: without
it, the receiver would have to trial-verify against every public key in the
accepted set. With it, one key lookup and one signature check suffices.

**Message handling rules**

|Condition                                            |Action               |
|-----------------------------------------------------|---------------------|
|Malformed message (bad framing, unrecognised type)   |Drop silently        |
|Valid message, sender not in accepted producer set   |Drop + security alert|
|Valid message, sender in accepted set, bad signature |Drop + security alert|
|Valid message, sender in accepted set, good signature|Dispatch             |

Silent drop is appropriate for garbage. A security alert is appropriate when the
authorization model itself appears to be violated. These are operationally
distinct: an alert on an unauthorized sender means either the salt has leaked or
a deauthorized node is still subscribed. Operators must be notified.

Message types:

|Type|Name             |Payload                                         |
|----|-----------------|------------------------------------------------|
|1   |Introduce        |rcan granting `Producer` to a new endpoint ID   |
|2   |Depart           |Empty. Signals graceful departure from the mesh.|
|3   |ContractPublished|Service name, version, contract collection hash |
|4   |LeaseUpdate      |Service name, health status, addressing info    |

Implementations may define additional message types above 128. Types 0–127 are
reserved for the spec.

-----

### 2.7 Deauthorization

Deauthorization in Aster is **intentionally epochal**, not incremental.

There is no signed “remove this producer now” message that cryptographically
forces other nodes to evict a peer. A `Depart` message (§2.6) signals graceful
departure and peers will honor it, but it is voluntary — a misbehaving or
compromised node will not send one, and a replayed `Depart` cannot be
distinguished from a genuine one by signature alone.

The only hard deauthorization guarantee is salt rotation: moving the mesh to a
new gossip topic that the removed node cannot derive. This is coarse-grained by
design. It forces the entire mesh to rotate rather than surgically removing one
node, which is an acceptable trade-off for the simplicity it buys. There is no
persistent revocation list, no epoch counter, and no incremental signed-revoke
path.

A future `Revoke` message type (reserved in the type space above 128) could
serve as a soft deauthorization hint — nodes that receive and trust it would
voluntarily drop the target from their accepted sets. This would accelerate
eviction in cooperative deployments but would not be cryptographically enforced.
Salt rotation would remain the hard guarantee.

-----

### 2.8 Compromise and Recovery

Compromise response is an operational concern, not a protocol concern. The spec
provides the mechanism; operators provide the procedure.

The general shape of recovery is:

1. An operator retrieves the offline root key.
1. The operator generates a new random salt and a new producer set excluding
   compromised nodes.
1. Trusted producers receive the new salt and set out of band.
1. Trusted producers derive the new gossip topic and re-bootstrap on it.
1. Compromised producers are left on the old topic, unable to discover the new
   one.

Producers should also regenerate any namespace keys, blob store secrets, or
other material that the compromised node had access to. The specifics depend on
the deployment.

-----

### 2.9 Authorization Layer Composition

Three independent authorization gates operate in sequence for every call. They
are evaluated at different times and by different parts of the framework. Each
must pass; none substitutes for another.

**Gate 1 — Enrollment (mesh join time, once per node)**

The enrollment credential is checked when a node first connects to the mesh.
It is the operator’s pre-authorization: the root key asserts that this endpoint
ID is permitted to exist in this mesh with these attributes. This gate runs
once. Its output is the node’s verified `attributes` map, persisted for the
lifetime of the connection.

**Gate 2 — Service authorization (session open time, once per service session)**

The service’s `Authorize` method is called when a consumer opens a session with
a service. The auth handler inspects the caller’s `peer_id`, `attributes`, and
any presented token, then decides whether to mint an rcan and what capabilities
to grant. For `gateway` and `consumer` role nodes with an enrollment credential,
the handler may read `ctx.attributes` to make this decision — for example,
granting elevated capabilities to a node whose IID confirms it is running in a
trusted cloud account. The enrollment credential does not bypass this gate; it
is an input to it.

**Gate 3 — Method dispatch (call time, every call)**

The `AuthInterceptor` evaluates the method’s `requires` expression against the
rcan’s `capability` list before the handler runs. This is a pure set-membership
check — the framework does not call back into the service. If the capability
requirement is not satisfied, the call is rejected with `PERMISSION_DENIED`
before the handler sees it.

**Composition rules**

The gates are independent. Enrollment attributes do not automatically synthesize
rcan capabilities — the service author decides whether and how to translate
attributes into capabilities inside the `Authorize` handler. A service may
require both an rcan capability (enforced by Gate 3) and an attribute-backed
condition (enforced by Gate 2 at `Authorize` time or by the handler at call
time). These combine by conjunction: all conditions must hold.

The service is the intentional bridge between Gate 1 (operator trust) and
Gate 3 (method-level enforcement). This keeps the framework generic and the
policy in the service where the domain logic lives.

```
Enrollment credential   →  Gate 1 (mesh join)    →  attributes on CallContext
Token / attributes      →  Gate 2 (Authorize)    →  rcan with capability list
rcan capability list    →  Gate 3 (dispatch)     →  handler runs or PERMISSION_DENIED
```

-----

### 2.10 Clock Drift Detection

The `epoch_ms` field on every `ProducerMessage` (§2.6) is already load-bearing
for replay resistance. If a producer's clock drifts, it either rejects valid
messages from peers (because their `epoch_ms` falls outside its acceptance
window) or sends messages that peers silently drop. Both failure modes are
invisible and operationally catastrophic. This section formalizes how producers
detect and respond to clock drift.

**Timestamp tracking**

Every producer MUST track the most recent `epoch_ms` received from each peer in
its accepted producer set. No new message field is required — the existing
`ProducerMessage.epoch_ms` is the data source.

**LeaseUpdate frequency**

Producers MUST send a `LeaseUpdate` message at least every 60 minutes. They
SHOULD send one every 15 minutes. This serves as both a health heartbeat and
fresh input for drift detection. A producer that has not sent a `LeaseUpdate`
within 90 minutes MAY be treated as stale by peers.

**Drift computation**

When a producer receives a message from a peer, it computes the peer's apparent
clock offset:

```
offset = peer.epoch_ms - local_wall_clock_ms
```

The producer maintains a sliding record of the most recent offset from each
active peer. The **mesh median offset** is the median of all tracked peer
offsets. A peer is considered **drifted** if its offset deviates from the mesh
median by more than the configured tolerance.

The default tolerance is **5000 ms** (5 seconds). Implementations SHOULD make
this configurable (e.g., `ASTER_CLOCK_DRIFT_TOLERANCE_MS`).

**Minimum peer count**

Drift detection activates only when ≥3 active peers are being tracked. With
fewer peers, the median is not statistically meaningful. Producers with fewer
than 3 peers SHOULD log clock offset observations at debug level but MUST NOT
take enforcement action.

**Grace period**

A freshly joined producer is exempt from drift enforcement for 60 seconds after
joining the gossip channel. During this grace period, the node collects peer
timestamps but does not evaluate itself or others against the tolerance. This
prevents false positives during initial clock synchronization.

**Self-monitoring (self-departure)**

Each producer continuously compares its own clock against the mesh median. If
the producer's own offset from the mesh median exceeds the tolerance:

1. The producer logs an error: `"clock drift detected: local offset {offset}ms exceeds tolerance {tolerance}ms from mesh median"`.
2. The producer broadcasts a `Depart` message.
3. The producer shuts down its mesh participation.

Self-departure is fail-fast: a drifted producer removes itself rather than
silently corrupting the mesh. The producer process MAY remain running for
other purposes (e.g., serving cached data) but MUST NOT send further gossip
messages or accept new mesh connections.

**Peer monitoring (peer isolation)**

When a producer observes that another peer's offset deviates from the mesh
median by more than the tolerance:

1. The producer logs a warning: `"peer {endpoint_id} clock drift detected: offset {offset}ms exceeds tolerance {tolerance}ms from mesh median"`.
2. The producer marks the peer as **drift-isolated**.
3. While drift-isolated, the peer's `ContractPublished` and `LeaseUpdate`
   messages are ignored (not applied to local state). `Introduce` messages
   are still processed (they are idempotent and the drift may be transient).
4. The isolation is lifted when the peer sends any message with an `epoch_ms`
   that is within tolerance of the mesh median.

Drift isolation is not deauthorization — the peer remains in the accepted set.
It is a temporary quarantine that prevents stale or time-skewed data from
propagating. The peer can recover by fixing its clock (e.g., NTP resync) and
sending a new `LeaseUpdate`.

**Interaction with replay resistance**

The ±30-second replay acceptance window (§2.6) and the ±5-second drift tolerance
serve different purposes and are evaluated independently:

- The **replay window** is a message-level check: "is this message's `epoch_ms`
  recent enough to be non-stale?" It is evaluated on every message.
- The **drift tolerance** is a peer-level check: "is this peer's clock close
  enough to the mesh consensus to be trustworthy?" It is evaluated as a
  rolling aggregate.

A peer can pass the replay window check (its messages are within 30s of local
time) while still failing the drift tolerance check (its offset is consistently
>5s from the mesh median). The drift check catches systematic bias; the replay
check catches individual stale messages.

**Summary of thresholds**

| Threshold | Default | Configurable | Purpose |
|-----------|---------|--------------|---------|
| Replay acceptance window | ±30 s | Yes | Per-message staleness check |
| Clock drift tolerance | ±5 s | Yes (`ASTER_CLOCK_DRIFT_TOLERANCE_MS`) | Per-peer systematic drift check |
| LeaseUpdate interval | ≤60 min (SHOULD ≤15 min) | Yes | Heartbeat + drift data freshness |
| Minimum peers for drift detection | 3 | No | Statistical validity of median |
| Grace period after join | 60 s | No | Avoid false positives during bootstrap |
