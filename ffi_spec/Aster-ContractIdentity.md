---
title: "Aster Contract Identity"
sidebar_label: "Contract Identity"
sidebar_position: 3
description: "Content-addressed type definitions, canonical XLANG encoding, contract hashing, and publication procedures for Aster service contracts"
---

# Contract Identity via Content-Addressed Type Definitions

**Version:** 0.7.2 (tracking toward 1.0)
**Status:** Pre-release (0.1-alpha)
**Last Updated:** 2026-04-06
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
│   │   ├── tags/
│   │   │   └── {label}                          → contract_id
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

|Index|Logical name              |Content                                   |Required|
|-----|--------------------------|------------------------------------------|--------|
|0    |`manifest.json`           |`ContractManifest` JSON (metadata + method schemas)|Yes     |
|1    |`contract.xlang`          |Canonical XLANG bytes of `ServiceContract`|Yes     |
|2..N |`types/{type_hash}.xlang` |Canonical XLANG bytes of each `TypeDef`   |Yes     |
|opt  |`schema.fdl`              |Human-readable Fory IDL source text       |No      |
|opt  |`docs/`                   |Documentation bundle                      |No      |
|opt  |`compatibility/{other_id}`|Compatibility report vs another contract  |No      |

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

**Normative identity surface.** The `contract_id` is defined as the BLAKE3
hash of the canonical `ServiceContract` XLANG bytes — not of any FDL source
text. FDL is one authoring syntax; code-first decorators and proto/FlatBuffers
import are others. All front ends are equivalent: identity is defined solely by
the resulting `TypeDef` / `ServiceContract` descriptor graph. Two contracts
authored via different front ends that produce the same descriptor graph are the
same contract and will hash to the same `contract_id`.

**Alignment with Fory IDL and compiler IR.** Where the source contract is Fory
FDL, Aster aligns with stock Fory syntax and the normal compiler pipeline:

- FDL is parsed using the ordinary Fory parser and lowered into the compiler IR
  (`Schema`, `Message`, `Enum`, `Union`, `Service`, `RpcMethod`, `Field`).
- Aster contract hashing operates over a deterministic descriptor graph derived
  from that IR; it does **not** require a separate Aster-specific FDL parser.
- Aster-specific service metadata should be expressed, where needed, as normal
  Fory `option` entries so that stock tooling can parse the file. Unrecognised
  options may be ignored by non-Aster tooling.

This keeps authoring maximally compatible with Fory out of the box while still
allowing Aster to define stronger identity rules over the resulting IR.

FDL source, when present in the contract bundle, is advisory — useful for
human inspection and tooling, but not an input to identity. The `schema.fdl`
bundle member is optional for this reason.

Non-FDL front ends (proto import, code-first, FlatBuffers) carry a translation
correctness burden: they must produce a TypeDef-equivalent descriptor graph.
Whether they do so is a translator specification concern, not an Aster identity
concern. Aster does not specify proto→TypeDef or FBS→TypeDef mappings; those
are defined by the respective import tools.

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

1. **Fields emitted in ascending field ID order.** This is an Aster-specific
   canonicalization rule for hashing. It intentionally overrides Fory's normal
   struct field ordering algorithm so that identity depends on a smaller,
   simpler, explicitly specified rule.
1. **Schema-consistent mode.** No per-object TypeDef metadata headers in the
   payload. Types are known statically.
1. **No reference tracking.** Type descriptors are acyclic trees (self-
   references use a placeholder mechanism described below), so ref tracking
   is unnecessary overhead.
1. **Standalone serialization.** No stream context, no meta sharing state from
   prior objects, no session-scoped caches. Each canonical byte sequence is
   self-contained.
1. **No compression.** Canonical bytes are stored and hashed uncompressed.

With these constraints, the same `TypeDef` value produces identical bytes from
any conforming Fory XLANG implementation.

:::warning
This canonical serializer is **not** identical to stock Fory struct serialization. Aster reuses Fory's primitive, string, bytes, and collection
wire encodings, but defines its own field-emission order and disables all
optional framing features not explicitly listed here. Implementations must not
hash the output of a generic `fory.serialize(...)` call unless that call is
configured to match this profile exactly.
:::

#### 11.3.2.1 Canonical byte layout

For Aster contract hashing, `canonical_xlang_bytes(T)` means the XLANG encoding
of a **non-null root value of statically known type `T`**, with all optional
and implementation-dependent framing removed or pinned as follows:

- **No outer Fory header.** The 1-byte top-level Fory bitmap header is not part
  of canonical bytes.
- **No outer reference/null meta for the root value.** The root `TypeDef` /
  `ServiceContract` being hashed is always present and statically known.
- **No root type meta.** The canonical bytes do not include a top-level type ID,
  user type ID, named-type metadata, or shared TypeDef marker.
- **No schema hash prefix.** The optional 4-byte schema hash used by some Fory
  schema-consistent encodings is not part of canonical bytes.
- **Nested field values use the ordinary XLANG field-value encodings for their
  statically known field types**, subject to the constraints in this section.

Operationally, implementations should behave as if they are serializing the
field values of a known message directly, in ascending field-ID order, using
Fory's primitive/container encodings with all optional metadata features turned
off unless this profile explicitly requires them.

For nested fields:

- **Non-optional nested message fields** serialize as their field values only,
  with no nested type meta and no nested null/reference flag.
- **Optional nested message fields** serialize a single null/presence flag using
  the ordinary XLANG nullable-field convention, followed by the nested field
  values only when present.
- **No nested value in canonical hashing writes runtime type metadata.** All
  types are statically known from the framework-internal schema.

#### 11.3.2.2 Primitive, string, bytes, and collection pinning

To avoid implementation drift, the following details are normative:

- **Primitive mapping follows Fory IDL exactly.** In particular, in stock Fory
  IDL `int32` maps to `VARINT32` and `int64` maps to `VARINT64` (confirmed by
  the Fory compiler's `PRIMITIVE_TYPES` table, which maps `"int32"` →
  `PrimitiveKind.VARINT32` and `"int64"` → `PrimitiveKind.VARINT64`). Aster
  adopts that mapping for its framework-internal schema. Concretely, every
  `int32` field in §11.3.3 (e.g. `FieldDef.id`, `EnumValueDef.value`,
  `ServiceContract.version`) is encoded as a **signed ZigZag varint** — not as
  a fixed 4-byte little-endian integer. If fixed-width encoding is desired in a
  future revision, the schema must use `fixed_int32` / `fixed_int64` explicitly.
