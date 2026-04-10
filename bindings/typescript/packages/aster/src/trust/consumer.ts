/**
 * Consumer admission — client-side and server-side admission handshake.
 *
 * Spec reference: Aster-trust-spec.md S5
 *
 * Client side (performAdmission):
 *   1. Open a stream on the admission ALPN
 *   2. Send a ConsumerAdmissionRequest (credential + optional IID token)
 *   3. Receive a ConsumerAdmissionResponse (services list + registry namespace)
 *
 * Server side (handleConsumerAdmissionRpc, serveConsumerAdmission):
 *   Verify credential, admit peer, return response with services + registry namespace.
 *
 * Wire format: newline-delimited JSON over a QUIC bidi-stream on
 * aster.consumer_admission ALPN. Client sends one JSON line; server
 * responds with one JSON line and closes the stream.
 */

import { bytesToHex, type ConsumerEnrollmentCredential } from './credentials.js';
import { admit } from './admission.js';
import type { MeshEndpointHook } from './hooks.js';
import { MAX_ADMISSION_PAYLOAD_SIZE, MAX_SERVICES_IN_ADMISSION, validateHexField } from '../limits.js';
import type { NonceStore } from './nonce.js';
import { createPeerAdmission, type PeerAttributeStore } from '../peer-store.js';

/**
 * Service summary returned in admission response.
 *
 * NOTE: Wire format uses snake_case keys (contract_id) for Python interop.
 * The camelCase interface fields here need mapping when parsing from wire.
 * TODO: Add proper wire-format mapping for ServiceSummary (pre-existing gap).
 */
export interface ServiceSummary {
  name: string;
  version: number;
  contractId: string;
  pattern: string;
  methods: string[];
  channels?: Record<string, string>;
}

/** Consumer admission request. */
export interface ConsumerAdmissionRequest {
  credentialJson: string;
  iidToken?: string;
}

/** Consumer admission response from producer. */
export interface ConsumerAdmissionResponse {
  admitted: boolean;
  reason?: string;
  services: ServiceSummary[];
  registryNamespace?: string;
  attributes?: Record<string, string>;
  rootPubkey?: string;
  /** Hex-encoded 32-byte gossip topic — only populated for root node. */
  gossipTopic?: string;
}

/** Options for server-side consumer admission handlers. */
export interface ConsumerAdmissionOpts {
  nonceStore?: NonceStore;
  services?: ServiceSummary[];
  registryNamespace?: string;
  allowUnenrolled?: boolean;
  /** Gossip topic ID (32 bytes). Included in response only for root node. */
  gossipTopicId?: Uint8Array;
  /** When supplied, successful admissions are recorded so the RPC dispatch
   *  layer can read the peer's attributes from CallContext. */
  peerStore?: PeerAttributeStore;
  logger?: { info(msg: string, ...args: unknown[]): void; warn(msg: string, ...args: unknown[]): void; error(msg: string, ...args: unknown[]): void };
}

// ── Client-side ───────────────────────────────────────────────────────────────

/**
 * Perform the consumer admission handshake.
 *
 * @param connection - The QUIC connection to the producer (admission ALPN)
 * @param credential - The consumer enrollment credential
 * @param iidToken - Optional cloud instance identity token
 * @returns The admission response with services and registry namespace
 */
