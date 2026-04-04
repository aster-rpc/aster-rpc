# Aster Spec — Open Issues & Upstream Blockers

**Last updated:** 2026-04-04

Issues discovered while auditing Aster-SPEC.md, Aster-session-scoped-services.md,
Aster-ContractIdentity.md, and Aster-trust-spec.md against the Python
implementation plan (ASTER_PLAN.md). Editorial fixes that were resolvable locally
have been applied to the spec files directly; this document tracks items that
require **upstream action** — new content, design decisions, or artifacts that
must be produced by the reference implementation.

---

## Phase 9 Bootstrap — Python is the Reference Implementation

### B3. Canonical XLANG golden byte vectors (bootstrap path)

**Status:** Phase 9 produces the first set of canonical vectors. Python is
the reference implementation; the Java implementation (future) will be the
first cross-verification.

Aster-ContractIdentity.md **Appendix A** defines five fixture cases for the
canonical XLANG byte encoding:

- A.2: empty ServiceContract
- A.3: enum TypeDef
- A.4: TypeDef with TYPE_REF field
- A.5: MethodDef without `requires`
- A.6: MethodDef with `requires`

The byte payloads and expected BLAKE3 hashes are marked `<TO BE GENERATED>`
because no implementation has produced them yet. Python's Phase 9 work will
fill them in.

**Bootstrap protocol:**

1. **Phase 9 produces candidate vectors.** The Python canonical encoder is
   built against `Aster-ContractIdentity.md §11.3.2` rules. Vectors are
   generated from Appendix A fixture inputs + additional rule-level
   micro-fixtures (see below), committed to
   `tests/fixtures/canonical_test_vectors.json`, and copied into
   `Aster-ContractIdentity.md` Appendix A as **"Python-reference v1"**.
2. **Java implementation verifies.** When the Java binding is built, its
   canonical encoder is written fresh against the spec (not ported from
   Python). It runs against the same fixture inputs and compares byte
   outputs.
3. **Discrepancies are resolved by spec re-reading.** If Python and Java
   disagree on any vector, neither side "wins" automatically — the
   disagreement localizes a §11.3.2 rule the spec under-specified, and we
   tighten the spec until both implementations agree on a re-read.
4. **Vectors marked "stable" after two-implementation agreement.**

**Risk mitigation — per-rule micro-fixtures.** Because a subtle bug in
Python could become locked-in as "the spec" via the composite fixtures,
Phase 9 also produces **one micro-fixture per §11.3.2 rule**:

- ZigZag VARINT32 edge cases: 0, 1, -1, INT32_MAX, INT32_MIN
- ZigZag VARINT64 edge cases: same
- Varint boundaries: 0x7F (1 byte), 0x80 (2 bytes), 0x3FFF (2 bytes), 0x4000 (3 bytes)
- Empty string vs absent string (`""` vs NULL_FLAG)
- Empty bytes vs absent bytes
- Empty list (header `0x0C 0x00`) vs absent list
- NULL_FLAG byte value + position for each optional `CapabilityRequirement` field
- Zero-value conventions for each `TypeKind` discriminator (PRIMITIVE, REF, SELF_REF, ANY)
- Sort stability for `ServiceContract.methods` with ASCII-tied names
- `scoped` field distinctness (SHARED vs STREAM produce different bytes)

If Java disagrees on a composite fixture, the micro-fixtures pinpoint which
rule is at issue.

**No "upstream" blocker.** Phase 9 is free to start immediately. The vectors
produced by Phase 9 ARE the spec's reference — they will land in
`Aster-ContractIdentity.md` Appendix A directly, with a cross-verification
note pending Java work.

---

## WATCH items (resolve when convenient, not blocking)

### ~~W1. QUIC direct_addresses may be relay-mediated~~ ✅ Resolved 2026-04-04

**Resolution: removed CIDR filtering from the normative spec entirely.**

