# Contract Publication Flow

**Status:** Reference implementation complete (Python). Blueprint for TypeScript, Java, Go, etc.

**Reference:** `bindings/python/aster/high_level.py` → `_publish_contracts()` (lines 535-662)

---

## Overview

When an AsterServer starts, it publishes each service's contract to a
registry doc. This enables consumers to discover method schemas, type
definitions, and download full contract artifacts — without sharing
source code or proto files.

The registry doc is an iroh CRDT document (replicated key-value store).
The server creates it, writes contract metadata, and shares it read-only.
Consumers receive the doc's namespace ID during admission and can join
it to fetch schemas.

```
┌─────────────────────────────────────────────────────────────┐
│                    AsterServer.start()                       │
│                                                             │
│  1. Create registry doc (iroh docs)                         │
│  2. Create author ID (ed25519 keypair)                      │
│  3. For each @service:                                      │
│     a. Walk type graph from method signatures               │
│     b. Resolve cycles (Tarjan SCC)                          │
│     c. Compute canonical bytes + BLAKE3 hashes              │
│     d. Build collection (contract.bin + types + manifest)   │
│     e. Upload collection to blob store (HashSeq)            │
│     f. Write ArtifactRef to registry doc                    │
│     g. Write manifest shortcut to registry doc              │
│     h. Write version pointer to registry doc                │
│  4. Share registry doc (read-only)                          │
│  5. Store namespace ID for admission response               │
└─────────────────────────────────────────────────────────────┘
```

---

## Prerequisites

The server must have:
- A running `IrohNode` with docs and blobs subsystems
- At least one registered service with `@service` / `@Service` decorator
- Service methods with typed request/response annotations

---

## Step-by-Step Flow

### Step 1: Create registry doc and author

```
dc = docs_client(node)    → DocsClient handle
bc = blobs_client(node)   → BlobsClient handle

registry_doc = dc.create()       → new iroh Doc (CRDT key-value store)
author_id    = dc.create_author() → new AuthorId (ed25519 public key)
```

**Implementation notes:**
- One registry doc per server instance
- One author per server (the server owns write access)
- The doc is initially empty and not shared

### Step 2: For each service — build the type graph

```
root_types = []
for method in service.methods:
    root_types.append(method.request_type)
    root_types.append(method.response_type)

type_graph = build_type_graph(root_types)
→ dict[fully_qualified_name → type_class]
```

**Algorithm: `build_type_graph(root_types)`**

1. Start from root types (method request/response types)
2. Recursive DFS through dataclass field annotations
3. Skip primitives (str, int, float, bool, bytes, None)
4. Unwrap generics (List[T] → visit T, Optional[T] → visit T)
5. Only include dataclass types
6. Track visited set by FQN to avoid infinite loops
7. Return all discovered types as `{FQN: type_class}`

**Language-agnostic requirement:**
- Walk the type graph from method signatures
- Collect all structured types (dataclasses / decorated classes)
- Skip primitives and built-in containers
- Handle generic type arguments

### Step 3: Resolve cycles with Tarjan's SCC

```
type_defs = resolve_with_cycles(type_graph)
→ dict[fully_qualified_name → TypeDef]
```

**Algorithm: `resolve_with_cycles(types)`**

1. Build reference graph: for each type, find which other types it references
2. Run Tarjan's SCC to find strongly-connected components
3. Process SCCs in reverse topological order (leaves first)
4. For single-node SCCs with self-reference: mark back-edge as `SELF_REF`
5. For multi-node SCCs:
   a. Sort members by NFC codepoint order
   b. DFS from lexicographically smallest member to find spanning tree
   c. Non-tree edges within the SCC become `SELF_REF`
6. For each type in processing order:
   a. Build `TypeDef` (field names, field types, type references)
   b. Serialize to canonical bytes
   c. Compute BLAKE3 hash
   d. Store hash for downstream references