export async function performAdmission(
  connection: { openBi(): Promise<{ takeSend(): any; takeRecv(): any }> },
  credential: ConsumerEnrollmentCredential,
  iidToken?: string,
): Promise<ConsumerAdmissionResponse> {
  const bi = await connection.openBi();
  const send = bi.takeSend();
  const recv = bi.takeRecv();

  // Build and send request (snake_case keys for Python interop).
  // The credential is serialised to its snake_case wire form via
  // consumerCredToJson — JSON.stringify on the camelCase TS interface
  // would produce the wrong keys and the server would reject it.
  const wireRequest = {
    credential_json: credential ? consumerCredToJson(credential) : '',
    iid_token: iidToken ?? '',
  };
  const reqBytes = new TextEncoder().encode(JSON.stringify(wireRequest));
  if (reqBytes.byteLength > MAX_ADMISSION_PAYLOAD_SIZE) {
    throw new Error(`admission request too large: ${reqBytes.byteLength} > ${MAX_ADMISSION_PAYLOAD_SIZE}`);
  }

  // Write request + finish send side
  await send.writeAll(reqBytes);
  await send.finish();

  // Read response (snake_case keys from Python wire format)
  const respBytes = await recv.readToEnd(MAX_ADMISSION_PAYLOAD_SIZE);
  const respText = new TextDecoder().decode(respBytes);
  if (!respText || respText.length === 0) {
    throw new Error('admission failed: server returned empty response (may be overloaded, retry)');
  }
  const d = JSON.parse(respText);
  const response: ConsumerAdmissionResponse = {
    admitted: d.admitted,
    reason: d.reason,
    services: d.services ?? [],
    registryNamespace: d.registry_namespace ?? d.registryNamespace ?? '',
    attributes: d.attributes ?? {},
    rootPubkey: d.root_pubkey ?? d.rootPubkey ?? '',
    gossipTopic: d.gossip_topic ?? d.gossipTopic,
  };

  // Validate
  if (response.services && response.services.length > MAX_SERVICES_IN_ADMISSION) {
    throw new Error(`admission response has ${response.services.length} services, max is ${MAX_SERVICES_IN_ADMISSION}`);
  }

  return response;
}

// ── Credential serialisation helpers ──────────────────────────────────────────

/**
 * Serialise a ConsumerEnrollmentCredential to the wire JSON format.
 * Hex-encodes rootPubkey, nonce, and signature fields.
 */
export function consumerCredToJson(cred: ConsumerEnrollmentCredential): string {
  return JSON.stringify({
    credential_type: cred.credentialType,
    root_pubkey: cred.rootPubkey,  // already hex in TS type
    expires_at: cred.expiresAt,
    attributes: cred.attributes,
    endpoint_id: cred.endpointId ?? null,
    nonce: cred.nonce ?? null,
    signature: cred.signature,
  });
}

/**
 * Deserialise a ConsumerEnrollmentCredential from the wire JSON format.
 * Validates hex field lengths (pubkey=64, nonce=64, signature=128 hex chars).
 */
export function consumerCredFromJson(json: string): ConsumerEnrollmentCredential {
  const d = JSON.parse(json);

  // Validate hex field lengths
  validateHexField('root_pubkey', d.root_pubkey ?? '');
  const nonceHex: string = d.nonce ?? '';
  if (nonceHex) {
    validateHexField('nonce', nonceHex);
  }
  const sigHex: string = d.signature ?? '';
  if (sigHex) {
    validateHexField('signature', sigHex);
  }
  const eid: string = d.endpoint_id ?? '';
  if (eid) {
    validateHexField('endpoint_id', eid);
  }

  return {
    credentialType: d.credential_type,
    rootPubkey: d.root_pubkey,
    expiresAt: Number(d.expires_at),
    attributes: d.attributes ?? {},
    endpointId: d.endpoint_id || undefined,
    nonce: nonceHex || undefined,
    signature: sigHex || '',
  };
}

// ── Server-side handler ─────────────────────────────────────────────────────

/**
 * Server-side handler for the aster.consumer_admission ALPN.
 *
 * @param requestJson - JSON-serialised ConsumerAdmissionRequest.
 * @param rootPubkey - The server's root public key (hex string, 64 chars).
 * @param hook - MeshEndpointHook; addPeer is called on successful admission.
 * @param peerNodeId - QUIC peer identity from the connection handshake.
 * @param opts - Additional options (nonceStore, services, registryNamespace, allowUnenrolled, logger).
 * @returns ConsumerAdmissionResponse — always returned, never throws.
 */
