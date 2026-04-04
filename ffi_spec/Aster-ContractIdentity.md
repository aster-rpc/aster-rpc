# Contract Identity via Content-Addressed Type Definitions

**Replaces:** §11.2 (namespace structure update), §11.3 (contract canonicalization),
§11.4 (contract publication). **Resolves:** Open question #10 (canonical contract encoding).

-----

## §11.2 Registry Data Model and Namespace Structure

The registry separates **immutable type and contract artifacts** (stored as
Iroh Blobs collections) from **mutable service aliases and endpoint leases**
(stored as iroh-docs entries). Types and contracts are identified by their
content address (BLAKE3 hash). The content address *is* the identity — no
external ID assignment, no collision risk, no coordination required.

**Storage model:** Immutable contract bundles are published as **Iroh
collections** (HashSeq format). An Iroh collection is an ordered sequence of
blob hashes; by convention the first element (index 0) is the application's
metadata blob. Aster uses a `ContractManifest` JSON blob at index 0, which
carries the name-to-hash mapping for all other members. iroh-docs stores
lightweight `ArtifactRef` pointers that resolve to collection root hashes. This
avoids simulating a filesystem hierarchy in docs keys for artifact storage and
aligns with Iroh's native content-addressed transfer primitives.

```text
{namespace}/
├── _aster/
│   ├── acl/
│   │   ├── writers                              → list[AuthorId]
│   │   ├── readers                              → list[AuthorId]
│   │   ├── admins                               → list[AuthorId]
│   │   └── policy                               → RegistryPolicy config
│   └── config/
│       ├── gossip_topic                         → TopicId for change notifications
│       ├── lease_duration_s                     → int (default: 45)
│       └── lease_refresh_interval_s             → int (default: 15)
│
├── contracts/
│   └── {contract_id}                            → ArtifactRef JSON (see below)
│
├── services/
│   ├── {service_name}/
│   │   ├── versions/
│   │   │   └── v{version}                       → contract_id
│   │   ├── channels/
│   │   │   ├── stable                           → contract_id
│   │   │   ├── canary                           → contract_id
│   │   │   └── dev                              → contract_id
│   │   ├── meta                                 → service metadata
│   │   └── contracts/
│   │       └── {contract_id}/
│   │           └── endpoints/
│   │               ├── {endpoint_id_hex}        → EndpointLease
│   │               └── ...
│   └── {another_service}/
│       └── ...
│
├── endpoints/
│   └── {endpoint_id_hex}/
│       ├── meta                                 → optional static endpoint metadata
│       └── tags                                 → optional discovery tags
│
└── compatibility/
    └── {contract_id}/
        └── {other_contract_id}                  → Compatibility report / diff
```

All entries are signed by their author's keypair. The `AuthorId` on each entry
is the cryptographic proof of who wrote it.

**ArtifactRef** — each `contracts/{contract_id}` docs entry stores a small JSON
pointer to the immutable Iroh collection containing the contract artifacts:

```text
ArtifactRef {
    contract_id: string              // hex-encoded BLAKE3 of ServiceContract
    collection_hash: string          // hex-encoded BLAKE3 root hash of the Iroh collection
    provider_endpoint_id: string?    // optional: endpoint serving the blobs ALPN
    relay_url: string?               // optional: relay for the provider
    ticket: string?                  // optional: bearer blob ticket for direct fetch
    published_by: AuthorId
    published_at_epoch_ms: int64
}
```

**Contract collection layout** — a contract is published as an Iroh collection
with the following positional layout. Iroh collections are positional HashSeqs;
member names are an Aster convention carried in the manifest, not an Iroh API
feature. The manifest at index 0 maps logical names to blob hashes, allowing
consumers to fetch any member by its content hash.

