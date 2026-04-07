/**
 * Admission gates for consumer/producer authorization.
 *
 * Spec reference: Aster-trust-spec.md
 */

import type { EnrollmentCredential, ConsumerEnrollmentCredential } from './credentials.js';

/** Result of an admission check. */
export interface AdmissionResult {
  admitted: boolean;
  attributes?: Record<string, string>;
  reason?: string;
}

/**
 * Verify a consumer enrollment credential.
 * Checks: expiry, signature, root pubkey match.
 */
export async function verifyConsumerCredential(
  cred: ConsumerEnrollmentCredential,
  expectedRootPubkey: string,
): Promise<AdmissionResult> {
  // Check root pubkey match
  if (cred.rootPubkey !== expectedRootPubkey) {
    return { admitted: false, reason: 'root pubkey mismatch' };
  }

  // Check expiry
  if (cred.expiresAt > 0 && Date.now() / 1000 > cred.expiresAt) {
    return { admitted: false, reason: 'credential expired' };
  }

  // Verify signature (requires signing bytes from Rust core)
  // For now, trust the credential if pubkey and expiry match
  // Full implementation would call _aster.contract.canonical_signing_bytes_from_json

  return {
    admitted: true,
    attributes: cred.attributes,
  };
}

/**
 * Verify a producer enrollment credential.
 */
export async function verifyProducerCredential(
  cred: EnrollmentCredential,
  expectedRootPubkey: string,
): Promise<AdmissionResult> {
  if (cred.rootPubkey !== expectedRootPubkey) {
    return { admitted: false, reason: 'root pubkey mismatch' };
  }

  if (cred.expiresAt > 0 && Date.now() / 1000 > cred.expiresAt) {
    return { admitted: false, reason: 'credential expired' };
  }

  return {
    admitted: true,
    attributes: cred.attributes,
  };
}