**Why Tarjan's:** Types can have mutual references (A references B, B
references A). The SCC algorithm identifies these cycles and breaks them
deterministically so the hash is stable across languages.

**Language-agnostic requirement:**
- Implement Tarjan's SCC (or equivalent cycle detection)
- Use NFC normalization for deterministic ordering
- Mark back-edges with a sentinel value (`SELF_REF`)
- Hash bottom-up: leaves before parents

### Step 4: Compute type hashes

```
for each (fqn, type_def) in type_defs:
    canonical = canonical_xlang_bytes(type_def)  → bytes
    hash      = BLAKE3(canonical)                → 32 bytes
    type_hashes[fqn] = hash
```

**`canonical_xlang_bytes(obj)`** produces a deterministic byte
representation of a TypeDef, MethodDef, or ServiceContract. The encoding
is defined by the Aster XLANG canonical format (see `Aster-SPEC.md`
§5.3).

**Language-agnostic requirement:**
- Delegate to the shared Rust core: `canonical_bytes_from_json(type_name, json)`
- Or reimplement the canonical encoder per the XLANG spec
- Hash with BLAKE3 (32-byte output)

### Step 5: Build ServiceContract

```
contract = ServiceContract.from_service_info(service_info, type_hashes)
```

**ServiceContract fields:**

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Service name (e.g. "MissionControl") |
| `version` | int | Version number |
| `methods` | MethodDef[] | Sorted by NFC name |
| `serialization_modes` | string[] | e.g. ["xlang"] |
| `scoped` | enum | SHARED (0) or STREAM (1) |
| `requires` | optional | Capability requirement |

**MethodDef fields:**

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Method name |
| `pattern` | enum | UNARY, SERVER_STREAM, CLIENT_STREAM, BIDI_STREAM |
| `request_type` | 32 bytes | BLAKE3 hash of request TypeDef |
| `response_type` | 32 bytes | BLAKE3 hash of response TypeDef |
| `idempotent` | bool | Safe to retry |
| `default_timeout` | float | Seconds (0 = no default) |

**Contract ID computation:**

```
contract_bytes = canonical_xlang_bytes(contract)
contract_id    = BLAKE3(contract_bytes).hex()   → 64-char hex string
```

### Step 6: Build collection entries

```
entries = build_collection(contract, type_defs, service_info)
→ list[(name, bytes)]
```

**Collection structure (deterministic order):**

```
contract.bin                    ← canonical ServiceContract bytes
types/{hash_hex_1}.bin          ← canonical TypeDef bytes (one per type)
types/{hash_hex_2}.bin
...
manifest.json                  ← human-readable manifest with method schemas
```

**manifest.json structure:**

```json
{
  "service": "MissionControl",
  "version": 1,
  "contract_id": "a1b2c3...",
  "canonical_encoding": "fory-xlang/0.15",
  "type_count": 4,
  "type_hashes": ["aabb...", "ccdd...", ...],
  "method_count": 3,
  "methods": [
    {
      "name": "getStatus",
      "pattern": "unary",
      "request_type": "aabb...",
      "response_type": "ccdd...",
      "timeout": 30.0,
      "idempotent": false,
      "fields": [
        {"name": "agent_id", "type": "str", "required": true}
      ]
    }
  ],
  "serialization_modes": ["xlang"],
  "scoped": "shared"
}
```

The manifest includes field-level detail (names, types, required flags)
extracted from the service method signatures. This powers the shell's
tab completion and the MCP tool schema generation.

### Step 7: Upload collection to blob store

```
collection_hash = await blobs.add_collection(entries)
→ 64-char hex hash of the HashSeq blob
```

**What `add_collection` does:**
1. Store each `(name, bytes)` entry as an individual content-addressed blob
2. Build an iroh `Collection` (HashSeq format) indexing all entries
3. Set a persistent tag on the collection for GC protection
4. Return the BLAKE3 hash of the collection root blob

**Optional: create download ticket**

