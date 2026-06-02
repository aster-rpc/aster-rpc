# Aster Simple Trust Config Pattern

**Status:** Application pattern, not protocol  
**Date:** 2026-06-02  
**Related:**
- [ownership-attestations.md](ownership-attestations.md) - portable root/node proof artifacts
- [aster-trust-architecture.md](aster-trust-architecture.md) - broader trust architecture and aster.site model
- [../../ffi_spec/Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md) - current implemented trust spec

---

## Summary

This document captures a simpler trust and configuration pattern that Aster
applications can use at their discretion. It does not replace ownership
attestations or the broader aster.site trust architecture. Those remain useful
for portable proof, cross-organisation trust, transparency logging, and
externally verifiable identity.

The simple pattern is a docs-backed deployment control plane:

- A root node owns a replicated root policy namespace.
- Regular nodes sync that namespace after passing the application's auth gate.
- The root policy namespace carries admitted node ids, delegates, roles, and
  optionally encrypted replicated configuration.
- Each regular node may also own a node config namespace derived from its own
  node key.

The pattern can be implemented by an end application today using existing docs
and RPC primitives. Framework support should be limited to reusable helpers and
guardrails until multiple applications converge on the same schema.

---

## Premise

Every Aster node already has:

- a private node key for its own transport identity;
- a configured root public key representing its owner or manager.

Iroh authenticates the peer node id at transport level. Aster can build a simple
deployment-local policy layer on top of that by using docs namespaces whose ids
are derived from node/root keys.

The important distinction:

- **Ownership attestations** are portable proof artifacts. They verify locally,
  require no live docs sync, and are suitable for external verifiers.
- **This pattern** is live deployment policy. It is replicated state used by an
  application or fleet to decide which nodes may connect and what config they
  should consume.

---

## Namespace Model

### Root Policy Namespace

The root node opens a writable docs namespace where:

```text
namespace secret = root node secret
namespace id     = root node id / root public key
writer author    = root node id
```

Regular nodes import the namespace read-only by `namespace id`, which is already
known from their configured root public key. They sync it only after passing the
application's auth gate.

The root namespace is deployment-visible control-plane state. Admission controls
who can sync it, but anything written there should be safe for every admitted
node to read. Sensitive values must be omitted or encrypted.

### Node Config Namespace

Each regular node may also open a writable namespace where:

```text
namespace secret = node secret
namespace id     = node id
writer author    = node id
```

This namespace is useful for local node state, accepted config, observed
runtime state, or node-authored status. If root/delegate-originated changes need
to appear there, the preferred pattern is that the node verifies the request and
then writes the accepted result itself. This keeps a clean invariant: entries in
the node config namespace are authored by the node.

---

## Root Policy Keys

Applications can choose their own key schema. A conservative starting point is
to use per-entity keys rather than one large mutable list:

```text
/policy/schema
/policy/epoch

/produces/nodes/<node_id>
/produces/contracts/<contract_id>/nodes/<node_id>

/delegates/<delegate_id>
/delegates/<delegate_id>/scopes/<scope>

/roles/<node_id>
/capabilities/<node_id>
```

Example node record:

```json
{
  "status": "active",
  "roles": ["producer"],
  "contracts": ["task-manager"],
  "enc_pubkey": "<node encryption public key>",
  "since": 1780425600,
  "expires": 0
}
```

Example delegate record:

```json
{
  "status": "active",
  "scopes": ["nodes.write", "config.write"],
  "since": 1780425600,
  "expires": 0,
  "max_epoch": 0
}
```

Applications should treat keys written by the root author as authoritative.
Delegate-authored keys should be accepted only when the root policy explicitly
authorises that delegate and scope.

---

## Connection Check

A simple producer/consumer admission check can be:

1. QUIC authenticates the remote node id.
2. The application reads the root policy namespace.
3. The application checks that `/produces/nodes/<remote_node_id>` exists,
   is root-authored, is active, and is not expired.
4. Optional role/capability checks are applied from root-authored policy keys.
5. Docs sync or RPC access is allowed only after the gate passes.

For auth-sensitive reads, applications should not read "latest value across all
authors" unless that is explicitly intended. Prefer exact-author reads for root
policy keys, for example:

