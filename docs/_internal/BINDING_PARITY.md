# Spec Conformance Matrix

Tracks whether each binding produces **wire-identical output** for spec-defined behaviors. These are the things that MUST match across all bindings for interoperability.

**Legend:** `pass` = produces identical bytes, verified by golden vectors | `impl` = implemented, not yet cross-validated | `—` = not implemented

**Source of truth:** Spec docs in `ffi_spec/`, golden vectors in `conformance/vectors/`, Rust core in `core/src/`.

## Wire Protocol (Aster-SPEC.md)

| Requirement | Spec | Python | TypeScript | Java |
|-------------|------|--------|------------|------|
| Frame encoding (4-byte LE + flags + payload) | S6.1 | pass | pass | — |
| Frame decoding | S6.1 | pass | pass | — |
| Flag constants (COMPRESSED, TRAILER, HEADER, CALL, CANCEL) | S6.1 | pass | pass | — |
| MAX_FRAME_SIZE enforcement (16 MiB) | S6.1 | pass | pass | — |
| StreamHeader wire format | S6.2 | impl | impl | — |
| CallHeader wire format (session streams) | S6.3 | impl | impl | — |
| RpcStatus trailer format | S6.4 | impl | impl | — |
| Status codes (0–16, gRPC-compatible) | S6.5 | pass | pass | — |

## Contract Identity (Aster-ContractIdentity.md)

| Requirement | Spec | Python | TypeScript | Java |
|-------------|------|--------|------------|------|
| Canonical XLANG encoding (varint, zigzag, strings) | S11.3 | pass (Rust core) | pass (Rust core) | — |
| BLAKE3 hashing | S11.3 | pass (Rust core) | pass (Rust core) | — |
| Golden vector 1: Echo (94 bytes) | App B.1 | pass | pass | — |
| Golden vector 2: DataService + capability (127 bytes) | App B.2 | pass | pass | — |
| Golden vector 3: Analytics multi-method (267 bytes) | App B.3 | pass | pass | — |
| Golden vector 4: ChatRoom session-scoped (106 bytes) | App B.4 | pass | pass | — |
| NFC identifier normalization | S11.3.3 | pass (Rust core) | pass (Rust core) | — |
| Method sorting by NFC name | S11.3 | pass (Rust core) | pass (Rust core) | — |
| Version field affects contract ID | S11.3 | pass | pass | — |
| Different signatures produce different IDs | S11.3 | pass | pass | — |

## Serialization (Aster-SPEC.md S5)

| Requirement | Spec | Python | TypeScript | Java |
|-------------|------|--------|------------|------|
| Fory XLANG mode (cross-language bytes) | S5.1 | impl (pyfory) | partial (@apache-fory/core) | — |
| Zstd compression (threshold 4KB) | S5.2 | impl | impl (node:zlib) | — |
| Decompression bomb protection (16 MiB limit) | S5.2 | impl | impl | — |
| Wire type tag format ("namespace/TypeName") | S5.3 | impl | impl | — |

## Trust (Aster-trust-spec.md)

| Requirement | Spec | Python | TypeScript | Java |
|-------------|------|--------|------------|------|
| Ed25519 canonical signing bytes | TS S3 | pass (Rust core) | impl (Rust core) | — |
| Credential JSON canonical form | TS S3 | pass (Rust core) | impl (Rust core) | — |
| Consumer admission handshake protocol | TS S5 | impl | — | — |
| Producer admission handshake protocol | TS S6 | impl | — | — |

## Session Protocol (Aster-session-scoped-services.md)

| Requirement | Spec | Python | TypeScript | Java |
|-------------|------|--------|------------|------|
| CALL frame (0x10) demultiplexing | SS S2 | impl | impl | — |
| CANCEL frame (0x20) handling | SS S2 | impl | impl | — |
| Empty method in StreamHeader for session mode | SS S2 | impl | impl | — |

## Cross-Language Interop

| Test | Status |
|------|--------|
| Python server + TypeScript client (unary) | not tested |
| TypeScript server + Python client (unary) | not tested |
| Python server + TypeScript client (streaming) | not tested |
| Fory XLANG wire compat (pyfory ↔ @apache-fory/core) | not tested |