| Index | Logical name               | Content                                       | Required |
|-------|---------------------------|-----------------------------------------------|----------|
| 0     | `manifest.json`           | `ContractManifest` JSON (metadata blob)       | Yes      |
| 1     | `contract.xlang`          | Canonical XLANG bytes of `ServiceContract`    | Yes      |
| 2..N  | `types/{type_hash}.xlang` | Canonical XLANG bytes of each `TypeDef`       | Yes      |
| opt   | `schema.fdl`              | Human-readable Fory IDL source text           | No       |
| opt   | `docs/`                   | Documentation bundle                          | No       |
| opt   | `compatibility/{other_id}`| Compatibility report vs another contract      | No       |

Required members occupy fixed indices 0–(2+len(types)). Optional members follow
in any order; their hashes are listed in the manifest.

Key design points:

- The `types/` namespace no longer exists as docs keys. Type definitions are
  members of the contract collection, fetched by their content hash as listed in
  the manifest.
- Contract definitions reference types by hash, forming a Merkle DAG. Changing
  a type changes its hash, which changes the hash of every contract that
  references it.
- The `contract_id` is derived from the canonical `ServiceContract` bytes
  (the `contract.xlang` member), **not** from the collection root hash. The
  collection root hash identifies the *bundle*; the `contract_id` identifies
  the *contract*.
- After fetching a contract collection, consumers must verify
  `blake3(contract.xlang bytes) == contract_id` before trusting the bundle.

-----

## §11.3 Contract Canonicalization and Identity

### 11.3.1 Design

Contract identity is derived from content, not assigned. Every type definition
and service contract is serialized to a deterministic byte sequence using Fory
XLANG, then hashed with BLAKE3. The hash *is* the identity.

Types reference other types by hash, forming a Merkle DAG. A service contract
references its method request/response types by hash. The contract's own hash
is therefore transitively dependent on every type in its closure — a change to
any leaf type propagates upward automatically.

```text
                    ServiceContract
                    hash: 9f3a...
                   /       |       \
            MethodDef  MethodDef  MethodDef
           /        \
    TypeDef          TypeDef
    hash: abc1...    hash: def4...
       |                |
    FieldDef         FieldDef ──► TypeDef (hash: 77b2...)
```

### 11.3.2 Canonical XLANG Profile

The framework-internal types defined in this section are serialized using a
constrained subset of Fory XLANG called the **canonical XLANG profile**. This
profile ensures byte-identical output from any conforming implementation:

1. **Fields emitted in ascending field ID order.** Fory XLANG normally leaves
   field order implementation-defined; this profile makes it mandatory.
2. **Schema-consistent mode.** No per-object TypeDef metadata headers in the
   payload. Types are known statically.
3. **No reference tracking.** Type descriptors are acyclic trees (self-
   references use a placeholder mechanism described below), so ref tracking
   is unnecessary overhead.
4. **Standalone serialization.** No stream context, no meta sharing state from
   prior objects, no session-scoped caches. Each canonical byte sequence is
   self-contained.
5. **No compression.** Canonical bytes are stored and hashed uncompressed.

With these constraints, the same `TypeDef` value produces identical bytes from
any conforming Fory XLANG implementation.

### 11.3.3 Framework-Internal Type Definitions

These types live in the `_aster` reserved namespace and are used exclusively
for registry storage and contract identity. They are not application-visible
message types.

