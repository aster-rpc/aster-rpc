# Aster Trust Model

This guide explains how Aster controls who can connect to your services and what they can do. It is written for Python developers building on Aster -- you do not need to read the formal spec to use it.

## The big picture

Aster runs on a peer-to-peer network. There is no central server to act as a gatekeeper. Instead, every node enforces access control locally using cryptographic credentials signed by a shared trust anchor: the **root key**.

There are three gates that a connection passes through:

1. **Gate 0** -- Can this peer connect at all? (QUIC handshake level)
2. **Gate 1** -- Does this peer hold a valid credential? (Admission)
3. **Gate 2** -- Is this peer allowed to call this specific method? (Application-level interceptors)

Each gate is independent. Gate 0 blocks unadmitted peers from even establishing a connection. Gate 1 verifies credentials. Gate 2 is your application logic. You can enable or disable each one depending on your needs.

## Key concepts

### The root key

The root key is an ed25519 keypair generated once by the operator of a deployment. It has two halves:

- **The private key** signs credentials. It is held offline by the operator -- on a locked-down workstation, in a secrets vault, wherever your security policy dictates. The private key **never touches a running Aster node**. This is a hard security rule, not a suggestion.

- **The public key** verifies signatures. It is distributed to every node in the deployment. When a node receives a credential, it checks the signature against this public key. If the signature is valid, the node knows the credential was issued by someone who holds the private key.

The root key is not the same as a node's identity key. The root key is a deployment-wide trust anchor. Node identity keys are per-node.

### Node secret key (EndpointId)

Every iroh node has an ed25519 identity key that determines its **EndpointId** (also called NodeId). This is the peer's network address -- other nodes use it to connect.

If you configure a `secret_key` in your `AsterConfig` or TOML file, the node gets a stable EndpointId that survives restarts. If you do not, iroh generates an ephemeral identity each run. Stable identities matter for production (other nodes need to find you); ephemeral identities are fine for development.

The node secret key and the root key are completely separate. The root key authorizes credentials. The node key identifies a peer on the network.

### Enrollment credentials

An enrollment credential is a pre-signed token that proves a node is authorized to participate. The operator mints credentials offline using the root private key, then distributes them to nodes.

There are two kinds:

**Producer enrollment credentials** authorize a node to join the producer mesh (the set of nodes that serve RPC methods). A producer credential is bound to a specific EndpointId -- it cannot be reused by a different node.

```python
from aster.trust.credentials import EnrollmentCredential

cred = EnrollmentCredential(
    endpoint_id="abc123...",           # the producer's NodeId (hex)
    root_pubkey=root_pub_bytes,        # 32-byte ed25519 public key
    expires_at=1735689600,             # Unix epoch seconds
    attributes={"aster.role": "producer"},
    signature=b"...",                  # 64-byte ed25519 signature
)
```

**Consumer enrollment credentials** authorize a node to call RPC methods on producers. There are two subtypes:

- **Policy credentials** are reusable. They are not bound to a specific NodeId and do not carry a nonce. Any node that presents a valid policy credential is admitted. Think of them as "API keys with an expiry date".

- **OTT (one-time token) credentials** are single-use. They carry a 32-byte random nonce. The first node to present the credential is admitted; subsequent presentations are rejected. Think of them as "invite links".

```python
from aster.trust.credentials import ConsumerEnrollmentCredential

# Policy credential (reusable)
policy_cred = ConsumerEnrollmentCredential(
    credential_type="policy",
    root_pubkey=root_pub_bytes,
    expires_at=1735689600,
    attributes={"aster.role": "consumer"},
    # endpoint_id is None (not bound to a specific node)
    # nonce is None (policy credentials do not carry a nonce)
)

# OTT credential (single-use)
import secrets
ott_cred = ConsumerEnrollmentCredential(
    credential_type="ott",
    root_pubkey=root_pub_bytes,
    expires_at=1735689600,
    attributes={"aster.role": "consumer"},
    nonce=secrets.token_bytes(32),      # exactly 32 bytes required
)
```

