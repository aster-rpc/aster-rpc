---
title: "Aster Docs Schema"
sidebar_label: "Docs Schema"
sidebar_position: 5
description: "Namespace-local schema discovery for typed Fory values stored directly in Iroh Docs"
---

# Aster Docs Schema

**Version:** 0.7.3 (tracking toward 1.0)
**Status:** Pre-release (0.1-alpha)
**Last Updated:** 2026-06-04
**Applies to:** Aster Contract Identity v0.7.2+
**Sections affected:** Registry data model, Docs APIs, language binding type
registration, schema/code generation.

-----

## 1. Motivation

Aster's contract identity work makes RPC request and response types
content-addressed and discoverable. The same is not yet true for applications
that write Fory XLANG values directly into Iroh Docs namespaces.

That gap matters because Iroh Docs is not only a service registry primitive. It
is also the natural replicated data plane for application state: policy records,
CRDT metadata, filesystem manifests, configuration, audit trails, and other
typed records that are not RPC messages.

Without a schema discovery mechanism, a producer can write cross-language Fory
bytes to Docs, but consumers in other bindings have no normative way to know:

- which keys contain typed values,
- which root type should be used to decode each value,
- which transitive `TypeDef` graph defines those root types,
- which author is trusted to define the namespace's schema,
- and which Fory profile applies to the value bytes.

This addendum defines a namespace-local schema declaration. Every discoverable
Docs namespace carries a well-known `_aster/docs/schema` entry whose value is a
framework-internal Fory record describing the namespace's key-to-type map.

The model is intentionally local to the namespace. A consumer that receives a
Docs namespace capability can discover that namespace's value schema from the
namespace itself, without first consulting an RPC service registry.

-----

## 2. Requirements

1. **Cross-language Fory.** Application values covered by a Docs schema MUST use
   Aster's cross-language Fory XLANG type mapping. Language-native or
   producer-private Fory modes are not valid for discoverable Docs values.
1. **Discoverable from the namespace.** The schema declaration lives at a
   reserved key inside the Docs namespace it describes.
1. **Self-contained v1.** The schema declaration contains the canonical
   `TypeDef` bytes for every type in the closure needed by the key-to-type map.
   A reader does not need an external registry to resolve types.
1. **Trust is explicit.** The schema value is trusted because of the Docs entry
   author, not because of a field inside the value.
1. **Existing annotations are reused.** `@wire_type`, `@WireType`, Java
   `@WireType`, and future Rust derives/codegen are the source-level mechanism
   for declaring application value types.
1. **Rust code generation is acceptable.** Rust is not required to load dynamic
   schemas at runtime. Rust consumers may fetch a schema for verification and
   use generated Rust types/codecs for decoding.

Non-goals for v1:

- Global cataloging of all Docs schemas.
- Runtime dynamic schema loading in Rust.
- Backward compatibility for pre-release application schemas.
- A generic authorization language. The schema declares decode shape; the
  application still enforces domain authority.

-----

## 3. Reserved Schema Key

Every discoverable Docs namespace MUST contain a schema declaration at the exact
byte key:

```text
_aster/docs/schema
```

The key is the UTF-8 byte sequence shown above, with no leading slash.

All Docs keys beginning with the UTF-8 byte prefix `_aster/` are reserved for
Aster framework metadata. Applications MUST NOT store application values under
that prefix. Readers MUST NOT match application schema entries against keys
under `_aster/`.

The schema entry MUST be written before any application value covered by the
schema is written, and before the namespace capability is shared with other
peers.

### 3.1 Shared Schema Immutability

In v1, a discoverable Docs namespace's schema is immutable once the namespace
capability has been shared outside the creator process.

After sharing, the schema author MUST NOT overwrite or delete
`_aster/docs/schema`. A reader that observes multiple trusted schema entries
for the same namespace MUST treat the namespace as schema-corrupt unless the
application is explicitly running in a development mode that permits schema
replacement.

