# Aster: Adoption Improvements

**Status:** Staging draft — pending integration into main spec  
**Relates to spec version:** 0.7.1  
**Resolves open questions:** §16.2 items 6, 12, 13

This document captures resolved design improvements across three adoption-blocking
problem areas: **observability**, **versioning and discovery**, and **operational
resilience**. Each section is self-contained. Items are ordered by the section they
will eventually update in the main spec.

---

## 1. Observability (resolves §16.2 Q12)

### 1.1 The Problem

Aster traffic is opaque to standard L7 infrastructure. QUIC is E2E encrypted, so
sidecar interception of the kind Envoy or Istio provide for HTTP/2 is not
architecturally possible. An SRE who cannot see request rate, error rate, and
latency on their existing dashboard will veto adoption. Aster cannot wait for
ecosystem tooling to emerge; it must ship with this solved.

### 1.2 Resolution: Mandatory OpenTelemetry Integration

OpenTelemetry (OTel) is promoted from Phase 2 optional to **Phase 2 mandatory**.
The framework emits OTel metrics and traces in-process. A companion HTTP server
(`aster-metrics`) exposes `/metrics` in Prometheus text format and `/health` as a
JSON liveness probe. No sidecar is required. Existing Prometheus/Grafana stacks
work without modification.

The companion server is a thin adapter over the OTel SDK — not a separate process.
It runs in the same process as the Aster node and shares its in-process metric
registry.

### 1.3 Canonical Metric Names

All Aster language implementations must emit metrics using the following names and
label sets. A conformance test validates this. Implementations adding
language-specific metrics must use a namespaced prefix (`aster_{lang}_*`) and
must not reuse the canonical names for different semantics.

| Metric | Type | Labels |
|--------|------|--------|
| `aster_rpc_calls_total` | Counter | `service`, `method`, `status` |
| `aster_rpc_duration_seconds` | Histogram | `service`, `method` |
| `aster_active_streams` | Gauge | `service` |
| `aster_connection_errors_total` | Counter | `reason` |
| `aster_registry_sync_lag_seconds` | Gauge | — |
| `aster_rcan_rejections_total` | Counter | `service`, `method` |
| `aster_endpoint_lease_renewals_total` | Counter | `service` |
| `aster_compat_prefetch_total` | Counter | `service`, `result` |

The `status` label on `aster_rpc_calls_total` uses the Aster status code string
(`OK`, `PERMISSION_DENIED`, etc.) from §6.5, not an HTTP status code.

### 1.4 Load Balancing Boundary

Aster does not require L7 load balancers in the call path. Endpoint selection is
performed by the client framework via the resolution and selection logic in §11.9.
The appropriate integration point for teams that route traffic through an L7 proxy
is the Web Gateway pattern (§12). This boundary should be stated plainly in §2.5
rather than left implicit.

---

## 2. Versioning and Discovery

### 2.1 The Problem

Two distinct problems exist today:

**Discovery by hash is operationally brittle.** Any field addition changes the
`contract_id`, making a service appear to vanish for clients that pinned the old
hash. This forces teams into a high-frequency versioning model for changes that
are wire-compatible.

**The `versions/v{version}` path is not SemVer-native.** The current flat
`/versions/` path requires the client to fetch and parse every manifest to
determine ordering, and offers no support for range resolution.

### 2.2 SemVer Release Path

Replace `services/{name}/versions/v{version}` with a three-segment release path:

```
services/{name}/releases/{major}/{minor}/{patch}  →  contract_id
```

The `semver` field in `ContractManifest` (§11.4.4) becomes **normative**. The
integer `version: int32` field on `@service` is derived from the semver major
component and retained only for wire compatibility. The registry path segments are
derived from the parsed semver string by the `aster publish` toolchain.

**Resolution by range.** When a client specifies a SemVer range (e.g., `@^1.2.0`),
the registry client performs a prefix scan over `releases/{major}/` to retrieve all
matching keys, then performs SemVer comparison **in local memory** using a standard
SemVer library. Sorting is never delegated to the key space — iroh-docs key order
is lexicographic and would misorder `1.9.0` ahead of `1.10.0`. Because iroh-docs is
a local CRDT store, the prefix scan is a fast in-memory operation.

### 2.3 Three-Tier Client Resolution

When a client dials a service, it follows a strict priority order:

**Tier 1 — Exact contract_id (hex string)**  
Zero ambiguity. The client resolves `contracts/{contract_id}` directly. No version
or channel lookup occurs. Appropriate for pinned deployments and conformance tests.