## The three gates

### Gate 0: Connection-level filtering

Gate 0 operates at the QUIC handshake layer, before any application protocol runs. It is implemented by `MeshEndpointHook`, which maintains an allowlist of admitted peer EndpointIds.

The decision logic is simple:

- If the connection is on an **admission ALPN** (`aster.producer_admission` or `aster.consumer_admission`), it is **always allowed**. Peers need to be able to present their credentials.
- If the peer's EndpointId is in the **admitted set**, the connection is **allowed**.
- Otherwise, the connection is **denied**. The peer gets a QUIC-level close with no diagnostic information (to prevent probing).

Gate 0 blocks *all* protocols for unadmitted peers -- not just Aster RPC, but also blobs, docs, and gossip. This means an unadmitted peer cannot download blobs or subscribe to gossip topics on your node.

`AsterServer` wires Gate 0 automatically when any admission flag is active (`allow_all_consumers=False` or `allow_all_producers=False`). You do not need to set it up manually:

```python
async with AsterServer(
    services=[MyService()],
    allow_all_consumers=False,  # Gate 0 is enabled automatically
    root_pubkey=pub_bytes,
) as srv:
    # Gate 0 is running. Unadmitted peers cannot connect.
    await srv.serve()
```

If you need to interact with the hook directly (advanced use):

```python
from aster.trust.hooks import MeshEndpointHook

hook = MeshEndpointHook(allow_unenrolled=False)

# After successful credential verification:
hook.add_peer("abc123...")

# Check a connection:
allowed = hook.should_allow("abc123...", b"aster/1")  # True

# Remove a peer (e.g., on credential expiry):
hook.remove_peer("abc123...")
```

### Gate 1: Credential admission

Gate 1 verifies the credential a peer presents during admission. It runs in two phases:

**Offline checks** (no network calls):
1. Structural validity -- is the credential well-formed?
2. Signature verification -- does the signature match the root public key?
3. Expiry -- is `expires_at` in the future?
4. EndpointId binding -- does the credential's EndpointId match the connecting peer?
5. OTT nonce -- for one-time tokens, has this nonce been consumed before?

**Runtime checks** (optional, one network call):
- Instance Identity Document (IID) verification -- for cloud deployments, verify the peer's claimed cloud identity against the provider's metadata endpoint.

If any check fails, the peer is rejected. The rejection reason is logged server-side for debugging but is **never sent to the peer** -- this prevents attackers from using error messages to probe the system.

The `admit()` function orchestrates both phases:

```python
from aster.trust.admission import admit
from aster.trust.nonces import InMemoryNonceStore

result = await admit(
    cred,
    peer_endpoint_id="abc123...",
    nonce_store=InMemoryNonceStore(),  # required for OTT credentials
)

if result.admitted:
    print("Welcome!", result.attributes)
else:
    # result.reason is for your logs only -- never send it to the peer
    print(f"Denied (internal): {result.reason}")
```

You rarely call `admit()` directly. `AsterServer` handles it internally when consumers or producers connect.

### Gate 2: Per-call authorization (interceptors)

Gate 2 is application-level. After a peer is admitted through Gates 0 and 1, they can make RPC calls. Gate 2 lets you control which methods each peer can call, based on their credential attributes or any other logic.

Gate 2 is implemented using Aster's interceptor system. This is not covered in detail here -- see the interceptors documentation. The key point is that Gate 2 is orthogonal to Gates 0 and 1: a peer can be admitted to the network but still be denied access to specific methods.

## The founding node

Every producer mesh starts with a **founding node** -- the first producer. The founding node is special:

- It does not need an enrollment credential to join (there is no existing mesh to join).
- It bootstraps the accepted-producer set with just its own EndpointId and the root public key.
- It generates a random salt, derives the gossip topic, and initializes the mesh state.
- Subsequent producers join by presenting their enrollment credential to the founding node (or any other already-admitted producer).

In code, the founding node is simply the first `AsterServer` you start. If you are using `AsterServer` with `allow_all_producers=False`, it creates an ephemeral mesh state automatically:

