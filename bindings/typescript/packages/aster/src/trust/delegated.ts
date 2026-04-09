/**
 * aster/trust/delegated -- Delegated admission via @aster-issued enrollment tokens.
 *
 * Implements the `aster.admission` ALPN handler (Aster-trust-spec S3a).
 *
 * Protocol:
 *   1. Consumer sends: EnrollmentToken (JSON) + SigningKeyAttestation (JSON)
 *   2. Service verifies: attestation -> token -> service binding
 *   3. Service sends: AdmissionChallenge (32-byte nonce + service identity)
 *   4. Consumer sends: AdmissionProof (signature over challenge with root key)
 *   5. Service verifies: proof of possession
 *   6. Service admits with: {handle, roles}
 *
 * All crypto is ed25519. No network calls -- verification uses cached
 * attestations and the @aster root pubkey received at publish time.
 */

import { RpcError, StatusCode } from '../status.js';
import { verify, hexToBytes, bytesToHex, loadPublicKey, ATTR_ROLE } from './credentials.js';
import type { MeshEndpointHook } from './hooks.js';
import type { PeerAttributeStore } from '../peer-store.js';
import { createPeerAdmission } from '../peer-store.js';

// ── Wire types for the admission protocol ──────────────────────────────────

/** Attestation binding a signing key to the @aster root key. */
export interface SigningKeyAttestation {
  signingPubkey: string;
  keyId: string;
  validFrom: number;
  validUntil: number;
  rootSignature: string;
}

/** @aster-issued token granting a consumer access to a service. */
export interface EnrollmentToken {
  consumerHandle: string;
  consumerPubkey: string;
  targetHandle: string;
  targetService: string;
  targetContractId: string;
  roles: string[];
  issuedAt: number;
  expiresAt: number;
  signingKeyId: string;
  signature: string;
}

/** Service-side policy for verifying delegated tokens. */
export interface DelegatedAdmissionPolicy {
  targetHandle: string;
  targetService: string;
  targetContractId: string;
  /** Hex-encoded @aster root public key -- the trust anchor. */
  asterRootPubkey: string;
}

/** Result of a successful delegated admission. */
export interface DelegatedAdmissionResult {
  admitted: boolean;
  handle: string;
  roles: string[];
  consumerPubkey: string;
}

// ── Canonical JSON for signature verification ──────────────────────────────

const encoder = new TextEncoder();

/** Deterministic JSON encoding for signature verification. */
function canonicalJsonBytes(obj: Record<string, unknown>): Uint8Array {
  const sorted = Object.keys(obj).sort();
  const ordered: Record<string, unknown> = {};
  for (const k of sorted) ordered[k] = obj[k];
  return encoder.encode(JSON.stringify(ordered));
}

// ── Verification functions ─────────────────────────────────────────────────

/**
 * Verify a signing-key attestation against the @aster root pubkey.
 * Throws RpcError on failure.
 */
export async function verifyAttestation(
  attestation: SigningKeyAttestation,
  opts: { asterRootPubkeyHex: string; now?: number },
): Promise<void> {
  const now = opts.now ?? Math.floor(Date.now() / 1000);

  const payload: Record<string, unknown> = {
    signing_pubkey: attestation.signingPubkey,
    key_id: attestation.keyId,
    valid_from: attestation.validFrom,
    valid_until: attestation.validUntil,
  };

  try {
    const rootPubkey = loadPublicKey(hexToBytes(opts.asterRootPubkeyHex));
    const valid = await verify(
      rootPubkey,
      canonicalJsonBytes(payload),
      hexToBytes(attestation.rootSignature),
    );
    if (!valid) {
      throw new Error('signature invalid');
    }
  } catch (cause) {
    throw new RpcError(
      StatusCode.UNAUTHENTICATED,
      'signing-key attestation verification failed',
    );
  }

  if (!(attestation.validFrom <= now && now <= attestation.validUntil)) {
    throw new RpcError(
      StatusCode.UNAUTHENTICATED,
      'signing-key attestation is not currently valid',
    );
  }
}

/**
 * Verify an enrollment token against the attestation and service policy.
 * Throws RpcError on failure.
 */