**Tier 2 — Channel alias (`@stable`, `@canary`, `@dev`)**  
The client reads `services/{name}/channels/{channel}` to obtain the current
`contract_id`, then proceeds as Tier 1. Appropriate for CI/CD pipelines and
production service mesh configuration.

**Tier 3 — SemVer range (`@^1.2.0`, `@1.x`, `@>=2.0.0 <3.0.0`)**  
The client performs a prefix scan over `releases/{major}/` (or a tighter prefix
if the range constrains minor), applies SemVer range logic in memory, and selects
the highest matching release. Appropriate for development and integration testing.

These tiers are mutually exclusive per dial call. The client does not fall through
from one tier to the next on failure; a failed resolution is an error.

### 2.4 The Administrative Model

The registry enforces a clear boundary between **artifact publication** (any
authorized writer) and **release promotion** (admin only). This mirrors `git push`
vs. `git tag -s` in everyday terms.

| Registry Path | Write Requirement |
|---|---|
| `contracts/{contract_id}` | Writer AuthorId |
| `services/{name}/releases/{major}/{minor}/{patch}` | Writer AuthorId (written by `aster publish`) |
| `services/{name}/channels/canary` | Writer AuthorId (auto-updated by `aster publish`) |
| `services/{name}/channels/stable` | **Admin AuthorId only** |
| `compatibility/{new_id}/{old_id}` | Toolchain (written by `aster publish`) |

The iroh-docs `_aster/acl/` entries already track `writers`, `admins`, and
`readers` AuthorId lists (§11.2). Enforcement is read-side: a registry client
that receives a write to `channels/stable` from a non-Admin AuthorId rejects the
entry and emits a security alert. This is consistent with the existing ACL model.

**Promotion workflow:**

```
# Developer: publish a new version (Writer)
aster publish AgentControlService --semver 1.3.0

# Writes:
#   contracts/{new_contract_id}
#   services/AgentControl/releases/1/3/0  →  new_contract_id
#   services/AgentControl/channels/canary  →  new_contract_id
#   compatibility/{new_id}/{old_stable_id}  →  CompatibilityReport

# Admin: promote to stable after validation (Admin)
aster promote AgentControl@1.3.0 --channel stable

# Writes:
#   services/AgentControl/channels/stable  →  new_contract_id
```

The promotion step requires the Admin's node key to be used directly — there is
no API token substitution for stable promotion.

---

## 3. Compatibility

### 3.1 The Problem

Adding a non-required field to a message type changes the `contract_id`. Under the
current model, this forces every client to be updated to the new hash
simultaneously, even though the change is wire-compatible when Fory `compatible=True`
is active.

The naive fix — allowing publishers to self-report compatibility via a
`parent_contract_id` field in the manifest — is rejected. Self-reported
compatibility is unverified and creates a false safety guarantee. A consumer that
auto-upgrades based on an unchecked self-assertion is in a worse position than one
that requires an exact hash match.

### 3.2 Toolchain-Driven Compatibility Reports

Compatibility is verified by the **toolchain at publish time**, not asserted by the
publisher. The `compatibility/{new_id}/{old_id}` table (§11.2) is the audit log.

`aster publish` performs this automatically whenever a previous contract exists
for the same service:

1. Resolve the current `channels/stable` pointer to `contract_id` A (the baseline).
2. Compute the new `contract_id` B from the local service definition.
3. Fetch contract bundle A from the local blob store (or the registry).
4. Run a structural compatibility check: compare TypeDef graphs field by field.
5. Emit a `CompatibilityReport` and publish it to `compatibility/B/A`.
6. Proceed with contract and release publication.

Step 6 proceeds regardless of the report result — a breaking change is not
blocked at publish time, it is surfaced to the client at resolution time. The
decision to block breaking changes on `stable` belongs to the Admin promotion
gate (§2.4), not the toolchain.

### 3.3 CompatibilityReport Schema

```
CompatibilityReport {
    from_id:              string           // baseline contract_id (A)
    to_id:                string           // new contract_id (B)
    result:               enum {
                              Compatible,      // safe for clients of A to connect to B servers
                              BreakingChange,  // at least one incompatibility found
                              Unknown          // toolchain could not determine
                          }
    breaking:             list<FieldDiff>  // empty if Compatible
    generated_by:         string           // "aster/{version}"
    generated_at_epoch_ms: int64
}

FieldDiff {
    path:   string   // dotted path to the changed element, e.g. "TaskAssignment.priority"
    kind:   enum { FieldRemoved, TypeChanged, RequiredAdded, MethodRemoved, MethodSignatureChanged }
    detail: string   // human-readable description of the change
}
```

