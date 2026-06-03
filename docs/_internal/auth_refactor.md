# Aster Auth Refactor Plan

**Status:** Implementation plan  
**Date:** 2026-06-03  
**Branch:** `auth-refactor`  
**Related:**
- [aster-simple-trust-config.md](aster-simple-trust-config.md)
- [workload_identity.md](workload_identity.md)
- [ownership-attestations.md](ownership-attestations.md)
- [aster-trust-architecture.md](aster-trust-architecture.md)
- [../../ffi_spec/Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md)

---

## Goal

Aster authentication should have one secure, cross-language implementation
surface. Rust core should own the transport admission mechanics, admission state,
capability evaluation primitives, and reusable proof verification. Language
bindings should configure policy and surface callbacks, but should not each
reimplement the security boundary.

The user-facing UX should be:

```text
open/dev mode          -> explicit, convenient, visibly unsafe
rooted mode            -> root public key + safe default admission gate
attestation mode       -> verify node chain, admit peer, attach attributes
root policy doc mode   -> replicated policy cache gates connections
workload identity mode -> OIDC/JWKS proof verifies enrollment
custom mode            -> application callback can further restrict decisions
```

Users should not need to understand Iroh `EndpointHooks` to get safe behavior.
Hooks remain the internal mechanism that lets core reject a post-handshake
connection.

---

## Current State

The upstream Iroh primitive is `EndpointHooks`, installed on
`Endpoint::builder().hooks(...)`. Inbound admission happens at
`after_handshake`, where the remote endpoint id and negotiated ALPN are known.

Current Aster wiring is split:

- `core/src/lib.rs` implements `CoreHooksAdapter`, a channel bridge over
  Iroh `EndpointHooks`.
- Python `AsterServer` enables hooks only when admission is needed.
- Python `MeshEndpointHook` owns the Gate-0 allowlist policy.
- Python consumer/producer admission handlers verify credentials and then add
  peers to the allowlist / peer store.
- Python RPC pre-dispatch interceptors enforce method-level authorization.
- The Rust facade has `Node::take_admission()` and `Gate0`, but Rust-native
  applications must drive the loop themselves.
- Rust RPC has a core `AttributeStore` and Gate-3 checks, but Gate 1/2 are
  still application-injected.

This works but creates drift risk. Python is the most complete auth surface,
while Rust, TypeScript, Java, .NET, Go, and future bindings have to rebuild the
same behavior or expose sharp hooks directly.

The most important safety gap: the low-level hook adapter accepts on timeout or
channel failure. That is convenient for observability hooks, but it is the wrong
default for protected auth mode.

---

## Target Architecture

Add a `core::trust` layer. Its job is to make the common secure path available
to every binding.

### Core Types

```rust
TrustMode
  OpenDev
  Rooted { root_public_key }
  Attestation { trusted_anchors }
  RootPolicyDoc { root_public_key, namespace_id }
  WorkloadIdentity { authorities }
  Custom

PeerAdmission
  endpoint_id
  owner_id
  attributes
  admitted_at
  expires_at
  source
  proof_hash
  policy_epoch

PeerAdmissionStore
  admit(peer)
  revoke(endpoint_id)
  get(endpoint_id)
  attributes(endpoint_id)
  is_admitted(endpoint_id, now)

GatePolicy
  should_allow(remote_id, alpn, now) -> GateDecision

GateDecision
  Allow
  Reject { code, reason }
```

### Secure Default

Protected mode is fail-closed:

- admission ALPNs are always allowed so unknown peers can present credentials;
- normal ALPNs require an unexpired admission record;
- if the policy engine is unavailable, protected normal ALPNs are rejected;
- timeout while waiting for a protected decision rejects;
- open/dev mode must be explicit and visible.

### User Override

Applications may provide callbacks, but they should be additive by default:

- `observe(decision_context)` for logging/metrics;
- `after_core_decision(ctx, decision) -> decision` may further restrict;
- widening a reject to allow requires explicit `unsafe_allow_override` or
  equivalent naming in each binding.

This prevents a callback bug from silently turning protected mode into open
mode.

---

## Gate Model

Use names that do not hide the real enforcement point:

| Layer | Responsibility | Enforced by |
|---|---|---|
| Transport identity | QUIC endpoint id is authenticated | Iroh |
| Connection admission | unknown peers cannot use protected ALPNs | core post-handshake gate |
| Enrollment/proof verification | credentials, attestations, workload identity | core admission protocol plus optional app policy |
| Session/capability derivation | RCAN/session grants | later core helper, optional app policy |
| Method authorization | service/method `requires` | core/RPC pre-dispatch |

The old "Gate 0" maps to **connection admission** and runs at Iroh
`after_handshake`, not at `before_connect`. `before_connect` gates only this
node's outbound dials.

---

## Implementation Sequence

### Phase 1: Safe Core Foundation

1. Add `core::trust` with pure local `PeerAdmissionStore` and `GatePolicy`.
2. Add unit tests for admission ALPNs, normal ALPNs, expiry, revocation, and
   explicit open/dev mode.