```python
async with AsterServer(
    services=[MyService()],
    allow_all_producers=False,
    root_pubkey=pub_bytes,
) as srv:
    # This node is the founding node. It created the mesh.
    print(srv.endpoint_addr_b64)
```

For persistent mesh state (crash recovery, multi-node deployments), use the lower-level bootstrap functions or pass `persist_mesh_state=True`.

## Dev mode

In development, you usually do not want to deal with key management. Aster makes this easy.

**Fully open mode** (the default): no gates, no keys, no credentials. Anyone can connect and call anything:

```python
# All gates open. No root key needed.
async with AsterServer(services=[MyService()]) as srv:
    await srv.serve()
```

**Dev mode with consumer admission**: `AsterServer` generates an ephemeral root keypair in memory. The private key exists only for the lifetime of the process. `AsterClient` can use it to auto-mint credentials:

```python
from aster import AsterServer, AsterClient, AsterConfig

# Server: ephemeral root key, consumer admission enabled
config = AsterConfig(allow_all_consumers=False)
pub = config.resolve_root_pubkey()
# config._ephemeral_privkey is set (transient, never persisted)

async with AsterServer(services=[MyService()], config=config) as srv:
    # Client: use the ephemeral private key to mint a credential on the fly
    async with AsterClient(
        endpoint_addr=srv.endpoint_addr_b64,
    ) as client:
        svc = await client.client(MyService)
        result = await svc.my_method(request)
```

This is useful for integration tests and local demos. The ephemeral private key is an internal detail -- it is stored on `config._ephemeral_privkey` and is never written to disk or exposed through any public API.

## Production workflow

### Step 1: Generate the root keypair

Run this once, on the operator's machine. Not on a server node.

```bash
aster keygen root --out ~/.aster/root.key
```

This creates a JSON file with both the private and public keys:

```json
{
  "private_key": "abcdef0123456789...",
  "public_key": "fedcba9876543210..."
}
```

Keep the **private key secret**. Extract the public key and distribute it to your nodes.

You can also do this in Python:

```python
from aster.trust.signing import generate_root_keypair

priv_bytes, pub_bytes = generate_root_keypair()
# priv_bytes: 32-byte ed25519 seed -- keep secret
# pub_bytes:  32-byte ed25519 public key -- distribute to nodes
```

### Step 2: Distribute the public key

Copy the public key to each node. You can distribute it as:

- A file containing just the hex-encoded public key (64 characters).
- A JSON file with a `"public_key"` field (same format as `root.key`).
- An environment variable: `ASTER_ROOT_PUBKEY=fedcba9876543210...`
- A TOML config entry: `root_pubkey_file = "/etc/aster/root_pub.key"`

The public key is not secret. It is safe to check it into version control, embed it in container images, or pass it through any channel.

### Step 3: Sign enrollment credentials

Use the CLI to mint credentials for each node. This is done offline, on the machine that holds the root private key.

**For producers** (bound to a specific node):

```bash
# First, generate a stable producer key to learn the NodeId:
aster keygen producer --out ~/.aster/node.key
# Output: node_id = abc123...

# Then sign an enrollment credential for that NodeId:
aster trust sign \
    --root-key ~/.aster/root.key \
    --endpoint-id abc123... \
    --type producer \
    --attributes '{"aster.role": "producer"}' \
    --expires 2027-01-01T00:00:00Z \
    --out producer-enrollment.json
```

**For consumers (policy -- reusable)**:

```bash
aster trust sign \
    --root-key ~/.aster/root.key \
    --type policy \
    --attributes '{"aster.role": "consumer"}' \
    --expires 2027-01-01T00:00:00Z \
    --out consumer-policy.json
```

**For consumers (OTT -- single-use)**:

```bash
aster trust sign \
    --root-key ~/.aster/root.key \
    --type ott \
    --attributes '{"aster.role": "consumer"}' \
    --expires 2027-01-01T00:00:00Z \
    --out consumer-ott.json
```