```
blob_ticket = blobs.create_collection_ticket(collection_hash)
→ serialized BlobTicket string
```

The ticket contains the server's endpoint address + the collection hash,
allowing consumers to download all collection blobs in one transfer.

### Step 8: Write ArtifactRef to registry doc

```
ref = ArtifactRef(
    contract_id         = contract_id,        # 64-char hex
    collection_hash     = collection_hash,    # 64-char hex
    ticket              = blob_ticket,        # serialized BlobTicket
    published_by        = author_id,          # AuthorId hex
    published_at_epoch_ms = now_ms,           # Unix epoch milliseconds
    collection_format   = "index",            # multi-file collection
)

await registry_doc.set_bytes(
    author_id,
    key   = b"contracts/{contract_id}",
    value = ref.to_json().encode(),
)
```

**ArtifactRef JSON structure:**

```json
{
  "contract_id": "a1b2c3...",
  "collection_hash": "d4e5f6...",
  "provider_endpoint_id": null,
  "relay_url": null,
  "ticket": "blob1...",
  "published_by": "author_hex...",
  "published_at_epoch_ms": 1712567890123,
  "collection_format": "index"
}
```

### Step 9: Write manifest shortcut to registry doc

```
await registry_doc.set_bytes(
    author_id,
    key   = b"manifests/{contract_id}",
    value = manifest_json_bytes,
)
```

**Why a shortcut:** Consumers that only need method schemas (like the
shell or MCP server) can read the manifest directly from the registry
doc without downloading the full collection from the blob store. This
avoids a round-trip and works even when blob transfer is slow.

### Step 10: Write version pointer

```
await registry_doc.set_bytes(
    author_id,
    key   = b"services/{name}/versions/v{version}",
    value = contract_id.encode(),
)
```

**Purpose:** Allows consumers to look up the current contract ID for a
service by name and version, without knowing the contract hash in advance.

### Step 11: Share the registry doc

```
await registry_doc.share_with_addr("read")
namespace_id = registry_doc.doc_id()
→ 64-char hex string (BLAKE3 public key of the namespace keypair)
```

**What `share_with_addr("read")` does:**
1. Enables the sync engine on this document
2. Makes the document discoverable and replicable by peers
3. Peers receive read-only access (can fetch entries, not write)

**The namespace ID is stored as `_registry_namespace` and returned in
the admission response.**

---

## Registry Doc Key Schema

```
contracts/{contract_id}                    → ArtifactRef JSON
manifests/{contract_id}                    → manifest.json bytes
services/{name}/versions/v{version}        → contract_id (UTF-8)
```

All keys are UTF-8 encoded bytes. All values are either JSON or raw
bytes depending on the key prefix.

---

## Consumer Flow (read path)

After admission, the consumer receives `registry_namespace` (64-char hex).

```
1. Join the registry doc by namespace ID
   doc = dc.join_and_subscribe(namespace_id, server_endpoint_id)

2. Look up the service version pointer
   contract_id = doc.get(b"services/MissionControl/versions/v1")

3. Read the manifest (fast path — no blob download)
   manifest = doc.get(b"manifests/{contract_id}")
   → parse JSON → method names, patterns, field schemas

4. (Optional) Download full collection (for codegen)
   artifact_ref = doc.get(b"contracts/{contract_id}")
   → parse ArtifactRef JSON → extract collection_hash
   → blobs.download(collection_hash) or use ticket
   → read contract.bin + types/*.bin for full type definitions
```

---

## Diagram: Data Flow

