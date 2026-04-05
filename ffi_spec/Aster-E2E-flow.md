## X. Service Metadata

### X.1 Admission Response

Service metadata is returned as part of the Gate 0 admission response (§3.2).
A consumer that has just been admitted receives the node's service listing and
registry doc ticket in the same round-trip, with no additional call required.

The admission success response is:

```
ConsumerAdmissionResponse {
    status:          AdmissionStatus       // Admitted | Denied
    attributes:      map<string, string>   // verified attributes echo
    services:        list[ServiceSummary]  // one entry per published service
    registry_ticket: string               // read-only iroh-docs share ticket; empty if
                                          // no registry doc is operated by this node
    root_pubkey:     bytes                // ed25519 public key, 32 bytes
}

ServiceSummary {
    name:        string
    version:     uint32
    contract_id: bytes                    // BLAKE3 content hash (§6.2)
    channels:    map<string, bytes>       // channel name → contract_id (stable, canary, dev)
}
```

`root_pubkey` is included so consumers can independently verify enrollment
credentials and any future signed artifacts without an additional round-trip.

`registry_ticket` is a read-only iroh-docs share ticket. Consumers that join
the doc using this ticket can read the full registry namespace (§11.2),
including all contracts, service aliases, and historical leases. This is the
path for consumers that need to browse the full catalog or watch for newly
published services. A consumer connecting to a known single service does not
need to join the doc — `services` in the admission response is sufficient.

If the node does not operate a registry doc, `registry_ticket` is empty.
`services` still contains live summaries derived from the node's in-memory
lease state.

### X.2 Re-connecting Consumers

A consumer that has already been admitted and holds a cached admission response
does not re-present its enrollment credential on subsequent connections — the
EndpointHooks allowlist persists the admission decision across connections. If
a re-connecting consumer needs a fresh `registry_ticket` or updated `services`
list (for example, after a deployment that changed the published service set),
it re-presents its enrollment credential to trigger a new admission exchange.

Consumers that have joined the registry doc via a previously issued
`registry_ticket` can watch the doc for service changes directly and do not
need to re-admit for this purpose.

### X.3 Reserved: `_aster.meta`

The method name `_aster.meta.GetPublisherInfo` is reserved for a future
convenience endpoint. If defined, it would allow an already-admitted consumer
to request a fresh metadata snapshot without re-presenting its enrollment
credential. This is not required by any current flow and is deferred to a
future revision.

-----

## Y. Client Deployment Flows - CONCEPTUAL

Two deployment flows are supported. They differ in when the contract is
resolved and how the client is constructed. The wire protocol, gate model, and
serialization layer are identical in both.

### Y.1 Generated Client Flow (Human-Driven Deployment) - CONCEPTUAL

This flow targets human developers who want typed, IDE-friendly client stubs
and are willing to perform an offline setup step.

**Preconditions.** The service is running. The operator has the root key
available offline.

**Setup (on the service node):**

```
# 1. Generate offline root key (one-time)
aster keygen root → root.key

# 2. Generate stable producer key
aster keygen producer → node.key           # prints NodeId

# 3. Sign an enrollment credential for this producer node
aster authorize --root-key ./root.key \
                --producer-id <NodeId> → enrollment.token

# 4. Start the service
ASTER_ENROLLMENT=<token> aster service start --key node.key
# prints: EndpointTicket on stdout

# 5. Mint a consumer enrollment credential for the client node
aster authorize consumer --root-key ./root.key \
                --type ott → consumer_enrollment.token

# 6. Mint an access token (rcan) for the client
aster token mint --capabilities <cap,...> → consumer.rcan

# 7. Generate the client project archive
aster generate client --lang python → client.zip
```

The client archive contains generated stubs derived from the published
contract. It is connection-agnostic: no endpoint ticket or token is embedded
in the generated code. Connection details are supplied at runtime via
environment variables or explicit constructor arguments.

**Handoff.** Transfer to the client machine:
`(EndpointTicket, consumer_enrollment.token, consumer.rcan, client.zip)`.

**Client machine:**

```
# .env
ASTER_PEER_TICKET=<EndpointTicket>
ASTER_ENROLLMENT=<consumer_enrollment.token>
ASTER_ACCESS_TOKEN=<consumer.rcan>
```

```python
from my_service_client import MyServiceClient

client = await MyServiceClient.connect()  # reads env vars
result = await client.my_method(...)
```

The generated client performs Gate 0 admission on first connect, presenting
the consumer enrollment credential. On success the admission response is
discarded — the generated client already has the contract baked in. Subsequent
calls on the same session skip Gate 0.

