# Aster Trust and Authorization

**Companion to:** Aster Specification v0.7.1
**Status:** Draft
**Last Updated:** 2026-04-02

-----

## 1. Endpoint Roles

An Aster endpoint is either a **producer**, a **consumer**, or both.

**Producers** expose services. They publish contracts, accept RPCs, serve blobs, and coordinate with each other over a private gossip channel. Together, the set of producers for a service form the **producer mesh**.

**Consumers** use services. They discover producers via the registry, call RPCs, and fetch blobs. They cannot publish or modify service contracts.

An endpoint may be a producer for some services and a consumer of others.

-----

## 2. Producer Mesh

### 2.1 Bootstrap

The producer mesh is established by an offline root key. This key exists outside the running system — it is generated once, stored securely, and only brought online for initial setup or catastrophic recovery.

At bootstrap:

1. The root key signs an initial set of producer endpoint IDs.
1. Each producer starts with this signed set and its own secret key.
1. Producers connect to each other. QUIC handshake verifies identity.
1. Producers join a private gossip topic for coordination.

### 2.2 Gossip Topic Derivation

The producer gossip topic ID is deterministic:

```
TopicId = blake3(root_public_key || "aster-producer-mesh" || salt)
```

At bootstrap, `salt` is empty (zero-length). The salt exists so that, in future, an admin can rotate the mesh to a new topic by distributing a new random salt out of band. A compromised producer that doesn’t receive the new salt cannot follow.

### 2.3 Introduction

A new producer joins the mesh when an existing producer vouches for it.

1. An existing producer mints an rcan granting the `Producer` capability to the new endpoint ID, with an expiry.
1. The existing producer broadcasts a signed `Introduce` message on the producer gossip channel.
1. Other producers verify the signature (the introducer is in their accepted set) and add the new endpoint ID.
1. The new producer receives the current mesh membership so it can connect to other producers directly.

The rcan introduction may carry an expiry. The expiry defines the **admission window** — the time by which the new producer must present the rcan and be admitted to the mesh. If a producer presents an expired rcan, it is refused admission. Once a producer has been admitted, whether to also treat the rcan expiry as an ongoing membership lease (requiring re-vouching to stay in the mesh) is an implementation decision — not a protocol requirement.

### 2.4 Producer Gossip Messages

Every message on the producer gossip channel has a common envelope:

```
ProducerMessage {
    type: uint8
    payload: binary
    sender: EndpointId
    signature: binary
}
```

The `signature` covers `type || payload` and is verified against `sender`. If `sender` is not in the receiver’s accepted producer set, the message is dropped silently.

Message types:

|Type|Name             |Payload                                         |
|----|-----------------|------------------------------------------------|
|1   |Introduce        |rcan granting `Producer` to a new endpoint ID   |
|2   |Depart           |Empty. Signals graceful departure from the mesh.|
|3   |ContractPublished|Service name, version, contract collection hash |
|4   |LeaseUpdate      |Service name, health status, addressing info    |

Implementations may define additional message types above 128. Types 0–127 are reserved for the spec.

### 2.5 Compromise and Recovery

Compromise response is an operational concern, not a protocol concern. The spec provides the mechanism; operators provide the procedure.

The general shape of recovery is:

1. An operator retrieves the offline root key.
1. The operator generates a new salt and a new producer set excluding compromised nodes.
1. Trusted producers receive the new salt and set out of band.
1. Trusted producers move to the new gossip topic and re-bootstrap.
1. Compromised producers are left on the old topic, unable to discover the new one.

Producers should also regenerate any namespace keys, blob store secrets, or other material that the compromised node had access to. The specifics depend on the deployment.

-----

## 3. Consumer Authorization

### 3.1 Principle

Authorization is per-service, defined by the service author, and enforced by the framework. The spec provides the mechanism — the service provides the policy.

A service that requires authorization provides an auth handler. A service with no auth handler is open to any consumer.

### 3.2 The Authorize Method

Every service may expose an `Authorize` method. It has a fixed signature:

```
Authorize(token: optional<string>) → optional<Rcan> | Error
```

The caller’s endpoint ID is implicit — it is already authenticated by the QUIC handshake.

Three outcomes:

|Result          |Meaning                                                                                                                                                                  |
|----------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|Returns an rcan |Consumer is authorized. The rcan encodes what they can do and when it expires.                                                                                           |
|Returns nothing |Not authorized. The token may have been wrong, missing, or the service doesn’t recognize this consumer. Not an error — the consumer can try again with a different token.|
|Returns an error|Something is broken — malformed input, misconfigured service, internal failure.                                                                                          |

### 3.3 Rcan Structure

An rcan is a signed, binary-encoded token. Its fields map directly to JWT claims, which makes the model familiar and the security properties well-understood. The rcan is serialized as Fory XLANG — there is no JSON encoding on the wire.

| rcan field   | JWT analogue | Required | Meaning                                                                                                                                                                 |
|--------------|--------------|----------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `jti`        | `jti`        | Yes      | Unique token ID. Used for revocation and replay detection.                                                                                                               |
| `iss`        | `iss`        | Yes      | The service key that minted this token.                                                                                                                                  |
| `aud`        | `aud`        | Yes      | The endpoint ID this token is bound to. Verified against the QUIC peer.                                                                                                  |
| `sub`        | `sub`        | No       | The human or agent identity behind the peer (e.g. `emrul@emrul.com`). Signed by the issuer — not metadata. Absent for pure peer-to-peer calls where the peer is the identity. |
| `capability` | custom claim | Yes      | List of role strings granted to this consumer (e.g. `["edit", "audit"]`). The service defines what these strings mean.                                                   |
| `exp`        | `exp`        | Yes      | Expiry timestamp. The framework rejects tokens past this time.                                                                                                           |

`iat` (issued-at) and `nbf` (not-before) are intentionally omitted. `iat` carries no enforcement value — `exp` is what the framework acts on. `nbf` has no practical use in a model where tokens are minted on demand at authorization time.

`capability` is a list, not a scalar, so a single rcan can express multi-role membership (e.g. a bearer who is both `EDITOR` and `AUDITOR`).

### 3.4 The Token Parameter

The `token` parameter is an opaque string. The spec does not define what it contains. The service’s auth handler interprets it however it sees fit:

- A one-time enrollment code generated by an admin
- A pre-shared key for batch device provisioning
- An API key
- An OAuth authorization code
- Null, if the service authorizes based on endpoint ID alone

The service decides whether a token is single-use, reusable, scoped, rate-limited, or anything else. The spec doesn’t know, doesn’t care.

### 3.5 Token Lifecycle

1. Consumer connects to a producer. QUIC handshake proves both identities.
1. Consumer calls `Authorize` on the service, optionally presenting a token.
1. The service’s auth handler decides whether to mint an rcan.
1. If authorized, the consumer receives an rcan scoped to that service, bound to their endpoint ID, with an expiry.
1. On subsequent calls, the framework attaches the rcan to `StreamHeader.auth_token` automatically. This is a typed, binary-encoded optional field — not a metadata string. Service code never touches it directly.
1. The framework’s `AuthInterceptor` verifies the rcan before dispatch: valid signature, `aud` matches the QUIC peer, not expired, `jti` not revoked, and the `CapabilityRequirement` for the called method is satisfied. If any check fails, the call is rejected with `PERMISSION_DENIED` before the handler runs.
1. The verified rcan claims are exposed on `CallContext`. The handler reads `ctx.subject`, `ctx.capability`, and `ctx.peer_id` without performing any token verification itself.
1. To refresh, the consumer calls `Authorize` again before expiry. The QUIC connection is the refresh credential.

### 3.6 Delegation

A service may allow its authorized consumers to introduce new consumers. This is opt-in — the service must explicitly enable delegation.

Delegation is an introduction, not offline capability transfer. The delegator does not give away their own access. They vouch for the delegatee, and the service makes the final decision.

The flow:

1. Alice holds a valid rcan for a service that has `delegation=True`.
1. Alice wants Bob to have access. Alice mints a delegation rcan — a signed statement that says “I, Alice, vouch for Bob.”
1. Bob connects to the service and calls `Authorize`, presenting Alice’s delegation rcan as his token (serialized as a string).
1. The service’s auth handler verifies Alice’s signature, checks that Alice is permitted to delegate, and applies its own policy to decide what Bob gets.
1. If approved, the service mints a fresh, direct rcan for Bob.
1. Bob uses his own direct token going forward. No chain in metadata, no `permits` algebra.

The delegation rcan is an introduction letter, not a bearer credential. The service has the database, the role store, the context. It makes the real access decision.

Services that do not set `delegation=True` reject delegation rcans in `Authorize`. The framework does not need to understand delegation semantics beyond passing the token to the handler.

### 3.7 Two-Layer Authorization

Authorization is enforced at two layers:

**Framework layer (AuthInterceptor).** Runs before the handler is invoked. The interceptor extracts `StreamHeader.auth_token`, then verifies: valid signature from the service key, `aud` matches the QUIC peer identity, token not expired, `jti` not in the revocation list, and the token’s `capability` list satisfies the method’s `CapabilityRequirement` (see §3.9). If any check fails, the call is rejected with `PERMISSION_DENIED` before the handler runs. This layer is generic, mechanical, and the same for every service.

**Application layer (service code).** The handler runs with a `CallContext` populated from the verified rcan. The service applies its own domain logic: is this user an author on this document, does this project allow external contributors, is this resource archived. If the check fails, the service returns a domain error.

The rcan `capability` list encodes coarse access — which methods a consumer may call. Fine-grained, resource-level decisions are made by the service at call time, where the data lives. This keeps tokens small and simple. The intelligence lives in the service, not in the token.

### 3.8 CallContext

Every handler receives an optional `ctx: CallContext` parameter. The framework populates it before the handler is invoked. All fields derived from the rcan are already verified — the handler does not need to re-check signatures or expiry.

```python
class CallContext:
    service: str            # Service name
    method: str             # Method name
    call_id: str            # Unique ID for this call (for tracing)
    session_id: str | None  # Non-None for session-scoped calls
    peer_id: EndpointId     # Authenticated QUIC peer (the rcan aud)
    subject: str | None     # rcan sub — human/agent identity behind the peer; None for direct peer-to-peer calls
    capability: list[str]   # Verified rcan capability list
    metadata: dict[str, str]# StreamHeader metadata, aster- keys stripped
    deadline: float | None  # Call deadline (epoch seconds), None if not set
    is_streaming: bool      # True for server_stream, client_stream, bidi_stream
```

`ctx.subject` is the appropriate identity to log, audit, or apply user-level policy against. `ctx.peer_id` identifies the connecting endpoint (the gateway or agent). For direct peer-to-peer calls without a gateway, `ctx.subject` will be absent and `ctx.peer_id` is the full identity.

### 3.9 CapabilityRequirement

Each `@rpc`, `@server_stream`, `@client_stream`, and `@bidi_stream` method may declare a `requires` parameter specifying which capabilities are sufficient to call it. The `requires` expression is a `CapabilityRequirement` — a small discriminated union:

```python
@rpc(requires=DocRole.VIEW)                           # single role
@rpc(requires=anyOf(DocRole.ADMIN, DocRole.EDIT))     # caller must hold at least one
@rpc(requires=allOf(DocRole.EDITOR, DocRole.AUDITOR)) # caller must hold all
```

The `AuthInterceptor` evaluates the requirement against the rcan’s `capability` list. No service-defined callback is needed for evaluation — the framework performs set membership checks directly against the string values in the list.

The `requires` expression is extracted at contract-build time and encoded into the `ServiceContract` for each method. The contract is therefore self-describing: a consumer inspecting the contract can determine what capability is needed to call each method before attempting a call. Methods with no `requires` annotation are callable by any consumer who holds a valid rcan for the service.

### 3.10 Reserved Metadata Keys

