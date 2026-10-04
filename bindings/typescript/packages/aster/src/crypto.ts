/**
 * HPKE recipient keys derived from Aster node identities (Rust
 * `aster::crypto`).
 *
 * A node's X25519 HPKE keypair can be derived from its Ed25519 identity:
 * anyone holding the node id computes the recipient public key, and the
 * node opens with its identity secret — nothing extra to publish. The keys
 * work with {@link hpkeSeal} / {@link hpkeOpen}:
 *
 * ```ts
 * const env = hpkeSeal(hpkeX25519PublicFromNodeId(peerId), aad, secretBytes);
 * // on the peer:
 * const plain = hpkeOpen(hpkeX25519SecretFromIdentity(node.exportSecretKey()), aad, env);
 * ```
 *
 * Security note: the identity secret also unlocks anything sealed to the
 * node; rotating the recipient key means rotating the node identity.
 */

import type { KeyMaterial } from './config.js';
import { loadNative } from './native.js';
import type { NodeId } from './node.js';

/**
 * The X25519 HPKE private key derived from an Ed25519 identity secret
 * (libsodium `crypto_sign_ed25519_sk_to_curve25519`).
 * @group Trust
 */
export function hpkeX25519SecretFromIdentity(secret: KeyMaterial): Uint8Array {
  return Uint8Array.from(loadNative().hpkeX25519SecretFromIdentity(secret));
}

/**
 * The X25519 HPKE public key derived from an Ed25519 identity public key
 * (libsodium `crypto_sign_ed25519_pk_to_curve25519`). Throws for an invalid
 * Ed25519 key.
 * @group Trust
 */
export function hpkeX25519PublicFromIdentity(publicKey: KeyMaterial): Uint8Array {
  return Uint8Array.from(loadNative().hpkeX25519PublicFromIdentity(publicKey));
}

/** {@link hpkeX25519PublicFromIdentity} for a node id. @group Trust */
export function hpkeX25519PublicFromNodeId(nodeId: NodeId | string): Uint8Array {
  return Uint8Array.from(loadNative().hpkeX25519PublicFromNodeId(nodeId));
}