The report is deterministic: the same A and B always produce the same report.
Re-publishing is idempotent.

### 3.4 The `@latest-compatible` Resolution

A client may specify `@latest-compatible` as a channel alias. Resolution:

1. Follow `channels/stable` → baseline `contract_id` A.
2. Scan `compatibility/*/A` — all entries whose `old_id` is A.
3. Filter entries where `result == Compatible`.
4. For each compatible `contract_id` B, look up its SemVer from the
   `releases/` path (a reverse lookup: scan `releases/` for keys pointing to B).
5. Select the highest SemVer B.
6. If no compatible entries exist beyond A itself, resolve to A.

This resolution is a SHOULD for clients, not a MUST. Clients that require strict
hash pinning may always use Tier 1 (exact `contract_id`) resolution and ignore
this mechanism entirely.

### 3.5 Background Prefetch

When the registry client receives a `CONTRACT_PUBLISHED` gossip event for a service
it has previously resolved, it SHOULD prefetch the new contract bundle in the
background if a compatibility report confirms the new contract is compatible with a
locally cached version:

```
on CONTRACT_PUBLISHED(service, new_contract_id, semver_hint):
    for each cached_id in local_blob_store[service]:
        report = registry.lookup(compatibility/new_contract_id/cached_id)
        if report.result == Compatible:
            background_fetch(new_contract_id)   // no call-path involvement
```

This prefetch happens before any client attempts to connect to a server running the
new contract, eliminating the on-demand fetch latency at first connection.

**Interceptors must not initiate registry operations.** Interceptors may read from
a registry cache that is already populated, but must not trigger blob fetches or
registry writes. An interceptor that detects a new `contract_id` on the wire should
emit a metric (`aster_contract_skew_detected_total`) and return the call result
normally. The registry background task handles the rest.

---

## 4. Operational Resilience

### 4.1 The Problem

Two operational gaps exist in the trust spec (Aster-trust-spec.md):

**Founding node fragility.** The salt that seeds the producer gossip topic is
generated by the founding node. If the founding node goes down before it has
admitted a second peer, the salt is lost and the mesh cannot grow.

**Salt rotation at scale.** §2.8 describes recovery as distributing a new salt
"out of band" to trusted nodes. At 1,000 nodes in a cloud failure scenario, this
is not a feasible operational procedure.

### 4.2 Founding Node Guidance

The founding node criticality window is the time between first startup and first
successful peer admission. Once any peer holds the salt (which it persists to local
storage on admission), the founding node can go down without consequence.

Operators MUST admit at least two peers before considering a mesh stable. The
`aster node start` command SHOULD warn if the node has been the sole mesh member
for more than a configurable interval (default: 60 seconds).

This is an operational requirement, not a protocol change.

### 4.3 Salt Rotation as a Protocol Operation

Salt rotation is formalized as an authenticated point-to-point Aster RPC, not a
manual procedure. The rotation is never broadcast over gossip — doing so would
deliver the new salt to the compromised node before it is excluded.

**CLI:**

```
aster node rotate-salt \
    --root-key ./root.key \
    --exclude <endpoint_id>[,<endpoint_id>...]
```

**Procedure:**

1. Generate a new random 32-byte salt.
2. Enumerate all trusted peers from the current persisted membership set, minus
   excluded endpoints.
3. Dial each trusted peer over Aster using the root key's EndpointId as caller
   identity.
4. Deliver a `SaltRotation` payload, signed by the root key:
   ```
   SaltRotation {
       new_salt:           bytes[32]
       effective_at_epoch_ms: int64   // grace period: both old and new topics valid until this time
       excluded:           list<EndpointId>
       signature:          binary     // root_key signs (new_salt || effective_at_epoch_ms || excluded)
   }
   ```
5. Each recipient verifies the signature against the root public key in its
   enrollment credential, updates its persisted salt, and acknowledges.
6. The command reports success and failure per peer, retrying failures with
   exponential backoff up to a configurable limit.

**Grace period.** During the window between delivery and `effective_at_epoch_ms`,
nodes accept both the old and new gossip topic. This prevents temporarily
unreachable nodes (transient network partition) from being permanently stranded by
a rotation.

