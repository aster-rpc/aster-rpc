# Consumer Admission Handshake

**Status:** Stub -- to be filled after chaos tests confirm invariants.

**Reference:** `bindings/python/aster/high_level.py` AsterClient._run_admission() + `bindings/python/aster/trust/consumer.py`

## What this flow covers

The full sequence from a consumer connecting to a producer through to
receiving the services list and opening an RPC transport. Covers both
dev mode (no credentials) and auth mode (signed enrollment credential).

## Sections to write

### 1. Credential loading
- Identity file (.aster-identity TOML) vs standalone JSON .cred file
- Secret key extraction -- QUIC endpoint id must match credential's endpoint_id
- `loadIdentity()` (TS) / `config.load_identity()` (Python) entry points
- Inline credential from peer entry vs file-based credential

### 2. Admission ALPN and wire format
- Connect on `aster.consumer_admission` ALPN
- ConsumerAdmissionRequest: `credential_json` (snake_case wire keys) + `iid_token`
- ConsumerAdmissionResponse: `admitted`, `services`, `registry_namespace`, `serialization_modes`, `gossip_topic`
- Why `consumerCredToJson()` must be used (not raw `JSON.stringify`) -- camelCase vs snake_case on the wire

### 3. ServiceSummary and serialization mode discovery
- The `serialization_modes` field on ServiceSummary tells the client what codec to use
- If server advertises only `['json']`, client must use JsonProxyCodec (not ForyCodec)
- The `pattern` field tells the client whether to use ProxyClient or SessionProxyClient
- Python: auto-detection in `AsterClient.client()` / TS: auto-detection in `proxy()`

### 4. Admission denial check
- Client MUST check `admissionResponse.admitted` and throw if false
- Error message should guide the user toward the credential/enrollment flow

### 5. Node creation with identity
- When credential has an endpoint_id, the client node must be created with the matching secret_key
- `IrohNode.memoryWithAlpns(alpns, { secretKey: ... })` (TS) / `IrohNode.memory_with_alpns(alpns, config)` (Python)
- Without this, QUIC peer id won't match credential and admission fails silently

## Invariants for new implementations

_(To be confirmed by chaos tests, then documented here)_

## Bugs this flow exposed

- TS client was passing `null` credential (always open-gate)
- `performAdmission` was using `JSON.stringify(credential)` producing camelCase wire keys
- Python server's early-return error trailers defaulted to Fory codec, unreadable by JSON clients
- ServiceSummary didn't carry `serialization_modes` or `pattern` -- TS client couldn't detect JSON-only servers or session-scoped services
