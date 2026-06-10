# Trust Anchor Inversion — RETIRED

> **This document has been superseded.** Retired 2026-05-06.
>
> The mechanism it proposed (offline anchor signs ephemeral CI-issued deployment root, chain walks via reciprocal signing) is now in [`ownership-attestations.md`](../ownership-attestations.md), which generalises it to N-tier delegation chains under a single primitive. The "anchor delegation" artifact this doc described is an ownership attestation with statement codes `0x0010 ANCHOR_AUTHORISED_ROOT` + `0x0011 ROOT_AUTHORISED_BY_ANCHOR` (already in the registry).
>
> Three orphan ideas were harvested into other docs before retirement:
>
> - **Handle binding rebound to anchor** (instead of to ephemeral deployment root) → captured in [`../aster-trust-architecture.md`](../aster-trust-architecture.md) §"Notes harvested from retired trust-anchor-inversion."
> - **Policy attributes on a delegation** (env / contract / region restrictions, max sub-delegation depth) → captured as an open question in the same section of `aster-trust-architecture.md`.
> - **"Deployment certificates, not certificate chains" / "closer to SSH host certificates than X.509 PKI"** mental model → captured in `aster-trust-architecture.md`.
> - **Commercial wedge** (HSM integration, CI plugin, ceremony scripts, rotation tooling, compliance reporting as paid tier) → folded into [`../aster-monetisation.md`](../aster-monetisation.md) §2 "Identity & Trust Management."
>
> The original content is preserved below for historical reference. Do not build from it; use `ownership-attestations.md` as the live design.

---

## Original content (historical reference)

# Trust Anchor Inversion — Two-Tier Identity for High-Trust Deployments

**Status:** Working idea — not normative
**Date:** 2026-04-15
**Companion doc:** [trust-discovery-thinking-session.md](retired.trust-discovery-thinking-session.md) — the muddled session this clarifies
**Related:** [../../../ffi_spec/Aster-trust-spec.md](../../../ffi_spec/Aster-trust-spec.md) (current implemented spec)

---

## The insight

The trust-discovery session got tangled trying to make `@aster`, handles, Pkarr, and transparency logs all carry weight to give organisations a stronger identity story. **Invert it.**

Don't push the trust problem outward into more infrastructure. Push it *upward* into a second, optional tier that the org owns directly:

> Today's "root key" stays exactly as it is. For organisations that want a stronger trust story, their CI/CD generates a fresh root key per deployment and signs it with their own offline anchor key. A new marker in the config tells producers what they can present to prove that anchor authorised this deployment.

Default users see no change. High-trust orgs add the anchor layer above. Same protocol, different ceremony. Potential paid add-on.

---

## The model

Two tiers of identity, with the existing root-key model as the *bottom* tier, untouched:

| Tier | Key | Custody | Lifetime | What it signs |
|---|---|---|---|---|
| **1 — Anchor** | Org's true offline identity | HSM, split custody, ceremony — whatever the org already uses for code signing | Years to decades | Deployment root delegations |
| **2 — Deployment root** | Today's "root key" | CI/CD secret, generated fresh per deployment | Per-deployment (days to weeks) | Admission tokens, endorsements (current spec) |

The protocol below tier 2 is unchanged. The deployment root key still does everything today's root key does. The protocol doesn't notice it's now ephemeral.

### The new artifact: anchor delegation

A signed blob shipped with the deployment, structurally:

```
sign(anchor_priv,
     deployment_root_pubkey
  || env_label              ("prod", "staging-eu", ...)
  || not_before, not_after
  || policy_attrs           (which contracts, regions, etc.)
)
```

Plus a marker in the producer config pointing at it:

```toml
[trust_anchor]
anchor_pubkey   = "<base64 ed25519>"
delegation_path = "/etc/aster/anchor-delegation.sig"   # or inline blob
```

Producers attach the delegation alongside admission tokens. Verifiers walk one hop up when they need higher assurance.

---

## What this dissolves from the muddled doc

A surprising amount.

### P1 — Root key lifecycle

The session parked this as "scaling custody from solo-dev secret to enterprise HSM/quorum." The inversion mostly answers it:

- **Solo dev / small team:** no anchor tier. Today's model. Zero ceremony.
- **Enterprise:** anchor lives in the HSM that already exists for code signing. Deployment roots are CI secrets, ephemeral, rotate freely. No need for FROST or quorum signing on the deployment key — it's no longer a long-lived secret worth that ceremony.

### Cross-operator discovery (open thread #2)

Maersk's producer wants to call CMA CGM. Both present chains to their respective anchors. Anchors are stable, OOB-distributable pubkeys. Two foreign operators verify each other without any shared `@aster` trust. The "no shared trust anchor" stress test passes by construction.

### Consumer trust bootstrap (open thread #1)

Consumers learn **anchors**, not deployment roots. Anchors are rare, stable, and easy to distribute out-of-band: vendor website, business cards, PGP fingerprints, security.txt. This is what TOFU-pinning was always supposed to pin to. Deployment roots are too ephemeral; raw root keys today are too low-level.

### `@aster`'s own bootstrap

In the muddled doc, `@aster` justified its own trust by the "vendor like Sigstore" argument — embedded root key in binaries, vendor-trust mechanisms. Fine, but special-cased.

In the inversion, `@aster` just runs the same pattern every customer uses: an `@aster` anchor (offline) signs each `@aster` deployment root. No separate bootstrap story. Eats its own dogfood the way a framework should.