```
// _aster/registry.fdl
package _aster;

// ── Type atoms ──────────────────────────────────────────────

message FieldDef {
    int32 id = 1;                   // Field number from IDL or code
    string name = 2;                // Canonical field name (snake_case)
    string type_kind = 3;           // "primitive", "ref", "self_ref", "any"
    string type_primitive = 4;      // e.g. "string", "int32", "bool" — set when type_kind = "primitive"
    binary type_ref = 5;            // BLAKE3 hash (32 bytes) of referenced TypeDef — set when type_kind = "ref"
    string self_ref_name = 6;       // Local type name — set when type_kind = "self_ref"
    bool optional = 7;
    bool ref_tracked = 8;           // Fory `ref` modifier
    string container = 9;           // "", "list", "set", "map"
    string container_key_kind = 10; // For maps: "primitive" or "ref"
    string container_key_primitive = 11;
    binary container_key_ref = 12;
}

message EnumValueDef {
    string name = 1;
    int32 value = 2;
}

message UnionVariantDef {
    string name = 1;                // Variant label
    int32 id = 2;                   // Variant case ID
    binary type_ref = 3;            // BLAKE3 hash of variant TypeDef
}

message TypeDef {
    string kind = 1;                // "message", "enum", "union"
    string package = 2;             // Dotted package name
    string name = 3;                // Unqualified type name
    list<FieldDef> fields = 4;      // Sorted by field id. Present when kind = "message".
    list<EnumValueDef> enum_values = 5;   // Sorted by value. Present when kind = "enum".
    list<UnionVariantDef> union_variants = 6; // Sorted by id. Present when kind = "union".
}

// ── Service contract ────────────────────────────────────────

message CapabilityRequirement {
    string kind = 1;               // "role", "any_of", "all_of"
    list<string> roles = 2;        // Role strings. Single item for kind="role".
}
// kind semantics:
//   "role"   — caller must hold exactly this one role (roles has one entry)
//   "any_of" — caller must hold at least one of the listed roles
//   "all_of" — caller must hold every listed role
// Absent field (default) means no capability check is required for this method.

message MethodDef {
    string name = 1;
    string pattern = 2;            // "unary", "server_stream", "client_stream", "bidi_stream"
    binary request_type = 3;       // BLAKE3 hash of request TypeDef
    binary response_type = 4;      // BLAKE3 hash of response TypeDef (stream item type for streaming)
    bool idempotent = 5;
    float64 default_timeout = 6;   // Seconds, 0 = none
    CapabilityRequirement requires = 7;  // Optional. Absent = no rcan check required.
}

message ServiceContract {
    string name = 1;                // Wire service name
    int32 version = 2;              // Human-facing version label
    list<MethodDef> methods = 3;    // Sorted by method name (lexicographic, ASCII)
    list<string> serialization_modes = 4; // Ordered by producer preference
    string alpn = 5;                // Always "aster/{wire_version}"
    string scoped = 6;              // "shared" (default) or "stream" (session-scoped)
    CapabilityRequirement requires = 7;  // Optional service-level baseline. Effective
                                         // requirement for a method is the conjunction of
                                         // this field and the method's own requires field.
                                         // Absent on both = no rcan check for that method.
}
```

**Capability requirement evaluation**

The effective requirement for a method call is resolved at call time from two
sources: the service-level `ServiceContract.requires` (baseline) and the
method-level `MethodDef.requires` (refinement). The rule is additive — the
caller must satisfy both independently:

```
effective = conjunction(service.requires, method.requires)
```

Evaluation of each `CapabilityRequirement` against the caller's rcan
`capability` list:

| `kind`    | Satisfied when |
|-----------|----------------|
| `"role"`  | `capability` contains `roles[0]` |
| `"any_of"`| `capability` contains at least one entry in `roles` |
| `"all_of"`| `capability` contains every entry in `roles` |

The conjunction means both the service requirement and the method requirement
must evaluate to satisfied. If either fails, the call is rejected with
`PERMISSION_DENIED` before the handler is invoked.

Absence of a `requires` field (at either level) is treated as unconditionally
satisfied — it contributes nothing to the conjunction. A method with no
`requires` on either level requires no rcan at all.