- **Strings must be encoded as UTF-8** in canonical bytes, even though general
  Fory XLANG may choose LATIN1 or UTF16 opportunistically.
- **Strings that represent identifiers (method names, type names, package
  names, enum/union member names, role names, and other programmer-visible
  identifiers defined in §11.3.3) must be in Unicode Normalization Form C
  (NFC) before encoding.** Unicode allows the same visual identifier to be
  represented by multiple code-point sequences (e.g. `é` as one NFC
  codepoint vs two NFD codepoints); without a pinned normal form, two
  implementations would produce different canonical bytes for the same
  source code. NFC is chosen because it is the form used by the web
  platform and most filesystems. Application-defined attribute values and
  free-text strings (descriptions, reasons, etc.) are NOT required to be
  NFC-normalized — only identifiers participating in canonical hashing
  are. Arbitrary string payloads are encoded verbatim.
- **Identifiers MUST conform to Unicode UAX #31** (`XID_Start` followed by
  zero or more `XID_Continue` codepoints — the same rule as Python's
  `str.isidentifier()`, Java identifiers, Rust identifiers). This includes
  method names, type names, package names, enum/union member names, and
  role names in `CapabilityRequirement`. Implementations SHOULD warn on
  registration of identifiers that mix Unicode scripts (e.g. Latin + Cyrillic)
  or contain confusables, as a usability safeguard — but the framework does
  not reject such identifiers, since `contract_id` already prevents any
  structural confusion between distinct services.
- **Hash-bearing `bytes` fields use standard XLANG bytes encoding** and must
  contain exactly 32 payload bytes when carrying a BLAKE3 digest.
- **Enum fields serialize as their Fory XLANG enum encoding** (unsigned varint
  of the enum value). Discriminator fields (`TypeKind`, `ContainerKind`,
  `TypeDefKind`, `MethodPattern`, `CapabilityKind`, `ScopeKind`) use enum
  types rather than strings to eliminate case/spelling sensitivity and produce
  more compact canonical bytes.
- **Lists in the framework-internal schema are homogeneous, declared-type,
  non-null, non-ref-tracked lists.** Their canonical elements header is
  therefore the XLANG optimal header for that case (`0x0C`).
- **Maps, if introduced into future framework-internal descriptor types, must
  likewise use statically known key/value types with nullability and ref
  tracking pinned by schema, not by runtime heuristics.**

For descriptor fields that act like discriminated unions by convention
(`FieldDef.type_kind`, `FieldDef.container_key_kind`, etc.), unused companion
fields are serialized using their ordinary zero values unless this spec states
otherwise:

- unused `string` fields serialize as the empty string `""`
- unused `bytes` fields serialize as zero-length bytes
- unused `bool` fields serialize as `false`
- unused `enum` fields serialize as value `0` (the first variant)

This is required because those fields are part of normal Fory messages, not a
wire-level union construct.

These rules mean canonical hashing is coupled not just to "Fory XLANG in
general" but to a precisely pinned subset of it.

### 11.3.3 Framework-Internal Type Definitions

These types live in the `_aster` reserved namespace and are used exclusively
for registry storage and contract identity. They are not application-visible
message types. They are defined using ordinary Fory IDL `message` and `enum`
syntax so they can be parsed by the stock Fory compiler and represented in the
normal compiler IR.

```text
// _aster/registry.fdl
package _aster;

// ── Discriminator enums ─────────────────────────────────────
// Using enums instead of strings for discriminator fields ensures
// that canonical hashing is not sensitive to string spelling/case
// and produces more compact wire encoding (varint vs UTF-8 string).

enum TypeKind [id=1] {
    PRIMITIVE = 0;
    REF = 1;
    SELF_REF = 2;
    ANY = 3;
}

enum ContainerKind [id=2] {
    NONE = 0;
    LIST = 1;
    SET = 2;
    MAP = 3;
}

enum TypeDefKind [id=3] {
    MESSAGE = 0;
    ENUM = 1;
    UNION = 2;
}

enum MethodPattern [id=4] {
    UNARY = 0;
    SERVER_STREAM = 1;
    CLIENT_STREAM = 2;
    BIDI_STREAM = 3;
}

enum CapabilityKind [id=5] {
    ROLE = 0;
    ANY_OF = 1;
    ALL_OF = 2;
}

enum ScopeKind [id=6] {
    SHARED = 0;
    STREAM = 1;
}

// ── Type atoms ──────────────────────────────────────────────

message FieldDef {
    int32 id = 1;                   // Field number from IDL or code
    string name = 2;                // Canonical field name (snake_case)
    TypeKind type_kind = 3;         // PRIMITIVE, REF, SELF_REF, ANY
    string type_primitive = 4;      // e.g. "string", "int32", "bool" — set when type_kind = PRIMITIVE
    bytes type_ref = 5;             // BLAKE3 hash (32 bytes) of referenced TypeDef — set when type_kind = REF
    string self_ref_name = 6;       // Fully-qualified type name — set when type_kind = SELF_REF
    bool optional = 7;
    bool ref_tracked = 8;           // Fory `ref` modifier
    ContainerKind container = 9;    // NONE, LIST, SET, MAP
    TypeKind container_key_kind = 10; // For maps: PRIMITIVE or REF
    string container_key_primitive = 11;
    bytes container_key_ref = 12;
}

// Canonical zero-value convention for discriminated fields:
// - when type_kind = PRIMITIVE: type_ref = empty bytes, self_ref_name = ""
// - when type_kind = REF: type_primitive = "", self_ref_name = ""
// - when type_kind = SELF_REF: type_primitive = "", type_ref = empty bytes
// - when type_kind = ANY: type_primitive = "", type_ref = empty bytes,
//   self_ref_name = ""; the field's type identity is carried solely by the
//   enum discriminator value ANY
// - when container != MAP: container_key_kind = PRIMITIVE (zero value),
//   container_key_primitive = "", container_key_ref = empty bytes

message EnumValueDef {
    string name = 1;
    int32 value = 2;
}

message UnionVariantDef {
    string name = 1;                // Variant label
    int32 id = 2;                   // Variant case ID
    bytes type_ref = 3;             // BLAKE3 hash of variant TypeDef
}

message TypeDef {
    TypeDefKind kind = 1;           // MESSAGE, ENUM, UNION
    string package = 2;             // Dotted package name
    string name = 3;                // Unqualified type name
    list<FieldDef> fields = 4;      // Sorted by field id. Present when kind = MESSAGE.
    list<EnumValueDef> enum_values = 5;   // Sorted by value. Present when kind = ENUM.
    list<UnionVariantDef> union_variants = 6; // Sorted by id. Present when kind = UNION.
}

// ── Service contract ────────────────────────────────────────

message CapabilityRequirement {
    CapabilityKind kind = 1;       // ROLE, ANY_OF, ALL_OF
    list<string> roles = 2;        // Role strings. Single item for kind=ROLE.
}
// kind semantics:
//   ROLE   — caller must hold exactly this one role (roles has one entry)
//   ANY_OF — caller must hold at least one of the listed roles
//   ALL_OF — caller must hold every listed role
// Absent field (default) means no capability check is required for this method.

message MethodDef {
    string name = 1;
    MethodPattern pattern = 2;     // UNARY, SERVER_STREAM, CLIENT_STREAM, BIDI_STREAM
    bytes request_type = 3;        // BLAKE3 hash of request TypeDef
    bytes response_type = 4;       // BLAKE3 hash of response TypeDef (stream item type for streaming)
    bool idempotent = 5;
    float64 default_timeout = 6;   // Seconds, 0 = none
    optional CapabilityRequirement requires = 7;  // Optional. Absent = no capability check required.
}

message ServiceContract {
    string name = 1;                // Wire service name
    int32 version = 2;              // Human-facing version label
    list<MethodDef> methods = 3;    // Sorted by method name (Unicode codepoint, NFC-normalized)
    list<string> serialization_modes = 4; // Ordered by producer preference
    string alpn = 5;                // Always "aster/{wire_version}"
    ScopeKind scoped = 6;           // SHARED (default) or STREAM (session-scoped)
    optional CapabilityRequirement requires = 7;  // Optional service-level baseline. Effective
                                         // requirement for a method is the conjunction of
                                         // this field and the method's own requires field.
                                         // Absent on both = no rcan check for that method.
}
```