This rule avoids the unsafe live-replication window where one peer observes a
new schema with old records, while another observes old schema with new records.

Schema iteration before sharing is allowed for local development only. Stable
applications that need a breaking change MUST use one of these strategies:

- create a new Docs namespace,
- use a new application key prefix or namespace kind in a new namespace, or
- design an application-level epoch/content-hash key prefix from the beginning
  and keep each epoch's records disjoint.

In all cases, records written under a shared schema MUST remain decodable by
that exact schema for the lifetime of the namespace.

-----

## 4. Data Model

The schema value at `_aster/docs/schema` is a Fory XLANG value of the
framework-internal type `_aster/DocsSchema`. Every Aster binding MUST be able to
decode this type without application schema registration.

### 4.1 Fory IDL

```text
package _aster;

enum DocsKeyPatternKind [id=20] {
    TEXT_EXACT = 0;
    TEXT_PREFIX = 1;
    TEXT_TEMPLATE = 2;
    BYTES_EXACT = 3;
    BYTES_PREFIX = 4;
    BYTES_TEMPLATE = 5;
}

enum DocsBytesSegmentKind [id=21] {
    LITERAL = 0;
    FIXED = 1;
    REST = 2;
    U16BE_LEN_BYTES = 3;
}

enum DocsValueEncoding [id=22] {
    FORY_XLANG_ROOT_KNOWN = 0;
}

message DocsTypeRef {
    string tag = 1;                 // e.g. "portal.policy/NodePolicy"
    bytes type_hash = 2;            // 32-byte BLAKE3 hash of canonical TypeDef
}

message DocsTypeDefRecord {
    DocsTypeRef ref = 1;
    bytes type_def = 2;             // canonical XLANG bytes of TypeDef
    bool root = 3;                  // true if it may appear as a top-level Docs value
}

message DocsBytesSegment {
    DocsBytesSegmentKind kind = 1;
    string name = 2;                // variable name for FIXED/REST/U16BE_LEN_BYTES
    bytes literal = 3;              // used when kind = LITERAL
    int32 width = 4;                // byte width for FIXED
}

message DocsBytesTemplate {
    list<DocsBytesSegment> segments = 1;
}

message DocsKeyPattern {
    DocsKeyPatternKind kind = 1;

    // Used by TEXT_EXACT, TEXT_PREFIX, TEXT_TEMPLATE.
    string text = 2;

    // Used by BYTES_EXACT, BYTES_PREFIX.
    bytes bytes = 3;

    // Used by BYTES_TEMPLATE.
    DocsBytesTemplate bytes_template = 4;
}

message DocsSchemaEntry {
    string name = 1;                // advisory stable label for diagnostics
    DocsKeyPattern key = 2;
    DocsTypeRef value_type = 3;     // root type used to decode matching values
    DocsValueEncoding encoding = 4; // v1: FORY_XLANG_ROOT_KNOWN

    // Tombstones are application values, not Docs key deletions. When
    // tombstone_type is absent and tombstone_allowed = true, the normal
    // value_type carries the tombstone discriminant.
    bool tombstone_allowed = 5;
    optional DocsTypeRef tombstone_type = 6;
}

message DocsSchema {
    int32 v = 1;                    // v1 for this layout
    string namespace_kind = 2;      // e.g. "portal.policy" or "portal.tree_manifest"
    string schema_name = 3;         // human-readable package name
    list<DocsTypeDefRecord> types = 4;
    list<DocsSchemaEntry> entries = 5;
}
```

### 4.2 Stored Encoding

The `_aster/DocsSchema` value itself is encoded with Aster framework Fory XLANG
in compatible mode. It is a bootstrap type: bindings pre-register it alongside
other framework-internal wire types such as `StreamHeader`, `CallHeader`, and
`RpcStatus`.

The schema value is not an application value and is not decoded using the
schema it describes.

### 4.3 TypeDef Records

Each `DocsTypeDefRecord` carries one canonical `TypeDef`:

```text
blake3(type_def) == ref.type_hash
```