Rationale: Iroh's transport model (relay-mediated paths, hole-punching,
multi-homing) makes source-IP-based admission fundamentally unreliable.
`RemoteInfo.direct_addresses` reflects whichever path Iroh chose, and can
surface the relay's IP rather than the peer's. Rather than paper over this
with post-handshake checks or flaky "best effort" guards, the spec now
declares **network-level controls out of scope**. Operators who need them
enforce at the network boundary (VPN, firewall, NetworkPolicy) or via
application-namespaced custom attributes (`app.allowed_cidrs`) with their
own runtime interpretation — the framework does not define semantics for
such attributes.

Changes applied:
- `aster.allowed_cidrs` removed from Aster-trust-spec.md §2.2 reserved
  attributes
- Runtime CIDR check removed from §2.4 and §3.2 admission flows
- New §1.2 note "Network-level controls are out of scope" added
- Phase 11 (ASTER_PLAN.md §13) and checklist items simplified — no CIDR
  matching code, no FFI dependency on post-handshake source IP

### ~~W5. `@fory_type` tag collision across packages~~ ✅ Resolved 2026-04-04

**Resolution: no collision-resolution policy is needed — two-layer natural defense.**

Analysis showed the concern was theoretical. Collisions are caught by:

1. **Fory registration (within a process):** the local `ForyCodec`
   registry rejects duplicate tag registrations with a fail-fast error.
   Developers see the conflict at startup.
2. **`contract_id` routing (across processes):** two services using the
   same tag string but different type structures produce different
   `type_hash`es → different `contract_id`s → the registry treats them
   as distinct services. They never connect, so there is no silent
   structural mismatch.

Genuinely-identical types (same tag + same canonical bytes) collapse to
the same `contract_id`, which is the correct behavior for a
content-addressed system.

Aster-SPEC.md §5.3.1 has been updated with explicit guidance: implementations
MUST fail at registration time on intra-process duplicates, MUST NOT
silently discard one of two registrations, and MUST NOT attempt
cross-process deduplication by tag alone (tags are hints, `contract_id` is
identity).

### ~~W13. LocalTransport peer identity~~ ✅ Resolved 2026-04-04

**Resolution: LocalTransport has no remote peer; `CallContext.peer` is
always `None`; Gates 0 and 1 are bypassed by construction.**

LocalTransport runs in a single process — there is no iroh connection to
gate, no credential to verify, and no security boundary inside the process.
Aster's gate model applies only to remote network calls. Trust semantics on
LocalTransport are "caller == callee == same process == fully trusted."

**Rules adopted:**
- `CallContext.peer` is `None` on every LocalTransport call (not a
  synthesized placeholder — absence of peer is part of the data model).
- `CallContext.attributes` is `{}` unless a test harness explicitly
  populates it.
- Gate 0 (connection-level admission via `EndpointHooks`) and Gate 1
  (credential verification) are bypassed — they are hooks on iroh
  connections, and there is no connection.
- Interceptors MUST handle `peer is None` gracefully. The canonical
  behavior is "allow" (in-process callers are trusted). Interceptors that
  genuinely require an authenticated remote identity MUST document that
  they don't work on LocalTransport and either fail fast with a clear
  error or be excluded from local chains by the test harness.
- Test harnesses MAY synthesize a `peer` (e.g. `peer="test://alice"`) to
  exercise auth-interceptor logic. This is test-scoped; production code
  paths must not rely on it.

Changes applied:
- Aster-SPEC.md §8.3.2 gains a "Trust model and `CallContext.peer`"
  subsection
- Aster-trust-spec.md §1.3 Gate Model gains a scope note: Gates 0 and 1
  apply only to remote calls
- ASTER_PLAN.md §5.3 (Phase 3 LocalTransport) documents the trust model
- ASTER_PLAN.md §13.1 (Phase 11 Trust Model) adds the scope note
- ASTER_PLAN_CHECKLIST.md Phase 11 tests: `peer is None` on LocalTransport,
  auth interceptor "allow" semantics

### ~~W17. ASCII-only method names~~ ✅ Resolved 2026-04-04