In the framework-internal schema, a `requires` field is explicitly marked
`optional` because "no requirement" is represented as field absence, not as an
empty `CapabilityRequirement` value.

**Capability requirement evaluation**

The effective requirement for a method call is resolved at call time from two
sources: the service-level `ServiceContract.requires` (baseline) and the
method-level `MethodDef.requires` (refinement). The rule is additive — the
caller must satisfy both independently:

```text
effective = conjunction(service.requires, method.requires)
```

Evaluation of each `CapabilityRequirement` against the caller's rcan
`capability` list:

|`kind`    |Satisfied when                                     |
|----------|---------------------------------------------------|
|`ROLE`    |`capability` contains `roles[0]`                   |
|`ANY_OF`  |`capability` contains at least one entry in `roles`|
|`ALL_OF`  |`capability` contains every entry in `roles`       |

The conjunction means both the service requirement and the method requirement
must evaluate to satisfied. If either fails, the call is rejected with
`PERMISSION_DENIED` before the handler is invoked.

Absence of a `requires` field (at either level) is treated as unconditionally
satisfied — it contributes nothing to the conjunction. A method with no
`requires` on either level requires no rcan at all.

**Example:** service sets `requires = {kind: ANY_OF, roles: ["Admin", "Operator"]}`,
method sets `requires = {kind: ROLE, roles: ["TaskManager"]}`. The effective
requirement is: caller must hold at least one of `{Admin, Operator}` AND must
hold `TaskManager`. An rcan carrying `["Admin", "TaskManager"]` passes. An rcan
carrying only `["Admin"]` fails the method check. An rcan carrying only
`["TaskManager"]` fails the service check.

### 11.3.4 Hashing Procedure

Given a source contract (FDL file, code-first decorators, or any other input):

**Step 1 — Resolve all types.** Walk the type graph reachable from every method
signature. For each unique type, construct a `TypeDef`. For Fory FDL input,
this starts from the standard Fory compiler IR (`Schema`/`Service`/`Message`/
`Enum`/`Union`) rather than reparsing into an Aster-only model.

**Step 2 — Hash leaves first.** Process the type graph bottom-up:

- Types with no type references (only primitive fields, enums with no type
  refs) are serialized to canonical XLANG bytes and hashed immediately.
- Types that reference other types replace each reference with the 32-byte
  BLAKE3 hash of the referenced `TypeDef` in the `type_ref` field of the
  corresponding `FieldDef`.

**Step 3 — Handle self-references.** A type that references itself (directly
or through mutual recursion) cannot be hashed bottom-up. For self-referencing
fields:

- Set `type_kind = SELF_REF` and `self_ref_name` to the type's own
  `package + "." + name`.
- All other (non-self) references are still resolved to hashes.
- The `TypeDef` is then serialized and hashed normally. The self-reference
  placeholder is deterministic (same name → same bytes → same hash).

Mutual recursion and larger recursive strongly-connected components are resolved
by a deterministic cycle-breaking rule over the type graph:

- Compute strongly connected components (SCCs) of the reachable named-type
  graph.
- Any SCC of size 1 with no self-edge is handled normally and has no
  `SELF_REF` placeholder.
- For every SCC that contains a cycle, choose a deterministic spanning tree by
  visiting member types in Unicode-codepoint order of the NFC-normalized
  fully-qualified type name, and traversing outgoing edges in Unicode-codepoint
  order of the NFC-normalized referenced fully-qualified
  type name.
- Any edge within the SCC that is **not** chosen as part of that spanning tree
  is encoded as `type_kind = SELF_REF` with `self_ref_name` set to the
  referenced type's fully-qualified name.

This general rule covers direct self-recursion, two-type mutual recursion, and
longer cycles such as `A → B → C → A` without requiring special cases.

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

Given this FDL (valid stock Fory IDL syntax plus Aster-specific `option`
entries that non-Aster tooling may ignore):

```text
package aster.agent;

message TaskAssignment {
    string task_id = 1;
    string workflow_yaml = 2;
    list<string> credential_refs = 3;
    int32 step_budget = 4;
}

message TaskAck {
    bool accepted = 1;
    optional string reason = 2;
}

service AgentControl {
    option version = 1;
    option serialization = "xlang";

    rpc assign_task(TaskAssignment) returns (TaskAck) {
        option timeout_ms = 30000;
        option idempotent = true;
        option requires = "any_of:TaskManager,Admin";
    }
}
```

