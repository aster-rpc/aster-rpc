## 2. Producer Mesh

### 2.1 Bootstrap

The producer mesh is established by an offline root key. This key exists outside
the running system — it is generated once, stored securely, and only brought
online to authorize new nodes or for catastrophic recovery.

Each node in the mesh has a stable producer key, generated once and reused across
restarts. This key is the node's Iroh secret key and determines its endpoint ID.
It is distinct from the offline root key.

**Founding node (first node in the mesh)**

The founding node has no peers to find and no one to present credentials to. It:

1. Generates its stable producer key if not already present.
2. Loads its enrollment credential and verifies the root key's signature.
3. Generates a random salt. This salt is secret — it is never encoded in the
   enrollment credential and is only handed to new nodes after they pass all
   admission checks.
4. Derives the gossip topic from the root public key and salt (§2.3).
5. Starts listening. Prints its endpoint ticket on startup.

The endpoint ticket is an iroh `EndpointTicket` — a postcard-encoded, base32
string containing the node's public key, relay URL, and direct addresses. The
operator passes this to subsequent nodes via `ASTER_BOOTSTRAP_TICKET`.

**Subsequent nodes**

A node joining an existing mesh:

1. Generates its stable producer key if not already present.
2. Loads its enrollment credential and `ASTER_BOOTSTRAP_TICKET`.
3. Dials the bootstrap peer. The QUIC handshake verifies both identities.
4. Presents its enrollment credential and, if available, its IID token (§2.4).
5. The bootstrap peer runs admission checks (§2.4). If all checks pass, the
   bootstrap peer broadcasts an `Introduce` message on the gossip channel and
   returns the salt and current mesh membership to the new node.
6. The new node derives the gossip topic, joins the channel, and connects to
   other mesh members directly.

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

| Key                  | Meaning                                                                 |
|----------------------|-------------------------------------------------------------------------|
| `aster.role`         | Node role: `producer`, `gateway`, `consumer`                            |
| `aster.name`         | Human-readable node name, e.g. `"payments-node-eu-1"`                  |
| `aster.allowed_cidrs` | Comma-separated CIDRs the source IP must match at least one of, e.g. `"10.0.4.0/24,192.168.1.0/24"` |
| `aster.iid_provider` | Required IID provider: `aws`, `gcp`, `azure`                            |
| `aster.iid_account`  | Expected cloud account or project ID                                    |
| `aster.iid_region`   | Expected cloud region (optional tightening)                             |
| `aster.iid_role_arn` | Expected IAM role ARN (AWS-specific, optional)                          |

The `aster.role` attribute governs what the admitting node does after admission:

- `producer` — admitted to the producer gossip channel, full mesh membership.
- `gateway` — admitted but not introduced to the gossip channel; receives a
  separate set of connection details appropriate for a gateway node.
- `consumer` — treated as an authorized consumer; does not join the mesh.

If `aster.role` is absent, the node is treated as `producer`.

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
2. The credential has not expired (`expires_at` is in the future).
3. The credential's `endpoint_id` matches the QUIC peer identity established
   by the handshake.

**Runtime checks (conditional on credential attributes)**

Runtime checks are only required if the corresponding attributes are present in
the credential. Absence of an attribute means no check is performed.

*Source IP restriction (`aster.allowed_cidrs`):*

The admitting node checks the peer's observed source IP against the comma-separated
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

1. Verifies the IID signature against the cloud provider's published public key.
2. Checks the IID claims against the `aster.iid_*` attributes in the credential.
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
2. Broadcasts an `Introduce` message on the gossip channel (§2.5).
3. Returns the salt and current mesh membership to the new node.

The new node derives the gossip topic, joins the channel, and connects to other
mesh members directly using the returned membership list.

-----

### 2.5 Introduction

Once a node has been admitted (§2.4), the admitting node introduces it to the
rest of the mesh via the gossip channel.

1. The admitting node mints an rcan granting the `Producer` capability to the
   new endpoint ID, with an expiry.
2. The admitting node broadcasts a signed `Introduce` message on the producer
   gossip channel. The payload carries the rcan.
3. Other producers verify the signature (the introducer is in their accepted set)
   and add the new endpoint ID to their own accepted sets.
4. The new producer receives the current mesh membership and connects to other
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
    signature: binary
}
```

The `signature` covers `type || payload` and is verified against `sender`.

iroh-gossip delivers raw bytes and the identity of the *forwarding neighbor* —
not the original sender. Messages are relayed through intermediate nodes, so the
peer that handed a message to you is not necessarily who sent it. `sender` and
`signature` are therefore Aster's own application-layer origin authentication,
not a duplication of anything iroh provides. `sender` is a lookup hint: without
it, the receiver would have to trial-verify against every public key in the
accepted set. With it, one key lookup and one signature check suffices.

**Message handling rules**

| Condition                                             | Action                  |
|-------------------------------------------------------|-------------------------|
| Malformed message (bad framing, unrecognised type)    | Drop silently           |
| Valid message, sender not in accepted producer set    | Drop + security alert   |
| Valid message, sender in accepted set, bad signature  | Drop + security alert   |
| Valid message, sender in accepted set, good signature | Dispatch                |

Silent drop is appropriate for garbage. A security alert is appropriate when the
authorization model itself appears to be violated. These are operationally
distinct: an alert on an unauthorized sender means either the salt has leaked or
a deauthorized node is still subscribed. Operators must be notified.

Message types:

| Type | Name              | Payload                                          |
|------|-------------------|--------------------------------------------------|
| 1    | Introduce         | rcan granting `Producer` to a new endpoint ID    |
| 2    | Depart            | Empty. Signals graceful departure from the mesh. |
| 3    | ContractPublished | Service name, version, contract collection hash  |
| 4    | LeaseUpdate       | Service name, health status, addressing info     |

Implementations may define additional message types above 128. Types 0–127 are
reserved for the spec.

-----

### 2.7 Compromise and Recovery

Compromise response is an operational concern, not a protocol concern. The spec
provides the mechanism; operators provide the procedure.

The general shape of recovery is:

1. An operator retrieves the offline root key.
2. The operator generates a new random salt and a new producer set excluding
   compromised nodes.
3. Trusted producers receive the new salt and set out of band.
4. Trusted producers derive the new gossip topic and re-bootstrap on it.
5. Compromised producers are left on the old topic, unable to discover the new
   one.

Producers should also regenerate any namespace keys, blob store secrets, or
other material that the compromised node had access to. The specifics depend on
the deployment.