# Consumer Admission Handshake

Implementation flow for Gate 1 consumer admission. A consumer (client)
presents a credential to a producer (server) and receives back a list
of available services plus a connection ticket.

**Spec:** Aster-trust-spec.md S2-S3

## Wire protocol

The admission handshake uses a dedicated ALPN (`aster-consumer-admission/1`),
separate from the RPC ALPN (`aster/1`). The flow is:

```
Consumer                                 Producer
   |                                        |
   |-- open QUIC stream (admission ALPN) -->|
   |-- StreamHeader(service="__admission__")>|
   |-- JSON request frame ----------------->|
   |                                        |-- validate credential
   |                                        |-- check offline rules
   |                                        |-- consume nonce (OTT only)
   |<- JSON response frame -----------------|
   |   (admitted/denied + services + ticket) |
   |                                        |
```

Both request and response are JSON-encoded, regardless of the server's
default serialization mode. Admission happens before codec negotiation.

## Credential types

| Type | Nonce | Description |
|------|-------|-------------|
| `policy` | `null` | Reusable, not bound to a specific peer. Signed by root key. |
| `ott` | 32 bytes | One-time token. Nonce consumed on first use, replay rejected. |

Both carry:
- `root_pubkey` (32 bytes) -- ed25519 public key of the trust anchor
- `expires_at` (epoch seconds) -- credential expiry
- `signature` (64 bytes) -- ed25519 signature over canonical bytes
- `attributes` (dict) -- key-value pairs carried into `CallContext`
- `endpoint_id` (optional) -- binds credential to a specific peer NodeID

## Credential loading

The consumer loads its credential from an `.aster-identity` TOML file:

```toml
type = "ott"
root_pubkey = "abcd..."
nonce = "ef01..."
signature = "2345..."
expires_at = 1712764800
attributes = { "aster.name" = "my-agent", "aster.role" = "ops.status" }
```

**Python:** `AsterConfig.load_identity()` at `config.py:443`.

**TypeScript:** `loadIdentity()` at `config.ts:252`. Uses `parseSimpleToml()`
with quote-aware comma splitting (`splitTopLevel()`) for inline table values
like `attributes = { "aster.role" = "ops.status,ops.ingest" }`.

### Pitfall: TOML inline tables with commas in values

The TOML value `{ "aster.role" = "ops.status,ops.ingest" }` contains a
comma inside a quoted string. A naive split on `,` would break the value
into two parts. The TS binding uses `splitTopLevel()` and
`findTopLevelEquals()` which respect quote boundaries.

## Wire format for the request

The credential is serialized to JSON with **snake_case** field names:

```json
{
  "credential_type": "ott",
  "root_pubkey": "abcd...",
  "nonce": "ef01...",
  "signature": "2345...",
  "expires_at": 1712764800,
  "attributes": {"aster.name": "my-agent"},
  "endpoint_id": null
}
```

**Critical:** The wire format uses `root_pubkey`, not `rootPubkey`. The TS
binding must use `consumerCredToJson()` (`trust/consumer.ts:146`) which
produces snake_case keys. Using `JSON.stringify(credential)` produces
camelCase and fails validation on the Python server.

## Server-side validation

Validation proceeds in order. The first failure stops processing and
returns `admitted: false` with a reason (reason is logged server-side
but not sent to the peer, to prevent oracle attacks).

### 1. Structural validation

Check field types, lengths, hex encoding. Constants in `limits.py`:
- `HEX_FIELD_LENGTHS` -- expected lengths for hex fields (e.g. `root_pubkey`: 64 hex chars)
- `validate_hex_field()` -- checks length and hex characters

### 2. Expiry check

Reject credentials whose `expires_at` is in the past.

### 3. Signature verification

Verify the ed25519 signature against the canonical signing bytes using
the `root_pubkey`. The canonical bytes include presence flags for optional
fields (nonce, endpoint_id) so an attacker cannot flip policy/ott without
invalidating the signature.

**Python:** `check_offline()` at `admission.py:37`.

### 4. Endpoint binding (optional)

If the credential has `endpoint_id`, it must match the connecting peer's
NodeID. Prevents credential theft (stolen credential only works from the
bound endpoint).

### 5. Nonce consumption (OTT only)

For OTT credentials, the nonce must be consumed exactly once:

```python
if nonce_store is None:
    return denied("no nonce_store configured")
consumed = await nonce_store.consume(cred.nonce)
if not consumed:
    return denied("nonce already consumed")
```

If `nonce_store` is None, OTT credentials are **denied** (not silently
accepted). The high-level `AsterServer` always creates an
`InMemoryNonceStore` when Gate 0 is enabled.

### 6. Attribute bridging

On successful admission, the credential's `attributes` dict is stored in
the `PeerAttributeStore`, keyed by the peer's NodeID:

```python
peer_store.admit(peer_endpoint_id, attributes)
```

These attributes are later threaded into `CallContext.attributes` on every
RPC call from this peer, enabling Gate 3 capability checks without
re-reading the credential.

## Response format

```json
{
  "admitted": true,
  "services": [
    {
      "name": "MissionControl",
      "version": 1,
      "methods": ["getStatus", "runCommand"],
      "pattern": "session",
      "serialization_modes": ["xlang"]
    }
  ],
  "ticket": "node1234..."
}
```

The `pattern` field tells the client whether to open session streams
(`method=""`) or per-call streams. The `serialization_modes` field tells
the client which codecs the server supports (see `codec_negotiation.md`).

### Node creation with identity

When the credential has an `endpoint_id`, the client node must be created
with the matching `secret_key`. Without this, the QUIC peer ID won't match
the credential and admission fails silently.

**Python:** `IrohNode.memory_with_alpns(alpns, config)` with `config.secret_key`.
**TypeScript:** `IrohNode.memoryWithAlpns(alpns, { secretKey: ... })`.

## Naming conventions (wire compatibility)

These field names are part of the wire protocol and **must be identical
across all bindings**:

| Wire name | Used in | Notes |
|-----------|---------|-------|
| `credential_type` | Request JSON | Not `credentialType` |
| `root_pubkey` | Request JSON | Not `rootPubkey` |
| `endpoint_id` | Request JSON | Not `endpointId` |
| `expires_at` | Request JSON | Not `expiresAt` |
| `serialization_modes` | Response JSON | Not `serializationModes` |
| `admitted` | Response JSON | Boolean, not numeric |

Internal field names (class properties, method names) can follow each
language's convention (camelCase in TS, snake_case in Python).

## Performance notes

- Do auth checks early: validate signature before any I/O beyond the
  initial stream read. Don't instantiate services or allocate resources
  for unauthenticated peers.
- Avoid dynamic imports in the admission hot path. Python's
  `from aster.trust.admission import ...` should be module-level, not
  inside the handler function.

## Invariants confirmed by chaos tests

- OTT nonce replay is rejected (`test_g10_ott_nonce_consumed_on_replay`)
- Missing nonce store denies OTT credentials, not silently accepts
  (`test_g10_ott_without_nonce_store_is_denied`)
- Oversized metadata in subsequent RPC calls is rejected
  (`test_g3_oversized_metadata_rejected`)

## Implementation checklist for new bindings

- [ ] Load credential from `.aster-identity` TOML with quote-aware parsing
- [ ] Serialize credential to JSON with **snake_case** keys
- [ ] Open admission ALPN stream, send StreamHeader + JSON request
- [ ] Parse response: `admitted`, `services[]`, `ticket`
- [ ] Store peer attributes in PeerAttributeStore on admission
- [ ] Handle both `policy` and `ott` credential types
- [ ] Validate nonce length (32 bytes) before consumption
- [ ] Reject OTT credentials when no nonce store is configured
- [ ] Thread `serialization_modes` from response into codec selection
- [ ] Create node with matching `secret_key` when credential has `endpoint_id`
- [ ] Use the response `ticket` for subsequent RPC connections

## Key files

| Binding | File | Entry point |
|---------|------|-------------|
| Python | `trust/consumer.py:187` | `handle_consumer_admission_rpc()` |
| Python | `trust/admission.py:37` | `check_offline()` |
| Python | `trust/credentials.py:38` | `ConsumerEnrollmentCredential` |
| Python | `trust/nonces.py:48` | `NonceStore` / `InMemoryNonceStore` |
| Python | `peer_store.py` | `PeerAttributeStore` |
| TS | `trust/consumer.ts:203` | `handleConsumerAdmissionRpc()` |
| TS | `trust/consumer.ts:89` | `performAdmission()` |
| TS | `trust/consumer.ts:146` | `consumerCredToJson()` |
| TS | `config.ts:252` | `loadIdentity()` |