All metadata keys prefixed with `aster-` are reserved for framework use. Services and applications must not use this prefix for their own metadata.

No `aster-` keys are currently defined for authorization. The rcan is carried in the typed `StreamHeader.auth_token` field, not in metadata.

### 3.11 Multi-Session Endpoints

An endpoint that manages sessions on behalf of multiple users — such as a web gateway fronting human clients authenticated via OIDC or SAML — must maintain a session map. The gateway is the Aster peer: its endpoint ID is what appears in `aud`, and the QUIC handshake authenticates it. The `sub` field carries the human identity behind each request.

Because different users may hold different roles, the gateway cannot use a single rcan for all traffic. It must call `Authorize` once per user session, presenting each user’s OIDC or SAML token, and cache the resulting per-user rcan. On each outbound call, the gateway attaches the rcan for the user making that specific request. The rcan cache is keyed by user identity and must respect `exp` — the gateway is responsible for refreshing rcans before they expire.

### 3.12 Session-Scoped Authorization

For session-scoped services (see Session-Scoped Services addendum), `Authorize` is called once at session open. The rcan is attached to the session’s `StreamHeader.auth_token` and verified by the `AuthInterceptor` at that point. Subsequent `CallHeader` frames within the session do not carry an rcan — the verified identity and capability from session open are retained on the session instance and propagated to `CallContext` for every call in the session.

Carrying the rcan on every `CallHeader` within an established session would be pointless bloat: the session stream itself proves continuity of the QUIC connection, and the identity was already verified at open.

### 3.13 Blob Access

Blob access follows the same authorization model. A consumer authorized to use a service may receive blob capabilities (tickets or `FileRef` values) as RPC responses. The blob fetch itself uses iroh-blobs’ native transfer — the rcan authorizes the RPC that mints the ticket, not the blob transfer directly.

For services that need to gate blob access independently of RPC access, the producer can use iroh-blobs’ `EventSender` with `RequestMode::Intercept` to verify that the requesting endpoint ID holds a valid rcan before serving content.

-----

## 4. Contracts as Blob Collections

### 4.1 Contract Identity

A service contract is published as an Iroh Blobs collection (HashSeq format
with built-in `CollectionMeta` naming). The `contract_id` is the BLAKE3 hash
of the canonical `ServiceContract` bytes — **not** the collection root hash.
The collection root hash identifies the *bundle*; the `contract_id` identifies
the *contract*. See the main spec §11.2–11.4 and the Contract Identity
addendum (§11.3) for the full canonical encoding and hashing procedure.

A contract collection contains (per main spec §11.2.2):

| Collection member name     | Content                                       | Required |
|---------------------------|-----------------------------------------------|----------|
| `contract.xlang`          | Canonical XLANG bytes of `ServiceContract`    | Yes      |
| `manifest.json`           | `ContractManifest` JSON                       | Yes      |
| `types/{type_hash}.xlang` | Canonical XLANG bytes of each `TypeDef`       | Yes      |
| `schema.fdl`              | Human-readable Fory IDL source text           | No       |

The collection member names are carried by Iroh's native `CollectionMeta`.
Two bundles with different optional members (e.g. one includes `schema.fdl`,
the other does not) may share the same `contract_id` if their `contract.xlang`
bytes are identical. After fetching a collection, consumers must verify
`blake3(contract.xlang bytes) == contract_id` before trusting the bundle.

### 4.2 Publication

Publishing a contract means building the collection, importing it into the
local iroh-blobs store, writing an `ArtifactRef` pointer into the registry
docs namespace (see main spec §11.2.1, §11.4), and advertising the
`contract_id` on the producer gossip channel via a `ContractPublished` message.
Other producers fetch the collection via iroh-blobs (verified transfer,
resumable, deduplicated).

### 4.3 Resolution

A consumer resolves a service by name and version to a `contract_id` via the
registry (see main spec §11.8), fetches the `ArtifactRef` to get the
collection root hash, downloads the collection via iroh-blobs, verifies
`blake3(contract.xlang) == contract_id`, and then includes that `contract_id`
in the `StreamHeader` when making calls. The producer verifies the
`contract_id` matches before dispatching.