Interpretation note:

- `option version = 1;` is Aster service metadata carried via standard Fory
  option syntax.
- `option serialization = "xlang";` expresses the preferred serialization mode
  using a plain string value.
- `option timeout_ms = 30000;` is interpreted by Aster tooling as
  `default_timeout = 30.0` seconds in `MethodDef`.
- `option requires = "any_of:TaskManager,Admin";` is an Aster-defined string
  DSL carried in a standard Fory option field. This authoring DSL is not itself
  part of contract identity; it must be lowered to a concrete
  `CapabilityRequirement { kind, roles }` value before canonical hashing.

Stock Fory tooling can parse these options, but current compiler builds may emit
warnings for unknown service- and method-level option names. Aster tooling is
expected to consume those option key/value pairs from the parsed IR despite such
warnings.

For interoperability, the current DSL grammar is:

```text
requires-dsl := role:<name>
              | any_of:<name>(,<name>)*
              | all_of:<name>(,<name>)*
```

Whitespace is not significant around commas after parsing, but canonical
hashing never consumes the raw DSL string directly.

Resolution:

1. `TaskAssignment` has only primitive fields → serialize `TypeDef`, hash →
   `ta_hash`.
1. `TaskAck` has only primitive fields → serialize `TypeDef`, hash →
   `ack_hash`.
1. Build `MethodDef`:
   `{name: "assign_task", pattern: UNARY, request_type: ta_hash, response_type: ack_hash, idempotent: true, default_timeout: 30.0, requires: {kind: ANY_OF, roles: ["TaskManager", "Admin"]}}`
1. Build `ServiceContract`:
   `{name: "AgentControl", version: 1, methods: [<above>], serialization_modes: ["xlang"], alpn: "aster/1", scoped: SHARED}`
1. Serialize → hash → `contract_id`.

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

:::info
The Aster spec version determines the Fory wire version used for
canonical encoding. If the Fory wire format changes incompatibly, the Aster
spec version must be bumped and all contract hashes recomputed.
:::

This is acceptable during the pre-1.0 phase of both projects. After Fory 1.0,
canonical encoding is stable indefinitely.

|Aster Spec Version|Fory Wire Version|Status    |
|------------------|-----------------|----------|
|0.9.x             |Fory 0.15.x XLANG|Pre-stable|
|1.0.x             |Fory 1.x XLANG   |Stable    |

-----

## §11.4 Contract Publication

### 11.4.1 Authoring Model

The `ContractManifest` is a **build artifact, not a source artifact.** The
service definition — FDL file or decorated source code — is the human-authored
source of truth and lives in version control. The manifest is generated from
that source at commit time, carries git provenance, and is embedded in the
deployable artifact. It is never committed to the repository.

```text
git repo (committed)           build artifact (gitignored / generated)
────────────────────           ──────────────────────────────────────
service.py  ─────────────────► .aster/manifest.json
  (or service.fdl)               vcs_revision, vcs_tag, semver, ...
                                 ↓ embedded at build time
                               service_node binary / wheel / container
                                 ↓ on startup
                               Iroh collection published to registry
```

The analogy is exact: the FDL or source file is the `pyproject.toml`; the
published contract bundle is the `.whl` on a package registry. You commit the
former, you publish the latter.

**Fory IDL compatibility note.** When FDL is used as the source of truth,
authors should prefer stock Fory syntax (`message`, `enum`, `union`, `service`,
`rpc`, and ordinary `option` statements). Aster-specific semantics are layered
on top by interpreting selected option keys/values after parsing the normal
Fory compiler IR. This keeps source files consumable by standard Fory tooling,
with only advisory warnings for unknown options.

**Credential separation.** Contract publication uses the node's registry write
credential (the docs `NamespaceSecret` or an author key with write access) — the
same credential used for endpoint lease writes. The offline root key (§2.1 of
the trust spec) is not involved. Publication is a normal node operation, not an
administrative act.

### 11.4.2 `aster contract gen` — Offline Manifest Generation

`aster contract gen` is a **purely offline tool** — no running node, no
credentials, no network. It is intended for use as a git commit hook.

```bash
# .git/hooks/post-commit (or pre-commit)
aster contract gen --out .aster/manifest.json
```

It reads the service definition from the current source tree, resolves the type
graph, computes `contract_id`, captures the current VCS revision and tag
(if any), and writes `.aster/manifest.json`. Add `.aster/manifest.json` to
`.gitignore`.

The manifest is then embedded in the deployable artifact using the language's
native resource embedding mechanism:

|Language|Mechanism                                                                                                                                        |
|--------|-------------------------------------------------------------------------------------------------------------------------------------------------|
|Rust    |`include_bytes!(".aster/manifest.json")` or appended binary resource                                                                             |
|Python  |`importlib.resources` via package data in wheel                                                                                                  |
|Go      |`//go:embed .aster/manifest.json`                                                                                                                |
|Other   |Append to binary — binary formats permit trailing resource data; collection hash is computed at publish time from canonical bytes, not the binary|

### 11.4.3 Startup Publication

On node startup, before advertising endpoint leases, the node:

1. Reads the embedded `ContractManifest`.
1. Resolves the type graph from its own service definitions (already required
   to serve calls).
1. Serializes each `TypeDef` to canonical XLANG bytes.
1. Serializes the `ServiceContract` to canonical XLANG bytes. Verifies
   `blake3(bytes) == manifest.contract_id` — a mismatch means the embedded
   manifest does not match the compiled service definition and is a fatal
   startup error. The error message must include:
   - the expected `contract_id` from the manifest,
   - the actual hash computed from the live service definition,
   - the service name and version from the manifest,
   - a suggestion to re-run `aster contract gen` and rebuild.
   
   This diagnostic is critical because the most common cause of mismatch is a
   code change after the last `aster contract gen` run (e.g. a field was added
   to a request message but the manifest was not regenerated). The error should
   make this root cause obvious without requiring the developer to manually
   diff canonical bytes.