**Characteristics.** Schema resolution happens at code-generation time,
offline. The running client has no dependency on the registry doc. It is
robust to registry unavailability. Contract changes require regenerating and
redistributing the client archive.

### Y.2 Dynamic Client Flow (Machine-to-Machine Discovery)

This flow targets agents and automated systems that connect to services they
have not been pre-configured for, or that need to discover what a node
publishes at runtime.

**Preconditions.** The connecting peer has a consumer enrollment credential
(Gate 0) and an endpoint ticket for the target node. It does not need a
pre-distributed contract or generated stubs.

**Runtime flow:**

```
1. Dial the target node (EndpointTicket → QUIC connection)
2. Present consumer enrollment credential on aster.consumer_admission ALPN
   → Gate 0 passes; NodeId added to allowlist
   → ConsumerAdmissionResponse received: services[], registry_ticket, root_pubkey
3. Inspect services[]; select target service by name or contract_id
4. Fetch the contract blob via iroh-blobs using the contract_id
   (content-addressed; single round-trip to the same node)
5. Deserialize the contract; build a dynamic call descriptor
6. Open a service session using the rcan
7. Dispatch calls by method name using the dynamic call descriptor
```

Steps 1–2 are a single round-trip. Step 4 is a second round-trip to the same
node. A warm client that has cached the contract by `contract_id` skips step 4
— the content address is stable, so a cached contract is always valid for a
given ID.

**Dynamic call construction:**

```python
client = await AsterDynamicClient.connect(
    ticket=os.environ["ASTER_PEER_TICKET"],
    enrollment=os.environ["ASTER_ENROLLMENT"],
    token=os.environ["ASTER_ACCESS_TOKEN"],
)
# admission + contract fetch happen during connect()

result = await client.call("MyService", "MyMethod", {"field": "value"})
```

`AsterDynamicClient` exposes no typed methods. Arguments and return values are
plain dicts (or language-equivalent maps). The client validates argument
structure against the fetched schema before serializing, so the service never
receives a malformed call.

**Registry doc (optional).** If the connecting agent needs to watch for newly
published services or browse the full catalog, it joins the registry doc using
`registry_ticket` from the admission response. A single targeted connection
does not need the doc.

**Characteristics.** No offline setup beyond the consumer enrollment credential
and endpoint ticket. Contract resolution is on the critical path of first
connect but is a single blob fetch and cached thereafter. The flow degrades
gracefully to the generated client flow: an agent can call `aster generate
client` after dynamic discovery if it needs typed stubs for subsequent use.

### Y.3 Flow Comparison

| | Generated client | Dynamic client |
|---|---|---|
| Contract resolution | Offline, at code-generation time | Runtime, via admission response + blob fetch |
| Call style | Typed methods | `client.call("Method", dict)` |
| Registry doc dependency | None | Optional (full catalog / change watching only) |
| Schema change handling | Regenerate and redeploy archive | Automatic on next connect (or cache invalidation) |
| Offline robustness | High — no registry dependency at runtime | Lower — requires live node for first connect |
| Primary use case | Human developer, stable production client | Agent, scripting, exploratory connection |
| Gate 0 requirement | Yes | Yes |
| Wire protocol | `aster/1` | `aster/1` |

The two flows share the same gate model, wire protocol, and serialization
layer. A dynamic client and a generated client connecting to the same service
are indistinguishable at the wire level.

-----

## Trust Spec §3.2 — Required Edit

Replace the current "On success" block in `Aster-trust-spec.md §3.2`:

```
On success

1. The consumer's NodeID is added to the EndpointHooks allowlist.
2. Verified attributes are stored and attached to `CallContext` for all
   subsequent calls from this NodeID.
3. If `credential_type = OTT`: the nonce is marked consumed and persisted. The
   credential cannot be reused regardless of expiry.
4. The response includes the list of reachable services (service directory).
```

With:

```
On success

1. The consumer's NodeID is added to the EndpointHooks allowlist.
2. Verified attributes are stored and attached to `CallContext` for all
   subsequent calls from this NodeID.
3. If `credential_type = OTT`: the nonce is marked consumed and persisted.
   The credential cannot be reused regardless of expiry.
4. A `ConsumerAdmissionResponse` is returned (§X.1) containing the node's
   published service listing, a read-only registry doc ticket, and the
   deployment root public key. Consumers MAY use this response to resolve
   contract identifiers and join the registry doc without a further round-trip.
```