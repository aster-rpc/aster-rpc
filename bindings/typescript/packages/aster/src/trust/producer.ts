/**
 * Producer admission — the server-side admission handshake.
 *
 * Spec reference: Aster-trust-spec.md S6
 *
 * When a producer wants to join a mesh, the accepting producer:
 * 1. Accepts a connection on the producer admission ALPN
 * 2. Reads an AdmissionRequest (credential JSON + optional IID token)
 * 3. Validates the credential (signature, expiry, root pubkey)
 * 4. Responds with AdmissionResponse (accepted/rejected + mesh state)
 */

import type { EnrollmentCredential } from './credentials.js';
import { verifyProducerCredential } from './admission.js';
import type { MeshState } from './mesh.js';
import { MAX_ADMISSION_PAYLOAD_SIZE } from '../limits.js';
import type { NonceStore } from './nonce.js';

/** ALPN for producer admission. */
export const PRODUCER_ADMISSION_ALPN = new TextEncoder().encode('aster.producer_admission');

/** Producer admission request (inbound). */
export interface ProducerAdmissionRequest {
  credentialJson: string;
  iidToken?: string;
}

/** Producer admission response (outbound). */
export interface ProducerAdmissionResponse {
  accepted: boolean;
  salt: string; // hex
  acceptedProducers: string[];
  /** Reason is for internal logging only — never leaked to peer. */
  reason?: string;
}

/** Options for the producer admission server. */
export interface ProducerAdmissionOptions {
  /** The mesh's trusted root public key (hex). */
  rootPubkey: string;
  /** Current mesh state (mutated on successful admission). */
  meshState: MeshState;
  /** Optional nonce store for OTT replay protection. */
  nonceStore?: NonceStore;
  /** Logger. */
  logger?: { info(...args: any[]): void; warn(...args: any[]): void; error(...args: any[]): void };
}

/** QUIC connection interface for admission. */
interface AdmissionConnection {
  acceptBi(): Promise<{ takeSend(): AdmissionSend; takeRecv(): AdmissionRecv }>;
  remoteNodeId(): string;
}

interface AdmissionSend {
  writeAll(data: Uint8Array): Promise<void>;
  finish(): Promise<void>;
}

interface AdmissionRecv {
  readToEnd(maxLen: number): Promise<Uint8Array>;
}

/** Node interface for accepting admission connections. */
interface AdmissionNode {
  acceptAster(): Promise<AdmissionConnection>;
}

/**
 * Handle a single producer admission connection.
 *
 * Reads the credential, validates it, updates mesh state on success,
 * and responds with the admission result.
 */
export async function handleProducerAdmission(
  conn: AdmissionConnection,
  opts: ProducerAdmissionOptions,
): Promise<ProducerAdmissionResponse> {
  const peerNodeId = conn.remoteNodeId();
  const log = opts.logger ?? console;

  const bi = await conn.acceptBi();
  const send = bi.takeSend();
  const recv = bi.takeRecv();

  try {
    // Read request
    const raw = await recv.readToEnd(MAX_ADMISSION_PAYLOAD_SIZE);
    const text = new TextDecoder().decode(raw);
    let request: ProducerAdmissionRequest;

    try {
      request = JSON.parse(text);
    } catch {
      const resp = buildDenied('malformed request');
      await sendResponse(send, resp);
      return resp;
    }

    // Parse credential
    let cred: EnrollmentCredential;
    try {
      cred = JSON.parse(request.credentialJson);
    } catch {
      const resp = buildDenied('malformed credential JSON');
      await sendResponse(send, resp);
      return resp;
    }

    // Verify root pubkey match
    if (cred.rootPubkey !== opts.rootPubkey) {
      log.warn?.('admission: root pubkey mismatch', {
        peer: peerNodeId.slice(0, 8),
      });
      const resp = buildDenied('root pubkey mismatch');
      await sendResponse(send, resp);
      return resp;
    }

    // Verify credential (expiry, signature)
    const result = await verifyProducerCredential(cred, opts.rootPubkey);
    if (!result.admitted) {
      log.warn?.('admission: denied', {
        peer: peerNodeId.slice(0, 8),
        reason: result.reason,
      });
      const resp = buildDenied(result.reason ?? 'denied');
      await sendResponse(send, resp);
      return resp;
    }

    // Verify endpoint ID binding
    if (cred.endpointId && cred.endpointId !== peerNodeId) {
      log.warn?.('admission: endpoint ID mismatch', {
        peer: peerNodeId.slice(0, 8),
      });
      const resp = buildDenied('endpoint ID mismatch');
      await sendResponse(send, resp);
      return resp;
    }

    // Check nonce if OTT credential
    if (opts.nonceStore && cred.signature) {
      const nonceHex = cred.signature.slice(0, 64); // use signature prefix as nonce key
      if (opts.nonceStore.has(nonceHex)) {
        const resp = buildDenied('nonce already consumed');
        await sendResponse(send, resp);
        return resp;
      }
      opts.nonceStore.consume(nonceHex);
    }

    // Admission successful — update mesh state
    opts.meshState.addPeer(peerNodeId);
    log.info?.('admission: accepted producer', {
      peer: peerNodeId.slice(0, 8),
    });

    const resp: ProducerAdmissionResponse = {
      accepted: true,
      salt: '', // mesh salt (would be populated from mesh state)
      acceptedProducers: opts.meshState.allPeers(),
      reason: '', // never sent on wire
    };
    await sendResponse(send, resp);
    return resp;
  } catch (e) {
    log.error?.('admission: error', { error: String(e), peer: peerNodeId.slice(0, 8) });
    const resp = buildDenied('internal error');
    try { await sendResponse(send, resp); } catch { /* best effort */ }
    return resp;
  }
}

/**
 * Serve producer admission — accept loop that handles incoming
 * producer admission connections until stopped.
 */
export async function serveProducerAdmission(
  node: AdmissionNode,
  opts: ProducerAdmissionOptions & { running?: { value: boolean } },
): Promise<void> {
  const running = opts.running ?? { value: true };
  const log = opts.logger ?? console;

  while (running.value) {
    try {
      const conn = await node.acceptAster();
      handleProducerAdmission(conn, opts).catch(e => {
        log.error?.('admission connection error', { error: String(e) });
      });
    } catch (e) {
      if (!running.value) break;
      log.error?.('admission accept error', { error: String(e) });
    }
  }
}

// -- Helpers --

function buildDenied(reason: string): ProducerAdmissionResponse {
  return {
    accepted: false,
    salt: '',
    acceptedProducers: [],
    reason, // internal only — wire response strips this
  };
}

async function sendResponse(send: AdmissionSend, resp: ProducerAdmissionResponse): Promise<void> {
  // Never leak reason to peer (oracle protection)
  const wireResp = { ...resp, reason: '' };
  const bytes = new TextEncoder().encode(JSON.stringify(wireResp));
  await send.writeAll(bytes);
  await send.finish();
}