1. Builds the Iroh collection with the layout defined in §11.2:
- index 0: `manifest.json` → `ContractManifest` JSON
- index 1: `contract.xlang` → canonical `ServiceContract` bytes
- index 2..N: `types/{type_hash}.xlang` → canonical `TypeDef` bytes
- optional: `schema.fdl`, documentation bundle, compatibility reports
1. Imports the collection into the local `iroh-blobs` store.
1. Tags the collection for GC protection:
   
   ```
   tag name:  aster/contract/{friendly_name}@{contract_id}
   tag value: HashAndFormat { hash: collection_root_hash, format: HashSeq }
   ```
   
   `friendly_name` is taken from the manifest (e.g. `manifest.semver` or
   `manifest.service`). The `contract_id` in the tag name is authoritative;
   `friendly_name` is decorative. A single HashSeq tag protects all child blobs.
   To unpublish, delete the tag.
1. Writes an `ArtifactRef` to `contracts/{contract_id}` in the registry docs.
   Idempotent — re-publishing the same `contract_id` is a no-op.
1. Writes the version pointer at `services/{name}/versions/v{version}` →
   `contract_id`.
1. Optionally writes human tags at `services/{name}/tags/{label}` →
   `contract_id`, where `{label}` may be a semver string, git tag, or any
   human label from the manifest.
1. Optionally updates channel aliases
   (`services/{name}/channels/{channel}` → `contract_id`).
1. Broadcasts `CONTRACT_PUBLISHED` on gossip.
1. Begins advertising endpoint leases.

Step 13 is last deliberately — a contract is always discoverable before any
endpoint lease appears for it. Consumers will never observe an endpoint without
a resolvable contract.

### 11.4.4 `ContractManifest` Structure

```text
ContractManifest {
    // ── Contract identity ──────────────────────────────────
    service: string
    version: int32
    contract_id: string              // hex-encoded BLAKE3 of ServiceContract
    canonical_encoding: string       // "fory-xlang/0.15" (pinned Fory wire version)
    type_count: int32
    type_hashes: list<string>        // all TypeDef hashes (transitive closure)
    method_count: int32
    methods: list<MethodDescriptor>  // full method schemas for dynamic invocation
    serialization_modes: list<string>
    scoped: string                   // "shared" or "session"
    deprecated: bool

    // ── Source provenance (written by aster contract gen) ──
    semver: string?                  // e.g. "2.1.0" — advisory, not enforced
    vcs_revision: string?            // full commit hash (e.g. git SHA) that produced this contract
    vcs_tag: string?                 // e.g. "v2.1.0"
    vcs_url: string?                 // repository URL for traceability
    changelog: string?               // short human note about what changed

    // ── Publication metadata (written at startup) ─────────
    published_by: AuthorId           // set at publish time, not by aster contract gen
    published_at_epoch_ms: int64     // set at publish time
}

MethodDescriptor {
    name: string                     // method name (NFC-normalized)
    pattern: string                  // "unary" | "server_stream" | "client_stream" | "bidi_stream"
    request_type: string             // human-readable type name (e.g. "HelloRequest")
    response_type: string            // human-readable type name (e.g. "HelloResponse")
    timeout: float64?                // default timeout in seconds, null if none
    idempotent: bool
    fields: list<FieldDescriptor>    // request type fields (empty if type info unavailable)
}

FieldDescriptor {
    name: string                     // field name
    type: string                     // type name (e.g. "str", "int", "list[str]", "MyType")
    required: bool                   // true if no default value
    default: any?                    // JSON-safe default value, null if required or non-serializable
}
```

`vcs_*` and `semver` fields are written by `aster contract gen` at commit time.
`published_by` and `published_at_epoch_ms` are written by the node at startup.
None of these fields are inputs to `contract_id` — they live in the manifest
blob (collection index 0), which is separate from `contract.xlang` (index 1).

#### 11.4.4.1 Dual-format design: `contract.xlang` vs `manifest.json`

The same method and type information exists in two formats within the contract
collection, serving different purposes:

- **`contract.xlang`** (canonical XLANG bytes) is the **identity format**. It is
  deterministically serialized and hashed to produce the `contract_id`. It is
  **write-only** — implementations MUST NOT deserialize `contract.xlang` from
  untrusted sources. The only valid operation on fetched `contract.xlang` bytes
  is `blake3(bytes) == contract_id` verification.

- **`manifest.json`** is the **readable format**. It contains the same method
  signatures and field definitions as JSON, enabling dynamic clients, shells,
  and tooling to inspect a contract without needing the language-specific type
  definitions. It is the source for service discovery, autocomplete, and
  schema-driven invocation.

:::danger Security: never deserialize canonical bytes from untrusted peers
The canonical XLANG encoding uses varints, length-prefixed strings, and nested
structures. Deserializing these from an untrusted source creates attack surface
for allocation bombs, out-of-bounds reads, and parser confusion attacks. The
canonical format is designed exclusively for deterministic hashing — all
human/machine-readable access goes through `manifest.json` (standard JSON).
:::

**`published_by` is a bundle-level attribute, not an identity-level
attribute.** Two different authors can publish collections with the same
`contract_id` — the canonical bytes and hence the identity are identical;
only the packaging differs. This is by design:

- The `contract_id` content-addresses the *contract definition*, not the
  *publisher*. If two independent parties generate the same canonical
  bytes, they have produced the same contract.
- A consumer that needs to trust a specific publisher for a specific
  contract MUST combine `contract_id` with the registry's ACL
  (Aster-SPEC.md §11.2.3) — the registry ACL identifies which authors may
  write ArtifactRefs to `contracts/{contract_id}`.
- `published_by` is useful for audit trails and operational telemetry
  ("who first published this bundle to this registry?") but is NOT a
  routing input and MUST NOT be used as an authorization hint.

Consumers that read `published_by` for any purpose beyond logging/audit
should instead read the registry ACL (`_aster/acl/writers`) and verify
that the AuthorId on the `ArtifactRef` docs entry is in the trusted set.
That path is what actually gates publication.

The `type_hashes` field allows a consumer to verify the type closure without
walking the Merkle DAG. The authoritative type graph is encoded in the `TypeDef`
references themselves; `type_hashes` is an optimisation for prefetching and
integrity checking.

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
(`bindings/aster_rs/`) or the FFI layer. The following extensions are
required before §11.4 can be fully implemented in any language other than Rust.

**iroh-blobs extensions**