The `tag` MUST match the package/name encoded by the `TypeDef`:

```text
tag == "{TypeDef.package}/{TypeDef.name}"
```

All `DocsSchemaEntry.value_type` and `tombstone_type` references MUST resolve to
a record in `DocsSchema.types`.

An entry's top-level `value_type` MUST reference a type record with
`root = true`. Helper types may be present with `root = false` when they are only
reachable from root types.

The TypeDef canonicalization and hashing rules are exactly the rules in
`Aster-ContractIdentity.md` §11.3. No new type identity mechanism is introduced
by this addendum.

-----

## 5. Key Pattern Semantics

Docs keys are arbitrary bytes. Aster supports both UTF-8 text key templates and
byte-segment templates.

### 5.1 Text Patterns

Text patterns apply only to Docs keys that are valid UTF-8.

`TEXT_EXACT`
: The key's UTF-8 string must exactly equal `DocsKeyPattern.text`.

`TEXT_PREFIX`
: The key's UTF-8 string must start with `DocsKeyPattern.text`.

`TEXT_TEMPLATE`
: `DocsKeyPattern.text` is a slash-separated template with placeholders.

Text template placeholders have the form:

```text
{name}
```

where `name` matches:

```text
[A-Za-z_][A-Za-z0-9_]*
```

In v1, placeholders MUST occupy an entire slash-separated path segment. A
placeholder matches a non-empty UTF-8 segment that does not contain `/`.

Examples:

```text
/portal/v1/nodes/{node_id}
/portal/v1/trees/{tree_id}/grants/{node_id}
```

Literal `{` and `}` are forbidden in v1 text templates. If an application needs
literal braces in keys, it must use `TEXT_EXACT`, `TEXT_PREFIX`, or a bytes
pattern.

Text pattern validation:

- `TEXT_EXACT`, `TEXT_PREFIX`, and `TEXT_TEMPLATE` MUST set `text` and MUST
  leave `bytes` empty and `bytes_template.segments` empty.
- `TEXT_PREFIX` MUST NOT use an empty prefix.
- `TEXT_TEMPLATE` MUST reject duplicate placeholder names within one pattern.
- `TEXT_TEMPLATE` MUST reject empty placeholders, placeholders that are not an
  entire slash-separated segment, and literal `{` or `}` characters.
- Any text pattern that can match a key beginning with `_aster/` MUST be
  rejected.

### 5.2 Bytes Patterns

`BYTES_EXACT`
: The key bytes must exactly equal `DocsKeyPattern.bytes`.

`BYTES_PREFIX`
: The key bytes must start with `DocsKeyPattern.bytes`.

`BYTES_TEMPLATE`
: The key is matched by consuming `DocsBytesTemplate.segments` in order.

Segment semantics:

|Kind|Semantics|
|----|---------|
|`LITERAL`|Consume exactly `literal.len()` bytes equal to `literal`.|
|`FIXED`|Consume exactly `width` bytes and bind them to `name`.|
|`REST`|Consume all remaining bytes and bind them to `name`. MUST be the final segment.|
|`U16BE_LEN_BYTES`|Read a 2-byte unsigned big-endian length `n`, then consume exactly `n` bytes and bind those payload bytes to `name`.|

A `BYTES_TEMPLATE` match succeeds only if all segments match and the key is
fully consumed. Partial matches are not valid.

Bytes pattern validation:

- `BYTES_EXACT` and `BYTES_PREFIX` MUST set `bytes` and MUST leave `text` empty
  and `bytes_template.segments` empty.
- `BYTES_PREFIX` MUST NOT use an empty prefix.
- `BYTES_TEMPLATE` MUST leave `text` and `bytes` empty.
- Capture names in `FIXED`, `REST`, and `U16BE_LEN_BYTES` segments MUST match
  `[A-Za-z_][A-Za-z0-9_]*` and MUST be unique within one pattern.
- `LITERAL` segments MUST set non-empty `literal`, MUST use empty `name`, and
  MUST set `width = 0`.