**Resolution: permit Unicode identifiers (UAX #31) with NFC normalization + codepoint sort.**

Restricting identifiers to ASCII excluded non-English developers and
conflicted with every target language's own identifier rules. All target
languages (Python, Java, Go, Rust, C#, JavaScript) already support Unicode
identifiers per UAX #31, so aligning the spec with language-native rules is
both inclusive and pragmatic.

**Rules adopted:**
- Method names, type names, package names, enum/union member names, and
  role names MUST be valid identifiers per **UAX #31** (`XID_Start` +
  `XID_Continue` — the rule Python's `str.isidentifier()` implements).
- Canonical form is **Unicode NFC** (Normalization Form C) — resolves
  `café` (NFC, 4 codepoints) vs `café` (NFD, 5 codepoints) to a single
  deterministic byte sequence before hashing. NFC is chosen because it is
  the form used by the web platform and most filesystems.
- Canonical sort is by **Unicode codepoint on the NFC-normalized string** —
  deterministic, language-agnostic, no locale-sensitive collation.
- Wire format is already UTF-8; no wire change.
- **Security note:** implementations SHOULD warn (not fail) on identifiers
  that mix Unicode scripts (Latin + Cyrillic, etc.) or contain confusables.
  `contract_id` already prevents structural confusion between distinct
  services; the warning is an operator-usability safeguard.
- The framework's own `_aster/*` reserved tag prefix remains ASCII — those
  are framework identifiers, not user-facing.

Changes applied:
- `Aster-SPEC.md` §11.3 canonicalization step 3 rewritten: NFC + UAX #31
  normalization
- `Aster-SPEC.md` §11.3 canonicalization step 4 rewritten: codepoint sort
  (not "lexicographic")
- `Aster-ContractIdentity.md` §11.3.2.2 gains two new bullets: NFC
  normalization for identifiers, UAX #31 validation, mixed-script warning
- `Aster-ContractIdentity.md` §11.3.3 `ServiceContract.methods` sort comment
  updated
- `Aster-ContractIdentity.md` §11.3.4 cycle-breaking spanning tree now uses
  "codepoint order" terminology; walk-through examples updated
- `ASTER_PLAN.md` Phase 9 sort-order + writer documentation updated
- `ASTER_PLAN_CHECKLIST.md` Phase 9: added `normalize_identifier()` helper,
  added mixed-script warning helper, added NFC and Japanese-identifier tests

### ~~W18. Depart replay window tracking~~ ✅ Resolved 2026-04-04

**Resolution: no replay cache is required — replay is mitigated by threat
model + salt rotation.**

An in-band replay cache was considered and rejected. The reasoning: any
attacker capable of injecting replays onto the gossip channel has already
either passed admission legitimately (they are an authorized producer) OR
compromised an admitted producer's local state (salt + signing key). In
the first case the mesh is intentionally admitting them; in the second
case they can forge arbitrary new messages, not just replay old ones, so
a cache adds nothing. In both cases the canonical recovery is **salt
rotation** (§2.3) — operator distributes a new salt out of band, mesh
migrates to a new gossip topic, compromised node is locked out.

The ±30-second acceptance window bounds the blast radius of a pure-replay
attacker to ~30s of transient disruption per captured message (then the
message can no longer be replayed). The victim re-Introduces, the mesh
self-heals. This is acceptable degradation for an attacker who is, by
assumption, already on the channel.

The spec text in §2.6 has been tightened to:
- Document salt rotation as the recovery mechanism
- State explicitly that an in-band replay cache is optional (defence in
  depth, not a substitute for salt rotation)
- Note that compromising a producer likely yields its signing key too,
  so replay-only attacks are a narrow threat envelope not worth dedicated
  machinery

No changes to ASTER_PLAN.md or checklist — Phase 12's existing ±30s replay
window check (the `replay_window_ms` config + drop-silently handler) is
sufficient.

### ~~W20. Nonce length validation in OTT credentials~~ ✅ Resolved 2026-04-04

**Resolution: §3.1 now mandates 32-byte exact rejection.**

Trust-spec §3.1 gains a "Nonce length is normative" paragraph with three
MUST rules:
1. OTT credentials MUST carry exactly 32 bytes of nonce.
2. Admitting nodes MUST reject any OTT credential with
   `len(nonce) != 32` as malformed.
3. Policy credentials MUST NOT carry a `nonce` field; a Policy credential
   with a nonce MUST be rejected as malformed.

Also added guidance on secure random sources (`secrets.token_bytes(32)` /
`crypto/rand` / `SecureRandom`). Added 2 test items to Phase 11 checklist
covering `len(nonce) != 32` rejection and Policy-with-nonce rejection.

### ~~W22. ContractManifest.published_by is not in contract.xlang~~ ✅ Resolved 2026-04-04

**Resolution: §11.4.4 now explicitly documents `published_by` as bundle-level.**

Aster-ContractIdentity.md §11.4.4 gains three paragraphs clarifying:

1. `contract_id` content-addresses the contract *definition*, not the
   publisher. Identical canonical bytes → identical identity, regardless
   of who packaged the bundle.
2. Consumers that need to trust a specific publisher for a specific
   contract MUST combine `contract_id` with the registry ACL (Aster-SPEC.md
   §11.2.3). The registry ACL is what actually gates which AuthorIds can
   write `ArtifactRef`s to `contracts/{contract_id}`.
3. `published_by` is useful for audit trails and operational telemetry but
   is NOT a routing input and MUST NOT be used as an authorization hint.

This prevents consumers from inadvertently treating bundle metadata as
identity provenance.

---

## RESOLVED locally (editorial fixes applied to spec files)

| # | Ref | Resolution |
|---|-----|-----------|
| B1 | trust-spec §1 missing | Added §1 "Trust Foundations" to Aster-trust-spec.md (threat model, trust anchors, gate model) |
| B2 | SPEC §11.3 TODO conflict | Removed TODO; cross-referenced ContractIdentity §11.3 as normative |
| S1 | session discriminator | Added normative server validation for `method`/`scoped` mismatch in SPEC §6.2 |
| S3/S8 | trailer semantics conflict | Cross-referenced session addendum rules in SPEC §6.3; client-origin TRAILER now explicitly permitted on session streams |
| S4 | lease_seq tiebreak | Defined `(lease_seq, updated_at_epoch_ms, AuthorId_hex)` lex tiebreak in SPEC §11.2.3 |
| S5 | serialization preference | Added §6.2.1 selection algorithm (client picks first producer-listed mode it also supports) |
| S6 | session_id mapping | Documented `CallContext.session_id = StreamHeader.call_id` in SPEC §6.2 |
| S7 | CANCEL frame race | Clarified in addendum §5.4/§5.5: CANCELLED trailer is authoritative; client discards earlier trailers |
| S10 | drift vs deadline skew | Added startup warning requirement in trust-spec §2.10 for misconfigured tolerances |
| S11 | replay vs drift precedence | Specified evaluation order in trust-spec §2.10 (replay → drift; isolated peers: ContractPublished/LeaseUpdate skipped, membership still applied) |
| S12 | OTT nonce per-node footgun | Added §3.2.1 "OTT Nonce Store Scope" to trust-spec documenting single-endpoint vs mesh-gossiped consumption and the known-limitation of local stores |

---

## Process note

Python is the first implementation of the Aster RPC framework. Its Phase 9
canonical encoder produces the reference vectors for `contract_id` /
`type_hash` determinism. Cross-verification arrives when the Java binding
is written (fresh implementation, not a port). Until then, vectors are
marked **"Python-reference v1, pending cross-verification"** in
`Aster-ContractIdentity.md` Appendix A.

Rigour discipline: every §11.3.2 rule gets a micro-fixture test independently
of the composite Appendix A fixtures. This localizes any future Java ↔ Python
disagreement to a specific rule rather than the full encoder.