|Capability                                 |Status     |Why needed                                                                                                                                                                             |
|-------------------------------------------|-----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|`Tags` API (`set`, `get`, `delete`, `list`)|✅ Done (Phase 1c.1)|GC protection for published contract collections (step 5a)                                                                                                              |
|`FsStore`                                  |✅ Done    |`CoreNode::persistent(path)` uses `FsStore::load()`; exposed as `IrohNode.persistent()` in Python — implemented before Phase 1c                                                       |
|`Downloader`                               |✅ Done    |`CoreBlobsClient::download_blob/download_collection` use `Downloader::new(&store, &endpoint).download(hash, providers)` — single-provider today, multi-provider ready                 |
|`BlobTicket` serving                       |✅ Done    |`create_ticket`/`create_collection_ticket` mint tickets; `BlobsProtocol` in the router handles inbound connections automatically — implemented before Phase 1c                         |
|`Remote` API (`iroh_blobs::api::Remote`)   |✅ Done (Phase 1d.2)|`blob_local_info(hash_hex)` via `store.remote().local(HashAndFormat::raw(hash))` — returns `is_complete` + `local_bytes`; exposed in Python and FFI            |
|`observe()`                                |✅ Done (Phase 1d.1)|`blob_observe_snapshot(hash_hex)` (bitfield snapshot) and `blob_observe_complete(hash_hex)` (wait until complete) via `store.blobs().observe(hash)`; exposed in Python and FFI|

**iroh-docs extensions**

|Capability                     |Status     |Why needed                                                                                      |
|-------------------------------|-----------|------------------------------------------------------------------------------------------------|
|`import_and_subscribe()`       |✅ Done (Phase 1c.8)|Race-free join: subscribe before first sync to avoid missing initial `CONTRACT_PUBLISHED` events|
|`Doc.subscribe()` (live events)|✅ Done (Phase 1c.4)|React to `InsertRemote` / `ContentReady` events for registry change notifications         |
|`start_sync()` / `leave()`     |✅ Done (Phase 1c.5)|Explicit sync lifecycle control                                                           |
|`DownloadPolicy`               |✅ Done (Phase 1c.6)|`NothingExcept` policy to selectively sync `_aster/` prefix without pulling all service data|
|`DocTicket` creation           |✅ Done (Phase 1c.7)|Constructing share tickets with full relay+address info for registry namespace bootstrapping|

**Priority order for implementation:** Tags + FsStore are P0 (without them,
published contracts are lost on restart). `import_and_subscribe` and
`Doc.subscribe` are P1 (needed for live registry sync). Everything else is P2.

**Phase 1c completion (2026-04-04):** All iroh-docs extensions and the Tags API
are now implemented across Core, Python bindings, and the FFI layer.

**Phase 1d completion (2026-04-04):** All remaining iroh-blobs capabilities
(Remote API `blob_local_info`, `observe()` via `blob_observe_snapshot` /
`blob_observe_complete`) are now implemented across Core, Python bindings, and
the FFI layer. All §11.5 capabilities are complete.

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

-----

## Appendix A: Canonical Encoding Test Vectors

Conformance across implementations requires authoritative test vectors. This
appendix defines the procedure and minimal cases; the actual hex bytes must be
generated by the first conforming implementation (Rust) and committed to the
repository as the reference.

### A.1 Procedure

For each test case below:

1. Construct the value in the implementation's native representation.
2. Serialize using `canonical_xlang_bytes(T)` as defined in §11.3.2.
3. Compute `blake3(bytes)` and record both the hex-encoded bytes and the
   hex-encoded hash.
4. Every other implementation must produce identical bytes and hash.

### A.2 Minimal ServiceContract (no methods)

```text
Input:
  ServiceContract {
    name: "EmptyService"
    version: 1
    methods: []                    // empty list
    serialization_modes: ["xlang"]
    alpn: "aster/1"
    scoped: SHARED                 // enum value 0
    requires: absent               // optional field, not present
  }

Field-by-field encoding (ascending field ID order):

  Field 1 (name: string "EmptyService"):
    UTF-8 header: varuint36_small((12 << 2) | 2) = varuint36_small(50)
    Followed by 12 UTF-8 bytes: "EmptyService"

  Field 2 (version: int32 → varint32, value 1):
    ZigZag(1) = 2, varuint(2) = 0x02

  Field 3 (methods: list<MethodDef>, empty):
    varuint32(0)       // length = 0
    0x0C               // elements header: declared-type, same-type, non-null, no ref

  Field 4 (serialization_modes: list<string>, one element "xlang"):
    varuint32(1)       // length = 1
    0x0C               // elements header
    UTF-8 string "xlang" (5 bytes): varuint36_small((5 << 2) | 2) followed by "xlang"

  Field 5 (alpn: string "aster/1"):
    UTF-8 header: varuint36_small((7 << 2) | 2) = varuint36_small(30)
    Followed by 7 UTF-8 bytes: "aster/1"

  Field 6 (scoped: ScopeKind.SHARED = 0):
    varuint(0) = 0x00

  Field 7 (requires: optional, absent):
    NULL_FLAG = 0xFD

Expected bytes: 32456d7074795365727669636502000c010c16786c616e671e61737465722f3100fd
Expected hash:  66d4a269145ccf609d7f98130ab66f5d72175fd8ad456416455d9525d253df1f
```

*Python-reference v1, pending cross-verification (Java binding)*

### A.3 Minimal TypeDef (enum, no references)

```text
Input:
  TypeDef {
    kind: ENUM                     // TypeDefKind value 1
    package: "test"
    name: "Color"
    fields: []                     // empty (not MESSAGE)
    enum_values: [
      EnumValueDef { name: "RED",   value: 0 },
      EnumValueDef { name: "GREEN", value: 1 },
      EnumValueDef { name: "BLUE",  value: 2 },
    ]
    union_variants: []             // empty (not UNION)
  }

Expected bytes: 01127465737416436f6c6f72000c030c0e5245440016475245454e0212424c554504000c
Expected hash:  bac1586aaa144fa0b565268419da29f18e536f18c7290e4bdf3496919cfa29ce
```

*Python-reference v1, pending cross-verification (Java binding)*

### A.4 TypeDef with type references

A message type whose fields reference other types by hash, verifying that
`type_ref` bytes fields are encoded correctly:

```text
Input:
  TypeDef {
    kind: MESSAGE
    package: "test"
    name: "Wrapper"
    fields: [
      FieldDef {
        id: 1, name: "inner", type_kind: REF,
        type_primitive: "", type_ref: <32 bytes of 0xAA>, self_ref_name: "",
        optional: false, ref_tracked: false,
        container: NONE, container_key_kind: PRIMITIVE,
        container_key_primitive: "", container_key_ref: <0 bytes>
      }
    ]
    enum_values: []
    union_variants: []
  }

Expected bytes: 0012746573741e57726170706572010c0216696e6e6572010220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa02000000000200000c000c
Expected hash:  f72fd18c27bc41c07a1a582758142a7cb14b5c9ec8e6f286871ad280b66fbafa
```