- `FIXED` segments MUST set `width > 0`, MUST use empty `literal`, and MUST
  set a valid capture `name`.
- `REST` segments MUST use empty `literal`, MUST set `width = 0`, MUST set a
  valid capture `name`, and MUST be the final segment. A template MUST contain
  at most one `REST` segment.
- `U16BE_LEN_BYTES` segments MUST use empty `literal`, MUST set `width = 0`,
  and MUST set a valid capture `name`. The consumed length MUST NOT exceed the
  configured per-key cap.
- A `BYTES_TEMPLATE` MUST contain at least one segment.

Examples:

```text
Object entry key:
  LITERAL 0x01
  FIXED object_id width=16

Link entry key:
  LITERAL 0x02
  FIXED parent_object_id width=16
  U16BE_LEN_BYTES name
```

### 5.3 Match Resolution

Readers evaluate every `DocsSchemaEntry` against a concrete Docs key.

- Zero matches: the key is unknown to the schema. The reader MUST ignore it
  unless the application explicitly opts into raw-key access.
- One match: decode using that entry.
- More than one match: schema violation. The reader MUST reject the value and
  surface a diagnostic naming every matching entry.

Schema publishers SHOULD order entries for human readability only. Entry order
does not define precedence.

-----

## 6. Value Encoding

The only v1 value encoding is:

```text
FORY_XLANG_ROOT_KNOWN
```

Meaning:

1. The root type is determined by matching the Docs key against the
   `DocsSchemaEntry`.
1. Writers encode the value using Fory XLANG compatible mode for that exact
   root type.
1. Readers pass the matched root type as the decode hint.
1. Payloads MUST NOT depend on producer-language native Fory mode.
1. Payloads MUST NOT require a hidden application-level type registry that is
   absent from `DocsSchema.types`.

Implementations MAY use generated Fory registration code internally. Such
registration details are codec glue, not public identity. The public identity
is the `DocsTypeRef` `(tag, type_hash)` pair.

-----

## 7. Trust Model

Docs entries are signed by their author. The author of the
`_aster/docs/schema` entry is the schema authority for that namespace.

Consumers MUST select the schema entry according to an explicit trust policy
before decoding application values.

Recommended policies:

|Policy|Use case|
|------|--------|
|`ExactAuthor(author_id)`|Multi-writer namespaces, root-authored policy, tree creator schema.|
|`LatestAnyAuthor`|Single-writer development namespaces only. Not an authorization boundary.|
|`RegistryTrustedWriter`|Future integration with registry ACLs.|

Fields inside `DocsSchema` MUST NOT be used to decide schema authority. A
malicious writer can put any `author_id` string inside the value. The only
authoritative identity is the Docs entry author metadata.

For multi-writer namespaces, applications MUST provide or derive the trusted
schema author out of band. Examples:

- A root policy namespace uses the root/control author.
- A tree manifest namespace uses the tree creator or root-authorized schema
  author.
- A shared app namespace may include the schema author in its invitation ticket.

Application value authorization remains application-specific. `DocsSchema`
describes how to decode values; it does not grant authority to accept them.

Higher-level Docs helpers and generated decoders MUST carry the source Docs
entry metadata through the decode path. A generated decoder MUST NOT return a
bare domain value in contexts where authorization decisions are expected. It
MUST return or make available at least:

- the value key,
- the value author,
- the Docs timestamp,
- the matched schema entry name,
- the decoded value.

Decoding a value never implies that the value author is trusted. Helpers MUST
name this state explicitly, for example by returning
`authorization = "not_evaluated"` or by requiring the caller to pass an
application value-author policy before receiving a trusted domain object.

Bindings MUST NOT provide a convenience path that silently reads "latest value
by any author", decodes it, and presents it as an authorized application record.
If a caller requests latest-any-author reads, the API name or return type MUST
make the trust caveat explicit.

-----

## 8. Publication Flow

When creating a discoverable Docs namespace:

1. The application registers or generates its wire types.
1. The binding walks the declared root types and transitive references.
1. The binding constructs canonical `TypeDef` bytes for every type, using the
   contract identity canonicalizer.
1. The binding builds a `DocsSchema` with key patterns and root type refs.
1. The binding validates the schema:
   - `v == 1`
   - all `type_hash` values are 32 bytes,
   - `blake3(type_def) == type_hash`,
   - all entry type refs resolve,
   - every entry root has `root = true`,
   - every key pattern is syntactically valid,
   - no entry targets `_aster/` reserved keys.
1. The application creates or opens the Docs namespace with a schema-author
   write author.
1. The binding writes `_aster/docs/schema`.
1. Only after step 7 may application values be written.
1. Only after step 8 may the namespace capability be shared or advertised.

Overwriting `_aster/docs/schema` is allowed only before the namespace has been
shared. After sharing, the immutability rule in §3.1 applies.

-----

## 9. Consumption Flow

When opening a discoverable Docs namespace:

1. Read `_aster/docs/schema` using the caller's schema trust policy.
1. Reject if no trusted schema entry exists.
1. Enforce the raw schema byte size cap before Fory decoding.
1. Decode `_aster/DocsSchema`.
1. Validate all TypeDef records and entry refs.
1. Build or look up local codec support for all root types:
   - Python and TypeScript MAY synthesize dynamic types where supported.
   - Rust SHOULD use generated code and verify fetched type hashes against the
     generated constants.
   - Java, Go, and C# MAY use generated code or runtime registration depending
     on binding capability.
1. For each application Docs entry:
   - ignore `_aster/` framework keys,
   - match the key against schema entries,
   - reject ambiguous matches,
   - decode using the matched root type,
   - carry the value author and timestamp with the decoded result,
   - apply application trust and authorization rules.

-----

## 10. Binding API Requirements

Bindings MUST expose two separable capabilities:

1. **Local type registration.** The application tells the local runtime/codegen
   which concrete types it can encode and decode.
1. **Namespace schema publication.** The application maps Docs key patterns to
   root value types and writes `_aster/docs/schema`.

### 10.1 Python

Python reuses `@wire_type`.

Example target API:

```python
@wire_type("portal.policy/NodePolicy")
@dataclass
class NodePolicy:
    state: int = 0
    label: str = ""

schema = (
    DocsSchemaBuilder("portal.policy")
    .root(NodePolicy)
    .text("/portal/v1/nodes/{node_id}", NodePolicy)
)

doc = await node.docs.create_with_schema(schema, author=root_author)
```

The builder uses the existing `build_type_graph`, `resolve_with_cycles`,
`canonical_xlang_bytes`, and `compute_type_hash` pipeline.

### 10.2 TypeScript

TypeScript reuses `@WireType` and the `aster-gen` scanner.

The generated file SHOULD include a `DOCS_SCHEMAS` export or an API for
constructing `DocsSchema` from generated `WIRE_TYPES`.

Example target API:

```ts
@WireType('portal.policy/NodePolicy')
export class NodePolicy {
  state = 0;
  label = '';
}

export const PORTAL_POLICY_SCHEMA = docsSchema('portal.policy')
  .root(NodePolicy)
  .text('/portal/v1/nodes/{node_id}', NodePolicy);
```

### 10.3 Java

Java reuses `site.aster.annotations.WireType`.

Java codegen or runtime reflection MUST produce the same `TypeDef` JSON shape
used by RPC contracts and call the Rust canonicalizer through the FFI.

### 10.4 Rust

Rust is not required to load arbitrary dynamic schemas at runtime.

The supported v1 path is code generation:

```bash
aster schema gen-rust schemas/portal-policy.fdl \
  --schema-name portal.policy \
  --out crates/portal-cas/src/generated/portal_policy_schema.rs
```

Generated Rust should provide:

```rust
PortalPolicySchema::docs_schema_bytes() -> &'static [u8]
PortalPolicySchema::install(doc, author).await
PortalPolicySchema::match_key(key: &[u8]) -> Option<RootType>
PortalPolicySchema::verify(docs_schema_bytes: &[u8]) -> Result<()>
```

An ergonomic derive may be added later:

```rust
#[derive(AsterWireType)]
#[aster(wire_type = "portal.policy/NodePolicy")]
struct NodePolicy { ... }
```

The derive/codegen output, not Fory's Rust-only reflection, is the public schema
source for cross-language interoperability.

-----

## 11. Core and FFI Surface

The Rust core is the normative implementation for validation and matching.

Required core functions:

```text
docs_schema_validate(bytes) -> DocsSchemaSummary
docs_schema_match_key(schema_bytes, key_bytes) -> MatchResult
docs_schema_match_entry(schema_bytes, key_bytes, author_id, timestamp) -> EntryMatchResult
docs_schema_type_defs(schema_bytes) -> list[(tag, type_hash, type_def_bytes, root)]
docs_schema_build_from_type_defs(schema_spec_json) -> bytes
```

Required C FFI shape, following the existing caller-owned-buffer pattern:

```c
int32_t aster_docs_schema_validate(
    const uint8_t *schema_ptr,
    uintptr_t      schema_len,
    uint8_t       *out_json_ptr,
    uintptr_t     *out_json_len
);

int32_t aster_docs_schema_match_key(
    const uint8_t *schema_ptr,
    uintptr_t      schema_len,
    const uint8_t *key_ptr,
    uintptr_t      key_len,
    uint8_t       *out_json_ptr,
    uintptr_t     *out_json_len
);

int32_t aster_docs_schema_match_entry(
    const uint8_t *schema_ptr,
    uintptr_t      schema_len,
    const uint8_t *key_ptr,
    uintptr_t      key_len,
    const uint8_t *author_id_ptr,
    uintptr_t      author_id_len,
    int64_t        timestamp_ms,
    uint8_t       *out_json_ptr,
    uintptr_t     *out_json_len
);
```

For every function with `(out_json_ptr, out_json_len)`:

- `out_json_len` is both input and output.
- On entry, `*out_json_len` is the caller-owned output buffer capacity in
  bytes.
- On success (`0`), the function writes UTF-8 JSON bytes with no trailing NUL,
  stores the number of bytes written in `*out_json_len`, and never writes past
  the input capacity.
- If the buffer is too small, the function returns `BUFFER_TOO_SMALL`, stores
  the required byte length in `*out_json_len`, and MUST NOT write a partial JSON
  value.
- A caller MAY pass `out_json_ptr = NULL` and `*out_json_len = 0` to query the
  required output size.
- Negative return values are errors and leave the output buffer contents
  unspecified.

`aster_docs_schema_match_key` returns JSON describing:

```json
{
  "matched": true,
  "entry_name": "node_policy",
  "value_type": {
    "tag": "portal.policy/NodePolicy",
    "type_hash": "..."
  },
  "encoding": "fory-xlang/root-known"
}
```

If no entry matches, `matched` is `false`. If multiple entries match, the
function returns a schema violation error rather than picking one.

`aster_docs_schema_match_key` is a low-level key matcher. It does not receive a
value author and MUST NOT be used as an authorization result. Binding-level
decode helpers MUST operate on a full Docs entry or otherwise carry the value
author and timestamp beside the match result.

`aster_docs_schema_match_entry` applies the same key matching rules, but echoes
the supplied Docs entry author and timestamp into the JSON result. It does not
authorize the value; it only makes it hard for bindings to drop entry metadata
between matching and application authorization.

-----

## 12. Security and Resource Limits

Implementations MUST enforce the raw byte cap before Fory decoding. The raw
byte cap is authoritative. Count caps are secondary sanity limits for already
decoded values; in practice the raw byte cap may reject a schema before a count
cap is reached. Inputs above these caps are invalid unless a future spec
revision defines a larger profile:

|Limit|Maximum cap|
|-----|--------------------|
|Raw `_aster/docs/schema` bytes|1 MiB|
|`DocsSchema.types` length|4,096|
|`DocsSchema.entries` length|4,096|
|Text template length|8 KiB|
|Bytes exact/prefix length|8 KiB|
|Bytes template segment count|256|

Consumers MUST validate before decoding application values:

- Reject unknown `DocsSchema.v`.
- Reject malformed Fory schema bytes.
- Reject invalid TypeDef hashes.
- Reject entry refs to missing types.
- Reject app entries whose key ambiguously matches multiple schema entries.
- Reject schema entries that target `_aster/` reserved keys.
- Reject schema replacement after namespace sharing unless explicitly running
  in development mode.
- Reject `FORY_XLANG_ROOT_KNOWN` payloads when no local codec support exists
  for the matched root type.

Schema discovery does not imply value trust. A decoded value can still be
unauthorized, stale, revoked, or semantically invalid under application rules.

-----

## 13. Portal-Sync Application Plan

Portal-sync is pre-production and may change its wire shapes in place. It should
adopt this addendum without preserving compatibility with its current Rust-only
Fory records.

### 13.1 Namespace Kinds

Portal should publish two namespace-local schemas:

```text
portal.policy
portal.tree_manifest
```

This is a split by Docs namespace semantics, not necessarily by source package.
The schemas may be generated from one source file or multiple source files.

### 13.2 Policy Namespace

The root policy namespace writes `_aster/docs/schema` as the first root-authored
entry. The schema author is the root/control author.

Example entries:

```text
TEXT_EXACT     /portal/v1/root
  -> portal.policy/RootMarker

TEXT_TEMPLATE  /portal/v1/nodes/{node_id}
  -> portal.policy/NodePolicy

TEXT_TEMPLATE  /portal/v1/trees/{tree_id}
  -> portal.policy/TreePolicy

TEXT_TEMPLATE  /portal/v1/trees/{tree_id}/quota
  -> portal.policy/TreeQuota

TEXT_TEMPLATE  /portal/v1/trees/{tree_id}/topology
  -> portal.policy/TreeTopology

TEXT_TEMPLATE  /portal/v1/trees/{tree_id}/grants/{node_id}
  -> portal.policy/TreeGrant
```

Portal's authority rule remains unchanged: policy values are accepted only from
the exact root author. The schema describes decode shape; it does not grant
authority.

### 13.3 Tree Manifest Namespace

Each tree manifest namespace writes `_aster/docs/schema` before sharing the tree
namespace capability. The trusted schema author is the tree creator or the
root-authorized tree authority.

Example entries:

```text
BYTES_TEMPLATE
  LITERAL 0x01
  FIXED object_id width=16
  -> portal.tree_manifest/ObjectEntry

BYTES_TEMPLATE
  LITERAL 0x02
  FIXED parent_object_id width=16
  U16BE_LEN_BYTES name
  -> portal.tree_manifest/LinkEntry
```

### 13.4 Required Portal Wire Shape Changes

Portal should stop exposing Rust-specific Fory details as the ABI:

- Replace tuple fields such as `Vec<(String, XattrValueWire)>` with named
  messages such as `XattrEntry { name: string, value: XattrValue }`.
- Represent object IDs and hashes as `binary` fields in schema and enforce
  exact lengths in portal validation.
- Keep discriminant-message encodings for unions if that is simpler for Rust,
  but define the discriminant messages in the schema.
- Treat Fory registration IDs, if still required internally by Rust, as
  generated codec implementation details. They are not the public contract.

-----

## 14. Future Extensions

The v1 schema declaration is self-contained. Future revisions MAY add:

- Optional immutable schema package publication to Iroh Blobs for deduplication
  across many namespaces.
- Registry indexing of known namespace kinds.
- Schema compatibility reports.
- More bytes segment kinds.
- Rich non-canonical field descriptions and tags.
- Capability-aware author policies.

None of these are required for the initial implementation.