The OTT command generates a random 32-byte nonce automatically. The first node to present this credential is admitted; any subsequent presentation is rejected.

You can also sign credentials programmatically:

```python
import time
from aster.trust.credentials import EnrollmentCredential
from aster.trust.signing import sign_credential

cred = EnrollmentCredential(
    endpoint_id="abc123...",
    root_pubkey=pub_bytes,
    expires_at=int(time.time()) + 365 * 24 * 3600,  # 1 year
    attributes={"aster.role": "producer"},
)
cred.signature = sign_credential(cred, priv_bytes)
```

### Step 4: Configure nodes

Each node needs:
- The root **public** key (to verify incoming credentials).
- Optionally, its own enrollment credential (if joining an existing mesh as a non-founding producer).
- Its admission policy (`allow_all_consumers`, `allow_all_producers`).

Example TOML for a production producer node:

```toml
[trust]
root_pubkey_file = "/etc/aster/root_pub.key"
allow_all_consumers = false
allow_all_producers = false

[network]
secret_key = "base64-encoded-32-byte-key"
bind_addr = "0.0.0.0:9000"

[storage]
path = "/var/lib/aster"
```

### Step 5: Distribute credentials to nodes

Send each signed credential to the appropriate node. Configure nodes to find them:

```bash
export ASTER_ENROLLMENT_CREDENTIAL=/etc/aster/enrollment.json
```

Or in TOML:

```toml
[trust]
enrollment_credential = "/etc/aster/enrollment.json"
```

The founding node of a mesh does not need an enrollment credential -- it bootstraps the mesh with just the root public key and its own EndpointId.

## Nonce stores (OTT credentials)

One-time token credentials require a nonce store to track which nonces have been consumed. Aster provides two implementations:

**InMemoryNonceStore** -- for development and tests. Consumed nonces are lost on restart:

```python
from aster.trust.nonces import InMemoryNonceStore

store = InMemoryNonceStore()
```

**NonceStore** -- persistent, backed by a JSON file. Consumed nonces survive restarts. Writes are atomic (write-to-temp, fsync, rename):

```python
from aster.trust.nonces import NonceStore

store = NonceStore(path="/var/lib/aster/nonces.json")
# Default path: ~/.aster/nonces.json
```

`AsterServer` uses `InMemoryNonceStore` by default when consumer admission is enabled. For production with OTT credentials, pass a persistent store:

```python
from aster.trust.nonces import NonceStore

async with AsterServer(
    services=[MyService()],
    allow_all_consumers=False,
    root_pubkey=pub_bytes,
    nonce_store=NonceStore("/var/lib/aster/nonces.json"),
) as srv:
    await srv.serve()
```

## Consumer admission flow (what happens under the hood)

When an `AsterClient` connects to an `AsterServer` with consumer admission enabled:

1. The client opens a QUIC connection to the server on the `aster.consumer_admission` ALPN.
2. Gate 0 allows this connection (admission ALPNs are always permitted).
3. The client sends a `ConsumerAdmissionRequest` containing its credential as JSON.
4. The server runs Gate 1 checks: signature, expiry, EndpointId binding, nonce (for OTT).
5. If admitted, the server adds the client's EndpointId to the Gate 0 allowlist and responds with a `ConsumerAdmissionResponse` containing the list of available services.
6. The client can now open connections on the `aster/1` ALPN to make RPC calls. Gate 0 will allow them because the client is now in the admitted set.

If any check fails, the server responds with `admitted=false` and an empty reason string (oracle protection). The actual rejection reason is logged server-side.

## Summary

| Concept | What it is | Who has it |
|---------|-----------|------------|
| Root private key | Signs credentials | Operator only (offline) |
| Root public key | Verifies signatures | Every node |
| Node secret key | Determines EndpointId | Each individual node |
| Enrollment credential | Proof of authorization | Each node that needs admission |
| Gate 0 | Connection-level allowlist | Producer nodes (automatic) |
| Gate 1 | Credential verification | Producer nodes (automatic) |
| Gate 2 | Per-call authorization | Your application code (interceptors) |