*Python-reference v1, pending cross-verification (Java binding)*

### A.5 MethodDef with optional requires present

Verifies that the optional `CapabilityRequirement` field serializes its
presence flag and nested field values correctly:

```text
Input:
  MethodDef {
    name: "do_work"
    pattern: UNARY
    request_type: <32 bytes of 0x11>
    response_type: <32 bytes of 0x22>
    idempotent: true
    default_timeout: 30.0
    requires: CapabilityRequirement {
      kind: ANY_OF
      roles: ["Admin", "Operator"]
    }
  }

Expected bytes: 1e646f5f776f726b00201111111111111111111111111111111111111111111111111111111111111111202222222222222222222222222222222222222222222222222222222222222222010000000000003e400001020c1641646d696e224f70657261746f72
Expected hash:  c74c82db4d785d96141c6ee621176ce7c1628210802e64b04f9dd0ee4b268fa0
```

*Python-reference v1, pending cross-verification (Java binding)*

### A.6 MethodDef with optional requires absent

Same structure as A.5 but with `requires` absent, verifying the null flag
encoding:

```text
Input:
  MethodDef {
    name: "do_work"
    pattern: UNARY
    request_type: <32 bytes of 0x11>
    response_type: <32 bytes of 0x22>
    idempotent: false
    default_timeout: 0.0
    requires: absent
  }

Expected bytes: 1e646f5f776f726b00201111111111111111111111111111111111111111111111111111111111111111202222222222222222222222222222222222222222222222222222222222222222000000000000000000fd
Expected hash:  de4f82d4139d0897f3ecf93899258bfdaca00681beaea041aad0284a9cd9b569
```

*Python-reference v1, pending cross-verification (Java binding)*

**Implementation note:** The Python reference implementation has generated the
hex bytes and hashes committed to `tests/fixtures/canonical_test_vectors.json`.
Every language's conformance test suite should assert byte-equality and
hash-equality against those committed vectors.

-----

## Appendix B: Cycle-Breaking Worked Examples

These examples demonstrate the SCC-based cycle-breaking algorithm from §11.3.4
Step 3 on progressively complex recursive type graphs.

### B.1 Direct self-recursion (SCC size 1, self-edge)

```text
package example;

message TreeNode {
    string value = 1;
    optional TreeNode left = 2;    // self-reference
    optional TreeNode right = 3;   // self-reference
}
```

**Type graph:** `example.TreeNode` has edges to itself (via `left` and `right`).

**SCC:** `{example.TreeNode}` — size 1, with self-edge.

**Spanning tree:** The single member is the root. Its outgoing edges to itself
are all back-edges (not part of the spanning tree).

**Result:**
- `left.type_kind = SELF_REF`, `left.self_ref_name = "example.TreeNode"`
- `right.type_kind = SELF_REF`, `right.self_ref_name = "example.TreeNode"`
- The `TypeDef` is serialized and hashed. No other type's hash is needed.

### B.2 Two-type mutual recursion (SCC size 2)

```text
package example;

message Author {
    string name = 1;
    list<Book> books = 2;          // references Book
}

message Book {
    string title = 1;
    Author written_by = 2;         // references Author
}
```

**Type graph:**
```text
example.Author → example.Book
example.Book   → example.Author
```

**SCC:** `{example.Author, example.Book}` — size 2, contains a cycle.

**Spanning tree construction:**
1. Sort members by Unicode codepoint (NFC-normalized): `example.Author`, `example.Book`.
2. Start from `example.Author` (first in codepoint order).
3. Traverse outgoing edges of `example.Author` in codepoint order of target:
   - Edge `example.Author → example.Book` → add to spanning tree. Visit
     `example.Book`.
4. Traverse outgoing edges of `example.Book` in codepoint order of target:
   - Edge `example.Book → example.Author` → `example.Author` is already
     visited. This edge is **not** in the spanning tree.

**Spanning tree edges:** `{Author → Book}`
**Back-edges (SELF_REF):** `{Book → Author}`

**Hashing order:**
1. Hash `example.Book` first. Its field `written_by` references `Author`, but
   that edge is a back-edge, so:
   - `written_by.type_kind = SELF_REF`
   - `written_by.self_ref_name = "example.Author"`
   - Serialize and hash `Book` → `book_hash`.

2. Hash `example.Author`. Its field `books` references `Book`, and that edge is
   a spanning-tree edge (not a back-edge), so:
   - The list element's `type_kind = REF`
   - The list element's `type_ref = book_hash`
   - Serialize and hash `Author` → `author_hash`.

**Verification:** `book_hash` does not depend on `author_hash` (it uses a
`SELF_REF` placeholder). `author_hash` depends on `book_hash` (via `REF`).
No circular dependency.

### B.3 Three-type cycle (SCC size 3)

```text
package example;

message Alpha {
    Beta next = 1;
}

message Beta {
    Gamma next = 1;
}

message Gamma {
    Alpha next = 1;                // closes the cycle
}
```

**Type graph:**
```text
example.Alpha → example.Beta
example.Beta  → example.Gamma
example.Gamma → example.Alpha
```

**SCC:** `{example.Alpha, example.Beta, example.Gamma}` — size 3, cycle.

**Spanning tree construction:**
1. Sort members: `example.Alpha`, `example.Beta`, `example.Gamma`.
2. Start from `example.Alpha`.
3. `example.Alpha → example.Beta` → add to tree. Visit `example.Beta`.
4. `example.Beta → example.Gamma` → add to tree. Visit `example.Gamma`.
5. `example.Gamma → example.Alpha` → already visited. **Back-edge.**

**Spanning tree edges:** `{Alpha → Beta, Beta → Gamma}`
**Back-edges (SELF_REF):** `{Gamma → Alpha}`

**Hashing order:**
1. Hash `example.Gamma`. Its `next` field references `Alpha`, but that's a
   back-edge:
   - `next.type_kind = SELF_REF`, `next.self_ref_name = "example.Alpha"`
   - Hash → `gamma_hash`.

2. Hash `example.Beta`. Its `next` field references `Gamma` (tree edge):
   - `next.type_kind = REF`, `next.type_ref = gamma_hash`
   - Hash → `beta_hash`.