export async function verifyToken(
  token: EnrollmentToken,
  attestation: SigningKeyAttestation,
  opts: { policy: DelegatedAdmissionPolicy; now?: number },
): Promise<void> {
  const now = opts.now ?? Math.floor(Date.now() / 1000);

  // Token must reference the correct signing key
  if (token.signingKeyId !== attestation.keyId) {
    throw new RpcError(
      StatusCode.UNAUTHENTICATED,
      'token signing key does not match attestation',
    );
  }

  // Verify token signature against the attested signing key
  const tokenPayload: Record<string, unknown> = {
    consumer_handle: token.consumerHandle,
    consumer_pubkey: token.consumerPubkey,
    target_handle: token.targetHandle,
    target_service: token.targetService,
    target_contract_id: token.targetContractId,
    roles: token.roles,
    issued_at: token.issuedAt,
    expires_at: token.expiresAt,
    signing_key_id: token.signingKeyId,
  };

  try {
    const signingPubkey = loadPublicKey(hexToBytes(attestation.signingPubkey));
    const valid = await verify(
      signingPubkey,
      canonicalJsonBytes(tokenPayload),
      hexToBytes(token.signature),
    );
    if (!valid) {
      throw new Error('signature invalid');
    }
  } catch (cause) {
    if (cause instanceof RpcError) throw cause;
    throw new RpcError(
      StatusCode.UNAUTHENTICATED,
      'enrollment token signature verification failed',
    );
  }

  // Token expiry
  if (!(token.issuedAt <= now && now <= token.expiresAt)) {
    throw new RpcError(
      StatusCode.UNAUTHENTICATED,
      'enrollment token has expired',
    );
  }

  // Service binding -- all three must match
  if (token.targetHandle !== opts.policy.targetHandle) {
    throw new RpcError(StatusCode.PERMISSION_DENIED, 'token targets a different handle');
  }
  if (token.targetService !== opts.policy.targetService) {
    throw new RpcError(StatusCode.PERMISSION_DENIED, 'token targets a different service');
  }
  if (token.targetContractId !== opts.policy.targetContractId) {
    throw new RpcError(StatusCode.PERMISSION_DENIED, 'token targets a different contract');
  }
}

/**
 * Verify the consumer's proof of possession of their root key.
 * Throws RpcError on failure.
 */
export async function verifyProofOfPossession(opts: {
  consumerPubkeyHex: string;
  challengeBytes: Uint8Array;
  signatureHex: string;
}): Promise<void> {
  try {
    const consumerPubkey = loadPublicKey(hexToBytes(opts.consumerPubkeyHex));
    const valid = await verify(
      consumerPubkey,
      opts.challengeBytes,
      hexToBytes(opts.signatureHex),
    );
    if (!valid) {
      throw new Error('signature invalid');
    }
  } catch (cause) {
    if (cause instanceof RpcError) throw cause;
    throw new RpcError(
      StatusCode.UNAUTHENTICATED,
      'admission proof verification failed',
    );
  }
}

/** Build the challenge payload that the consumer must sign. */
export function buildChallengeBytes(
  nonce: Uint8Array,
  targetHandle: string,
  targetService: string,
  alpn = 'aster.admission',
): Uint8Array {
  const parts = [
    nonce,
    encoder.encode(targetHandle),
    encoder.encode(targetService),
    encoder.encode(alpn),
  ];
  const totalLen = parts.reduce((sum, p) => sum + p.length, 0);
  const result = new Uint8Array(totalLen);
  let offset = 0;
  for (const p of parts) {
    result.set(p, offset);
    offset += p.length;
  }
  return result;
}

// ── Connection handler ─────────────────────────────────────────────────────

/**
 * Bidirectional stream abstraction expected by the handler.
 * Matches the shape provided by Iroh QUIC connections.
 */
export interface BiStream {
  send: {
    writeAll(data: Uint8Array): Promise<void>;
    finish(): Promise<void>;
  };
  recv: {
    readToEnd(maxBytes: number): Promise<Uint8Array | null>;
  };
}

/**
 * Connection abstraction expected by the handler.
 */
export interface AdmissionConnection {
  remoteId(): string;
  acceptBi(): Promise<BiStream>;
}

/**
 * Handle one connection on the aster.admission ALPN.
 *
 * Runs the 6-step verification protocol:
 * 1. Read token + attestation from consumer
 * 2. Verify attestation against @aster root key
 * 3. Verify token against attestation + service binding
 * 4. Send challenge nonce
 * 5. Read proof of possession
 * 6. Verify proof, admit consumer
 */
