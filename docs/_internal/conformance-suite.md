# Conformance Test Suite

Language-neutral test vectors and scenarios that every Aster binding must pass.

## Directory Structure

```
conformance/
├── vectors/                     # Canonical byte vectors (JSON)
│   ├── framing.json             # Frame encode/decode test vectors
│   ├── contract-identity.json   # Contract hash test vectors
│   └── xlang-roundtrip.json     # XLANG serialization vectors (future)
└── scenarios/                   # Integration test scenarios (YAML)
    ├── unary-echo.yaml
    ├── server-stream.yaml
    └── ...
```

## Vector Files

### Format

Each vector file is a JSON object with:
- Top-level metadata and type definitions
- Arrays of test vectors with input and expected output
- Error vectors for invalid inputs

### `framing.json`

Tests the wire framing protocol (Spec S6.1):

```
Frame: [4-byte LE u32 length] [1-byte flags] [payload bytes]
Length = len(flags + payload) = len(payload) + 1
```

**Encode vectors:** Given `payload_hex` and `flags`, produce `expected_wire_hex`.
**Decode vectors:** Given `wire_hex`, extract `expected_payload_hex` and `expected_flags`.
**Error vectors:** Invalid frames that must be rejected (zero-length, oversized, etc.).

### `contract-identity.json`

Tests canonical serialization and BLAKE3 hashing (Spec: Aster-ContractIdentity.md):

**Hash vectors:** Given a `contract` (ServiceContract as JSON), the canonical XLANG serialization → BLAKE3 hash must equal `expected_hash`.

These hashes are generated from the Python reference implementation. Every binding must produce the same hash for the same contract definition.

### `xlang-roundtrip.json` (future)

Will test Fory XLANG serialization round-trips for primitive types, collections, and user-defined structs. Requires Fory JS (`@apache-fory/core`) XLANG support to be validated first.

## Scenario Files

### Format (YAML)

```yaml
name: scenario-name
description: What this tests

service:
  name: ServiceName
  version: 1
  scope: shared|stream

wire_types:
  - tag: "module/TypeName"
    fields:
      field_name: type   # string, int32, float64, bool, bytes, list[X]

methods:
  - name: method_name
    pattern: unary|server_stream|client_stream|bidi_stream
    request_type: module/TypeName
    response_type: module/TypeName

calls:
  - method: method_name
    request: { ... }
    expected_response: { ... }     # for unary / client_stream
    expected_stream: [ ... ]       # for server_stream
    expected_status: OK|NOT_FOUND|...
```

### How Scenarios Are Used

Scenarios define RPC interactions that a test harness executes:

1. **Intra-language:** Start server and client in the same language, run scenario
2. **Cross-language:** Start server in language A, client in language B, run scenario

The test harness:
- Reads the YAML
- Starts the server (registers the service, hardcodes responses matching expected_response)
- Starts the client (calls each method with the specified request)
- Asserts responses match expected values and status codes match

## Running Conformance Tests

### Python

```bash
# Vector tests (built into existing test suite)
uv run pytest tests/python/test_conformance.py -v

# Cross-language (Python server + TS client)
uv run python tests/cross-language/fixtures/echo_server.py &
bun vitest run tests/cross-language/interop-unary.test.ts
```

### TypeScript

```bash
# Vector tests
bun vitest run tests/unit/conformance.test.ts

# Cross-language (TS server + Python client)
bun run tests/cross-language/fixtures/echo_server.ts &
uv run pytest tests/cross-language/test_interop_unary.py
```

## Adding New Vectors

1. Define the test case in the appropriate `vectors/*.json` file
2. If the vector involves hashing or encoding, generate the expected value from the Python reference implementation
3. Run the conformance tests in all bindings to verify
4. Commit the updated vectors alongside the binding changes

## Generating Hash Vectors from Python

```python
from aster.contract.identity import (
    ServiceContract, MethodDef, MethodPattern, ScopeKind,
    canonical_xlang_bytes, compute_contract_id,
)

contract = ServiceContract(
    name="MyService", version=1, scoped=ScopeKind.SHARED,
    methods=[MethodDef(name="my_method", pattern=MethodPattern.UNARY, ...)],
)
canon = canonical_xlang_bytes(contract)
hash_hex = compute_contract_id(canon)
print(hash_hex)  # 64-char hex string — put this in contract-identity.json
```