```text
author = root_node_id
key    = /produces/nodes/<remote_node_id>
```

This avoids accepting a value shadowed by an untrusted author.

---

## Replicated Encrypted Config

Replicated config can live in the root policy namespace without making plaintext
visible to every namespace reader. Use envelope encryption:

1. The root generates a random symmetric content key for an epoch.
2. Config values are encrypted with that content key.
3. The content key is encrypted separately to each recipient node's encryption
   public key.
4. Nodes sync the namespace, find their recipient envelope, unwrap the content
   key, and decrypt the config they are allowed to read.

Suggested key layout:

```text
/encrypted/schema
/encrypted/epoch
/encrypted/recipients/<node_id>/<epoch>
/encrypted/config/<epoch>/<path>
```

Recipient envelope:

```json
{
  "epoch": 42,
  "recipient": "<node_id>",
  "alg": "HPKE-X25519-BLAKE3-AEAD",
  "encrypted_key": "<base64url>"
}
```

Encrypted config value:

```json
{
  "epoch": 42,
  "path": "/config/runtime/foo",
  "alg": "AEAD",
  "nonce": "<base64url>",
  "ciphertext": "<base64url>"
}
```

The node identity key is an Ed25519 signing key and should not be casually reused
as an encryption key. Prefer a separate node encryption key, such as an X25519
or HPKE key. The root-authored node policy record can publish the encryption
public key.

AEAD associated data should bind the ciphertext to its context:

```text
namespace_id
key_path
epoch
root_node_id
schema_version
```

This prevents ciphertext from being replayed into a different namespace, path,
or epoch.

---

## Key Rotation

Rotation is application policy, but the basic rules are:

- **Add node:** encrypt the current content key to the new node if it may read
  current config.
- **Remove node:** create a new epoch and content key, encrypt only to remaining
  nodes, and write new config under the new epoch.
- **Compromised node:** rotate immediately and assume old ciphertext available
  to that node is exposed.
- **Sensitive subgroups:** use per-node or subgroup content keys instead of a
  single fleet-wide content key.

Rotating a content key does not erase knowledge already available to a removed
or compromised node. It only prevents future decrypt access.

---

## Framework vs Application

This pattern should remain application-owned initially.

Framework core should provide primitives and guardrails:

- docs namespace import, read, write, and sync;
- auth-gated docs sync hooks;
- helpers for exact-author reads;
- monotonic epoch/version helpers;
- canonical encoding helpers for policy records;
- encryption envelope primitives for key wrapping and config encryption;
- watch helpers for policy prefixes.

Framework core should not assign protocol meaning to application paths such as
`/produces/nodes`. That meaning belongs to the application.

An optional Aster module could later provide a reference convention, for
example:

```text
aster.trust.control_plane.RootPolicyNamespace
aster.trust.control_plane.NodeConfigNamespace
aster.trust.control_plane.EncryptedConfigEnvelope
aster.trust.control_plane.RecipientKeyEnvelope
aster.trust.control_plane.DelegatePolicy
```

That module should remain opt-in until the schema has been exercised by real
applications.

---

## Non-goals

This pattern is not:

- a replacement for ownership attestations;
- a replacement for external trust publication or transparency logging;
- a cross-organisation PKI;
- a global discovery mechanism;
- a guarantee of confidentiality for anything written in plaintext to the root
  policy namespace.

Ownership attestations remain the right primitive when a node needs to present a
compact, self-contained proof that it is owned or authorised by a root or anchor.
The docs-backed pattern is the right primitive when a deployment wants simple,
replicated, live policy and config.

---

## Implementation Notes

The current codebase already supports several pieces of this pattern:

- Aster's default docs author is the node identity, so `AuthorId == NodeId`.
- Docs namespaces can be imported read-only by namespace id.
- Docs namespaces can be opened writable from a deterministic 32-byte namespace
  secret.
- Registry ACL code already demonstrates post-read author filtering.

Before promoting this beyond an application pattern, the framework should prove:

- docs sync is consistently gated by the same admission policy;
- root-authored reads cannot be shadowed by untrusted writers;
- epoch rollback checks are easy to apply;
- encryption envelopes have stable canonical encoding and associated data;
- key rotation is testable with add/remove/compromise flows.