**Example:** service sets `requires = any_of("Admin", "Operator")`, method sets
`requires = role("TaskManager")`. The effective requirement is: caller must hold
at least one of `{Admin, Operator}` AND must hold `TaskManager`. An rcan
carrying `["Admin", "TaskManager"]` passes. An rcan carrying only `["Admin"]`
fails the method check. An rcan carrying only `["TaskManager"]` fails the
service check.
```

### 11.3.4 Hashing Procedure

Given a source contract (FDL file, code-first decorators, or any other input):

**Step 1 — Resolve all types.** Walk the type graph reachable from every method
signature. For each unique type, construct a `TypeDef`.

**Step 2 — Hash leaves first.** Process the type graph bottom-up:

- Types with no type references (only primitive fields, enums with no type
  refs) are serialized to canonical XLANG bytes and hashed immediately.
- Types that reference other types replace each reference with the 32-byte
  BLAKE3 hash of the referenced `TypeDef` in the `type_ref` field of the
  corresponding `FieldDef`.

**Step 3 — Handle self-references.** A type that references itself (directly
or through mutual recursion) cannot be hashed bottom-up. For self-referencing
fields:

- Set `type_kind = "self_ref"` and `self_ref_name` to the type's own
  `package + "." + name`.
- All other (non-self) references are still resolved to hashes.
- The `TypeDef` is then serialized and hashed normally. The self-reference
  placeholder is deterministic (same name → same bytes → same hash).

Mutual recursion (A references B, B references A) is resolved by the same
mechanism: both A and B use `self_ref` for the cycle edge. Which edge becomes
the `self_ref` is determined by lexicographic ordering of the fully-qualified
type name — the type that sorts later uses `self_ref` for the back-edge.

**Step 4 — Build the `ServiceContract`.** Construct `MethodDef` entries with
request/response type hashes. Sort methods by name. Serialize the
`ServiceContract` to canonical XLANG bytes. Hash with BLAKE3.

```text
contract_id = hex(blake3(canonical_xlang_bytes(ServiceContract)))
```

**Step 5 — Package as collection.** Build an Iroh collection (see §11.2
contract collection layout) containing:
- `contract.xlang` → canonical `ServiceContract` bytes
- `manifest.json` → `ContractManifest` JSON
- `types/{hex(hash)}.xlang` → canonical `TypeDef` bytes for each type

Import the collection into `iroh-blobs`. Write an `ArtifactRef` to
`contracts/{contract_id}` in the registry namespace docs. These entries are
immutable — re-publishing the same bytes is idempotent and produces the same
collection root hash.

### 11.3.5 Worked Example

Given this FDL:

```
package aster.agent;

struct TaskAssignment {
    task_id: string;
    workflow_yaml: string;
    credential_refs: list<string>;
    step_budget: int32;
}

struct TaskAck {
    accepted: bool;
    reason: optional<string>;
}