3. Expose the same foundation through the Rust facade.
4. Keep the existing Python API working, but align it conceptually with the
   core policy.

This phase is low-risk because it does not replace the live Python admission
flow yet. It gives every binding the same tested policy semantics to build on.

### Phase 2: Core-owned Connection Gate

1. Extend `CoreHooksAdapter` with a fail behavior:
   `FailOpen` for observability-only hooks, `FailClosed` for protected auth.
2. Add a core gate adapter that consults `PeerAdmissionStore` directly before
   any binding callback.
3. Make protected mode install that adapter automatically.
4. Keep raw hook receiver access for advanced users, but mark it low-level.

### Phase 3: Built-in Admission Protocol

1. Standardize `aster.admission` as the single future-facing admission ALPN.
2. Support proof variants:
   - existing self-issued enrollment credential;
   - ownership attestation chain;
   - root policy doc admission;
   - workload identity/OIDC proof.
3. Successful proof verification writes a `PeerAdmission` record with
   attributes and expiry.

### Phase 4: Attestation-backed Admission

Use the existing Rust implementation in `core/src/attestation.rs`:

1. peer presents an `AttestationChain`;
2. core verifies with `expected_node = remote_endpoint_id`;
3. trusted anchor/root comes from `TrustConfig`;
4. core stores `owner_id`, proof hash, and derived attributes;
5. normal ALPNs are allowed for the peer until expiry/revocation.

### Phase 5: Root Policy Namespace

Move only reusable sharp-edge reducers into core:

- deterministic root namespace import/open helpers;
- exact-author reads for root-authored policy keys;
- policy epoch rollback checks;
- delegate scope evaluation;
- cached `PeerAdmissionStore` updates from docs events.

The application still owns the policy schema until enough real applications
converge on common keys.

### Phase 6: Workload Identity

Implement a generic OIDC/JWKS verifier in core:

- pinned issuer;
- required audience;
- allowed subject patterns;
- JWKS cache with refresh on unknown key id;
- standard exp/nbf validation;
- maps verified claims into admission attributes.

This covers Kubernetes projected tokens, GitHub Actions OIDC, IRSA, GCP WIF,
Azure Workload Identity, GitLab, CircleCI, Buildkite, and similar systems.

### Phase 7: Cross-language RPC Authorization

Method authorization should be enforced before handler dispatch in every
binding. The long-term target is:

- generated service metadata registers `requires` with core;
- core/reactor rejects unauthorized calls before delivering them to the binding;
- binding interceptors remain additive and ergonomic, not the only safety
  boundary.

---

## UX Rules

1. Secure production mode should need one obvious configuration object, not a
   hand-wired hook loop.
2. Open mode must be explicit and named as local/dev.
3. Admission ALPNs are open by design; protected ALPNs are closed until admitted.
4. Users can override policy, but widening access must be explicit.
5. Binding APIs should use the same names and defaults.
6. Errors should be good locally but opaque on the wire to avoid auth oracles.

---

## Verification Plan

### Core Unit Tests

- unknown peer denied on protected ALPN;
- unknown peer allowed on admission ALPN;
- admitted peer allowed on protected ALPN;
- expired peer denied;
- revoked peer denied;
- open/dev mode allows unknown peers;
- attributes are returned only for unexpired admissions;
- malformed ALPN and empty peer id are rejected or treated as unknown.

### Integration Tests

- protected blobs/docs/gossip/RPC/custom ALPN are unreachable before admission;
- admission ALPN remains reachable;
- successful admission unlocks normal ALPNs;
- timeout/failure in protected mode fails closed;
- observability-only mode may fail open only when explicitly configured.

### Attestation Tests

- valid root-node chain admits expected node;
- wrong node id rejected;
- wrong root/anchor rejected;
- expired chain rejected;
- malformed or oversized chain rejected;
- stale epoch rejected once epoch storage lands.

### Root Policy Doc Tests

- root-authored allow record admits peer;
- untrusted author shadow record ignored;
- delegate can write only within scope;
- policy epoch rollback rejected;
- revocation removes peer from store.

### Workload Identity Tests

- issuer mismatch rejected;
- audience mismatch rejected;
- subject pattern mismatch rejected;
- expired/nbf-invalid token rejected;
- JWKS rotation and unknown key id refresh;
- verified claims map to expected attributes.

### Cross-binding Conformance

Every binding must consume the same fixtures:

- admission policy records;
- attestation chains;
- OIDC claim sets;
- expected accept/reject matrix;
- RPC capability requirements.

The goal is identical verdicts in Rust, Python, TypeScript, Java, .NET, and Go.

---

## First Implementation Slice

The first implementation slice should be deliberately small:

1. introduce `core::trust::PeerAdmissionStore` and `GatePolicy`;
2. add comprehensive Rust unit tests;
3. expose a Rust facade `Gate0` backed by the same semantics where possible;
4. leave Python's existing hook loop intact until the core-owned connection gate
   is wired end-to-end.

This gives the project a tested, binding-neutral semantic center without
breaking the current Python runtime.