3. Hash `example.Alpha`. Its `next` field references `Beta` (tree edge):
   - `next.type_kind = REF`, `next.type_ref = beta_hash`
   - Hash → `alpha_hash`.

**Key property:** Exactly one edge in each cycle is broken. The choice is
deterministic — any conforming implementation that follows the Unicode-codepoint
spanning tree algorithm will break the same edge (`Gamma → Alpha`) and produce
identical hashes.

### B.4 Diamond with back-edge (SCC size 3, multiple paths)

```text
package example;

message A {
    B b_field = 1;
    C c_field = 2;
}

message B {
    C c_field = 1;
}

message C {
    A a_field = 1;                 // closes the cycle
}
```

**Type graph:**
```text
example.A → example.B
example.A → example.C
example.B → example.C
example.C → example.A
```

**SCC:** `{example.A, example.B, example.C}` — all three are in the same SCC
because `A → B → C → A`.

**Spanning tree construction:**
1. Sort: `example.A`, `example.B`, `example.C`.
2. Start from `example.A`. Outgoing edges sorted by target:
   - `A → B` → add to tree. Visit `B`.
3. From `example.B`. Outgoing edges:
   - `B → C` → add to tree. Visit `C`.
4. From `example.C`. Outgoing edges:
   - `C → A` → already visited. **Back-edge.**
5. Back to `A`'s remaining edges:
   - `A → C` → already visited. **Back-edge.**

**Spanning tree edges:** `{A → B, B → C}`
**Back-edges (SELF_REF):** `{C → A, A → C}`

**Hashing order:**
1. `example.C`: `a_field` is SELF_REF("example.A"). Hash → `c_hash`.
2. `example.B`: `c_field` is REF(`c_hash`). Hash → `b_hash`.
3. `example.A`: `b_field` is REF(`b_hash`), `c_field` is SELF_REF("example.C").
   Hash → `a_hash`.

Note: `A.c_field` uses SELF_REF even though `C` has already been hashed. This
is because `A → C` is a back-edge in the spanning tree, and the algorithm
uniformly encodes all back-edges as SELF_REF. This is intentional — using the
hash of `C` instead would change `A`'s hash depending on traversal order, which
would break the determinism guarantee. The spanning tree determines which edges
are REF vs SELF_REF, and that determination is fixed.

---

## Appendix B: Golden Vectors

These are normative test vectors generated from the Python reference
implementation. Every conforming implementation MUST produce byte-identical
canonical output and identical BLAKE3 hashes for these inputs.

The full machine-readable vectors are in `conformance/vectors/contract-identity.json`.

### B.1 Minimal Unary Service

**Input:**

```
ServiceContract {
  name: "Echo", version: 1, scoped: SHARED (0),
  methods: [
    MethodDef { name: "echo", pattern: UNARY (0),
      request_type: 00..00 (32 zero bytes),
      response_type: 00..00 (32 zero bytes),
      idempotent: false, default_timeout: 0.0,
      requires: absent }
  ],
  serialization_modes: [], requires: absent
}
```

**Canonical bytes (94 bytes):**

```
12 45 63 68 6f                            # string "Echo": varint(18)=0x12, "Echo"
02                                         # zigzag(1) = 2
01 0c                                      # list(1 method), 0x0C element header
  12 65 63 68 6f                           #   string "echo"
  00                                       #   pattern = UNARY (0)
  20 00..00                                #   request_type: 32 zero bytes
  20 00..00                                #   response_type: 32 zero bytes
  00                                       #   idempotent = false
  00 00 00 00 00 00 00 00                  #   default_timeout = 0.0 (float64 LE)
  fd                                       #   requires: absent (NULL_FLAG)
00 0c                                      # serialization_modes: empty list
00                                         # scoped = SHARED (0)
fd                                         # requires: absent (NULL_FLAG)
```

**Full hex:** `124563686f02010c126563686f00200000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000fd000c00fd`

**BLAKE3:** `73ac6c9e70c7dcdd825221a4eb1d1ac9432d890685e65987f7d8d74c8d3191be`

### B.2 Service with Capability Requirement

**Input:**

```
ServiceContract {
  name: "DataService", version: 1, scoped: SHARED,
  methods: [
    MethodDef { name: "get_record", pattern: UNARY,
      request_type: 00..00, response_type: 00..00,
      idempotent: true, default_timeout: 30000.0,
      requires: CapabilityRequirement {
        kind: ANY_OF (1),
        roles: ["reader", "ai-reader"]  // sorted: "ai-reader", "reader"
      }
    }
  ],
  serialization_modes: [], requires: absent
}
```

**Canonical bytes:** 127 bytes

**Full hex:** `2e446174615365727669636502010c2a6765745f7265636f7264002000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000000100000000004cdd400001020c2661692d7265616465721a726561646572000c00fd`

**BLAKE3:** `868a03134159c5797f36016c8445febf2e703456f0eb98eb02fa7dbc0d69bf89`

### B.3 Multi-Method Streaming Service

**Input:**

```
ServiceContract {
  name: "Analytics", version: 2, scoped: SHARED,
  methods: [  // sorted by NFC name: query, upload, watch
    MethodDef { name: "query", pattern: UNARY, idempotent: true, timeout: 0.0 },
    MethodDef { name: "upload", pattern: CLIENT_STREAM (2), idempotent: false, timeout: 0.0 },
    MethodDef { name: "watch", pattern: SERVER_STREAM (1), idempotent: false, timeout: 60000.0 },
  ]
}
```

**Canonical bytes:** 267 bytes

**Full hex:** `26416e616c797469637304030c16717565727900200000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000010000000000000000fd1a75706c6f616402200000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000fd167761746368012000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000000004ced40fd000c00fd`

**BLAKE3:** `4fcf7d24f1407d32ecda0526c5c087985086b5362ea8ed344c6838859c11d2d9`

### B.4 Session-Scoped Service

**Input:**

```
ServiceContract {
  name: "ChatRoom", version: 1, scoped: STREAM (1),
  methods: [
    MethodDef { name: "send_message", pattern: UNARY, timeout: 5000.0 }
  ]
}
```

**Canonical bytes:** 106 bytes

**Full hex:** `2243686174526f6f6d02010c3273656e645f6d6573736167650020000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000000000000000088b340fd000c01fd`

**BLAKE3:** `e49ce2b5992b58dc06d348511e05ebdb1fcf7ec504e12a931611913c5ea76ace`