service AgentControl {
    version = 1;
    serialization = [xlang];

    rpc assign_task(TaskAssignment) returns (TaskAck) {
        timeout = 30.0;
        idempotent = true;
        requires = any_of("TaskManager", "Admin");
    }
}
```

Resolution:

1. `TaskAssignment` has only primitive fields → serialize `TypeDef`, hash →
   `ta_hash`.
2. `TaskAck` has only primitive fields → serialize `TypeDef`, hash →
   `ack_hash`.
3. Build `MethodDef`:
   `{name: "assign_task", pattern: "unary", request_type: ta_hash, response_type: ack_hash, idempotent: true, default_timeout: 30.0, requires: {kind: "any_of", roles: ["TaskManager", "Admin"]}}`
4. Build `ServiceContract`:
   `{name: "AgentControl", version: 1, methods: [<above>], serialization_modes: ["xlang"], alpn: "aster/1"}`
5. Serialize → hash → `contract_id`.

If `TaskAssignment` gains a new field, its hash changes, which changes
`assign_task`'s `request_type`, which changes the `ServiceContract` hash.
The old and new contracts coexist as separate immutable entries.

### 11.3.6 Compatibility Detection

Because types are content-addressed, compatibility between two contract
versions can be checked structurally:

- **Method-level compatibility:** If two contracts share the same
  `request_type` and `response_type` hashes for a given method name, those
  methods are wire-identical regardless of version number.
- **Type-level compatibility:** Two `TypeDef` hashes are either equal (wire-
  identical) or not. Field-level diff is computed by fetching both `TypeDef`
  values and comparing their `FieldDef` lists.
- **Subset compatibility:** A new contract that adds methods but does not
  change existing method type hashes is a strict superset — clients using
  only the old methods can call the new contract safely.

Compatibility reports may be published under
`compatibility/{contract_id}/{other_contract_id}`. Whether these are advisory
or gating for channel promotion is a policy decision (see §16.2, question 13).

### 11.3.7 Version Coupling with Fory

The canonical XLANG profile is coupled to a specific Fory XLANG wire format
version. Before Fory reaches 1.0 and guarantees binary stability, Aster must
pin the Fory wire version used for canonical hashing.

Rule: The Aster spec version determines the Fory wire version used for
canonical encoding. If the Fory wire format changes incompatibly, the Aster
spec version must be bumped and all contract hashes recomputed. This is
acceptable during the pre-1.0 phase of both projects. After Fory 1.0,
canonical encoding is stable indefinitely.

| Aster Spec Version | Fory Wire Version | Status      |
|--------------------|-------------------|-------------|
| 0.9.x              | Fory 0.15.x XLANG | Pre-stable  |
| 1.0.x              | Fory 1.x XLANG    | Stable      |

-----

## §11.4 Contract Publication

A published contract is immutable. Publication creates an Iroh collection
bundle containing the contract artifacts and writes an `ArtifactRef` pointer
into docs. Re-publishing the same canonical bytes is idempotent — the
`contract_id` (BLAKE3 of canonical `ServiceContract` bytes) guarantees
identity.

**Publication procedure:**

1. Resolve the type graph from the service definition (decorators, IDL, or
   code-first annotations).
2. For each type in the closure, serialize a `TypeDef` to canonical XLANG
   bytes.
3. Serialize the `ServiceContract` to canonical XLANG bytes. Compute
   `contract_id = hex(blake3(bytes))`.
4. Build an Iroh collection with the layout defined in §11.2:
   - `contract.xlang` → canonical `ServiceContract` bytes
   - `manifest.json` → `ContractManifest` JSON (see below)
   - `types/{type_hash}.xlang` → canonical `TypeDef` bytes for each type
   - Optionally: `schema.fdl`, documentation bundle, compatibility reports
5. Import the collection into the local `iroh-blobs` store. The collection
   root hash is the BLAKE3 of the HashSeq (computed automatically by Iroh).
5a. **Tag the collection for GC protection.** Untagged blobs are eligible for
   garbage collection. Set a persistent named tag immediately after import:

   ```
   tag name: aster/contract/{friendly_name}@{contract_id}
   tag value: HashAndFormat { hash: collection_root_hash, format: HashSeq }
   ```

   `friendly_name` is a human-readable label (e.g. `AgentControl-v1`) chosen
   by the publisher. It is decorative — the `contract_id` in the tag name is
   the authoritative identity and is what tooling must use. Because HashSeq tags
   protect all referenced child blobs, a single tag on the collection root is
   sufficient to protect the manifest, `contract.xlang`, and all type blobs.
   To unpublish, delete the tag; the blobs become GC-eligible on the next
   collection cycle.
6. Write an `ArtifactRef` to `contracts/{contract_id}` in the registry
   namespace docs (see §11.2). If the key already exists with matching
   `contract_id`, the write is idempotent.
7. Write or confirm the version pointer at
   `services/{name}/versions/v{version}` → `contract_id`.
8. Optionally update channel aliases
   (`services/{name}/channels/{channel}` → `contract_id`).
9. Broadcast `CONTRACT_PUBLISHED` on gossip.

```text
ContractManifest {
    service: string
    version: int32
    contract_id: string              // hex-encoded BLAKE3 of ServiceContract
    canonical_encoding: string       // "fory-xlang/0.15" (pinned Fory wire version)
    type_count: int32                // number of distinct types in closure
    type_hashes: list<string>        // all TypeDef hashes (transitive closure)
    method_count: int32
    serialization_modes: list<string>
    alpn: string
    deprecated: bool
    published_by: AuthorId
    published_at_epoch_ms: int64
}
```

The `type_hashes` field allows a consumer to verify the type closure without
walking the Merkle DAG. The authoritative type graph is encoded in the
`TypeDef` references themselves; `type_hashes` is an optimisation for
prefetching and integrity checking.

**Fetching a contract:** A consumer that knows a `contract_id` reads the
`ArtifactRef` from `contracts/{contract_id}` in docs, fetches the Iroh
collection via `iroh-blobs` using the `collection_hash` (or the `ticket` string
passed directly to the blobs fetch API — no deserialization required),
reads the manifest blob at collection index 0 to resolve logical names to blob
hashes, fetches `contract.xlang` and all type blobs by their hashes, and
verifies `blake3(contract.xlang bytes) == contract_id` before trusting the
bundle.

-----

## §11.5 Required Python and FFI Surface Extensions

The publication and consumption procedures in §11.4 depend on iroh-blobs and
iroh-docs capabilities that are not yet exposed in the Python bindings
(`bindings/aster_python_rs/`) or the FFI layer. The following extensions are
required before §11.4 can be fully implemented in any language other than Rust.

**iroh-blobs extensions**

| Capability | Why needed |
|---|---|
| `Tags` API (`set`, `get`, `delete`, `list`) | GC protection for published contract collections (step 5a) |
| `FsStore` | Persistent blob storage across restarts; in-memory store loses all blobs on shutdown |
| `Downloader` | Multi-provider parallel fetch of contract collections |
| `Remote` API | Single-provider fetch with resume support |
| `BlobTicket` serving | Accepting inbound connections from consumers holding an `ArtifactRef.ticket`; the ticket string itself is opaque and requires no parsing — only serving requires Rust-level integration |
| `observe()` | Partial transfer detection and resumable download progress |

**iroh-docs extensions**

| Capability | Why needed |
|---|---|
| `import_and_subscribe()` | Race-free join: subscribe before first sync to avoid missing initial `CONTRACT_PUBLISHED` events |
| `Doc.subscribe()` (live events) | React to `InsertRemote` / `ContentReady` events for registry change notifications |
| `start_sync()` / `leave()` | Explicit sync lifecycle control |
| `DownloadPolicy` | `NothingExcept` policy to selectively sync `_aster/` prefix without pulling all service data |
| `DocTicket` creation | Constructing share tickets for registry namespace bootstrapping |

**Priority order for implementation:** Tags + FsStore are P0 (without them,
published contracts are lost on restart). `import_and_subscribe` and
`Doc.subscribe` are P1 (needed for live registry sync). Everything else is P2.

-----

## Changes to §16.2 (Open Design Questions)

**Question 10 — Canonical contract encoding:** Resolved. Canonical encoding
uses Fory XLANG with the canonical profile defined in §11.3.2. Types are
content-addressed individually, forming a Merkle DAG. The `ServiceContract`
hash is the `contract_id`. See §11.3 for full specification.

**Question 6 — Schema compatibility checking:** Partially addressed by
§11.3.6. Structural compatibility is detectable automatically by comparing
type hashes across contract versions. Full compatibility reports (field-level
diffs, breaking change analysis) remain a tooling concern built on top of
the content-addressed type store.

-----

## Changes to §6.2 (StreamHeader)

The Phase 1 blocker TODO in §6.2 is resolved. `contract_id` in the
`StreamHeader` is the hex-encoded BLAKE3 hash of the `ServiceContract`
serialized per the canonical XLANG profile (§11.3). Conformance test vectors
can now be generated by serializing known `ServiceContract` values and
computing their hashes.

-----

## Changes to §16.1 (Blocking Questions)

Update to read:

> All blocking questions are resolved. See §5.3 (type ID assignment), §5.5
> (ROW mode framing and streaming), §6.1 (ROW_SCHEMA flag), §8.3 (local
> client transport abstraction), and §11.3 (canonical contract encoding
> via content-addressed Merkle DAG).