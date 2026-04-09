/**
 * Admission gates for consumer/producer authorization.
 *
 * Spec reference: Aster-trust-spec.md §2.4, §3.2
 *
 * Two-phase admission:
 *   1. checkOffline  — signature, expiry, endpoint ID binding, nonce
 *   2. checkRuntime  — IID verification (cloud identity)
 *
 * admit() orchestrates both. Refusal reasons are logged internally —
 * never sent to peer (oracle protection).
 */

import type { EnrollmentCredential, ConsumerEnrollmentCredential } from './credentials.js';
import { verifyCredentialSignature, hexToBytes } from './credentials.js';
import type { NonceStore } from './nonce.js';
import { verifyIID, type IIDBackend } from './iid.js';

/** Result of an admission check. */
export interface AdmissionResult {
  admitted: boolean;
  attributes?: Record<string, string>;
  /** Internal only — never send to peer. */
  reason?: string;
}

// ── Structural validation ───────────────────────────────────────────────────

function validateStructure(
  cred: EnrollmentCredential | ConsumerEnrollmentCredential,
): { ok: boolean; reason?: string } {
  if ('credentialType' in cred) {
    const consumer = cred as ConsumerEnrollmentCredential;
    if (consumer.credentialType === 'ott') {
      if (consumer.nonce == null) {
        return { ok: false, reason: 'OTT credential must carry a nonce' };
      }
      const nonceBytes = hexToBytes(consumer.nonce);
      if (nonceBytes.length !== 32) {
        return { ok: false, reason: `OTT nonce must be exactly 32 bytes; got ${nonceBytes.length}` };
      }
    } else if (consumer.credentialType === 'policy') {
      if (consumer.nonce != null) {
        return { ok: false, reason: 'Policy credential must not carry a nonce' };
      }
    } else {
      return { ok: false, reason: `Unknown credentialType: ${consumer.credentialType}` };
    }
  }
  return { ok: true };
}

// ── Offline checks ──────────────────────────────────────────────────────────

/**
 * Offline admission checks — no network calls.
 *
 * 1. Structural validity (nonce length, policy-vs-OTT constraints)
 * 2. Signature valid against rootPubkey
 * 3. expiresAt > now
 * 4. Endpoint ID match (always for EnrollmentCredential; if set for Consumer)
 * 5. OTT nonce not already consumed
 */
export async function checkOffline(
  cred: EnrollmentCredential | ConsumerEnrollmentCredential,
  peerEndpointId: string,
  nonceStore?: NonceStore,
): Promise<AdmissionResult> {
  // 1. Structural validation
  const { ok, reason: structReason } = validateStructure(cred);
  if (!ok) {
    console.log(`[DEBUG] admission checkOffline: structure failed: ${structReason}`);
    return { admitted: false, reason: structReason };
  }

  // 2. Signature verification
  const sigValid = await verifyCredentialSignature(cred);
  if (!sigValid) {
    return { admitted: false, reason: 'invalid signature' };
  }

  // 3. Expiry check
  const nowSec = Math.floor(Date.now() / 1000);
  if (cred.expiresAt <= nowSec) {
    return { admitted: false, reason: `credential expired (expiresAt=${cred.expiresAt}, now=${nowSec})` };
  }

  // 4. Endpoint ID binding (only for OTT credentials; policy credentials are not bound)
  if (!('credentialType' in cred)) {
    // EnrollmentCredential — strict binding
    const producer = cred as EnrollmentCredential;
    if (producer.endpointId !== peerEndpointId) {
      return { admitted: false, reason: `endpoint ID mismatch: credential=${producer.endpointId}, peer=${peerEndpointId}` };
    }
  }
  // For policy credentials, endpoint_id is informational only — no binding check

  // 5. OTT nonce consumption
  if ('credentialType' in cred) {
    const consumer = cred as ConsumerEnrollmentCredential;
    if (consumer.credentialType === 'ott') {
      if (consumer.endpointId != null && consumer.endpointId !== peerEndpointId) {
        return { admitted: false, reason: `OTT endpoint ID mismatch: credential=${consumer.endpointId}, peer=${peerEndpointId}` };
      }
      if (!nonceStore) {
        return { admitted: false, reason: 'OTT credential presented but no nonceStore configured' };
      }
      const nonceHex = consumer.nonce!;
      if (nonceStore.has(nonceHex)) {
        return { admitted: false, reason: 'OTT nonce already consumed' };
      }
      nonceStore.consume(nonceHex);
    }
  }

  return { admitted: true, attributes: { ...cred.attributes } };
}

// ── Runtime checks ──────────────────────────────────────────────────────────

/**
 * Runtime admission checks (IID verification).
 * Only runs if aster.iid_provider is present in attributes.
 */
export async function checkRuntime(
  cred: EnrollmentCredential | ConsumerEnrollmentCredential,
  iidBackend?: IIDBackend,
  iidToken?: string,
): Promise<AdmissionResult> {
  const [ok, reason] = await verifyIID(cred.attributes, iidBackend, iidToken);
  if (!ok) {
    return { admitted: false, reason };
  }
  return { admitted: true, attributes: { ...cred.attributes } };
}

// ── Orchestrator ────────────────────────────────────────────────────────────

/** Options for the admit() orchestrator. */
export interface AdmitOptions {
  nonceStore?: NonceStore;
  iidBackend?: IIDBackend;
  iidToken?: string;
}

/**
 * Orchestrate offline + runtime admission checks.
 * Fails fast: if offline checks fail, runtime checks are skipped.
 * Refusal reason is logged but never sent to peer.
 */
export async function admit(
  cred: EnrollmentCredential | ConsumerEnrollmentCredential,
  peerEndpointId: string,
  opts?: AdmitOptions,
): Promise<AdmissionResult> {
  const offline = await checkOffline(cred, peerEndpointId, opts?.nonceStore);
  if (!offline.admitted) return offline;

  const runtime = await checkRuntime(cred, opts?.iidBackend, opts?.iidToken);
  if (!runtime.admitted) return runtime;

  return { admitted: true, attributes: offline.attributes };
}

// ── Legacy compat (kept for existing callers) ───────────────────────────────

/**
 * @deprecated Use checkOffline + checkRuntime via admit() instead.
 * Legacy: checks only pubkey match + expiry (no signature verification).
 */
export async function verifyConsumerCredential(
  cred: ConsumerEnrollmentCredential,
  expectedRootPubkey: string,
): Promise<AdmissionResult> {
  if (cred.rootPubkey !== expectedRootPubkey) {
    return { admitted: false, reason: 'root pubkey mismatch' };
  }
  const nowSec = Math.floor(Date.now() / 1000);
  if (cred.expiresAt > 0 && cred.expiresAt <= nowSec) {
    return { admitted: false, reason: 'credential expired' };
  }
  return { admitted: true, attributes: { ...cred.attributes } };
}

/**
 * @deprecated Use checkOffline + checkRuntime via admit() instead.
 * Legacy: checks only pubkey match + expiry (no signature verification).
 */
export async function verifyProducerCredential(
  cred: EnrollmentCredential,
  expectedRootPubkey: string,
): Promise<AdmissionResult> {
  if (cred.rootPubkey !== expectedRootPubkey) {
    return { admitted: false, reason: 'root pubkey mismatch' };
  }
  const nowSec = Math.floor(Date.now() / 1000);
  if (cred.expiresAt > 0 && cred.expiresAt <= nowSec) {
    return { admitted: false, reason: 'credential expired' };
  }
  return { admitted: true, attributes: { ...cred.attributes } };
}