---

## The killer follow-on: rebind handles to anchors

In the original DNS layout, `<handle>.id.aster.site` resolves to the operator's **root key**. With ephemeral deployment roots, that record would thrash on every CI run.

**Bind the handle to the anchor pubkey instead.** The chain becomes:

```
@maersk  →  anchor pubkey            (in @aster handle registry, decade-stable)
         →  deployment root pubkey   (signed by anchor, rotates per CI)
         →  endpoint endorsements    (signed by deployment root, today's model)
```

`@aster`'s job shrinks dramatically: handle ↔ anchor. Single record per operator, updated maybe once a decade. The "freshness and revocation" section of the muddled doc is unaffected — discovery records still rotate at the deployment-root layer with the same tiered cadence.

---

## Open corners worth nailing down before drafting a spec

### 1. When is the marker checked?

Two viable policies:

- **Always (anchor-mode opt-in).** Operator config flag. Verifier requires the delegation blob; one extra signature verify per session. Cleanest for orgs that want anchor-mode end-to-end.
- **On-demand (mixed mode).** Verifier escalates only when policy demands it ("contract X requires anchor-rooted identity"). Lets free-tier and paid-tier producers coexist on the same mesh.

Probably want both, configured per-contract or per-verifier. Decide which is the default.

### 2. Where does the marker travel?

- Attached to admission token at handshake?
- Published as a discovery record alongside endorsements?
- Fetched on-demand from a known endpoint?

All three could work. Trade-off is bandwidth on every connect vs. an extra round-trip when escalation hits.

### 3. Anchor compromise / rotation

Anchors will rotate eventually (decade-scale, but real). Specify now so it doesn't get bolted on:

- **Transitional signing.** New anchor's first delegation also signed by the old anchor during a transition window. Same pattern as `@aster`'s root rotation in the muddled doc's §"Rotation".
- **Handle re-binding.** `@aster` handle registry update points the handle at the new anchor. Old anchor remains valid for verifying historical artifacts.
- **Compromise procedure.** Out-of-band revocation announcement, transparency-log entry (v2), etc.

### 4. What's in `policy_attrs`?

The delegation can carry constraints the deployment root must honour:

- Allowed environments (`env=prod` only)
- Allowed contract IDs (this deployment can only serve `ShippingAPI`)
- Regions / clusters
- Maximum sub-delegation depth (probably 0 — deployment root cannot itself further delegate)

Worth scoping carefully — too much policy here turns it into a capability layer, which the muddled doc explicitly parked for Gate 3.

### 5. Anchor → handle binding integrity

The whole inversion rests on `@aster`'s handle registry binding `@maersk` to the right anchor. That binding is now load-bearing for everything. The transparency-log idea from the muddled doc resurfaces here — but cleanly, with one job: "every handle ↔ anchor binding ever asserted by `@aster` is publicly auditable." Much smaller surface than logging every operator's every record.

---

## Why this is also a good commercial wedge

The protocol bits are tiny: one extra signature verification, one extra config field. Free tier ships it for everyone — even solo devs benefit if they later want to upgrade.

The **operational tooling** is where orgs pay:

- HSM integration (PKCS#11, cloud KMS, YubiHSM)
- CI/CD plugin that performs per-deployment signing
- Key ceremony scripts and runbooks
- Audit log of every delegation issued
- Rotation tooling
- Compliance reporting (SOC2, ISO27001 evidence)

Same revenue model as code-signing infra orgs already pay for. Aster gives the protocol away; the enterprise tier is the boring-but-essential plumbing that turns "we have an anchor key somewhere" into "we have a defensible key custody story our auditors accept."

Self-hosted deployments get the protocol bits free; they can build their own tooling or buy ours.

---

## Invariants preserved

- **`@aster` is never in the trust chain.** Still true. Anchors are operator-owned. `@aster` only asserts handle ↔ anchor bindings.
- **Self-hosting parity.** Operators who skip `@aster` can publish anchor delegations anywhere (Pkarr under the anchor key, static HTTPS, git, etc.). Verification algorithm is unchanged.
- **Today's deployments keep working.** No-anchor mode is the default; the inversion is purely additive.
- **Trust always terminates at an operator-owned key.** Just one tier higher than today.

---

## Mental model

Think of it as **deployment certificates**, not certificate chains:

- The **anchor** is your org's identity. Long-lived, offline, rare.
- Each **deployment root** is a short-lived "deployment certificate" the anchor issues — like a per-deployment code-signing cert.
- Producers carry one delegation hop (anchor → deployment), not an x509-style chain of N. Cheap to verify, hard to misuse.

Closer to **SSH host certificates** (one CA, signs short-lived host certs) than to **x509 PKI** (multi-level CAs, CRLs, OCSP). The simplicity is the point.

---

## Next steps

1. Decide the marker semantics: always-checked vs on-demand vs hybrid.
2. Sketch the delegation blob format (probably reuse `EnrollmentCredential`'s shape with an `anchor_delegation` field).
3. Decide handle binding: anchor-keyed only, or back-compat with root-keyed handles for existing deployments.
4. Spec the anchor rotation procedure.
5. Identify what subset of the muddled doc's §"Proposed v1 registration flow" needs to change to support anchor-mode.
6. Cost out the enterprise tooling MVP (HSM integration + CI plugin) — that's the actual commercial deliverable.