### 4.4 Relationship to Main Spec

The canonical contract encoding question (main spec §11.3, §16.2 #10) is
resolved by the Contract Identity addendum. The canonical encoding uses Fory
XLANG with a constrained canonical profile (§11.3.2). Types are content-
addressed individually, forming a Merkle DAG. The `ServiceContract` hash is
the `contract_id`. Iroh Blobs collections provide the packaging and transfer
mechanism; the canonical XLANG profile provides the deterministic hashing
input. Both work together — the collection carries the artifacts, the canonical
bytes produce the identity.

-----

## 5. Example: Enterprise ACL

This example shows how a service author maps a classic role-based access control model onto Aster’s authorization primitives, including resource-level checks that go beyond what the token encodes.

### 5.1 The Scenario

A company runs an internal document management service. There are three roles: viewers can read documents, editors can read and write, and admins can read, write, and manage access. Additionally, editing a document requires being listed as an author on that document — unless you’re an admin. The service runs on a producer mesh of three nodes for fault tolerance.

### 5.2 Define Capabilities

The rcan capability is a simple role. It controls which methods a consumer can call. It does not encode which documents they can access — that’s a resource-level decision made by the service at call time.

```python
from enum import Enum

class DocRole(Enum):
    VIEW = "view"
    EDIT = "edit"
    ADMIN = "admin"

    def permits(self, action: "DocRole") -> bool:
        hierarchy = {
            DocRole.ADMIN: {DocRole.ADMIN, DocRole.EDIT, DocRole.VIEW},
            DocRole.EDIT: {DocRole.EDIT, DocRole.VIEW},
            DocRole.VIEW: {DocRole.VIEW},
        }
        return action in hierarchy[self]
```

This is all that goes in the token. Tokens stay small, `permits` stays trivial.

### 5.3 Define the Auth Handler

The auth handler maps the organization’s enrollment process onto the `Authorize` method. It also handles delegation introductions when enabled.

```python
class DocServiceAuth(AuthPolicy):
    delegation = True  # This service accepts introductions from existing users

    def __init__(self, service_key, role_store):
        self.service_key = service_key
        self.role_store = role_store

    async def authorize(self, peer_id: EndpointId, token: str | None) -> Rcan | None:
        # Delegation: token is a serialized rcan from an existing user vouching for peer_id
        if token and self.is_delegation_rcan(token):
            return self.handle_delegation(peer_id, token)

        # First-time enrollment: token is a one-time code from IT
        if token and self.role_store.is_valid_enrollment_code(token):
            role = self.role_store.redeem_enrollment_code(token)
            self.role_store.assign_role(peer_id, role)
            return self._mint(peer_id, role)

        # Returning user: already enrolled, no token needed
        role = self.role_store.get_role(peer_id)
        if role:
            return self._mint(peer_id, role)

        # Unknown device, no valid token
        return None

    def handle_delegation(self, peer_id, token):
        delegation = Rcan.decode(token)
        # Verify the introducer has a valid role
        introducer_role = self.role_store.get_role(delegation.issuer())
        if not introducer_role:
            return None
        # Policy: only admins and editors can introduce, and they can only grant VIEW
        if introducer_role not in (DocRole.ADMIN, DocRole.EDIT):
            return None
        delegated_role = DocRole.VIEW
        self.role_store.assign_role(peer_id, delegated_role)
        return self._mint(peer_id, delegated_role)

    def _mint(self, peer_id, role):
        return Rcan.issuing_builder(
            self.service_key, peer_id, role
        ).with_capability([role.value]).sign(Expires.valid_for(Duration.hours(8)))
```

Note that the delegation policy is entirely in the handler. The framework doesn’t know that “only admins and editors can introduce” or that “introductions only grant VIEW.” That’s this service’s rule.

### 5.4 Wire It Up

```python
@service(
    name="DocManagement",
    version=1,
    serialization=[SerializationMode.XLANG],
    auth=DocServiceAuth(service_key, role_store),
)
class DocManagementService:

    @rpc(requires=DocRole.VIEW)
    async def get_document(self, req: GetDocRequest) -> Document:
        return self.doc_store.get(req.doc_id)

    @rpc(requires=anyOf(DocRole.EDIT, DocRole.ADMIN))
    async def update_document(self, ctx: CallContext, req: UpdateDocRequest) -> UpdateAck:
        # Application layer: check resource-level permission
        doc = self.doc_store.get(req.doc_id)
        if DocRole.ADMIN.value not in ctx.capability and ctx.peer_id not in doc.authors:
            raise RpcError(PERMISSION_DENIED, "not an author on this document")
        return self.doc_store.update(req.doc_id, req.content)

    @rpc(requires=DocRole.ADMIN)
    async def grant_access(self, req: GrantAccessRequest) -> GrantAck:
        self.role_store.assign_role(req.target_peer_id, req.role)
        return GrantAck(success=True)
```

The two-layer model in action:

- `get_document`: `requires=DocRole.VIEW` is encoded in the contract. The `AuthInterceptor` evaluates it against the rcan’s `capability` list before dispatch. The handler runs unconditionally if the check passes.
- `update_document`: `requires=anyOf(DocRole.EDIT, DocRole.ADMIN)` gates method access. The handler then applies a resource-level check — is the caller an author on this specific document? — which cannot be expressed in the token and lives where the data lives.
- `grant_access`: `requires=DocRole.ADMIN` gates the method. No further check needed.

### 5.5 The Employee Experience

1. IT generates a one-time enrollment code for the new hire and sends it via a secure channel (email, Slack, printed card, whatever).
1. The employee’s laptop generates a keypair in its TPM during onboarding.
1. The Aster client calls `Authorize(token="<enrollment-code>")` on first launch.
1. The service redeems the code, records the device’s endpoint ID with the assigned role, and returns an rcan valid for 8 hours.
1. The client uses the rcan for all calls during the workday.
1. Before expiry, the client calls `Authorize(token=None)`. The service recognizes the endpoint ID, checks the role store, and mints a fresh token.
1. If the employee is offboarded, IT removes them from the role store. The next refresh attempt returns nothing. The existing token expires within 8 hours. Access revoked.

An editor’s token grants them `EDIT` — the ability to call `update_document`. But which documents they can actually edit depends on the `authors` list on each document, checked at call time. IT doesn’t need to enumerate documents in the enrollment code. The role is coarse; the resource check is live.

### 5.6 Delegation

A manager wants a contractor to be able to view documents for a week, without involving IT.

1. The manager mints a delegation rcan vouching for the contractor’s endpoint ID, signed with the manager’s key.
1. The manager sends this to the contractor out of band.
1. The contractor connects to the service and calls `Authorize(token="<delegation-rcan>")`.
1. The service’s auth handler verifies the manager’s signature, confirms the manager is an ADMIN or EDITOR (and thus allowed to introduce), and mints a direct VIEW token for the contractor.
1. The contractor uses their own token. No chain, no `permits` algebra at call time.

If the manager is later offboarded, the contractor’s token still works until it expires — they were granted a direct rcan by the service, not a derivative of the manager’s token. If the service wants tighter coupling, it can track who introduced whom and revoke accordingly. That’s policy, not protocol.

-----

## 6. What This Spec Does Not Cover

The following are intentionally left as implementation or operational concerns:

- **Misbehavior detection.** How a producer determines another producer is misbehaving.
- **Automatic re-vouching.** Whether and how producers renew each other’s membership tokens.
- **Key rotation ceremony.** The operational procedure for using the offline root key to recover from compromise.
- **Admin tier.** Whether certain producers have elevated privileges beyond introduction rights.
- **Revocation lists.** Whether expired-but-not-yet-timed-out tokens should be explicitly revoked.
- **Rate limiting and quotas.** How services throttle consumers.

These may be specified in future versions or left permanently as implementation choices.