```
                    ┌──────────────────┐
                    │  @service class   │
                    │  method sigs      │
                    └────────┬─────────┘
                             │
                    ┌────────▼─────────┐
                    │ build_type_graph  │ Walk annotations
                    └────────┬─────────┘
                             │
                    ┌────────▼─────────┐
                    │resolve_with_cycles│ Tarjan SCC + hashing
                    └────────┬─────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
    ┌─────────▼──┐  ┌───────▼────┐  ┌──────▼──────┐
    │contract.bin│  │types/*.bin │  │manifest.json│
    │(canonical) │  │(canonical) │  │(human-read) │
    └─────────┬──┘  └───────┬────┘  └──────┬──────┘
              │              │              │
              └──────────────┼──────────────┘
                             │
                    ┌────────▼─────────┐
                    │ add_collection   │ Upload to blob store
                    │ (HashSeq)        │ as content-addressed
                    └────────┬─────────┘ collection
                             │
                      collection_hash
                             │
              ┌──────────────┼──────────────┐
              │              │              │
    ┌─────────▼──────┐ ┌────▼─────┐ ┌──────▼──────┐
    │ contracts/     │ │manifests/│ │ services/   │
    │ {contract_id}  │ │{c_id}   │ │ {name}/     │
    │ → ArtifactRef  │ │→ JSON   │ │ versions/   │
    └────────────────┘ └──────────┘ │ v{N} → c_id │
                                    └─────────────┘
                    ┌──────────────────┐
                    │   Registry Doc   │  (iroh CRDT doc)
                    │   shared read    │
                    └────────┬─────────┘
                             │
                      namespace_id
                             │
                    ┌────────▼─────────┐
                    │  Admission       │
                    │  Response        │
                    │  registry_       │
                    │  namespace: hex  │
                    └──────────────────┘
```

---

## Implementation Checklist (per language)

### Core dependencies (from Rust `aster_transport_core`)

These MUST be delegated to the shared Rust core for cross-language hash
consistency:

- [ ] `canonical_bytes_from_json(type_name, json_str) → bytes`
- [ ] `compute_type_hash(canonical_bytes) → 32 bytes`

### Iroh subsystem calls (via native binding)

- [ ] `docs_client.create() → DocHandle`
- [ ] `docs_client.create_author() → AuthorId`
- [ ] `doc.set_bytes(author_id, key, value) → hash`
- [ ] `doc.share_with_addr("read") → ticket`
- [ ] `doc.doc_id() → namespace_id`
- [ ] `blobs_client.add_collection(entries) → collection_hash`
- [ ] `blobs_client.create_collection_ticket(hash) → ticket_string`

### Pure language-side logic

- [ ] Type graph walker (walk annotations/decorators to find all types)
- [ ] Tarjan's SCC (or equivalent cycle detection)
- [ ] `build_collection()` — assemble `(name, bytes)` entries
- [ ] `ArtifactRef` — JSON serialization
- [ ] Registry key formatting (`contracts/`, `manifests/`, `services/`)
- [ ] Manifest JSON builder (extract field names/types from method sigs)
- [ ] Wire into server startup (call after node creation, before serve)
- [ ] Pass `namespace_id` to admission handler opts
- [ ] Non-fatal: wrap in try/catch, log warning on failure

### Verification

- [ ] Contract ID matches Python for the same service definition
- [ ] Manifest JSON is parseable by the shell and MCP server
- [ ] Consumer can join the registry doc and read manifests
- [ ] `gen-client` can fetch contracts from the registry doc

---

## Notes for implementers

1. **Hash consistency is critical.** The contract ID must be identical
   across languages for the same service definition. This is why
   canonical serialization and hashing are delegated to Rust.

2. **The manifest shortcut is optional but strongly recommended.** Without
   it, consumers must download the full collection just to get method
   schemas. The shortcut avoids this.

3. **Publication is non-fatal.** If it fails (e.g. no docs subsystem),
   the server still works — consumers just don't get rich metadata.
   The admission response still includes service names and methods
   from the service summaries.

4. **The collection format is "index" (multi-file).** Older implementations
   used "raw" (single blob). New implementations should always use "index".

5. **GC protection is automatic.** The `add_collection` call tags the
   collection, preventing iroh's garbage collector from deleting it.

6. **The author ID is not security-critical.** It's used for CRDT
   conflict resolution, not authentication. Any valid ed25519 public
   key works.