The compromised node is not contacted. It holds the old salt, cannot derive the
new gossip topic, and falls off the mesh at `effective_at_epoch_ms`. No
cryptographic revocation mechanism is required.

---

## 5. Canonical Encoding Conformance (resolves §16.2 Q6 partial)

The canonical XLANG encoding used to derive `contract_id` is a conformance surface.
A discrepancy in canonical encoding between two language implementations produces
hash mismatch, which is a silent correctness failure — clients and servers silently
fail to find each other without any error that names the real cause.

**Test vectors are required.** Appendix D (to be added to the main spec) will
define a set of `TypeDef` and `ServiceContract` structures with their expected
BLAKE3 hashes. Every language implementation must produce matching hashes for all
test vectors before it is considered conformant. Cross-language conformance tests
must include hash equality assertions, not only round-trip serialization tests.

The `canonical_encoding` field in `ContractManifest` (e.g., `"fory-xlang/0.15"`)
is the version pin. A change to the canonical encoding rules requires a new version
string and produces different hashes — this is intentional and correct. Old and new
encoding versions are treated as disjoint contract spaces.

---

## 6. Open Questions Resolved by This Document

| Q# | Question | Resolution |
|----|----------|-----------|
| 6 | Schema compatibility checking | Toolchain-driven at publish time; CompatibilityReport schema defined in §3.3 above |
| 12 | OTel span and metric schema | Canonical metric names defined in §1.3 above; OTel promoted to Phase 2 mandatory |
| 13 | Channel promotion rules | Admin-only writes to `channels/stable`; compatibility report SHOULD be present as precondition; defined in §2.4 above |

---

## 7. Items Not Adopted and Why

**`parent_contract_id` in ContractManifest.** Rejected. Self-reported compatibility
is unverified. The toolchain-generated `CompatibilityReport` in the `compatibility/`
table serves the same purpose with a verifiable audit trail.

**Compatibility Epochs as a named concept.** Rejected. The `compatibility/` table
already encodes epoch membership implicitly — every `Compatible` entry from B to A
makes B a member of A's compatibility set. A named epoch abstraction adds a new
concept without adding expressive power, and raises questions about epoch identity
and branching that the table model sidesteps cleanly.

**Auto-update interceptor.** Rejected. Interceptors live on the call path; registry
operations do not belong there. An interceptor that triggers a blob fetch introduces
non-deterministic call latency and hidden failure modes. The gossip-triggered
background prefetch (§3.5) achieves the same outcome — contracts are cached before
clients need them — without call-path involvement.

**Zero-padded SemVer keys (`releases/0001/0002/0003`).** Rejected. Lexicographic
key sorting is not SemVer sorting. Constraining version numbers to four-digit
segments is an ugly workaround. In-memory sorting after a prefix scan is simpler,
correct, and has no version number constraints.

---

## 8. Monetisation Strategy

This section records the rationale behind the open/closed boundary and the
commercial product strategy. It is not normative — it does not constrain
implementations — but it informs which parts of the spec are intentionally left
as extension points and why.

### 8.1 Why the Registry Is Not a SaaS Hosting Play

The registry is not a service to be hosted — it is the mesh itself. Every
participant node replicates registry state via iroh-docs (CRDT), iroh-gossip
(change notifications), and iroh-blobs (immutable contract bundles). There is no
registry server to run, any more than there is a Git server embedded in the Git
protocol. A "managed registry" offering would mean operating bootstrap
infrastructure and sync peers, not controlling data.

The correct analogy is Git and GitHub. GitHub did not win by hosting Git — it won
by building the coordination layer, the social layer, and the developer experience
around the decentralised protocol. The protocol stayed fully open. The business
was the ecosystem.

### 8.2 What Is Actually Centralised

Exactly one element of Aster is structurally centralised: **the root key**.

All trust flows from credentials signed by the offline root private key. Managing
this key securely — HSM custody, key ceremony procedures, offline storage, audit
logging, credential issuance on demand — is genuinely hard and genuinely
expensive to do well. This is the natural anchor for a commercial product.

**Aster Trust Authority** is a managed root key custody service. It holds the
root private key in an HSM, runs the credential issuance workflow, tracks expiry,
and provides the operational tooling (key ceremony, recovery procedures, audit
exports) that enterprises need but do not want to own. Customers pay for the
service and the operational guarantees, not for opaque code in their trust path.

### 8.3 Enterprise Gateway (OIDC/SAML2)

