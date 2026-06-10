# Aster Trust Directory — Authority As Lookup, Not Artifact

**Status:** Simplification proposal (supersedes design *direction* of the three docs below; their mechanisms get explicit keep/delete/sever dispositions here)
**Date:** 2026-06-10
**Related:**
- [ownership-attestations.md](ownership-attestations.md) — chains; mostly simplified away in-deployment, single-edge survives at the boundaries
- [aster-trust-architecture.md](aster-trust-architecture.md) — FROST/log/epochs; severed to the aster.site platform product
- [workload_identity.md](workload_identity.md) — OIDC verifier; survives, output changes from credential to directory write
- portal-sync (the incubator): `portal-sync/docs/design/control-plane-auth-rpc.md`, `portal-sync/docs/product-vision-ux.md` §Console Authority
- [working_ideas/aster-orchestrator.md](working_ideas/aster-orchestrator.md) — the directory as the apiserver-equivalent of a flat workload orchestrator (the capability's second consumer after portal-sync)

---

## The realization (from portal-sync)

The three trust docs above solve trust with **artifacts** — chains, credentials,
JWTs, publications — because they implicitly assume the verifier and the
authority do not share live state. portal-sync ran a full enterprise-shaped
control plane for two phases and demonstrated the opposite premise:

> **Inside a deployment, verifier and authority always share live state — a
> replicated, authenticated, watchable directory (the root policy namespace).
> When they do, authority is a lookup. Artifacts are needed only at the
> boundaries the directory does not reach.**

Token/chain systems bind authority at **mint time**, which is why they need
expiry windows, epochs, replay counters, revocation infrastructure, and
redistribution tooling — a minted artifact is a stale copy of the directory.
A live directory binds authority at **evaluation time**: delegation is a row,
revocation is a tombstone that propagates at sync speed, and the audit log is
the directory itself.

The rule that organizes everything below:

> **Inside the directory: rows. At the directory's edges: one signed
> artifact.**

There are exactly two real edges: **above the root** (anchor rotation) and
**outside the deployment** (cross-org / public verification).

## The capability

**Aster Trust Directory** — a first-class Aster capability for enterprise P2P
deployments. Generalized from the portal-sync control plane
(`portal-control` / `portal-cas/policy`), which becomes the reference
implementation and first consumer.

### Core model

- **Directory = iroh-docs namespace whose id is the deployment root identity.**
  Namespace secret derived from the identity secret (no separate distribution
  path). Members import it read-only at enrollment (`--root-node-id` pattern)
  and react via watchers.
- **Authority comes from records, evaluated live.** A record is valid if it is
  root-authored, or authored in a namespace the root currently designates (see
  delegation below). Never from key existence, never from a carried artifact.
- **Record vocabulary** (generic tier; applications add their own records in
  the same namespaces):

  | Record | Meaning |
  | --- | --- |
  | `Admission` | node X is a member (Gate-0 input) |
  | `Role` | node X has role R, scope S (Gate-3 input, data-driven) |
  | `Designation` | node X's own namespace is authoritative for scope S |
  | `Revocation` | explicit tombstone for any of the above |

- **Gates become data-driven.** Gate-0 admits from `Admission` rows; Gate-3
  authorizes per-RPC from `Role` rows. Same three-gate model as
  [aster-trust-architecture.md](aster-trust-architecture.md); the role table
  moves from code/config into the directory.
- **Delegation without shared secrets.** A delegate (CI enroller, admin
  console, org-unit signer) writes records into **its own node-owned
  namespace** (id = its identity, same derivation). Members honor that
  namespace's records, within the designated scope, only while a live
  root-authored `Designation` exists. Revoking the designation drops the
  delegate's entire authority on the next watcher tick. Validity is one extra
  lookup against the same directory — not a chain.
- **Pre-revocation records stand** until individually revoked (a grant valid
  when made must not silently mass-revoke a subtree); new authorship is
  blocked instantly. Matches the existing principle: revocation is explicit
  records, never key deletion.
- **Optional `not_after` on rows** as defense-in-depth: a long-partitioned
  member that cannot hear a tombstone still has a time backstop. This is the
  one property where artifact expiry genuinely beats lookup; keep it as an
  optional field, not a mandatory lifecycle.

### What an "intermediate" becomes

The enterprise N-tier topology (corporate anchor → org-unit → region → env →
node) that motivated chain depth 8 is expressed as **designation rows with
scopes**, not as bridging identities in a bundled artifact:

```
Role/Designation: node ORG-UNIT-A  scope env=prod,region=eu   (root-authored)
Admission:        node N17                                    (authored by ORG-UNIT-A, in its namespace)
```

Compared to the chain model, in-deployment:

| Concern | Chain model | Directory model |
| --- | --- | --- |
| Revoke an intermediate | subtree expires within an epoch | tombstone; subtree authority gone at sync speed |
| Policy on a delegation | unsolved (`policy: bytes` v2 question) | ordinary record fields, evaluated live |
| Rotation | re-issue + redistribute chains to every leaf (open question #7) | rows propagate by sync; nothing redistributed |
| Replay protection | per-edge `epoch` counters (unimplemented) | not needed — there is no carried artifact to replay |

### The anchor tier — the one chain that stays

The directory model has one structural weakness: **the root id is the
namespace id**, so rotating the root means re-bootstrapping every member.
This is exactly what the anchor↔root attestation edge fixes, and it is the
chain design's genuine contribution:

- An **offline anchor** signs a single reciprocal edge
  (`ANCHOR_AUTHORISED_ROOT` / `ROOT_AUTHORISED_BY_ANCHOR`) binding the current
  directory root.
- Members pin the **anchor**, not the root, and accept directory-root rotation
  via a fresh anchor-signed edge (delivered in-band through the old directory
  while it is still trusted, or out-of-band at re-enrollment).
- Handles bind to the anchor (the decision already recorded in
  [aster-trust-architecture.md](aster-trust-architecture.md) §"Handle binding
  rebound to anchor").

**Chains therefore have depth ≤ 2, forever.** The already-implemented v1
(single-edge mint + verifier in `core/src/attestation.rs`) is sufficient; the
unimplemented parts (multi-tier minting, epoch enforcement, gossip revocation)
are exactly the parts the directory makes unnecessary.

### The boundary exports — the other surviving artifacts

For verifiers **outside** the deployment (no directory access):

- **Root↔node export**: a single-edge reciprocal attestation *derived from a
  directory row* — effectively a signed extract of the directory for someone
  who cannot read it. Already implemented.
- **Trust publication** (Day-1 JWS at `.well-known/aster`): survives as
  designed in [ownership-attestations.md](ownership-attestations.md), scoped
  to its real job — cross-org anchor discovery and rotation announcement. Not
  needed inside a deployment.

### Enrollment evidence — workload identity, output changed

The OIDC verifier from [workload_identity.md](workload_identity.md) survives
essentially unchanged (issuer + JWKS + `aud` + `sub` patterns; one verifier
covers k8s, GHA, IRSA, GCP WIF, Azure WI, GitLab, CircleCI, Buildkite). What
changes is the **output**: instead of minting an admission credential bound to
`sdk_pk` with a refresh-before-expiry loop, the verifier **gates a directory
write**. An *enroller* — a directory-designated delegate — validates the JWT
and writes the `Admission` row in its designated namespace. The JWT's job ends
at enrollment; ongoing authority is the row; the entire credential-refresh
lifecycle disappears.

## Dispositions, per doc

### [ownership-attestations.md](ownership-attestations.md)

| Mechanism | Disposition |
| --- | --- |
| Single-edge reciprocal attestation (mint + verifier, implemented) | **Keep** — the boundary artifact (anchor↔root; root↔node export) |
| Wire discipline (Fory opaque-body, domain separators, size gates, structural-before-crypto, exact-length checks) | **Keep** — applies to every Aster artifact including directory records |
| Trust publication (Day-1 JWS) | **Keep** — cross-org boundary only |
| Multi-anchor trust-set resolution | **Keep** — federation still happens at the trust-set layer |
| N-tier chains, intermediates, bridging, Step 0.5 multi-tier minting | **Delete in-deployment** — designation rows; chains capped at depth 2 |
| Epoch replay enforcement | **Delete** — no carried artifact to replay |
| Gossip-topic revocation infrastructure | **Delete** — revocation is a directory tombstone |
| Policy attributes on delegations (v2 schema question) | **Delete** — policy is record fields, evaluated live |
| Chain redistribution after rotation (open question #7) | **Dissolved** — rows propagate by sync |

### [aster-trust-architecture.md](aster-trust-architecture.md)

| Mechanism | Disposition |
| --- | --- |
| Three-gate model | **Keep** — made data-driven from the directory |
| Anchor-binding for handles | **Keep** — unchanged |
| FROST threshold signing, epochal ephemeral keys, regional signing nodes, credential TTL postures | **Sever** — aster.site *platform* infrastructure (public marketplace credential issuance), a separate product on a separate timeline. Not enterprise-P2P core. |
| Transparency log | **Sever** to platform. In-deployment, its function (auditability, split-view detection) is largely supplied by the directory itself: replicated to every member, append-mostly, cross-synced so an equivocating root is caught when members reconcile. Caveat: detection-eventually, not CT-grade proofs — acceptable for single-org trust where the root is your own org and the threat is compromised delegates, not malicious self. A log Trust Mark can return later for regimes that need more. |
| "Mode 1: self-issued" | **Renamed** — *run a directory* |

### [workload_identity.md](workload_identity.md)

| Mechanism | Disposition |
| --- | --- |
| OIDC verifier (issuer/JWKS/aud/sub-patterns; the platform table) | **Keep** — unchanged |
| Per-tenant enrollment-authority config | **Keep** — becomes enroller-delegate config |
| Credential (rcan) minting + refresh loop | **Delete** — verifier gates a directory write; authority is the row |

## What this requires of Aster

1. **`aster-directory` (or equivalent) in core**: directory open/import,
   record schemas (Admission/Role/Designation/Revocation), watchers, the
   validity rule (root-authored ∨ designated-namespace-authored within scope),
   and data-driven Gate-0/Gate-3 hookup. Most of this generalizes existing
   portal-sync code (`portal-control`, `portal-cas/policy`) — migrate the
   generic tier down; portal keeps its app-specific records (tree grants,
   sync-desired) and becomes the first consumer.
2. **Anchor tier wiring**: pin-the-anchor verification of the directory root
   via the existing single-edge verifier; root-rotation flow.
3. **Enroller module**: the OIDC verifier gating designated-namespace writes.
4. **Boundary export**: derive a root↔node edge from a directory row on
   demand (`aster_node_chain()` semantics preserved for outside verifiers).
5. **Honesty in docs**: the directory model requires *membership* — a
   verifier must sync the namespace. Brand-new or non-member verifiers are
   served by the boundary artifacts, not the directory. State the split-view
   caveat plainly rather than implying CT-grade auditability.

## What this deletes from the roadmap

Multi-tier chain minting (Step 0.5), gossip-revocation infrastructure,
epoch-replay enforcement, the policy-attributes-in-signed-bodies question,
chain redistribution tooling, and — for enterprise P2P — the entire
FROST / ephemeral-key / transparency-log stack (which remains the aster.site
platform design, evaluated on that product's own merits and timeline).

## The one-line version

> The trust architecture assumed verification without shared state.
> portal-sync demonstrated that enterprise deployments always have shared
> state — a replicated directory — and that authority-as-lookup beats
> authority-as-artifact everywhere the directory reaches. Artifacts remain at
> exactly two boundaries: above the root (anchor) and outside the deployment
> (export / publication).
