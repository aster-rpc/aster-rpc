# Aster Spec — Open Issues & Upstream Blockers

**Last updated:** 2026-04-04

Issues discovered while auditing Aster-SPEC.md, Aster-session-scoped-services.md,
Aster-ContractIdentity.md, and Aster-trust-spec.md against the Python
implementation plan (ASTER_PLAN.md). Editorial fixes that were resolvable locally
have been applied to the spec files directly; this document tracks items that
require **upstream action** — new content, design decisions, or artifacts that
must be produced by the reference implementation.

---

## BLOCKERS (upstream, must resolve before Phase 9 ships)

### B3. Canonical XLANG golden byte vectors missing

**Status:** ⛔ BLOCKER — Phase 9 tests cannot be written without these.

Aster-ContractIdentity.md **Appendix A** defines five fixture cases for the
canonical XLANG byte encoding:

- A.2: empty ServiceContract
- A.3: enum TypeDef
- A.4: TypeDef with TYPE_REF field
- A.5: MethodDef without `requires`
- A.6: MethodDef with `requires`

The byte payloads and expected BLAKE3 hashes are marked `<TO BE GENERATED>`.
Without reference bytes, two independent implementations will produce
different `contract_id`s and have no way to verify they agree.

**Action required (upstream):** produce the Rust reference encoder, emit
golden vectors for all five cases, and publish them in `Appendix A` of
`Aster-ContractIdentity.md`. Python implementation will consume these as
test fixtures at `tests/fixtures/canonical_test_vectors.json`.

**Workaround:** Phase 9 ships with placeholder fixtures; tests assert
byte-equality against the placeholder initially, then swap to reference
bytes when available. `aster contract gen` CLI remains consistent
intra-Python until the reference lands.

---

## WATCH items (resolve when convenient, not blocking)

### W1. QUIC direct_addresses may be relay-mediated

Aster-trust-spec.md §2.4 CIDR admission relies on the peer's observed source
IP. When an iroh connection traverses a relay, `RemoteInfo.direct_addresses`
may list the relay's IP, not the peer's. **Implication:** CIDR-based admission
silently admits peers whose actual source IP is *any* — as long as the relay
is trusted. Document the limitation; recommend direct connections only for
CIDR-gated production meshes.

### W5. `@fory_type` tag collision across packages

Aster-SPEC.md §5.3.1 reserves `_aster/*` tags for framework use but does not
prevent two independent application packages from declaring the same tag
(e.g. two services both tagging `my_org/User`). Behavior on collision is
implementation-defined (first-wins, last-wins, or error). **Action:** decide
and document.

### W13. LocalTransport peer identity

Aster-SPEC.md §9.1 declares `CallContext.peer: EndpointId | None`. On
`LocalTransport` there is no real remote peer. Interceptors (`AuthInterceptor`
in particular) that dereference `peer` unconditionally will crash locally.
**Action:** specify whether LocalTransport synthesizes a placeholder peer, or
require interceptors to handle `peer is None`.

### W17. ASCII-only method names

Aster-ContractIdentity.md §11.3.3 mandates ASCII lexicographic sort for
`ServiceContract.methods`. Python, JVM, and .NET all permit Unicode
identifiers. **Action:** explicitly forbid non-ASCII method names in contracts,
or define a normalization scheme (NFC + codepoint sort).

### W18. Depart replay window tracking

Aster-trust-spec.md §2.6 notes `Depart` replay as a "genuine problem" and
suggests tracking `(sender, epoch_ms)` pairs but leaves it implementation-
defined. Two implementations will have different eviction behaviour for
replayed Departs. **Action:** specify a bounded replay-cache retention
policy.

### W20. Nonce length validation in OTT credentials

Aster-trust-spec.md §3.1 specifies 32-byte nonces but doesn't mandate
validation. An implementation that accepts variable-length nonces is
wire-incompatible with one that rejects. **Action:** state MUST reject
credentials with `len(nonce) != 32`.

### W22. ContractManifest.published_by is not in contract.xlang

Aster-ContractIdentity.md §11.4.4: `ContractManifest.published_by` is in
`manifest.json` (collection index 0), **not** in `contract.xlang` (index 1).
Two publishers can therefore publish the same `contract_id` with different
`published_by` values. If consumers trust `published_by` for provenance,
they get ambiguous answers. **Action:** document that `published_by` is a
bundle-level attribute, not an identity-level attribute.

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

The Rust reference implementation is a dependency for Phase 9 sign-off. Once
reference bytes land, Python's canonical byte tests flip from
`expected = <our own output>` to `expected = <reference bytes>`. If they
don't match on flip, the Python canonical encoder has a bug — fix before
shipping Phase 9.