export async function handleConsumerAdmissionRpc(
  requestJson: string,
  rootPubkey: string,
  hook: MeshEndpointHook,
  peerNodeId: string,
  opts: ConsumerAdmissionOpts = {},
): Promise<ConsumerAdmissionResponse> {
  const log = opts.logger ?? console;
  const denied: ConsumerAdmissionResponse = {
    admitted: false,
    reason: '', // oracle protection — never leak reason on wire
    services: [],
    rootPubkey,
  };

  // Parse the outer request envelope
  let req: ConsumerAdmissionRequest;
  try {
    const parsed = JSON.parse(requestJson);
    req = {
      credentialJson: parsed.credentialJson ?? parsed.credential_json ?? '',
      iidToken: parsed.iidToken ?? parsed.iid_token ?? '',
    };
  } catch (err) {
    log.warn(`consumer admission: malformed request from ${peerNodeId}: ${err}`);
    return denied;
  }

  // Include gossip topic only when the connecting peer IS the root node
  // (its endpoint_id == root_pubkey hex). This lets the operator's shell
  // observe the producer mesh without exposing the topic to other consumers.
  let topicForPeer = '';
  if (opts.gossipTopicId && peerNodeId === rootPubkey) {
    topicForPeer = bytesToHex(opts.gossipTopicId);
    log.info('consumer admission: root node detected — including gossip topic');
  }

  // Dev mode / open gate: empty credential -> auto-admit
  const isEmptyCredential = !req.credentialJson || req.credentialJson === '{}' || req.credentialJson === 'null';
  if (isEmptyCredential && opts.allowUnenrolled) {
    hook.addPeer(peerNodeId);
    if (opts.peerStore) {
      opts.peerStore.admit(createPeerAdmission({
        endpointId: peerNodeId,
        attributes: new Map(),
        admissionPath: 'aster.consumer_admission',
      }));
    }
    const role = topicForPeer ? 'root' : 'open gate';
    log.info(`consumer admission: auto-admitted ${peerNodeId} (${role})`);
    return {
      admitted: true,
      attributes: {},
      services: opts.services ?? [],
      registryNamespace: opts.registryNamespace ?? '',
      rootPubkey,
      gossipTopic: topicForPeer || undefined,
      reason: '',
    };
  }

  // Parse the inner credential
  let cred: ConsumerEnrollmentCredential;
  try {
    cred = consumerCredFromJson(req.credentialJson);
  } catch (err) {
    log.warn(`consumer admission: malformed credential from ${peerNodeId}: ${err}`);
    return denied;
  }

  // Trust anchor check: credential's rootPubkey must match server's
  if (!cred.rootPubkey || cred.rootPubkey !== rootPubkey) {
    log.warn(
      `consumer admission: untrusted root key from ${peerNodeId} ` +
      `(got ${(cred.rootPubkey ?? '(none)').slice(0, 12)}, expected ${rootPubkey.slice(0, 12)})`,
    );
    return denied;
  }

  // Run admission checks (offline + runtime)
  const result = await admit(cred, peerNodeId, {
    nonceStore: opts.nonceStore,
    iidToken: req.iidToken || undefined,
  });

  if (!result.admitted) {
    log.info(`consumer admission: denied ${peerNodeId}`);
    return denied;
  }

  hook.addPeer(peerNodeId);
  if (opts.peerStore) {
    const attrMap = new Map<string, string>();
    for (const [k, v] of Object.entries(result.attributes ?? {})) {
      attrMap.set(k, String(v));
    }
    opts.peerStore.admit(createPeerAdmission({
      endpointId: peerNodeId,
      attributes: attrMap,
      admissionPath: 'aster.consumer_admission',
    }));
  }
  log.info(`consumer admission: admitted ${peerNodeId}`);

  return {
    admitted: true,
    attributes: result.attributes ?? {},
    services: opts.services ?? [],
    registryNamespace: opts.registryNamespace ?? '',
    rootPubkey,
    gossipTopic: topicForPeer || undefined,
    reason: '',
  };
}

// ── Per-connection handler ──────────────────────────────────────────────────