The trust spec defines `aster.role = gateway` as a first-class role. The
enterprise gateway pattern uses this:

1. The gateway node holds one EnrollmentCredential — minted offline by the root
   key, once, as a standard node admission.
2. External users authenticate to the gateway via enterprise IdP (OIDC/SAML2).
3. The gateway proxies calls into the mesh, forwarding the user's identity as
   a signed token presented to Gate 2 (`Authorize`).
4. Each service's `Authorize` handler validates the token and mints an rcan with
   the appropriate capabilities.

Individual user identity never touches EnrollmentCredentials. The offline root
key mints exactly one gateway credential. All user identity management stays in
the OIDC layer where enterprise security teams expect it.

The enterprise gateway product bundles:

- OIDC/SAML2 integration with major IdPs (Okta, Azure AD, Google Workspace)
- Session management and token refresh
- Per-service RBAC derived from IdP group claims
- Audit log of all calls routed through the gateway
- Web dashboard for access policy management and real-time traffic visibility
- SSO-controlled `channels/stable` promotion workflows

This is buildable today against the spec as written. No spec changes are required
for the gateway-as-proxy model.

### 8.4 Spec-Compliant Commercial Routing Extensions

§11.9 defines three endpoint selection strategies that all implementations MUST
provide: `round_robin`, `least_load`, and `random`. It does not prohibit
additional strategies. The `load` field on `EndpointLease` is opaque at the
protocol level. The `metadata` map is application-defined.

A commercial client implementation adds spec-compliant strategies on top of the
mandatory three:

| Strategy | Description |
|---|---|
| `session_affinity` | Remembers the `endpoint_id` holding an open session; bypasses §11.9 selection for reconnects to the same session. Required for session-scoped services at scale. |
| `consistent_hash` | Routes by a caller-supplied key (user ID, tenant ID, etc.) to the same producer without explicit session tracking. |
| `locality_aware` | Incorporates latency and topology signals (resolving open question Q18) to prefer nearby producers. |
| Circuit breakers | Not in the spec; a pure commercial addition. Removes failing endpoints from the candidate set faster than lease expiry. |
| Hedged requests | Fires to two endpoints simultaneously; takes the first response and cancels the second. Reduces tail latency under load. |
| Adaptive load | Uses proprietary `metadata` fields that the commercial client and server negotiate out-of-band for richer load signals than the single `load` float. |

Every one of these strategies is spec-compliant. A community implementation
interoperates fully because the protocol and wire format are identical. The
commercial client makes better routing decisions about which endpoint to dial.

This is the gRPC/Envoy split — the protocol is fully open, the intelligent
routing layer is the product.

### 8.5 Licensing Boundary

| Layer | License | Rationale |
|---|---|---|
| Transport runtime (`aster_transport_core`, bindings) | Apache 2.0 | Adoption driver; no central infrastructure to protect; compatible with Iroh crates |
| Protocol spec | Open (CC-BY or CNCF donation) | Standard-setting; multi-language ecosystem requires an open spec |
| CLI toolchain (`aster publish`, compatibility checking) | AGPL v3 | Strongest OSI-approved copyleft; AWS and Google maintain internal policies against AGPL use; dual-license available for proprietary embedding |
| Trust Authority service | Commercial | Managed HSM custody; operational complexity is the product |
| Enterprise gateway | Commercial | OIDC/SAML2 integration, dashboard, audit log |
| Commercial routing client | Commercial | Spec-compliant extension strategies above §11.9 minimum |

The AGPL toolchain requires a CLA from all external contributors to preserve the
dual-licensing option.

The open/closed line is drawn at **service boundaries and engineering depth**,
not at feature gates on core protocol functionality. The community implementation
is genuinely complete. The commercial products are operationally complex services
or engineering-intensive routing implementations that are non-trivial to
replicate from public documentation.

### 8.6 Features That Are Not Viable Paid Products

**IID verification.** Cloud provider IID checks are local cryptographic signature
verification against public keys published by AWS, GCP, and Azure. The
verification algorithm is fully documented. Any engineer can implement it from
the cloud provider's documentation. Closing this code provides no durable moat
and placing closed-source code in the trust path is a procurement blocker for
security-conscious enterprise buyers.

**The managed registry.** There is no registry server to host. Offering "bootstrap
peers" or "sync infrastructure" is thin infrastructure, not a product.

**Paid support.** Viable as an add-on to the Trust Authority or gateway
relationships, not as a standalone revenue line for infrastructure software.
