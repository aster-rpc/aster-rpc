# Delegation & Intermediate Keys — Design Notes

> Status: **Open question.** No implementation yet. Capturing requirements and trade-offs for future work.

## Problem Statement

Today Aster uses a flat trust model: one offline root key directly signs all enrollment credentials. This works but has operational limitations:

1. **Regional signing** — an EU operator can't mint credentials without accessing the offline root key
2. **Revocation granularity** — revoking one node requires salt rotation, which disrupts the entire mesh
3. **Key exposure window** — every signing ceremony requires the root key, increasing the number of times it's exposed
4. **Audit trail** — all credentials look identical ("signed by root"); no way to determine who authorized a particular node or when delegation was granted

### What we want to answer

- Does a particular node identity belong to a root key?
- Which root key?
- Who authorized this node? (root directly, or a delegated operator?)
- Is the delegation still valid?
- Can we revoke a delegated operator without disrupting all nodes they didn't sign?

### Ideal properties

- **Single signature verification at connection time** (no chain walking)
- **Independent regional signing** (operator doesn't need root key online)
- **Revocable delegation** (revoke an intermediate without salt rotation)
- **Minimal payload overhead** (credentials are sent once per connection)
- **No recursive chain validation** (bounded, predictable verification logic)
- **Compatible with existing ed25519 keys** (no curve change)

These properties are in tension — particularly single-sig verification vs independent signing.

---

## Current Model (Flat)

```
root_key ──signs──▶ EnrollmentCredential { endpoint_id, expires_at, attributes }
```

Verification: one `ed25519_verify` call.

**Pros:**
- Simplest possible model
- One signature, one check, minimal payload
- Easy to reason about and audit

**Cons:**
- Root key must be present for every enrollment
- No delegation — root operator is a single point of operational bottleneck
- Revocation is all-or-nothing (salt rotation)

---

## Option A: Delegation Tokens (Max Depth 1)

The root signs a `DelegationCredential` granting an intermediate key limited signing authority. Nodes carry both the delegation and their enrollment credential. Verification is always exactly two checks — never recursive.

```
root_key ──signs──▶ DelegationCredential { intermediate_pubkey, scope, expires_at }
intermediate_key ──signs──▶ EnrollmentCredential { endpoint_id, delegation, expires_at, attributes }
```

Verification:
```python
verify(cred.delegation.signature, root_pubkey)       # root authorized intermediate
verify(cred.signature, cred.delegation.intermediate_pubkey)  # intermediate authorized node
check_expiry(cred.delegation.expires_at)
check_expiry(cred.expires_at)
```

**Pros:**
- Independent regional signing (intermediate operates without root)
- Revocation by not renewing delegation (short TTL, e.g. 24h–7d)
- Bounded verification (always exactly 2 checks, no chain walking)
- Small payload overhead (~128 bytes for the embedded delegation)
- No new crypto primitives (standard ed25519)

**Cons:**
- Two signature verifications per connection (~100us total — negligible compute but additional logic)
- Doubles the signature payload in the credential
- Is fundamentally a CA chain with `maxPathLength=0` — we should be honest about that
- Delegation expiry adds clock-dependency at two levels

---

## Option B: Delegation at Signing Time (Co-Signing / Pre-Signed Batches)

The root and intermediate collaborate during an offline ceremony. The root's signature covers both the delegation scope and the final enrollment, producing a credential that verifies with a single signature against the root pubkey. The intermediate's involvement is invisible at verification time.

### Variant B1: Batch Pre-Signing

The root pre-signs a batch of enrollment credentials with specific endpoint IDs filled in. The intermediate operator selects from the batch during enrollment.

```
# Ceremony (offline)
for each endpoint_id in batch:
    sign(root_key, intermediate_pubkey || endpoint_id || expires_at || scope)

# Enrollment (regional)
operator picks the pre-signed credential matching the node's endpoint_id
```

Verification: one `ed25519_verify` — same as today.

**Pros:**
- Single signature, single check — no payload or logic overhead
- Intermediate can operate offline after receiving the batch
- Verification is identical to the flat model

**Cons:**
- Root must know all endpoint IDs at batch time (or sign wildcards, which is dangerous)
- Batch size is fixed — new nodes require a new ceremony
- Pre-signed credentials can't be revoked individually (only by expiry)
- Doesn't solve the "operator mints on demand" use case

### Variant B2: Commitment-Based Co-Signing

The root signs a commitment to the intermediate's future binding. The intermediate fills in the endpoint_id later. The credential carries a combined proof.

```
delegation = sign(root_key, intermediate_pubkey || expires_at || scope)
credential = sign(intermediate_key, endpoint_id || delegation)
```

Verification: still two checks (same as Option A). The root's signature doesn't cover the endpoint_id, so the verifier must check the intermediate's binding separately.

**Pros:** Same as Option A.
**Cons:** Same as Option A. The "co-signing" framing doesn't actually reduce to one check because the root can't sign over data that doesn't exist yet.

---

## Option C: FROST Threshold Signing

FROST allows multi-party signing where N-of-M parties collaborate to produce a single standard ed25519 signature. The verifier sees one signature from one public key.

```
# Setup: root key is split into shares via FROST DKG
# Signing: root-holder + intermediate-holder collaborate to produce one signature
credential = frost_sign([root_share, intermediate_share], enrollment_data)
```

Verification: one `ed25519_verify` against the combined public key.

**Pros:**
- Single signature, single check — best of both worlds
- Standard ed25519 on the wire — verifiers don't know FROST was involved
- No payload overhead

**Cons:**
- Requires online collaboration between root-holder and intermediate-holder at signing time
- Doesn't enable independent regional signing (the whole point) — both parties must be online
- FROST DKG is a ceremony that must happen before any signing
- Adds significant complexity to key management
- If one share is lost, signing is impossible (threshold properties)

FROST solves multi-party *authorization* but requires *co-presence*. It doesn't give us independent delegation.

---

## Option D: Stay Flat, Solve Operationally

Don't add intermediate keys. Instead, make the flat model more ergonomic:

- **Time-boxed signing sessions** — root key signs a set of "blank" credentials with short TTLs during a ceremony. Operator fills in endpoint IDs from the batch.
- **Automated ceremony via secure enclave** — root key lives in HSM/KMS; enrollment requests are queued and signed asynchronously.
- **Shorter credential TTLs + automated renewal** — instead of 30-day credentials, issue 24h credentials with an automated renewal loop that hits the KMS.

**Pros:**
- Zero spec changes
- No additional verification logic
- Single signature model preserved
- KMS/HSM integration is well-understood infrastructure

**Cons:**
- Still no true delegation — the root (or its KMS proxy) signs everything
- Requires infrastructure (KMS, renewal service) that small deployments may not have
- Doesn't solve the audit trail question ("who authorized this?")

---

## Option E: BLS Aggregate Signatures

BLS signatures (on BLS12-381 curve) support aggregation: `sig_1 + sig_2` produces a single aggregate signature that verifies against the corresponding aggregate public key. This would allow:

```
agg_sig = bls_aggregate(sign(root, delegation_scope), sign(intermediate, enrollment_data))
agg_pubkey = bls_aggregate_pubkey(root_pubkey, intermediate_pubkey)
verify(agg_sig, agg_pubkey, delegation_scope || enrollment_data)
```

Verification: one `bls_verify` call.

**Pros:**
- True single-signature verification with independent signing
- Mathematically elegant — the only option that achieves all ideal properties

**Cons:**
- Requires switching from ed25519 to BLS12-381 (or maintaining two curves)
- BLS verification is ~10x slower than ed25519 (~1ms vs ~50us)
- BLS signatures are larger (96 bytes vs 64 bytes for ed25519)
- BLS is less widely deployed and tooled than ed25519
- Iroh's identity model is built on ed25519 — changing this is invasive
- Aggregation requires coordination at signing time (both signatures must exist)

---

## Summary Matrix

| Property | Flat (today) | A: Depth-1 Chain | B1: Batch Pre-Sign | B2: Co-Sign | C: FROST | D: Operational | E: BLS |
|----------|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| Single sig verification | Y | N (2) | Y | N (2) | Y | Y | Y |
| Independent regional signing | N | Y | N | Y | N | N | Y |
| Revocable delegation | N | Y (TTL) | N | Y (TTL) | N | N | Y |
| No curve change | Y | Y | Y | Y | Y | Y | N |
| Minimal payload | Y | ~128B extra | Y | ~128B extra | Y | Y | +32B |
| Low complexity | Y | Low | Low | Low | High | Y | High |
| Audit trail (who signed) | N | Y | N | Y | N | N | Y |

---

## Open Questions

1. How often do we actually need independent regional signing? If the answer is "rarely," Option D (operational improvements) may be enough.
2. Is the 128-byte payload overhead of Option A actually a problem? Credentials are sent once per connection.
3. Could we start with Option D (KMS + short TTLs) and add Option A later if delegation demand materialises?
4. Is there appetite for a curve change (Option E)? This would be a major version break.
5. For Option A: should delegation scope be attribute-based ("can sign credentials with region=eu") or capability-based ("can sign up to N credentials")?