export async function handleDelegatedAdmissionConnection(
  conn: AdmissionConnection,
  opts: {
    policy: DelegatedAdmissionPolicy;
    hook?: MeshEndpointHook;
    peerStore?: PeerAttributeStore;
  },
): Promise<void> {
  const peerId = conn.remoteId();
  const now = Math.floor(Date.now() / 1000);

  try {
    const { send, recv } = await conn.acceptBi();

    // Step 1: Read token + attestation
    const raw = await recv.readToEnd(64 * 1024);
    if (!raw || raw.length === 0) {
      console.warn(`delegated admission: empty request from ${peerId}`);
      return;
    }

    let request: Record<string, any>;
    try {
      request = JSON.parse(new TextDecoder().decode(raw));
    } catch {
      console.warn(`delegated admission: malformed JSON from ${peerId}`);
      await sendReject(send, 'malformed request');
      return;
    }

    // Parse token and attestation from wire format (snake_case) to TS (camelCase)
    const tokenData = request.token ?? {};
    const attData = request.attestation ?? {};

    const token: EnrollmentToken = {
      consumerHandle: tokenData.consumer_handle ?? '',
      consumerPubkey: tokenData.consumer_pubkey ?? '',
      targetHandle: tokenData.target_handle ?? '',
      targetService: tokenData.target_service ?? '',
      targetContractId: tokenData.target_contract_id ?? '',
      roles: tokenData.roles ?? [],
      issuedAt: tokenData.issued_at ?? 0,
      expiresAt: tokenData.expires_at ?? 0,
      signingKeyId: tokenData.signing_key_id ?? '',
      signature: tokenData.signature ?? '',
    };

    const attestation: SigningKeyAttestation = {
      signingPubkey: attData.signing_pubkey ?? '',
      keyId: attData.key_id ?? '',
      validFrom: attData.valid_from ?? 0,
      validUntil: attData.valid_until ?? 0,
      rootSignature: attData.root_signature ?? '',
    };

    // Steps 2-3: Verify attestation and token
    try {
      await verifyAttestation(attestation, {
        asterRootPubkeyHex: opts.policy.asterRootPubkey,
        now,
      });
      await verifyToken(token, attestation, { policy: opts.policy, now });
    } catch (e) {
      if (e instanceof RpcError) {
        console.info(`delegated admission: denied ${peerId}: ${e.message}`);
        await sendReject(send, e.message);
        return;
      }
      throw e;
    }

    // Step 4: Send challenge
    const nonce = crypto.getRandomValues(new Uint8Array(32));
    const challenge = {
      nonce: bytesToHex(nonce),
      target_handle: opts.policy.targetHandle,
      target_service: opts.policy.targetService,
    };
    await send.writeAll(encoder.encode(JSON.stringify(challenge)));

    // Step 5: Read proof
    const proofRaw = await recv.readToEnd(4096);
    if (!proofRaw || proofRaw.length === 0) {
      console.warn(`delegated admission: no proof from ${peerId}`);
      return;
    }

    let proof: Record<string, any>;
    try {
      proof = JSON.parse(new TextDecoder().decode(proofRaw));
    } catch {
      await sendReject(send, 'malformed proof');
      return;
    }

    // Step 6: Verify proof of possession
    const challengeBytes = buildChallengeBytes(
      nonce,
      opts.policy.targetHandle,
      opts.policy.targetService,
    );
    try {
      await verifyProofOfPossession({
        consumerPubkeyHex: token.consumerPubkey,
        challengeBytes,
        signatureHex: proof.signature ?? '',
      });
    } catch (e) {
      if (e instanceof RpcError) {
        console.info(`delegated admission: proof failed ${peerId}: ${e.message}`);
        await sendReject(send, e.message);
        return;
      }
      throw e;
    }

    // Admitted!
    console.info(
      `delegated admission: admitted ${peerId} (handle=${token.consumerHandle}, roles=${token.roles.join(',')})`,
    );

    // Store admission attributes
    if (opts.peerStore) {
      opts.peerStore.admit(
        createPeerAdmission({
          endpointId: peerId,
          handle: token.consumerHandle,
          attributes: new Map([[ATTR_ROLE, token.roles.join(',')]]),
          admissionPath: 'aster.admission',
        }),
      );
    }

    // Add to Gate 0 allowlist
    if (opts.hook) {
      opts.hook.addPeer(peerId);
    }

    // Send success response
    const result = {
      admitted: true,
      handle: token.consumerHandle,
      roles: token.roles,
    };
    await send.writeAll(encoder.encode(JSON.stringify(result)));
    await send.finish();
  } catch (err: unknown) {
    // Cancellation -- silently exit
    if (err instanceof Error && (err.name === 'AbortError' || err.message.includes('cancel'))) {
      return;
    }
    console.error(`delegated admission: error for ${peerId}:`, err);
  }
}

/** Send a rejection response and finish the stream. */
async function sendReject(
  send: { writeAll(data: Uint8Array): Promise<void>; finish(): Promise<void> },
  reason: string,
): Promise<void> {
  try {
    const result = JSON.stringify({ admitted: false, reason });
    await send.writeAll(encoder.encode(result));
    await send.finish();
  } catch {
    // Best-effort
  }
}