/**
 * Handle one consumer admission connection: read request, write response.
 *
 * @param conn - A QUIC connection with acceptBi() and remoteId() methods.
 * @param rootPubkey - Hex-encoded root public key.
 * @param hook - MeshEndpointHook for peer admission tracking.
 * @param opts - Additional options.
 */
export async function handleConsumerAdmissionConnection(
  conn: {
    acceptBi(): Promise<{ takeSend(): any; takeRecv(): any }>;
    remoteId(): string;
  },
  rootPubkey: string,
  hook: MeshEndpointHook,
  opts: ConsumerAdmissionOpts = {},
): Promise<void> {
  const peerNodeId = conn.remoteId();
  const log = opts.logger ?? console;
  try {
    const bi = await conn.acceptBi();
    const send = bi.takeSend();
    const recv = bi.takeRecv();

    const raw: Uint8Array = await recv.readToEnd(MAX_ADMISSION_PAYLOAD_SIZE);
    if (!raw || raw.length === 0) {
      log.warn(`consumer admission: empty request from ${peerNodeId}`);
      return;
    }

    const requestJson = new TextDecoder().decode(raw);

    const response = await handleConsumerAdmissionRpc(
      requestJson,
      rootPubkey,
      hook,
      peerNodeId,
      opts,
    );

    // Serialise response — snake_case keys for Python interop.
    // Strip reason on wire (oracle protection).
    // Convert ServiceSummary to snake_case for wire compat
    const wireServices = (response.services ?? []).map((s: any) => ({
      name: s.name,
      version: s.version,
      contract_id: s.contractId ?? s.contract_id ?? '',
      pattern: s.pattern,
      methods: s.methods,
      channels: s.channels ?? {},
    }));
    const wireResponse: Record<string, unknown> = {
      admitted: response.admitted,
      attributes: response.attributes ?? {},
      services: wireServices,
      registry_namespace: response.registryNamespace ?? '',
      root_pubkey: response.rootPubkey ?? '',
      reason: '', // never leak reason on wire
    };
    if (response.gossipTopic) {
      wireResponse.gossip_topic = response.gossipTopic;
    }

    const responseBytes = new TextEncoder().encode(JSON.stringify(wireResponse));
    try {
      await send.writeAll(responseBytes);
      await send.finish();
    } catch (writeErr) {
      log.warn(`consumer admission: failed to send response to ${peerNodeId}: ${writeErr}`);
      // Stream write failed — client will see an empty/closed stream.
      // Nothing more we can do; the connection may already be gone.
    }
    // Don't conn.close() — let QUIC drain the streams naturally.
    // Calling close() sends CONNECTION_CLOSE which kills in-flight
    // data before the consumer can readToEnd().
  } catch (err) {
    log.warn(`consumer admission: error handling ${peerNodeId}: ${err}`);
  }
}

// ── Accept loop ─────────────────────────────────────────────────────────────

/**
 * Accept and process connections on aster.consumer_admission until cancelled.
 *
 * Runs as a background task alongside the main server. Each connection is
 * handled concurrently so one slow consumer cannot block others.
 *
 * @param node - An endpoint/node bound to the consumer admission ALPN, with accept().
 * @param rootPubkey - Hex-encoded root public key.
 * @param hook - MeshEndpointHook allowlist manager.
 * @param opts - Additional options.
 */
export async function serveConsumerAdmission(
  node: { accept(): Promise<any> },
  rootPubkey: string,
  hook: MeshEndpointHook,
  opts: ConsumerAdmissionOpts = {},
): Promise<void> {
  const log = opts.logger ?? console;
  try {
    while (true) {
      const conn = await node.accept();
      // Fire-and-forget: handle each connection concurrently
      handleConsumerAdmissionConnection(conn, rootPubkey, hook, opts).catch((err) => {
        log.warn(`consumer admission: connection handler error: ${err}`);
      });
    }
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    if (msg.includes('abort') || msg.includes('cancel')) return;
    log.error(`serveConsumerAdmission: unexpected error: ${msg}`);
  }
}
